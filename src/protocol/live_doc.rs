//! The live half of a session document (docs/protocol.md §4.2–§4.6).
//!
//! A pure projection of the registry's live state into the JSON a replica
//! holds under `live`. This part covers what does not depend on which scope
//! shows an operation or an interaction: the session's identity, its reduced
//! controls, its terminals (those with a handle and the frozen output a tool
//! embeds) and whether a close is outstanding.
use serde_json::Value;

use super::documents::{AgentInfo, Scope};
use super::engine;
use super::project;
use crate::session_registry::SessionRegistry;

/// `live` as a document without the members every scope has (`op`, `interactions`, `urlFlows`).
fn without_common_members(live: impl serde::Serialize) -> Value {
    let mut live = serde_json::to_value(live).expect("a live document serializes");
    if let Some(members) = live.as_object_mut() {
        for common in ["op", "interactions", "urlFlows"] {
            members.remove(common);
        }
    }
    live
}

/// What `live` holds for the session besides its operation, interactions and
/// URL flows; `None` when the bridge does not hold the session.
pub(crate) fn session_live(registry: &SessionRegistry, session_id: &str) -> Option<Value> {
    project::project_session_live(registry, session_id).map(without_common_members)
}

/// The complete `live` of `scope` (protocol §4.1–§4.4): the members every scope
/// has (`op`, `interactions`, `urlFlows`) and those of its kind, serialized from
/// the types the generated TypeScript comes from. `None` when the bridge does not
/// hold the session.
pub(crate) fn scope_live(
    registry: &SessionRegistry,
    scope: &Scope,
    agent: &AgentInfo,
    catalog_revision: u64,
) -> Option<Value> {
    let live = match scope {
        Scope::Session(session_id) => {
            serde_json::to_value(project::project_session_live(registry, session_id)?)
        }
        Scope::Global => serde_json::to_value(project::project_global(registry, agent)),
        Scope::Catalog => {
            serde_json::to_value(project::project_catalog(registry, catalog_revision))
        }
    };
    Some(live.expect("a live document serializes"))
}

/// What the publisher is handed for `scope` (protocol §5.2): the epoch the
/// document is published under, the history and timeline of a session (none
/// while its history is unknown), and the complete `live`. `global` and
/// `catalog` have no timeline and never change epoch while the bridge runs, so
/// they are published under `process_epoch`.
pub(crate) fn engine_input(
    registry: &SessionRegistry,
    scope: &Scope,
    agent: &AgentInfo,
    catalog_revision: u64,
    process_epoch: &str,
) -> Option<engine::Input> {
    let live = scope_live(registry, scope, agent, catalog_revision)?;
    let Scope::Session(session_id) = scope else {
        return Some(engine::Input {
            epoch: process_epoch.to_string(),
            history: None,
            timeline: None,
            live,
        });
    };
    let state = &registry.sessions.get(session_id)?.state;
    Some(engine::Input {
        // Only a known timeline has an epoch; an unknown one is published under
        // the process's, until the history is learned and the document rebuilt.
        epoch: state
            .timeline_epoch
            .clone()
            .unwrap_or_else(|| process_epoch.to_string()),
        history: state.history,
        timeline: state.timeline.clone(),
        live,
    })
}

/// What `live` holds for `global` besides its operation, interactions and URL
/// flows (protocol §4.3): the Agent, the sessions the bridge holds and the
/// terminal logins.
pub(crate) fn global_live(registry: &SessionRegistry, agent: &AgentInfo) -> Value {
    without_common_members(project::project_global(registry, agent))
}

/// What `live` holds for `catalog` besides its operation, interactions and URL
/// flows (protocol §4.3): the revision of the Agent's list and the pages listed
/// since it last changed.
pub(crate) fn catalog_live(registry: &SessionRegistry, revision: u64) -> Value {
    without_common_members(project::project_catalog(registry, revision))
}

/// The operation `scope` shows (protocol §4.4): that of the scope whose
/// operation occupies it, if any. Its `scope` is the one it was written to and
/// its `after` the operation it replaced there; a scope's first operation
/// replaced none. Pending interactions are not part of it.
pub(crate) fn operation_doc(registry: &SessionRegistry, scope: &Scope) -> Option<Value> {
    let (operation, _) = project::shown_operation(registry, scope)?;
    Some(serde_json::to_value(operation).expect("an operation serializes"))
}

#[cfg(test)]
mod tests;
