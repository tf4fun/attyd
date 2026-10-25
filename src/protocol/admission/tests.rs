use std::collections::BTreeMap;

use super::*;
use crate::protocol::tree::Spec;

const ID: &str = "AAAAAAAAAAAAAAAAAAAAAA";
const SESSION: &str = "a/session~id";

fn operation(kind: &str, status: &str) -> Value {
    json!({ "id": ID, "kind": kind, "status": status })
}

fn published(initialize: Value, global_op: Value, session_op: Value) -> Tree {
    let mut tree = Tree::new("admission");
    tree.commit(BTreeMap::from([
        (
            "global".into(),
            Spec::object(
                json!({ "agent": { "initialize": initialize } }).as_object().unwrap().clone(),
                [
                    ("op", Spec::value(global_op)),
                    ("sessions", Spec::map([(SESSION, Spec::value(json!({ "op": session_op })))])),
                ],
            ),
        ),
        (
            "catalog".into(),
            Spec::object(
                json!({ "revision": "7" }).as_object().unwrap().clone(),
                [("op", Spec::value(operation("none", "completed")))],
            ),
        ),
        (
            "sessions".into(),
            Spec::map([(
                SESSION,
                Spec::object(Default::default(), [("op", Spec::value(session_op))]),
            )]),
        ),
    ])).expect("initial publication");
    tree
}

fn write(kind: &str, args: Value) -> OpWrite {
    OpWrite { id: None, expect: ID.into(), kind: kind.into(), args }
}

#[test]
fn repeated_and_stale_writes_are_decided_by_the_committed_operation_head() {
    let tree = published(Value::Null, operation("none", "completed"), operation("prompt", "running"));
    let scope = Scope::Session(SESSION.into());
    // The target is a raw map key even when the ID contains JSON Pointer delimiters.
    let mut request = write("prompt", Value::Null);
    request.id = Some(ID.into());
    request.expect = "old".into();
    assert_eq!(admit(&tree, &|_, _| None, &scope, &request), Ok(Admitted::Repeat { op: ID.into() }));

    request.id = None;
    assert_eq!(admit(&tree, &|_, _| None, &scope, &request).unwrap_err().code, ErrorCode::Stale);
    request.expect = ID.into();
    assert_eq!(admit(&tree, &|_, _| None, &scope, &request).unwrap_err().code, ErrorCode::NotAllowed);
    assert_eq!(
        admit(&tree, &|_, _| None, &Scope::Session("missing".into()), &request).unwrap_err().code,
        ErrorCode::NotFound,
    );
}

#[test]
fn fork_and_delete_keep_their_cross_target_exclusions() {
    let session = Scope::Session(SESSION.into());
    for (scope, global_op, session_op, request) in [
        (
            session.clone(),
            operation("authenticate", "running"),
            operation("open", "completed"),
            write("fork", json!({})),
        ),
        (
            Scope::Global,
            operation("none", "completed"),
            operation("fork", "running"),
            write("create", json!({ "cwd": "/tmp" })),
        ),
        (
            Scope::Global,
            operation("none", "completed"),
            operation("prompt", "running"),
            write("delete", json!({ "sessionId": SESSION })),
        ),
    ] {
        let tree = published(Value::Null, global_op, session_op);
        assert_eq!(admit(&tree, &|_, _| None, &scope, &request).unwrap_err().code, ErrorCode::NotAllowed);
    }
    let tree = published(Value::Null, operation("none", "completed"), operation("open", "completed"));
    assert!(matches!(
        admit(&tree, &|_, _| None, &session, &write("fork", json!({}))),
        Ok(Admitted::Start { kind: OperationKind::Fork, .. }),
    ));
}

#[test]
fn capabilities_and_catalog_revision_are_read_from_the_target_fields() {
    let tree = published(json!({}), operation("none", "completed"), operation("open", "completed"));
    assert_eq!(
        admit(&tree, &|_, _| None, &Scope::Catalog, &write("list", Value::Null)).unwrap_err().code,
        ErrorCode::Unsupported,
    );

    let tree = published(
        json!({ "agentCapabilities": { "sessionCapabilities": { "list": {} } } }),
        operation("none", "completed"),
        operation("open", "completed"),
    );
    assert_eq!(
        admit(&tree, &|_, _| None, &Scope::Catalog, &write("list", json!({ "revision": "6" })))
            .unwrap_err().code,
        ErrorCode::CatalogChanged,
    );
    assert!(matches!(
        admit(&tree, &|_, _| None, &Scope::Catalog, &write("list", json!({ "revision": "7" }))),
        Ok(Admitted::Start { kind: OperationKind::List, .. }),
    ));
}
