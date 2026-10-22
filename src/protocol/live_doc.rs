//! The live half of a session document (docs/protocol.md §4.2–§4.6).
//!
//! A pure projection of the registry's live state into the JSON a replica
//! holds under `live`. This part covers what does not depend on which scope
//! shows an operation or an interaction: the session's identity, its reduced
//! controls, its terminals (those with a handle and the frozen output a tool
//! embeds) and whether a close is outstanding.
use serde_json::{Value, json};

use super::documents::{AgentInfo, Scope};
use super::engine;
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

/// The interactions pending in `scope`, by interaction ID (protocol §4.4).
/// They are a member of `live` beside the operation, not part of it.
fn interactions_doc(registry: &SessionRegistry, scope: &Scope) -> Value {
    let interactions = project::shown_operation(registry, scope)
        .map(|(operation, _)| operation.interactions)
        .unwrap_or_default();
    serde_json::to_value(interactions).expect("interactions serialize")
}

/// The accepted URL flows of `scope`, by interaction ID (protocol §4.6). A
/// flow is waiting until it is settled.
fn url_flows_doc(registry: &SessionRegistry, scope: &Scope) -> Value {
    let flows = match scope {
        Scope::Session(session_id) => registry
            .sessions
            .get(session_id)
            .and_then(|entry| entry.state.live.as_ref())
            .filter(|live| held(live))
            .map(|live| {
                project::url_flows(
                    live.url_flows
                        .values()
                        .chain(project::request_flows(registry, scope)),
                )
            })
            .unwrap_or_default(),
        _ => project::url_flows(project::request_flows(registry, scope)),
    };
    let mut flows = serde_json::to_value(flows).expect("flows serialize");
    for flow in flows
        .as_object_mut()
        .into_iter()
        .flat_map(|flows| flows.values_mut())
    {
        flow["status"] = Value::String("waiting".into());
    }
    flows
}

/// The complete `live` of `scope` (protocol §4.1–§4.4): the members every scope
/// has (`op`, `interactions`, `urlFlows`) and those of its kind. `None` when the
/// bridge does not hold the session.
pub(crate) fn scope_live(
    registry: &SessionRegistry,
    scope: &Scope,
    agent: &AgentInfo,
    catalog_revision: u64,
) -> Option<Value> {
    let mut live = match scope {
        Scope::Session(session_id) => session_live(registry, session_id)?,
        Scope::Global => global_live(registry, agent),
        Scope::Catalog => catalog_live(registry, catalog_revision),
    };
    live["op"] = operation_doc(registry, scope)?;
    live["interactions"] = interactions_doc(registry, scope);
    live["urlFlows"] = url_flows_doc(registry, scope);
    Some(live)
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
