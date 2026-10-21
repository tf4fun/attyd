//! The live half of a session document (docs/protocol.md §4.2–§4.6). Expected
//! values are written from the protocol's shapes, not computed by the builder.

use serde_json::{Value, json};

use super::*;
use crate::protocol::documents::{AgentInfo, AgentLaunch, AgentPhase};
use crate::runtime_state::SessionLifecycle;
use crate::session_registry::SessionRegistry;

fn agent() -> AgentInfo {
    AgentInfo {
        phase: AgentPhase::Ready,
        error: None,
        launch: AgentLaunch::default(),
        initialize: None,
        authenticated_method_id: Some("login".into()),
    }
}

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
        let what = format!("{closing:?}");
        lifecycle(&mut registry, closing);
        assert_eq!(live(&registry)["closing"], true, "{what}");
    }
    lifecycle(&mut registry, SessionLifecycle::Active);
    assert_eq!(live(&registry)["closing"], false);
    lifecycle(&mut registry, SessionLifecycle::Closed);
    assert_eq!(session_live(&registry, "s"), None);
    assert_eq!(session_live(&registry, "unknown"), None);
}

// --- global and catalog (§4.3) -----------------------------------------------------

#[test]
fn global_names_the_agent_the_held_sessions_and_the_terminal_logins() {
    let mut registry = registry();
    registry
        .update_control_state(
            "epoch",
            "s",
            json!({ "sessionUpdate": "session_info_update", "title": "Titled" }),
        )
        .unwrap();
    registry.start_login_terminal("login-op", "agent-login");
    registry.login_output("login-op", "Opening a browser...");

    let live = global_live(&registry, &agent());
    assert_eq!(live["agent"]["phase"], "ready");
    assert_eq!(live["agent"]["authenticatedMethodId"], "login");
    assert_eq!(live["agent"]["error"], Value::Null);
    assert_eq!(
        live["sessions"],
        json!({ "s": {
            "sessionId": "s", "cwd": "/work", "title": "Titled",
            "op": { "kind": "open", "status": "completed" },
            "pending": { "interactions": 0, "urlFlows": 0 },
        } })
    );
    assert_eq!(
        live["terminals"],
        json!({ "login-op": {
            "terminalId": "login-op", "output": "Opening a browser...", "truncated": false,
            "exitStatus": null, "methodId": "agent-login", "released": false,
        } })
    );
}

#[test]
fn global_lists_only_the_sessions_the_bridge_holds() {
    let mut registry = registry();
    registry
        .open_new("epoch", "t", "/other", json!({ "sessionId": "t" }))
        .unwrap();
    let live = global_live(&registry, &agent());
    let mut held: Vec<&String> = live["sessions"].as_object().unwrap().keys().collect();
    held.sort();
    assert_eq!(held, ["s", "t"]);
    registry
        .sessions
        .get_mut("t")
        .unwrap()
        .state
        .live
        .as_mut()
        .unwrap()
        .lifecycle = SessionLifecycle::Closed;
    assert!(global_live(&registry, &agent())["sessions"].get("t").is_none());
}

#[test]
fn the_catalog_names_the_revision_of_the_agents_list() {
    assert_eq!(catalog_live(&registry(), 7), json!({ "revision": "7" }));
}
