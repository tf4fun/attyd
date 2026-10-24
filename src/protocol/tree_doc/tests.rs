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
    tree.commit(projector.project(&registry, &agent(), 1));
    assert_eq!(registry.sessions["s"].state.timeline.as_ref().unwrap().entries().len(), 2);
    chunk(&mut registry, "a", " more");
    let rev = tree.commit(projector.project(&registry, &agent(), 1)).unwrap();
    assert_eq!(tree.get("/sessions/s/entries/0").unwrap().rev(), rev);
    assert_eq!(tree.get("/sessions/s/entries/1").unwrap().rev(), 1);
    assert_eq!(tree.get("/sessions/s/entries").unwrap().rev(), 1, "no entry was added");
}

#[test]
fn an_unchanged_entry_is_published_with_the_same_value_object() {
    let mut registry = registry();
    chunk(&mut registry, "a", "one");
    let mut projector = Projector::default();
    let first = projector.project(&registry, &agent(), 1);
    let second = projector.project(&registry, &agent(), 1);
    let entry = |tree: &BTreeMap<String, Spec>| match &tree["sessions"] {
        Spec::Map(sessions) => match &sessions["s"] {
            Spec::Object { children, .. } => match &children["entries"] {
                Spec::List(entries) => match &entries[0] {
                    Spec::Value(value) => value.clone(),
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
    projector.project(&registry, &agent(), 1);
    assert_eq!(projector.entries["s"].len(), 2);
    registry.register_cold("s");
    projector.project(&registry, &agent(), 1);
    assert!(projector.entries["s"].is_empty());
    registry.sessions.get_mut("s").unwrap().state.live.as_mut().unwrap().lifecycle =
        crate::runtime_state::SessionLifecycle::Closed;
    projector.project(&registry, &agent(), 1);
    assert!(!projector.entries.contains_key("s"));
}
