//! The revision 4 tree projection (docs/protocol-r4-design.md §4.2, §4.4). Expected values
//! are written from the design's shapes, not computed by the projection.
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
