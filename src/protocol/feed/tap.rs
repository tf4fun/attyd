//! Test observation of the real revision 4 publisher: atomic wire events and their committed
//! node values. The tap's subscriber never participates in session holding or view demand.
use std::collections::BTreeMap;

use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use super::*;
use crate::protocol::tree::Spec;
use crate::protocol::tree_stream::params::Follow;

/// One tree commit and the native node values it leaves. The gate applies these events through
/// the R4 client; scope-shaped assertion documents are reconstructed only after replication.
pub(crate) struct TapCommit {
    pub events: Vec<TreeEvent>,
    pub documents: BTreeMap<String, Value>,
}

#[derive(Default)]
pub(super) struct Tap {
    sender: Option<UnboundedSender<TapCommit>>,
    subscriber: Option<TreeSubscriber>,
}

impl Feed {
    /// How many real subscriptions still follow this target (or a path inside its session).
    pub(crate) fn followers(&self, scope: &str) -> usize {
        let session_id = scope.strip_prefix("session/");
        self.tree_subscribers.values().filter(|subscriber| {
            subscriber.followed().any(|path| {
                if let Some(id) = session_id {
                    session_of(path).as_deref() == Some(id)
                } else {
                    path.strip_prefix('/').is_some_and(|path| {
                        path == scope || path.strip_prefix(scope).is_some_and(|tail| tail.starts_with('/'))
                    })
                }
            })
        }).count()
    }

    /// Register observation and return its first real stream event as an atomic cut.
    pub(crate) fn tap(&mut self, sender: UnboundedSender<TapCommit>) -> TapCommit {
        let query = StreamQuery {
            follow: ["/global", "/catalog", "/sessions"].into_iter().map(|path| Follow {
                path: path.into(), have: None, window: None,
            }).collect(),
            ..Default::default()
        };
        let mut subscriber = TreeSubscriber::new(&query, &self.tree);
        let events = std::iter::from_fn(|| subscriber.next()).collect();
        self.tap = Tap { sender: Some(sender), subscriber: Some(subscriber) };
        TapCommit { events, documents: self.documents() }
    }

    /// Every native target node now, without any R3 snapshot or replica adapter.
    pub(crate) fn documents(&self) -> BTreeMap<String, Value> {
        let mut documents = BTreeMap::new();
        for scope in ["global", "catalog"] {
            if let Some(node) = self.tree.get(&format!("/{scope}")) {
                documents.insert(scope.to_string(), node.to_value());
            }
        }
        if let Some(sessions) = self.tree.get("/sessions") {
            for id in sessions.child_names() {
                documents.insert(format!("session/{id}"), sessions.child(id).unwrap().to_value());
            }
        }
        documents
    }

    pub(super) fn tap_publish(&mut self, committed: &Committed, now: Instant, ends: &BTreeMap<String, TreeEnd>) {
        if self.tap.sender.as_ref().is_none_or(UnboundedSender::is_closed) {
            return;
        }
        let subscriber = self.tap.subscriber.as_mut().expect("a tap has a subscriber");
        subscriber.publish_ended(&self.tree, committed, now, ends);
        let events: Vec<_> = std::iter::from_fn(|| subscriber.next()).collect();
        if !events.is_empty() {
            let commit = TapCommit { events, documents: self.documents() };
            let _ = self.tap.sender.as_ref().unwrap().send(commit);
        }
    }

    /// Detects state changes omitted from a commit, without changing publication or its cache.
    pub(crate) fn diverges(&self, registry: &SessionRegistry, agent: &AgentInfo, catalog_revision: u64) -> bool {
        let mut projector = self.projector.clone();
        let mut root = projector.project(registry, agent, catalog_revision, self.tree.rev() + 1);
        // Views are derived from these canonical nodes. Retain their current demand so an
        // expired read pin cannot masquerade as an uncommitted registry transition.
        if self.tree.get("/views").is_some() {
            root.insert("views".into(), Spec::Keep);
        }
        self.tree.clone().commit(root).is_some()
    }
}

impl FeedHandle {
    pub(crate) fn followers(&self, scope: &str) -> usize {
        self.feed().followers(scope)
    }

    pub(crate) fn tap(&self, sender: UnboundedSender<TapCommit>) -> TapCommit {
        self.feed().tap(sender)
    }

    pub(crate) fn documents(&self) -> BTreeMap<String, Value> {
        self.feed().documents()
    }

    pub(crate) fn diverges(&self, registry: &SessionRegistry, agent: &AgentInfo, catalog_revision: u64) -> bool {
        self.feed().diverges(registry, agent, catalog_revision)
    }
}
