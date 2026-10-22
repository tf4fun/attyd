//! The live half of a session document (docs/protocol.md §4.2–§4.6). Expected
//! values are written from the protocol's shapes, not computed by the builder.

use serde_json::{Value, json};

use super::*;
use crate::protocol::documents::{AgentInfo, AgentLaunch, AgentPhase, OperationKind, Scope};
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
    assert_eq!(catalog_live(&registry(), 7), json!({ "revision": "7", "pages": [] }));
}

// --- the operation a scope shows (§4.4) ----------------------------------------------


fn operation(registry: &SessionRegistry, scope: &Scope) -> Value {
    operation_doc(registry, scope).expect("the scope shows an operation")
}

#[test]
fn a_scopes_first_operation_is_over_and_replaced_nothing() {
    let registry = registry();
    let open = operation(&registry, &Scope::Session("s".into()));
    assert_eq!(open["kind"], "open");
    assert_eq!(open["status"], "completed");
    assert_eq!(open["after"], Value::Null);
    assert_eq!(open["cancelRequested"], false);
    assert_eq!(open["result"], json!({}));
    assert!(open.get("interactions").is_none() && open.get("process").is_none());
    let global = operation(&registry, &Scope::Global);
    assert_eq!(global["kind"], "none");
    assert_eq!(global["after"], Value::Null);
}

#[test]
fn an_operation_names_its_scope_the_operation_it_replaced_and_its_input() {
    let mut registry = registry();
    let scope = Scope::Session("s".into());
    let open = operation(&registry, &scope)["id"].as_str().unwrap().to_string();
    registry
        .install_operation_as(
            &scope,
            OperationKind::SetMode,
            json!({ "modeId": "plan" }),
            Some("Zm9yLXRoZS1jbGllbnQtMQ".into()),
            Some(open.clone()),
        )
        .unwrap();
    assert_eq!(
        operation(&registry, &scope),
        json!({
            "id": "Zm9yLXRoZS1jbGllbnQtMQ", "scope": "session/s", "after": open,
            "kind": "set_mode", "status": "running", "input": { "modeId": "plan" },
            "cancelRequested": false,
        })
    );
}

#[test]
fn a_prompts_input_is_its_normalized_blocks_with_attachment_references() {
    let mut registry = registry();
    let scope = Scope::Session("s".into());
    registry
        .install_prompt_as(
            "s",
            &[
                json!({ "type": "text", "text": "look" }),
                json!({ "type": "image", "data": "AAAA", "mimeType": "image/png" }),
            ],
            None,
            None,
        )
        .unwrap();
    let input = &operation(&registry, &scope)["input"];
    assert_eq!(input["prompt"][0], json!({ "type": "text", "text": "look" }));
    assert_eq!(input["prompt"][1]["type"], "attachment");
    assert_eq!(input["prompt"][1]["block"], "image");
}

#[test]
fn a_scope_shows_the_operation_that_occupies_it() {
    let mut registry = registry();
    registry
        .install_operation_as(
            &Scope::Global,
            OperationKind::Delete,
            json!({ "sessionId": "s" }),
            None,
            None,
        )
        .unwrap();
    let deleting = operation(&registry, &Scope::Global);
    let shown = operation(&registry, &Scope::Session("s".into()));
    assert_eq!(shown["id"], deleting["id"], "the session shows the delete that occupies it");
    assert_eq!(shown["kind"], "delete");
    assert_eq!(shown["scope"], "global", "an operation names the scope it was written to");
}

#[test]
fn a_finished_operation_keeps_its_result_or_error() {
    let mut registry = registry();
    let scope = Scope::Global;
    registry
        .install_operation(&scope, OperationKind::Authenticate, json!({ "methodId": "m" }))
        .unwrap();
    registry.complete_scope_operation(&scope, OperationKind::Authenticate, json!({}));
    let done = operation(&registry, &scope);
    assert_eq!(done["status"], "completed");
    assert_eq!(done["result"], json!({}));

    registry
        .install_operation(&scope, OperationKind::Logout, json!({}))
        .unwrap();
    registry.fail_scope_operation(
        &scope,
        OperationKind::Logout,
        crate::protocol::documents::ProtocolError::new(
            crate::protocol::documents::ErrorCode::Unavailable,
            "no connection",
        ),
    );
    let failed = operation(&registry, &scope);
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["error"]["code"], "unavailable");
    assert!(failed.get("result").is_none());
}

// --- interactions and URL flows (§4.4, §4.6) ------------------------------------------

fn pending_permission(registry: &mut SessionRegistry, id: &str) {
    registry
        .bind_interaction(id, Scope::Session("s".into()));
    registry
        .upsert_permission("epoch", "s", id, json!({ "sessionId": "s", "toolCall": { "toolCallId": "t" } }))
        .unwrap();
}

