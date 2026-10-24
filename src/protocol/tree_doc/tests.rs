//! The revision 4 tree projection (docs/protocol-r4-design.md §4.2, §4.4). Expected values
//! are written from the design's shapes, not computed by the projection.
use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::protocol::documents::{AgentLaunch, AgentPhase, Scope};
use crate::protocol::tree::Tree;

fn agent() -> AgentInfo {
    AgentInfo {
        phase: AgentPhase::Ready,
        error: None,
        launch: AgentLaunch::default(),
        initialize: None,
        authenticated_method_id: None,
    }
}

fn registry() -> SessionRegistry {
    let mut registry = SessionRegistry::new("epoch");
    registry
        .open_new("epoch", "s", "/work", json!({ "sessionId": "s" }))
        .unwrap();
    registry
}

fn tree_of(registry: &SessionRegistry) -> Tree {
    let mut tree = Tree::new("i");
    tree.commit(project_tree(registry, &agent(), 1));
    tree
}

fn at(tree: &Tree, path: &str) -> Value {
    tree.get(path).unwrap_or_else(|| panic!("{path} is published")).to_value()
}

#[test]
fn global_is_the_agent_with_its_nodes() {
    let tree = tree_of(&registry());
    let global = at(&tree, "/global");
    assert_eq!(global["agent"]["phase"], "ready");
    assert_eq!(
        global["sessions"],
        json!({ "s": {
            "sessionId": "s", "cwd": "/work", "title": null,
            "op": { "kind": "open", "status": "completed" },
            "pending": { "interactions": 0, "urlFlows": 0 },
        } })
    );
    assert_eq!(global["terminals"], json!({}));
    assert_eq!(global["interactions"], json!({}));
    assert_eq!(global["urlFlows"], json!({}));
    assert!(global.get("loadFailures").is_none(), "loadFailures is added with the stream");
}

#[test]
fn a_session_is_one_node_under_sessions() {
    let mut registry = registry();
    let mut tree = tree_of(&registry);
    let before = tree.get("/global/sessions/s").unwrap().rev();
    registry
        .update_control_state(
            "epoch",
            "s",
            json!({ "sessionUpdate": "session_info_update", "title": "Titled" }),
        )
        .unwrap();
    tree.commit(project_tree(&registry, &agent(), 1));
    assert!(tree.get("/global/sessions/s").unwrap().rev() > before);
    assert_eq!(at(&tree, "/global/sessions/s")["title"], "Titled");
    assert_eq!(tree.get("/catalog").unwrap().srev(), 1);
}

#[test]
fn catalog_is_the_revision_and_its_pages() {
    let tree = tree_of(&registry());
    let catalog = at(&tree, "/catalog");
    assert_eq!(catalog["revision"], "1");
    assert_eq!(catalog["pages"], json!([]));
    assert_eq!(catalog["op"]["kind"], "none");
}

fn failed(registry: &mut SessionRegistry, error: ProtocolError) {
    registry
        .install_operation(&Scope::Global, OperationKind::Logout, json!({ "secret": "input" }))
        .unwrap();
    registry.fail_scope_operation(&Scope::Global, OperationKind::Logout, error);
}

#[test]
fn the_operation_head_holds_no_input_result_or_error_body() {
    let mut registry = registry();
    failed(&mut registry, ProtocolError::new(ErrorCode::Unavailable, "no connection"));
    let tree = tree_of(&registry);
    let head = at(&tree, "/global/op");
    assert_eq!(head["kind"], "logout");
    assert_eq!(head["status"], "failed");
    assert_eq!(head["cancelRequested"], false);
    assert_eq!(head["error"], json!({ "code": "unavailable" }));
    for body in ["input", "result", "message", "data"] {
        assert!(head.get(body).is_none(), "{body} is in opDetail");
    }
    assert_eq!(
        at(&tree, "/global/opDetail"),
        json!({
            "input": { "secret": "input" },
            "error": { "code": "unavailable", "message": "no connection" },
        })
    );
}

#[test]
fn an_agent_error_summary_carries_the_acp_code_and_not_its_message_or_data() {
    let big = "x".repeat(1 << 20);
    let mut registry = registry();
    failed(
        &mut registry,
        ProtocolError::agent(&json!({ "code": -32000, "message": big, "data": { "big": big } })),
    );
    let tree = tree_of(&registry);
    assert_eq!(
        at(&tree, "/global/op")["error"],
        json!({ "code": "agent_error", "agentCode": -32000 })
    );
    assert!(serde_json::to_string(&at(&tree, "/global/op")).unwrap().len() < 512);
    assert_eq!(at(&tree, "/global/opDetail")["error"]["message"], Value::String(big));
}

