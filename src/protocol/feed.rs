//! The feed (docs/protocol.md §5): publishes the bridge's state after each
//! committed change and queues the events of every subscriber that follows what
//! changed.
//!
//! Each commit publishes every scope the bridge holds through the publisher
//! (`engine`): a scope that did not change publishes nothing, a scope that is new
//! or has a new epoch is a snapshot for its subscribers, and any other change is
//! the next rev as ops. A session the bridge no longer holds ends its scope. A
//! subscriber whose queue went past its budget is sent snapshots instead.
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;
use tokio::sync::{Notify, watch};
use tokio::time::Instant;

use super::documents::{AgentInfo, End, EndReason, Scope};
use super::engine::{Commit, Engine, StreamEvent};
use super::live_doc::engine_input;
use super::pacing::Pacer;
use super::project::held_sessions;
use super::subscription::{Encoded, Subscribed, Subscriber};
use super::tree::{Committed, Tree};
use super::tree_doc::{ErrorSummary, Projector};
use super::tree_stream::params::{StreamQuery, session_of};
use super::views::{self, View};
use super::tree_stream::subscriber::Subscriber as TreeSubscriber;
use super::tree_stream::wire::{End as TreeEnd, EndReason as TreeEndReason, Event as TreeEvent};
use crate::session_registry::SessionRegistry;

pub(crate) type SubscriptionId = u64;

/// A subscription that follows a session the bridge has not published yet, and
/// the directory it gave for the session's load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Waiting {
    pub id: SubscriptionId,
    pub cwd: Option<String>,
}

pub(crate) struct Feed {
    engine: Engine,
    /// The epoch `global` and `catalog` are published under.
    process_epoch: String,
    subscribers: BTreeMap<SubscriptionId, Subscriber>,
    next_id: SubscriptionId,
    /// When a scope that changed is due its checkpoint.
    pacer: Pacer,
    /// The revision 4 state tree (docs/protocol-r4-design.md §4), committed beside the scopes.
    tree: Tree,
    projector: Projector,
    /// The streams of revision 4, which follow paths of the tree.
    tree_subscribers: BTreeMap<SubscriptionId, TreeSubscriber>,
    /// Views a read keeps materialized for a while, so that a client that polls finds the same
    /// path version and is told nothing changed.
    view_pins: BTreeMap<View, Instant>,
    /// Sessions a read asked for, by the directory it gave: the bridge starts their load once,
    /// after the read's subscription is gone.
    pending_loads: BTreeMap<String, Option<String>>,
    /// Sessions a read touched since the bridge last refreshed its unobserved timers: each
    /// counts as followed once, so that its release timer starts over when the read ends.
    touched: std::collections::BTreeSet<String>,
    #[cfg(test)]
    tap: tap::Tap,
}

/// How long a read keeps a view materialized.
pub(crate) const VIEW_LINGER: std::time::Duration = std::time::Duration::from_secs(30);

impl Feed {
    pub(crate) fn new(process_epoch: impl Into<String>) -> Self {
        Self {
            engine: Engine::default(),
            process_epoch: process_epoch.into(),
            subscribers: BTreeMap::new(),
            next_id: 1,
            pacer: Pacer::default(),
            // A random instance per process, apart from the scopes' epoch (§4.1).
            tree: Tree::new(super::op::operation_id()),
            projector: Projector::default(),
            tree_subscribers: BTreeMap::new(),
            view_pins: BTreeMap::new(),
            pending_loads: BTreeMap::new(),
            touched: std::collections::BTreeSet::new(),
            #[cfg(test)]
            tap: tap::Tap::default(),
        }
    }

