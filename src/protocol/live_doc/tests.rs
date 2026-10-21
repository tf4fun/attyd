//! The live half of a session document (docs/protocol.md §4.2–§4.6). Expected
//! values are written from the protocol's shapes, not computed by the builder.

use serde_json::{Value, json};

use super::*;
use crate::runtime_state::SessionLifecycle;
use crate::session_registry::SessionRegistry;

fn registry() -> SessionRegistry {
    let mut registry = SessionRegistry::new("epoch");
    registry
        .open_new("epoch", "s", "/work", json!({ "sessionId": "s" }))
        .unwrap();
    registry
}

fn live(registry: &SessionRegistry) -> Value {
    session_live(registry, "s").expect("the session is held")
}

#[test]
fn a_new_session_names_itself_and_its_directory_and_has_nothing_running() {
    let live = live(&registry());
    assert_eq!(live["sessionId"], "s");
    assert_eq!(live["cwd"], "/work");
    assert_eq!(live["closing"], false);
    assert_eq!(live["terminals"], json!({}));
    assert_eq!(live["controls"]["modes"], Value::Null);
    assert_eq!(live["controls"]["commands"], json!([]));
}

#[test]
fn the_controls_are_the_reduced_current_state() {
    let mut registry = registry();
    registry
        .update_control_state(
            "epoch",
            "s",
            json!({ "sessionUpdate": "available_commands_update",
                    "availableCommands": [{ "name": "plan", "description": "Plan" }] }),
        )
        .unwrap();
    registry
        .update_control_state(
            "epoch",
            "s",
            json!({ "sessionUpdate": "session_info_update", "title": "Titled" }),
        )
        .unwrap();
    let live = live(&registry);
    assert_eq!(live["controls"]["commands"][0]["name"], "plan");
    assert_eq!(live["controls"]["info"]["title"], "Titled");
}

#[test]
fn terminals_with_a_handle_and_frozen_ones_are_published_with_their_state() {
    let mut registry = registry();
    registry
        .upsert_terminal(
            "epoch",
            "s",
            "running",
            json!({ "output": "so far", "truncated": true, "released": false }),
        )
        .unwrap();
    registry
        .upsert_terminal(
            "epoch",
            "s",
            "exited",
            json!({ "output": "done\n", "released": false,
                    "exitStatus": { "exitCode": 3, "signal": null } }),
        )
        .unwrap();
    // A terminal a tool embeds keeps its output after its release.
    registry
        .append_session_update(
            "s",
            json!({ "sessionUpdate": "tool_call", "toolCallId": "run", "title": "Run",
                    "content": [{ "type": "terminal", "terminalId": "exited" }] }),
        )
        .unwrap();
    registry
        .upsert_terminal(
            "epoch",
            "s",
            "exited",
            json!({ "output": "done\n", "released": true,
                    "exitStatus": { "exitCode": 3, "signal": null } }),
        )
        .unwrap();

    assert_eq!(
        live(&registry)["terminals"],
        json!({
            "exited": { "terminalId": "exited", "output": "done\n", "truncated": false,
                        "exitStatus": { "exitCode": 3, "signal": null }, "released": true },
            "running": { "terminalId": "running", "output": "so far", "truncated": true,
                         "exitStatus": null, "released": false },
        })
    );
}

#[test]
fn a_session_closing_says_so_and_a_closed_one_is_not_published() {
    let mut registry = registry();
    let lifecycle = |registry: &mut SessionRegistry, lifecycle| {
        registry
            .sessions
            .get_mut("s")
            .unwrap()
            .state
            .live
            .as_mut()
            .unwrap()
            .lifecycle = lifecycle;
    };
    for closing in [SessionLifecycle::Closing, SessionLifecycle::ClosingForDelete] {
        lifecycle(&mut registry, closing);
        assert_eq!(live(&registry)["closing"], true, "{closing:?}");
    }
    lifecycle(&mut registry, SessionLifecycle::Active);
    assert_eq!(live(&registry)["closing"], false);
    lifecycle(&mut registry, SessionLifecycle::Closed);
    assert_eq!(session_live(&registry, "s"), None);
    assert_eq!(session_live(&registry, "unknown"), None);
}