#[test]
fn pending_interactions_are_a_member_of_live_beside_the_operation() {
    let mut registry = registry();
    pending_permission(&mut registry, "p1");
    let live = scope_live(&registry, &Scope::Session("s".into()), &agent(), 0).unwrap();
    assert_eq!(
        live["interactions"]["p1"],
        json!({ "interactionId": "p1", "kind": "permission",
                "request": { "sessionId": "s", "toolCall": { "toolCallId": "t" } } })
    );
    assert!(live["op"].get("interactions").is_none());
    assert_eq!(live["urlFlows"], json!({}));
}

#[test]
fn a_session_live_document_has_every_member_of_the_protocol() {
    let live = scope_live(&registry(), &Scope::Session("s".into()), &agent(), 0).unwrap();
    let mut members: Vec<&str> = live.as_object().unwrap().keys().map(String::as_str).collect();
    members.sort_unstable();
    assert_eq!(
        members,
        ["closing", "controls", "cwd", "interactions", "op", "sessionId", "terminals", "urlFlows"]
    );
    assert_eq!(live["op"]["kind"], "open");
}

#[test]
fn global_and_catalog_have_the_scope_members_too() {
    let registry = registry();
    let global = scope_live(&registry, &Scope::Global, &agent(), 4).unwrap();
    let mut members: Vec<&str> = global.as_object().unwrap().keys().map(String::as_str).collect();
    members.sort_unstable();
    assert_eq!(members, ["agent", "interactions", "op", "sessions", "terminals", "urlFlows"]);
    assert_eq!(global["op"]["kind"], "none");

    let catalog = scope_live(&registry, &Scope::Catalog, &agent(), 4).unwrap();
    let mut members: Vec<&str> = catalog.as_object().unwrap().keys().map(String::as_str).collect();
    members.sort_unstable();
    assert_eq!(members, ["interactions", "op", "pages", "revision", "urlFlows"]);
    assert_eq!(catalog["revision"], "4");
}

#[test]
fn an_accepted_url_flow_waits_with_its_status() {
    let mut registry = registry();
    let scope = Scope::Session("s".into());
    let request = json!({ "mode": "url", "elicitationId": "e", "url": "https://example.test" });
    registry.bind_interaction("i1", scope.clone());
    registry
        .upsert_elicitation("epoch", Some("s"), "i1", request.clone())
        .unwrap();
    registry
        .resolve_elicitation("epoch", Some("s"), "i1", Some("e"))
        .unwrap();
    let live = scope_live(&registry, &scope, &agent(), 0).unwrap();
    assert_eq!(
        live["urlFlows"],
        json!({ "i1": { "interactionId": "i1", "elicitationId": "e", "request": request,
                        "status": "waiting" } })
    );
    assert_eq!(live["interactions"], json!({}), "an accepted elicitation is no longer pending");
}

#[test]
fn an_unknown_scope_has_no_live_document() {
    assert_eq!(scope_live(&registry(), &Scope::Session("unknown".into()), &agent(), 0), None);
}

// --- what the publisher is handed (engine input) ----------------------------------------

use crate::protocol::timeline::History;

#[test]
fn a_sessions_input_carries_its_epoch_history_timeline_and_complete_live() {
    let mut registry = registry();
    registry
        .append_session_update(
            "s",
            json!({ "sessionUpdate": "agent_message_chunk", "messageId": "m",
                    "content": { "type": "text", "text": "hi" } }),
        )
        .unwrap();
    let input = engine_input(&registry, &Scope::Session("s".into()), &agent(), 0, "process")
        .expect("the session is held");
    let state = &registry.sessions.get("s").unwrap().state;
    assert_eq!(Some(input.epoch.clone()), state.timeline_epoch);
    assert_ne!(input.epoch, "process", "a session's epoch is its timeline's");
    assert_eq!(input.history, Some(History::Complete));
    assert_eq!(input.timeline.as_ref().unwrap().entries().len(), 1);
    assert_eq!(input.live["sessionId"], "s");
    assert!(input.live.get("op").is_some() && input.live.get("interactions").is_some());
}

#[test]
fn a_session_with_an_unknown_history_has_no_timeline() {
    let mut registry = SessionRegistry::new("epoch");
    registry
        .open_new("epoch", "c", "/c", json!({ "sessionId": "c" }))
        .unwrap();
    // The session stays held, but what it knew of its history is gone.
    registry.register_cold("c");
    let input = engine_input(&registry, &Scope::Session("c".into()), &agent(), 0, "process").unwrap();
    assert!(input.timeline.is_none());
    assert_eq!(input.history, None);
}

#[test]
fn global_and_catalog_are_published_under_the_process_epoch_without_a_timeline() {
    let registry = registry();
    for scope in [Scope::Global, Scope::Catalog] {
        let input = engine_input(&registry, &scope, &agent(), 3, "process-epoch").unwrap();
        assert_eq!(input.epoch, "process-epoch");
        assert!(input.timeline.is_none() && input.history.is_none());
    }
}

#[test]
fn an_unheld_session_has_no_input() {
    assert!(engine_input(&registry(), &Scope::Session("nope".into()), &agent(), 0, "p").is_none());
}

// -- The origin of a session (protocol §4.3, §6.2) ---------------------------------------

