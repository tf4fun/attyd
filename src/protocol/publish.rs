//! The commit step (docs/state-sync.md §2.3): the only writer of published
//! documents and events (I1).
//!
//! Each commit compares the projected documents with the published ones and
//! emits one event per changed scope. A log change and the same scope's live
//! patch are one event (S2). The Hub (migration step 5) delivers the events.
use std::sync::Arc;

use serde_json::Value;

use super::canon::LogEntry;
use super::documents::{AgentInfo, Event};
use crate::session_registry::SessionRegistry;

/// A published document, as a one-off read returns it (protocol §8).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Document {
    pub live: Value,
    /// `None` while the session's log is unknown.
    pub log: Option<Vec<Arc<LogEntry>>>,
}

#[derive(Default)]
pub(crate) struct Publisher {}

impl Publisher {
    /// Publishes every change since the last commit.
    pub(crate) fn commit(
        &mut self,
        registry: &mut SessionRegistry,
        agent: &AgentInfo,
        catalog_revision: u64,
    ) -> Vec<Event> {
        let _ = (registry, agent, catalog_revision);
        todo!("the commit step")
    }

    /// Whether a commit now would publish anything: a transition that changed
    /// what clients see without committing it (the gate's check, §4.1).
    pub(crate) fn diverges(
        &self,
        registry: &SessionRegistry,
        agent: &AgentInfo,
        catalog_revision: u64,
    ) -> bool {
        let _ = (registry, agent, catalog_revision);
        todo!("the divergence check")
    }

    /// The published document of `scope`.
    pub(crate) fn document(&self, scope: &str) -> Option<Document> {
        let _ = scope;
        todo!("a published document")
    }

    /// The published scopes.
    pub(crate) fn scopes(&self) -> Vec<String> {
        todo!("the published scopes")
    }

    /// An attachment of a held session, while its published `log` or `live`
    /// references it (protocol §8).
    pub(crate) fn attachment(&self, session_id: &str, attachment_id: &str) -> Option<Value> {
        let _ = (session_id, attachment_id);
        todo!("an attachment")
    }
}