#[test]
fn an_error_that_is_not_the_agents_has_no_agent_code() {
    let summary = ErrorSummary::of(&ProtocolError {
        code: ErrorCode::Unavailable,
        message: "m".into(),
        data: Some(json!({ "code": 5 })),
    });
    assert_eq!(summary.agent_code, None);
}

#[test]
fn the_detail_changes_with_the_operation_and_the_head_with_its_status() {
    let mut registry = registry();
    let mut tree = tree_of(&registry);
    registry
        .install_operation(&Scope::Global, OperationKind::Logout, json!({}))
        .unwrap();
    tree.commit(project_tree(&registry, &agent(), 1));
    let running = tree.rev();
    assert_eq!(tree.get("/global/op").unwrap().rev(), running);
    assert_eq!(tree.get("/global/opDetail").unwrap().rev(), running);
    registry.complete_scope_operation(&Scope::Global, OperationKind::Logout, json!({}));
    tree.commit(project_tree(&registry, &agent(), 1));
    assert!(tree.get("/global/op").unwrap().rev() > running);
}

// --- /sessions/<id> -----------------------------------------------------------------

#[test]
fn a_held_session_is_published_with_identity_controls_and_nodes() {
    let tree = tree_of(&registry());
    let session = at(&tree, "/sessions/s");
    assert_eq!(session["sessionId"], "s");
    assert_eq!(session["cwd"], "/work");
    assert_eq!(session["closing"], false);
    assert_eq!(session["controls"]["commands"], json!([]));
    assert_eq!(session["op"]["kind"], "open");
    assert_eq!(session["opDetail"]["input"], json!({}));
    assert_eq!(session["terminals"], json!({}));
    assert_eq!(session["interactions"], json!({}));
    assert_eq!(session["urlFlows"], json!({}));
}

#[test]
fn a_controls_change_moves_only_the_controls_node() {
    let mut registry = registry();
    let mut tree = tree_of(&registry);
    registry
        .update_control_state(
            "epoch",
            "s",
            json!({ "sessionUpdate": "session_info_update", "title": "Titled" }),
        )
        .unwrap();
    let rev = tree.commit(project_tree(&registry, &agent(), 1)).unwrap();
    assert_eq!(tree.get("/sessions/s/controls").unwrap().rev(), rev);
    assert_eq!(tree.get("/sessions/s").unwrap().rev(), 1);
    assert_eq!(tree.get("/sessions/s/op").unwrap().rev(), 1);
    // The title is also in the summary global shows.
    assert_eq!(tree.get("/global/sessions/s").unwrap().rev(), rev);
}

#[test]
fn a_session_the_bridge_does_not_hold_is_not_published() {
    let registry = registry();
    assert!(session(&registry, "t").is_none());
    assert!(tree_of(&registry).get("/sessions/t").is_none());
}

#[test]
fn a_session_an_operation_occupies_shows_that_operation() {
    let mut registry = registry();
    registry
        .install_operation(&Scope::Global, OperationKind::Delete, json!({ "sessionId": "s" }))
        .unwrap();
    let tree = tree_of(&registry);
    let op = at(&tree, "/sessions/s/op");
    assert_eq!(op["scope"], "global");
    assert_eq!(op["kind"], "delete");
}

// --- entries and prompts ------------------------------------------------------------

fn chunk(registry: &mut SessionRegistry, id: &str, text: &str) {
    registry
        .append_session_update(
            "s",
            json!({ "sessionUpdate": "agent_message_chunk", "messageId": id,
                    "content": { "type": "text", "text": text } }),
        )
        .unwrap();
}

#[test]
fn entries_are_a_flat_list_of_the_published_entries_with_the_history() {
    let mut registry = registry();
    chunk(&mut registry, "m", "hi");
    let tree = tree_of(&registry);
    let session = at(&tree, "/sessions/s");
    assert_eq!(session["history"], "complete");
    assert_eq!(session["entries"][0]["kind"], "message");
    assert_eq!(session["entries"][0]["chunks"][0]["content"]["text"], "hi");
    assert_eq!(session["prompts"], json!([]));
    assert_eq!(tree.get("/sessions/s/entries/0").unwrap().to_value(), session["entries"][0]);
}

#[test]
fn an_unknown_history_has_no_entries_and_no_history() {
    let mut registry = registry();
    registry.register_cold("s");
    let session = at(&tree_of(&registry), "/sessions/s");
    assert!(session.get("history").is_none());
    assert!(session.get("entries").is_none());
    assert_eq!(session["prompts"], json!([]));
}