fn summary_of(registry: &SessionRegistry, session_id: &str) -> Value {
    global_live(registry, &agent())["sessions"][session_id].clone()
}

#[test]
fn a_session_a_create_installed_names_that_operation_as_its_origin() {
    let mut registry = registry();
    assert!(summary_of(&registry, "s").get("origin").is_none(), "not made by an operation");

    let scope = Scope::Global;
    registry
        .install_operation(&scope, OperationKind::Create, json!({ "cwd": "/work" }))
        .unwrap();
    let id = operation(&registry, &scope)["id"].clone();
    registry.complete_scope_operation(&scope, OperationKind::Create, json!({ "sessionId": "s" }));
    assert_eq!(summary_of(&registry, "s")["origin"], id);
}

#[test]
fn a_session_a_fork_installed_names_the_fork_as_its_origin() {
    let mut registry = registry();
    registry
        .open_new("epoch", "f", "/work", json!({ "sessionId": "f" }))
        .unwrap();
    let source = Scope::Session("s".into());
    registry
        .install_operation(&source, OperationKind::Fork, json!({}))
        .unwrap();
    let id = operation(&registry, &source)["id"].clone();
    registry.complete_session_operation(
        "s",
        crate::runtime_state::SessionOperationKind::Fork,
        &json!({ "sessionId": "f" }),
    );
    assert_eq!(summary_of(&registry, "f")["origin"], id);
    assert!(summary_of(&registry, "s").get("origin").is_none(), "the source is not the fork's");
}

#[test]
fn a_create_that_installed_a_session_not_held_names_no_origin() {
    let mut registry = registry();
    let scope = Scope::Global;
    registry
        .install_operation(&scope, OperationKind::Create, json!({ "cwd": "/work" }))
        .unwrap();
    registry.complete_scope_operation(&scope, OperationKind::Create, json!({ "sessionId": "gone" }));
    assert!(summary_of(&registry, "s").get("origin").is_none());
}

// -- The pages of the catalog (protocol §4.3, §6.2) ----------------------------------------

fn list(registry: &mut SessionRegistry, input: Value, revision: &str, sessions: Value, next: Value) {
    registry
        .install_operation(&Scope::Catalog, OperationKind::List, input)
        .unwrap();
    registry.complete_scope_operation(
        &Scope::Catalog,
        OperationKind::List,
        json!({ "revision": revision, "sessions": sessions, "nextCursor": next }),
    );
}

fn pages(registry: &SessionRegistry, revision: u64) -> Value {
    catalog_live(registry, revision)["pages"].clone()
}

#[test]
fn the_catalog_has_no_pages_before_it_is_listed() {
    assert_eq!(pages(&registry(), 0), json!([]));
}

#[test]
fn a_listing_without_a_cursor_starts_over_and_a_cursor_appends_the_next_page() {
    let mut registry = registry();
    list(&mut registry, json!({}), "1", json!([{ "sessionId": "a" }]), json!("c1"));
    assert_eq!(
        pages(&registry, 1),
        json!([{ "sessions": [{ "sessionId": "a" }], "nextCursor": "c1" }])
    );

    list(&mut registry, json!({ "cursor": "c1" }), "1", json!([{ "sessionId": "b" }]), Value::Null);
    assert_eq!(
        pages(&registry, 1),
        json!([
            { "sessions": [{ "sessionId": "a" }], "nextCursor": "c1" },
            { "sessions": [{ "sessionId": "b" }], "nextCursor": null },
        ])
    );

    list(&mut registry, json!({}), "1", json!([{ "sessionId": "c" }]), Value::Null);
    assert_eq!(
        pages(&registry, 1),
        json!([{ "sessions": [{ "sessionId": "c" }], "nextCursor": null }]),
        "a new listing replaces the pages"
    );
}

#[test]
fn pages_listed_before_the_revision_changed_are_not_shown() {
    let mut registry = registry();
    list(&mut registry, json!({}), "1", json!([{ "sessionId": "a" }]), Value::Null);
    assert_eq!(pages(&registry, 2), json!([]), "listed at revision 1, shown at 2");
    // A cursor listed after the change does not continue the old pages.
    list(&mut registry, json!({ "cursor": "c" }), "2", json!([{ "sessionId": "b" }]), Value::Null);
    assert_eq!(
        pages(&registry, 2),
        json!([{ "sessions": [{ "sessionId": "b" }], "nextCursor": null }])
    );
}

#[test]
fn a_failed_listing_leaves_the_pages() {
    let mut registry = registry();
    list(&mut registry, json!({}), "1", json!([{ "sessionId": "a" }]), Value::Null);
    registry
        .install_operation(&Scope::Catalog, OperationKind::List, json!({}))
        .unwrap();
    registry.fail_scope_operation(
        &Scope::Catalog,
        OperationKind::List,
        crate::protocol::documents::ProtocolError::new(
            crate::protocol::documents::ErrorCode::Unavailable,
            "gone",
        ),
    );
    assert_eq!(pages(&registry, 1).as_array().map(Vec::len), Some(1));
}
