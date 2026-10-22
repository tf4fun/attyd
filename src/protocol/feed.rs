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

#[cfg(test)]
mod tests;