    /// The revision 4 tree as last committed.
    pub(crate) fn tree(&self) -> &Tree {
        &self.tree
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

    /// The views something follows or has read lately, and those `extra` follows.
    fn wanted_views(&mut self, extra: Option<&StreamQuery>, now: Instant) -> std::collections::BTreeSet<View> {
        self.view_pins.retain(|_, until| *until > now);
        let followed = self
            .tree_subscribers
            .values()
            .flat_map(|subscriber| subscriber.tracked())
            .chain(extra.into_iter().flat_map(|query| query.follow.iter().map(|follow| follow.path.as_str())));
        let mut wanted: std::collections::BTreeSet<View> = followed.filter_map(View::of).collect();
        wanted.extend(self.view_pins.keys().cloned());
        wanted
    }

    /// Materializes the views `extra` follows, so that its first event carries them; they are
    /// kept for `VIEW_LINGER` when `linger` (a read).
    fn materialize_views(&mut self, extra: &StreamQuery, linger: bool, now: Instant) {
        if linger {
            for follow in &extra.follow {
                if let Some(view) = View::of(&follow.path) {
                    self.view_pins.insert(view, now + VIEW_LINGER);
                }
                if let Some(session_id) = session_of(&follow.path) {
                    self.pending_loads.insert(session_id.clone(), extra.cwd.get(&session_id).cloned());
                    self.touched.insert(session_id);
                }
            }
        }
        let wanted = self.wanted_views(Some(extra), now);
        if let Some(committed) = self.tree.commit_views(|tree| views::derive(tree, &wanted)) {
            self.publish_tree(&committed, now, &BTreeMap::new());
        }
    }

    /// Registers a stream of revision 4 and queues its first event.
    pub(crate) fn subscribe_tree(&mut self, query: &StreamQuery) -> SubscriptionId {
        self.subscribe_tree_at(query, false)
    }

    /// `subscribe_tree`; a read (`linger`) keeps the views it asks for.
    pub(crate) fn subscribe_tree_at(&mut self, query: &StreamQuery, linger: bool) -> SubscriptionId {
        self.materialize_views(query, linger, Instant::now());
        let id = self.next_id;
        self.next_id += 1;
        self.tree_subscribers.insert(id, TreeSubscriber::new(query, &self.tree));
        id
    }

    pub(crate) fn next_tree(&mut self, id: SubscriptionId) -> Option<TreeEvent> {
        self.tree_subscribers.get_mut(&id)?.next()
    }

    pub(crate) fn unsubscribe(&mut self, id: SubscriptionId) {
        self.subscribers.remove(&id);
        self.tree_subscribers.remove(&id);
    }

    /// The next event to write to subscription `id`.
    #[cfg(test)]
    pub(crate) fn next(&mut self, id: SubscriptionId) -> Option<StreamEvent> {
        self.subscribers.get_mut(&id)?.next()
    }

    pub(crate) fn next_encoded(&mut self, id: SubscriptionId) -> Option<Encoded> {
        self.subscribers.get_mut(&id)?.next_encoded()
    }

    pub(crate) fn subscribers(&self) -> usize {
        self.subscribers.len() + self.tree_subscribers.len()
    }

    /// The sessions some subscription follows, with the subscriptions that
    /// follow each and wait for it to be published (protocol §5.1).
    pub(crate) fn followed_sessions(&self) -> BTreeMap<String, Vec<Waiting>> {
        let mut sessions: BTreeMap<String, Vec<Waiting>> = BTreeMap::new();
        for (&id, subscriber) in &self.subscribers {
            for (scope, cwd) in subscriber.followed() {
                let Some(Scope::Session(session_id)) = Scope::parse(scope) else {
                    continue;
                };
                let waiting = sessions.entry(session_id).or_default();
                if self.engine.version(scope).is_none() {
                    waiting.push(Waiting {
                        id,
                        cwd: cwd.map(String::from),
                    });
                }
            }
        }
        for (&id, subscriber) in &self.tree_subscribers {
            for path in subscriber.followed() {
                let Some(session_id) = session_of(path) else {
                    continue;
                };
                let published = self
                    .tree
                    .get(&format!("/sessions/{}", super::tree::escape(&session_id)))
                    .is_some();
                let waiting = sessions.entry(session_id.clone()).or_default();
                if !published && !waiting.iter().any(|waiting| waiting.id == id) {
                    waiting.push(Waiting {
                        id,
                        cwd: subscriber.cwd().get(&session_id).cloned(),
                    });
                }
            }
        }
        for (session_id, cwd) in &self.pending_loads {
            let published = self
                .tree
                .get(&format!("/sessions/{}", super::tree::escape(session_id)))
                .is_some();
            let waiting = sessions.entry(session_id.clone()).or_default();
            if !published {
                waiting.push(Waiting { id: 0, cwd: cwd.clone() });
            }
        }
        sessions
    }

    /// The bridge dealt with the read of `session_id`: its load is started or will never be.
    pub(crate) fn clear_pending(&mut self, session_id: &str) {
        self.pending_loads.remove(session_id);
    }

    /// The sessions reads touched since the last call.
    pub(crate) fn take_touched(&mut self) -> std::collections::BTreeSet<String> {
        std::mem::take(&mut self.touched)
    }

    /// Queues a tree commit for the revision 4 streams that follow what it changed.
    fn publish_tree(&mut self, committed: &Committed, now: Instant, ends: &BTreeMap<String, TreeEnd>) {
        for subscriber in self.tree_subscribers.values_mut() {
            subscriber.publish_ended(&self.tree, committed, now, ends);
            if subscriber.over_budget() {
                subscriber.coalesce(&self.tree);
            }
        }
    }

    /// Ends `session/<session_id>` for the subscriptions still waiting for it:
    /// a subscription that has the session published is not ended by this.
    pub(crate) fn end_waiting(&mut self, session_id: &str, end: &End) {
        let tree_end = tree_end(end);
        let published = self
            .tree
            .get(&format!("/sessions/{}", super::tree::escape(session_id)))
            .is_some();
        if !published
            && let (EndReason::LoadFailed, Some(error)) = (end.reason, &end.error)
        {
            self.projector.record_failure(session_id, error.clone());
            let failures = self.projector.load_failures();
            if let Some(committed) = self.tree.commit_at("/global/loadFailures", failures) {
                self.publish_tree(&committed, Instant::now(), &BTreeMap::new());
            }
        }
        for subscriber in self.tree_subscribers.values_mut() {
            subscriber.end_session(&self.tree, session_id, &tree_end);
        }
        let scope = Scope::Session(session_id.to_string()).to_string();
        if self.engine.version(&scope).is_some() {
            return;
        }
        for subscriber in self.subscribers.values_mut() {
            subscriber.end(&scope, end.clone());
        }
    }

    /// The committed `live` of `scope`: what a write is admitted against.
    pub(crate) fn live(&self, scope: &Scope) -> Option<Value> {
        self.engine.live(&scope.to_string()).cloned()
    }

    /// The block an attachment of a held session names, while a published entry references it.
    pub(crate) fn attachment(&self, session_id: &str, id: &str) -> Option<Value> {
        self.engine.attachment(&format!("session/{session_id}"), id).cloned()
    }

    /// The whole document of `scope`, as a snapshot from nothing.
    pub(crate) fn snapshot(&self, scope: &str) -> Option<StreamEvent> {
        self.engine.snapshot(scope, None)
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
        self.commit_at(registry, agent, catalog_revision, Instant::now());
    }

    /// `commit` at `now`, which paces the checkpoints of what changed.
    pub(crate) fn commit_at(
        &mut self,
        registry: &SessionRegistry,
        agent: &AgentInfo,
        catalog_revision: u64,
        now: Instant,
    ) {
        self.commit_ending(registry, agent, catalog_revision, now, &[]);
    }

    /// `commit_at` with the scopes the transitions ended and why. A session the
    /// bridge no longer holds ends with its reason, and one that ended before it
    /// was published ends for the subscriptions that wait for it.
    pub(crate) fn commit_ending(
        &mut self,
        registry: &SessionRegistry,
        agent: &AgentInfo,
        catalog_revision: u64,
        now: Instant,
        ended: &[(String, End)],
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
                    #[cfg(test)]
                    self.tap_event(&event);
                    self.pacer.changed(&name, now);
                    let event = Encoded::new(event);
                    for subscriber in self.subscribers.values_mut() {
                        subscriber.publish(&event);
                    }
                }
                Commit::Rebuilt => {
                    #[cfg(test)]
                    self.tap_rebuilt(&name);
                    // A snapshot carries a digest of its own.
                    self.pacer.settled(&name);
                    for subscriber in self.subscribers.values_mut() {
                        subscriber.rebuilt(&name, &self.engine);
                    }
                }
            }
        }
        for (session_id, end) in ended {
            self.end_waiting(session_id, end);
        }
        // A session the bridge no longer holds is over.
        let held: HashSet<String> = scopes.iter().map(Scope::to_string).collect();
        let gone: Vec<String> = self
            .engine
            .scopes()
            .filter(|published| published.starts_with("session/") && !held.contains(*published))
            .map(String::from)
            .collect();
        for scope in gone {
            let cwd = self
                .engine
                .live(&scope)
                .and_then(|live| live["cwd"].as_str().map(String::from));
            self.engine.remove(&scope);
            self.pacer.settled(&scope);
            let recorded = ended
                .iter()
                .find(|(session_id, _)| Scope::Session(session_id.clone()).to_string() == scope)
                .map(|(_, end)| end.clone());
            #[cfg(test)]
            self.tap_released();
            // A session that went with a reason ends its scope. One released with none ends
            // nothing: a client keeps its copy, a follower it still has waits for the
            // session to be loaded again (S6).
            match recorded {
                Some(end) => {
                    #[cfg(test)]
                    self.tap_event(&StreamEvent::End {
                        scope: scope.clone(),
                        end: end.clone(),
                    });
                    for subscriber in self.subscribers.values_mut() {
                        subscriber.end(&scope, end.clone());
                    }
                }
                None => {
                    for subscriber in self.subscribers.values_mut() {
                        subscriber.released(&scope, cwd.as_deref());
                    }
                }
            }
        }
        for subscriber in self.subscribers.values_mut() {
            if subscriber.over_budget() {
                subscriber.coalesce(&self.engine);
            }
        }
        for (session_id, end) in ended {
            match (end.reason, &end.error) {
                (EndReason::LoadFailed, Some(error)) => {
                    self.projector.record_failure(session_id, error.clone());
                }
                (EndReason::Deleted, _) => self.projector.forget_failure(session_id),
                _ => {}
            }
        }
        let next = self.tree.rev() + 1;
        let root = self.projector.project(registry, agent, catalog_revision, next);
        let wanted = self.wanted_views(None, now);
        if let Some(committed) = self.tree.commit_with_views(root, |tree| views::derive(tree, &wanted)) {
            let ends: BTreeMap<String, TreeEnd> = ended
                .iter()
                .map(|(session_id, end)| (session_id.clone(), tree_end(end)))
                .collect();
            self.publish_tree(&committed, now, &ends);
        }
        #[cfg(test)]
        self.tap_flush();
    }

    /// Queues the checkpoints that are due at `now` for the subscribers that
    /// follow their scopes; whether any was queued.
    pub(crate) fn checkpoints(&mut self, now: Instant) -> bool {
        let mut queued = false;
        for scope in self.pacer.due(now) {
            let Some(checkpoint) = self.engine.checkpoint(&scope) else {
                continue;
            };
            queued = true;
            let checkpoint = Encoded::new(checkpoint);
            for subscriber in self.subscribers.values_mut() {
                subscriber.publish(&checkpoint);
            }
        }
        for subscriber in self.tree_subscribers.values_mut() {
            queued |= subscriber.checkpoints(&self.tree, now);
        }
        queued
    }

    /// When the next checkpoint is due, if a scope or a path waits for one.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.tree_subscribers
            .values()
            .filter_map(TreeSubscriber::deadline)
            .chain(self.pacer.deadline())
            .min()
    }
}

