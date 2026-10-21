//! The live half of a session document (docs/protocol.md §4.2–§4.6).
//!
//! A pure projection of the registry's live state into the JSON a replica
//! holds under `live`. This part covers what does not depend on which scope
//! shows an operation or an interaction: the session's identity, its reduced
//! controls, its terminals (those with a handle and the frozen output a tool
//! embeds) and whether a close is outstanding.
use serde_json::{Value, json};

use super::documents::{AgentInfo, Scope};
use super::project;
use crate::runtime_state::{SessionLifecycle, SessionLiveState};
use crate::session_registry::SessionRegistry;

/// A terminal as §4.6 publishes it. `released`: its handle is gone and the
/// output frozen.
fn terminal(terminal_id: &str, snapshot: &Value, released: bool) -> Value {
    let exit_status = snapshot
        .get("exitStatus")
        .filter(|status| status.is_object())
        .map(|status| {
            json!({
                "exitCode": status.get("exitCode").and_then(Value::as_i64),
                "signal": status.get("signal").and_then(Value::as_str),
            })
        });
    json!({
        "terminalId": terminal_id,
        "output": snapshot["output"].as_str().unwrap_or_default(),
        "truncated": snapshot["truncated"].as_bool().unwrap_or(false),
        "exitStatus": exit_status,
        "released": released,
    })
}

fn held(live: &SessionLiveState) -> bool {
    live.lifecycle != SessionLifecycle::Closed
}

/// What `live` holds for the session besides its operation, interactions and
/// URL flows; `None` when the bridge does not hold the session.
pub(crate) fn session_live(registry: &SessionRegistry, session_id: &str) -> Option<Value> {
    let live = registry
        .sessions
        .get(session_id)?
        .state
        .live
        .as_ref()
        .filter(|live| held(live))?;
    let terminals: serde_json::Map<String, Value> = live
        .terminals
        .iter()
        .map(|(id, snapshot)| (id, snapshot, false))
        .chain(live.frozen_terminals.iter().map(|(id, snapshot)| (id, snapshot, true)))
        .map(|(id, snapshot, released)| (id.clone(), terminal(id, snapshot, released)))
        .collect();
    Some(json!({
        "sessionId": session_id,
        "cwd": live.cwd.to_string_lossy(),
        "controls": live.controls,
        "terminals": terminals,
        "closing": matches!(
            live.lifecycle,
            SessionLifecycle::Closing | SessionLifecycle::ClosingForDelete
        ),
    }))
}

/// What `live` holds for `global` besides its operation, interactions and URL
/// flows (protocol §4.3): the Agent, the sessions the bridge holds and the
/// terminal logins. A login terminal is a terminal like any other, whose handle
/// lasts until it is dismissed.
pub(crate) fn global_live(registry: &SessionRegistry, agent: &AgentInfo) -> Value {
    let global = serde_json::to_value(project::project_global(registry, agent))
        .expect("the global live document serializes");
    let mut terminals = global["terminals"].clone();
    for terminal in terminals
        .as_object_mut()
        .into_iter()
        .flat_map(|terminals| terminals.values_mut())
    {
        terminal["released"] = Value::Bool(false);
    }
    json!({
        "agent": global["agent"],
        "sessions": global["sessions"],
        "terminals": terminals,
    })
}

/// What `live` holds for `catalog` besides its operation, interactions and URL
/// flows (protocol §4.3): the revision of the Agent's list.
pub(crate) fn catalog_live(registry: &SessionRegistry, revision: u64) -> Value {
    let catalog = serde_json::to_value(project::project_catalog(registry, revision))
        .expect("the catalog live document serializes");
    json!({ "revision": catalog["revision"] })
}

/// The operation `scope` shows (protocol §4.4): that of the scope whose
/// operation occupies it, if any. Its `scope` is the one it was written to and
/// its `after` the operation it replaced there; a scope's first operation
/// replaced none. Pending interactions are not part of it.
pub(crate) fn operation_doc(registry: &SessionRegistry, scope: &Scope) -> Option<Value> {
    let slot = registry.shown_slot(scope)?;
    let mut operation = json!({
        "id": slot.id,
        "scope": slot.scope.clone().unwrap_or_else(|| scope.to_string()),
        "after": slot.after,
        "kind": slot.kind,
        "status": slot.status,
        "input": slot.input,
        "cancelRequested": slot.cancel_requested,
    });
    if let Some(result) = &slot.result {
        operation["result"] = result.clone();
    }
    if let Some(error) = &slot.error {
        operation["error"] = serde_json::to_value(error).expect("an error serializes");
    }
    Some(operation)
}

#[cfg(test)]
mod tests;
