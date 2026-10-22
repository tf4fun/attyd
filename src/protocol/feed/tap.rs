//! What a test sees of the feed: every commit's events and the documents it left, and whether
//! a commit now would publish anything (docs/state-sync.md §4.1).
use std::collections::BTreeMap;

use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use super::*;

/// One commit as the tap receives it: the events it published, in order, and every document
/// published after it, each as the snapshot a subscriber would be aligned with.
pub(crate) struct TapCommit {
    pub events: Vec<Value>,
    pub documents: BTreeMap<String, Value>,
}

#[derive(Default)]
pub(super) struct Tap {
    sender: Option<UnboundedSender<TapCommit>>,
    pending: Vec<Value>,
}

impl Feed {
    /// Taps every later commit and returns the published documents now: a cut.
    pub(crate) fn tap(&mut self, sender: UnboundedSender<TapCommit>) -> BTreeMap<String, Value> {
        self.tap.sender = Some(sender);
        self.documents()
    }

    /// Every published document, as a snapshot from nothing.
    pub(crate) fn documents(&self) -> BTreeMap<String, Value> {
        self.engine
            .scopes()
            .filter_map(|scope| {
                let snapshot = self.engine.snapshot(scope, None)?;
                Some((scope.to_string(), serde_json::to_value(snapshot).ok()?))
            })
            .collect()
    }

    pub(super) fn tap_event(&mut self, event: &StreamEvent) {
        if self.tap.sender.is_some() {
            self.tap
                .pending
                .push(serde_json::to_value(event).expect("an event serializes"));
        }
    }

    pub(super) fn tap_rebuilt(&mut self, scope: &str) {
        if let Some(snapshot) = self.engine.snapshot(scope, None) {
            self.tap_event(&snapshot);
        }
    }

    /// Hands the commit's events and documents to the tap, if it published any.
    pub(super) fn tap_flush(&mut self) {
        if self.tap.pending.is_empty() {
            return;
        }
        let events = std::mem::take(&mut self.tap.pending);
        let documents = self.documents();
        if let Some(sender) = &self.tap.sender {
            let _ = sender.send(TapCommit { events, documents });
        }
    }

    /// Whether a commit now would publish anything: a transition that changed what clients see
    /// without committing it.
    pub(crate) fn diverges(
        &self,
        registry: &SessionRegistry,
        agent: &AgentInfo,
        catalog_revision: u64,
    ) -> bool {
        let mut scopes = vec![Scope::Global, Scope::Catalog];
        scopes.extend(held_sessions(registry).into_iter().map(Scope::Session));
        let held: Vec<String> = scopes.iter().map(Scope::to_string).collect();
        if self
            .engine
            .scopes()
            .any(|published| published.starts_with("session/") && !held.contains(&published.to_string()))
        {
            return true;
        }
        scopes.iter().any(|scope| {
            engine_input(registry, scope, agent, catalog_revision, &self.process_epoch)
                .is_some_and(|input| self.engine.diverges(&scope.to_string(), &input))
        })
    }
}

impl FeedSubscription {
    /// The next event if one is queued now.
    pub(crate) fn try_next(&self) -> Option<StreamEvent> {
        self.0.handle.feed().next(self.0.id)
    }
}

impl FeedHandle {
    /// Taps every later commit and returns the published documents now.
    pub(crate) fn tap(&self, sender: UnboundedSender<TapCommit>) -> BTreeMap<String, Value> {
        self.feed().tap(sender)
    }

    /// Every published document now.
    pub(crate) fn documents(&self) -> BTreeMap<String, Value> {
        self.feed().documents()
    }

    /// Whether a commit now would publish anything.
    pub(crate) fn diverges(
        &self,
        registry: &SessionRegistry,
        agent: &AgentInfo,
        catalog_revision: u64,
    ) -> bool {
        self.feed().diverges(registry, agent, catalog_revision)
    }
}