/// The end of a revision 3 scope as a path of the tree ends.
fn tree_end(end: &End) -> TreeEnd {
    TreeEnd {
        reason: match end.reason {
            EndReason::Deleted => TreeEndReason::Deleted,
            EndReason::Closed => TreeEndReason::Closed,
            EndReason::LoadFailed => TreeEndReason::LoadFailed,
        },
        error: end.error.as_ref().map(ErrorSummary::of),
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
    /// Changes whenever a subscription is dropped.
    closes: watch::Sender<u64>,
}

impl FeedHandle {
    pub(crate) fn new(process_epoch: impl Into<String>) -> Self {
        Self(Arc::new(Shared {
            feed: Mutex::new(Feed::new(process_epoch)),
            ready: Notify::new(),
            closed: AtomicBool::new(false),
            closes: watch::channel(0).0,
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
        self.commit_with(registry, agent, catalog_revision, &[]);
    }

    /// `commit` with the scopes the transitions ended and why.
    pub(crate) fn commit_with(
        &self,
        registry: &SessionRegistry,
        agent: &AgentInfo,
        catalog_revision: u64,
        ended: &[(String, End)],
    ) {
        self.feed()
            .commit_ending(registry, agent, catalog_revision, Instant::now(), ended);
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

    pub(crate) fn clear_pending(&self, session_id: &str) {
        self.feed().clear_pending(session_id);
    }

    pub(crate) fn take_touched(&self) -> std::collections::BTreeSet<String> {
        self.feed().take_touched()
    }

    /// `read` over the tree as committed now.
    pub(crate) fn with_tree<R>(&self, read: impl FnOnce(&Tree) -> R) -> R {
        read(self.feed().tree())
    }

    /// A short subscription for a read: its views are kept for a while.
    pub(crate) fn subscribe_tree_reading(&self, query: &StreamQuery) -> TreeSubscription {
        let id = self.feed().subscribe_tree_at(query, true);
        TreeSubscription(FeedSubscription(Arc::new(SubscriptionGuard {
            id,
            handle: self.clone(),
        })))
    }

    /// A stream of revision 4 over the paths of `query`, its first event queued.
    pub(crate) fn subscribe_tree(&self, query: &StreamQuery) -> TreeSubscription {
        let id = self.feed().subscribe_tree(query);
        TreeSubscription(FeedSubscription(Arc::new(SubscriptionGuard {
            id,
            handle: self.clone(),
        })))
    }

    pub(crate) fn subscribers(&self) -> usize {
        self.feed().subscribers()
    }

    /// Changes whenever a subscription is dropped: a session may be needed no
    /// longer.
    pub(crate) fn closes(&self) -> watch::Receiver<u64> {
        self.0.closes.subscribe()
    }

    /// The sessions some subscription follows (see `Feed::followed_sessions`).
    pub(crate) fn followed_sessions(&self) -> BTreeMap<String, Vec<Waiting>> {
        self.feed().followed_sessions()
    }

    /// Ends the session's scope for the subscriptions waiting for it, and wakes
    /// their streams.
    pub(crate) fn end_waiting(&self, session_id: &str, end: &End) {
        self.feed().end_waiting(session_id, end);
        self.0.ready.notify_waiters();
    }

    /// The committed `live` of `scope`: what a write is admitted against.
    pub(crate) fn live(&self, scope: &Scope) -> Option<Value> {
        self.feed().live(scope)
    }

    /// The block an attachment of a held session names, while a published entry references it.
    pub(crate) fn attachment(&self, session_id: &str, id: &str) -> Option<Value> {
        self.feed().attachment(session_id, id)
    }

    /// The whole document of `scope` as the snapshot a stream is aligned with (protocol §8): none
    /// while the bridge does not hold it. It holds, loads and renews nothing.
    pub(crate) fn snapshot(&self, scope: &str) -> Option<StreamEvent> {
        self.feed().snapshot(scope)
    }

    /// The epoch and rev `scope` is published at, if it is.
    pub(crate) fn version(&self, scope: &str) -> Option<(String, u64)> {
        self.feed().version(scope)
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
        self.handle
            .0
            .closes
            .send_modify(|closes| *closes = closes.wrapping_add(1));
    }
}

/// One stream's side of a subscription. It forgets the subscription when the
/// last clone is dropped.
#[derive(Clone)]
pub(crate) struct FeedSubscription(Arc<SubscriptionGuard>);

impl FeedSubscription {
    /// The next event to write; waits when none is queued, and is `None` once
    /// the feed is closed and the queue is drained.
    #[cfg(test)]
    pub(crate) async fn next(&self) -> Option<StreamEvent> {
        self.next_encoded().await.map(Encoded::into_event)
    }

    /// `next`, with the JSON it was encoded to once for every subscriber.
    pub(crate) async fn next_encoded(&self) -> Option<Encoded> {
        let handle = &self.0.handle;
        loop {
            // Enabled before the queue is read, so a commit between the two wakes
            // this wait instead of being missed: `notify_waiters` reaches only
            // a wait that is registered.
            let ready = handle.0.ready.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            let deadline = {
                let mut feed = handle.feed();
                // The checkpoints due are queued for every stream, whichever one
                // is waiting when they are.
                if feed.checkpoints(Instant::now()) {
                    handle.0.ready.notify_waiters();
                }
                if let Some(event) = feed.next_encoded(self.0.id) {
                    return Some(event);
                }
                feed.deadline()
            };
            if handle.0.closed.load(Ordering::SeqCst) {
                return None;
            }
            match deadline {
                Some(deadline) => {
                    tokio::select! {
                        () = ready => {}
                        () = tokio::time::sleep_until(deadline) => {}
                    }
                }
                None => ready.await,
            }
        }
    }
}

/// One revision 4 stream's side of a subscription.
#[derive(Clone)]
pub(crate) struct TreeSubscription(FeedSubscription);

impl TreeSubscription {
    /// The next event to write, as JSON; waits when none is queued, and is `None` once the feed
    /// is closed and the queue is drained.
    pub(crate) async fn next_json(&self) -> Option<String> {
        let subscription = &self.0;
        let handle = &subscription.0.handle;
        loop {
            let ready = handle.0.ready.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            let deadline = {
                let mut feed = handle.feed();
                if feed.checkpoints(Instant::now()) {
                    handle.0.ready.notify_waiters();
                }
                if let Some(event) = feed.next_tree(subscription.0.id) {
                    return Some(serde_json::to_string(&event).expect("an event serializes"));
                }
                feed.deadline()
            };
            if handle.0.closed.load(Ordering::SeqCst) {
                return None;
            }
            match deadline {
                Some(deadline) => {
                    tokio::select! {
                        () = ready => {}
                        () = tokio::time::sleep_until(deadline) => {}
                    }
                }
                None => ready.await,
            }
        }
    }
}

#[cfg(test)]
mod handle_tests;
#[cfg(test)]
mod tree_stream_tests;
#[cfg(test)]
mod tree_tests;
#[cfg(test)]
pub(crate) mod tap;
#[cfg(test)]
mod tests;
