//! The feed (docs/protocol.md §5): publishes the bridge's state after each
//! committed change and queues the events of every subscriber that follows what
//! changed.
//!
//! Each commit publishes every scope the bridge holds through the publisher
//! (`engine`): a scope that did not change publishes nothing, a scope that is new
//! or has a new epoch is a snapshot for its subscribers, and any other change is
//! the next rev as ops. A session the bridge no longer holds ends its scope. A
//! subscriber whose queue went past its budget is sent snapshots instead.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Notify;

use super::documents::{AgentInfo, End, EndReason, Scope};
use super::engine::{Commit, Engine, StreamEvent};
use super::live_doc::engine_input;
use super::project::held_sessions;
use super::subscription::{Subscribed, Subscriber};
use crate::session_registry::SessionRegistry;

pub(crate) type SubscriptionId = u64;

pub(crate) struct Feed {
    engine: Engine,
    /// The epoch `global` and `catalog` are published under.
    process_epoch: String,
    subscribers: BTreeMap<SubscriptionId, Subscriber>,
    next_id: SubscriptionId,
}

impl Feed {
    pub(crate) fn new(process_epoch: impl Into<String>) -> Self {
        Self {
            engine: Engine::default(),
            process_epoch: process_epoch.into(),
            subscribers: BTreeMap::new(),
            next_id: 1,
        }
    }

    /// Registers a subscriber and queues the alignment of every scope that is
    /// published; one that is not is aligned when it is.
    pub(crate) fn subscribe(&mut self, follow: &[Subscribed]) -> SubscriptionId {
        let id = self.next_id;
        self.next_id += 1;
        let mut subscriber = Subscriber::new(follow);
        subscriber.align(&self.engine);
        self.subscribers.insert(id, subscriber);
        id
    }

    pub(crate) fn unsubscribe(&mut self, id: SubscriptionId) {
        self.subscribers.remove(&id);
    }

    /// The next event to write to subscription `id`.
    pub(crate) fn next(&mut self, id: SubscriptionId) -> Option<StreamEvent> {
        self.subscribers.get_mut(&id)?.next()
    }

    pub(crate) fn subscribers(&self) -> usize {
        self.subscribers.len()
    }

    /// The version `scope` is published at.
    pub(crate) fn version(&self, scope: &str) -> Option<(String, u64)> {
        self.engine
            .version(scope)
            .map(|(epoch, rev)| (epoch.to_string(), rev))
    }

    /// The commit step: publishes every scope that changed since the last one.
    pub(crate) fn commit(
        &mut self,
        registry: &SessionRegistry,
        agent: &AgentInfo,
        catalog_revision: u64,
    ) {
        let mut scopes = vec![Scope::Global, Scope::Catalog];
        scopes.extend(held_sessions(registry).into_iter().map(Scope::Session));
        for scope in &scopes {
            let Some(input) = engine_input(
                registry,
                scope,
                agent,
                catalog_revision,
                &self.process_epoch,
            ) else {
                continue;
            };
            let name = scope.to_string();
            match self.engine.commit(&name, &input) {
                Commit::Unchanged => {}
                Commit::Ops(event) => {
                    for subscriber in self.subscribers.values_mut() {
                        subscriber.publish(&event);
                    }
                }
                Commit::Rebuilt => {
                    for subscriber in self.subscribers.values_mut() {
                        subscriber.rebuilt(&name, &self.engine);
                    }
                }
            }
        }
        // A session the bridge no longer holds is over.
        let gone: Vec<String> = self
            .engine
            .scopes()
            .filter(|published| published.starts_with("session/"))
            .filter(|published| !scopes.iter().any(|scope| scope.to_string() == *published))
            .map(String::from)
            .collect();
        for scope in gone {
            self.engine.remove(&scope);
            for subscriber in self.subscribers.values_mut() {
                subscriber.end(
                    &scope,
                    End {
                        reason: EndReason::Closed,
                        error: None,
                    },
                );
            }
        }
        for subscriber in self.subscribers.values_mut() {
            if subscriber.over_budget() {
                subscriber.coalesce(&self.engine);
            }
        }
    }
}

/// The feed shared between the bridge, which commits to it under its lock, and
/// the streams, which wait for what their subscription is queued.
#[derive(Clone)]
pub(crate) struct FeedHandle(Arc<Shared>);

struct Shared {
    feed: Mutex<Feed>,
    /// Wakes the streams waiting for an event.
    ready: Notify,
    closed: AtomicBool,
}

impl FeedHandle {
    pub(crate) fn new(process_epoch: impl Into<String>) -> Self {
        Self(Arc::new(Shared {
            feed: Mutex::new(Feed::new(process_epoch)),
            ready: Notify::new(),
            closed: AtomicBool::new(false),
        }))
    }

    fn feed(&self) -> MutexGuard<'_, Feed> {
        self.0.feed.lock().expect("the feed is poisoned")
    }

    /// Publishes what changed (see `Feed::commit`) and wakes the streams.
    pub(crate) fn commit(
        &self,
        registry: &SessionRegistry,
        agent: &AgentInfo,
        catalog_revision: u64,
    ) {
        self.feed().commit(registry, agent, catalog_revision);
        self.0.ready.notify_waiters();
    }

    /// A subscription to `follow`, aligned with what is published now. Once the
    /// feed is closed there is nothing to align or to wait for.
    pub(crate) fn subscribe(&self, follow: &[Subscribed]) -> FeedSubscription {
        let id = if self.0.closed.load(Ordering::SeqCst) {
            self.feed().subscribe(&[])
        } else {
            self.feed().subscribe(follow)
        };
        FeedSubscription(Arc::new(SubscriptionGuard {
            id,
            handle: self.clone(),
        }))
    }

    pub(crate) fn subscribers(&self) -> usize {
        self.feed().subscribers()
    }

    /// Ends every subscription: the streams drain what is queued and finish.
    pub(crate) fn close(&self) {
        self.0.closed.store(true, Ordering::SeqCst);
        self.0.ready.notify_waiters();
    }
}

struct SubscriptionGuard {
    id: SubscriptionId,
    handle: FeedHandle,
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        self.handle.feed().unsubscribe(self.id);
    }
}

/// One stream's side of a subscription. It forgets the subscription when the
/// last clone is dropped.
#[derive(Clone)]
pub(crate) struct FeedSubscription(Arc<SubscriptionGuard>);

impl FeedSubscription {
    /// The next event to write; waits when none is queued, and is `None` once
    /// the feed is closed and the queue is drained.
    pub(crate) async fn next(&self) -> Option<StreamEvent> {
        loop {
            // Registered before the queue is read, so a commit between the two
            // wakes this wait instead of being missed.
            let ready = self.0.handle.0.ready.notified();
            if let Some(event) = self.0.handle.feed().next(self.0.id) {
                return Some(event);
            }
            if self.0.handle.0.closed.load(Ordering::SeqCst) {
                return None;
            }
            ready.await;
        }
    }
}

#[cfg(test)]
mod handle_tests;
#[cfg(test)]
mod tests;