#[test]
fn streamed_text_changes_only_the_entry_that_grew() {
    let mut registry = registry();
    chunk(&mut registry, "a", "one");
    chunk(&mut registry, "b", "two");
    let mut projector = Projector::default();
    let mut tree = Tree::new("i");
    tree.commit(projector.project(&registry, &agent(), 1, tree.rev() + 1));
    assert_eq!(registry.sessions["s"].state.timeline.as_ref().unwrap().entries().len(), 2);
    chunk(&mut registry, "b", " more");
    let rev = tree.commit(projector.project(&registry, &agent(), 1, tree.rev() + 1)).unwrap();
    assert_eq!(tree.get("/sessions/s/entries/1").unwrap().rev(), rev);
    assert_eq!(tree.get("/sessions/s/entries/0").unwrap().rev(), 1);
    assert_eq!(tree.get("/sessions/s/entries").unwrap().rev(), 1, "no entry was added");
}

#[test]
fn an_unchanged_entry_is_published_with_the_same_value_object() {
    let mut registry = registry();
    chunk(&mut registry, "a", "one");
    let mut projector = Projector::default();
    let first = projector.project(&registry, &agent(), 1, 1);
    let second = projector.project(&registry, &agent(), 1, 1);
    let entry = |tree: &BTreeMap<String, Spec>| match &tree["sessions"] {
        Spec::Map(sessions) => match &sessions["s"] {
            Spec::Object { children, .. } => match &children["entries"] {
                Spec::List(entries) => match &entries[0] {
                    Spec::Value(value, _) => value.clone(),
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            },
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };
    assert!(Arc::ptr_eq(&entry(&first), &entry(&second)));
}

#[test]
fn a_replaced_history_replaces_the_entries_and_a_released_session_forgets_them() {
    let mut registry = registry();
    chunk(&mut registry, "a", "one");
    chunk(&mut registry, "b", "two");
    let mut projector = Projector::default();
    projector.project(&registry, &agent(), 1, 1);
    assert_eq!(projector.sessions["s"].entries.len(), 2);
    registry.register_cold("s");
    projector.project(&registry, &agent(), 1, 1);
    assert!(projector.sessions["s"].entries.is_empty());
    registry.sessions.get_mut("s").unwrap().state.live.as_mut().unwrap().lifecycle =
        crate::runtime_state::SessionLifecycle::Closed;
    projector.project(&registry, &agent(), 1, 1);
    assert!(!projector.sessions.contains_key("s"));
}

// --- historyRev and heldSince (§4.2) -------------------------------------------------

fn project_at(projector: &mut Projector, tree: &mut Tree, registry: &SessionRegistry) -> Option<u64> {
    tree.commit(projector.project(registry, &agent(), 1, tree.rev() + 1))
}

#[test]
fn a_session_is_held_since_the_revision_that_installed_it_and_keeps_it() {
    let mut registry = SessionRegistry::new("epoch");
    let (mut projector, mut tree) = (Projector::default(), Tree::new("i"));
    project_at(&mut projector, &mut tree, &registry);
    registry
        .open_new("epoch", "s", "/work", json!({ "sessionId": "s" }))
        .unwrap();
    let installed = project_at(&mut projector, &mut tree, &registry).unwrap();
    chunk(&mut registry, "m", "hi");
    project_at(&mut projector, &mut tree, &registry).unwrap();
    let session = at(&tree, "/sessions/s");
    assert_eq!(session["heldSince"], installed);
    assert_eq!(session["historyRev"], installed, "a new session's history is installed with it");
}

#[test]
fn growing_the_history_keeps_its_revision_and_replacing_it_takes_the_new_one() {
    let mut registry = registry();
    let (mut projector, mut tree) = (Projector::default(), Tree::new("i"));
    let first = project_at(&mut projector, &mut tree, &registry).unwrap();
    chunk(&mut registry, "m", "hi");
    project_at(&mut projector, &mut tree, &registry).unwrap();
    assert_eq!(at(&tree, "/sessions/s")["historyRev"], first);

    registry.register_cold("s");
    let unknown = project_at(&mut projector, &mut tree, &registry).unwrap();
    assert!(at(&tree, "/sessions/s").get("historyRev").is_none());

    registry.register_new("s");
    let replaced = project_at(&mut projector, &mut tree, &registry).unwrap();
    let session = at(&tree, "/sessions/s");
    assert!(replaced > unknown);
    assert_eq!(session["historyRev"], replaced);
    assert_eq!(session["heldSince"], first, "the session stayed held");
}

#[test]
fn a_session_released_and_held_again_is_held_since_the_later_revision() {
    let mut registry = registry();
    let (mut projector, mut tree) = (Projector::default(), Tree::new("i"));
    let first = project_at(&mut projector, &mut tree, &registry).unwrap();
    registry.sessions.get_mut("s").unwrap().state.live.as_mut().unwrap().lifecycle =
        crate::runtime_state::SessionLifecycle::Closed;
    project_at(&mut projector, &mut tree, &registry).unwrap();
    registry.sessions.get_mut("s").unwrap().state.live.as_mut().unwrap().lifecycle =
        crate::runtime_state::SessionLifecycle::Active;
    let again = project_at(&mut projector, &mut tree, &registry).unwrap();
    assert!(again > first);
    assert_eq!(at(&tree, "/sessions/s")["heldSince"], again);
}

// --- the turn index (§4.3) ----------------------------------------------------------

use crate::protocol::timeline::{PromptEnd, Timeline};

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn user(text: &str, id: Option<&str>) -> Value {
    let mut update = json!({ "sessionUpdate": "user_message_chunk", "content": text_block(text) });
    if let Some(id) = id {
        update["messageId"] = json!(id);
    }
    update
}

fn agent_message(text: &str, id: &str) -> Value {
    json!({ "sessionUpdate": "agent_message_chunk", "messageId": id, "content": text_block(text) })
}

fn head(start: usize, length: usize, id: Option<&str>, state: TurnState, output: Option<usize>) -> TurnHead {
    TurnHead {
        history_rev: 9,
        start,
        length,
        message_id: id.map(String::from),
        state,
        output,
    }
}

#[test]
fn turns_are_the_partition_of_the_entries_with_their_output() {
    let mut timeline = Timeline::default();
    for update in [
        agent_message("preface", "p"),
        user("q1", Some("u1")),
        json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "x" }),
        agent_message("a1", "a1"),
        agent_message("a1b", "a2"),
        user("q2", Some("u2")),
        json!({ "sessionUpdate": "tool_call", "toolCallId": "t2", "title": "y" }),
    ] {
        timeline.reduce(&update, None);
    }
    assert_eq!(
        turn_heads(&timeline, 9),
        vec![
            head(0, 1, None, TurnState::Ended, Some(0)),
            head(1, 4, Some("u1"), TurnState::Ended, Some(4)),
            head(5, 2, Some("u2"), TurnState::Ended, None),
        ]
    );
}

