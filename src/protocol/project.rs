//! Pure projections of bridge state into the published documents
//! (docs/state-sync.md §2.2). They are the only definition of what clients see.
use serde_json::Value;

use super::canon::Attachments;
use super::documents::{AgentInfo, CatalogLive, GlobalLive, SessionLive};
use super::registry::SessionLog;
use crate::runtime_state::SharedTurnUpdates;
use crate::session_registry::SessionRegistry;

/// A held session's document. `live.op.process` is left out: the commit step
/// projects the running prompt's updates one by one (`project_update`), so an
/// update it already published is not projected again.
#[derive(Debug, Clone)]
pub(crate) struct SessionDocument {
    pub log: SessionLog,
    pub live: SessionLive,
    /// The running prompt's folded updates, when `op` is that prompt.
    pub process: Option<SharedTurnUpdates>,
    /// The blocks `op.input` references.
    pub input_attachments: Attachments,
}

/// `session/<id>`, or `None` when the bridge does not hold the session.
pub(crate) fn project_session(
    registry: &SessionRegistry,
    session_id: &str,
) -> Option<SessionDocument> {
    let _ = (registry, session_id);
    todo!("project a session")
}

/// One process update: its image, audio and embedded resource blocks become
/// attachment references, kept in `attachments`.
pub(crate) fn project_update(update: &Value, attachments: &mut Attachments) -> Value {
    let _ = (update, attachments);
    todo!("project an update")
}

pub(crate) fn project_global(registry: &SessionRegistry, agent: &AgentInfo) -> GlobalLive {
    let _ = (registry, agent);
    todo!("project global")
}

pub(crate) fn project_catalog(registry: &SessionRegistry, revision: u64) -> CatalogLive {
    let _ = (registry, revision);
    todo!("project catalog")
}

/// The IDs of the held sessions.
pub(crate) fn held_sessions(registry: &SessionRegistry) -> Vec<String> {
    let _ = registry;
    todo!("held sessions")
}
