//! The live half of a session document (docs/protocol.md §4.2–§4.6).
//!
//! A pure projection of the registry's live state into the JSON a replica
//! holds under `live`. This part covers what does not depend on which scope
//! shows an operation or an interaction: the session's identity, its reduced
//! controls, its terminals (those with a handle and the frozen output a tool
//! embeds) and whether a close is outstanding.
use serde_json::{Value, json};

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

#[cfg(test)]
mod tests;