#[test]
fn a_turn_runs_exactly_while_its_prompt_has_no_end() {
    let mut timeline = Timeline::default();
    timeline.begin_prompt(&[text_block("a")]);
    timeline.reduce(&agent_message("x", "m"), None);
    assert_eq!(turn_heads(&timeline, 9)[0].state, TurnState::Running);
    timeline.end_prompt(PromptEnd::Response(json!({ "stopReason": "end_turn" })));
    assert_eq!(turn_heads(&timeline, 9)[0].state, TurnState::Ended);
    timeline.begin_prompt(&[text_block("b")]);
    let turns = turn_heads(&timeline, 9);
    assert_eq!((turns[0].state, turns[1].state), (TurnState::Ended, TurnState::Running));
    assert_eq!(turns[1].output, None);
}

#[test]
fn a_new_turn_adds_to_the_index_and_leaves_the_turns_before_it_unchanged() {
    let mut registry = registry();
    let (mut projector, mut tree) = (Projector::default(), Tree::new("i"));
    for update in [user("q", None), agent_message("a", "a")] {
        registry.append_session_update("s", update).unwrap();
    }
    project_at(&mut projector, &mut tree, &registry).unwrap();
    assert_eq!(at(&tree, "/sessions/s/turns").as_array().unwrap().len(), 1);
    let entries_rev = tree.get("/sessions/s/entries/0").unwrap().rev();

    registry.append_session_update("s", user("q more", None)).unwrap();
    project_at(&mut projector, &mut tree, &registry).unwrap();
    let turns = at(&tree, "/sessions/s/turns");
    assert_eq!(turns.as_array().unwrap().len(), 2);
    assert_eq!(tree.get("/sessions/s/entries/0").unwrap().rev(), entries_rev);
    assert_eq!(tree.get("/sessions/s/turns/0").unwrap().rev(), entries_rev, "turn 0 is unchanged");
    assert_eq!(tree.get("/sessions/s/turns/1").unwrap().rev(), tree.rev());
}

#[test]
fn turns_are_published_with_the_session_and_say_which_history_they_index() {
    let mut registry = registry();
    chunk(&mut registry, "m", "hi");
    let tree = tree_of(&registry);
    let session = at(&tree, "/sessions/s");
    assert_eq!(
        session["turns"],
        json!([{ "historyRev": session["historyRev"], "start": 0, "length": 1,
                 "state": "ended", "output": 0 }])
    );
}

#[test]
fn an_unknown_history_has_no_turns() {
    let mut registry = registry();
    registry.register_cold("s");
    assert!(at(&tree_of(&registry), "/sessions/s").get("turns").is_none());
}
