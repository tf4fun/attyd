//! The outcome of a lost write against the vectors an independent statement of the table
//! computed (tests/fixtures/protocol_r4_outcome_vectors.py); `shared/outcome.ts` replays them too.
use serde_json::{Value, json};

use super::*;

fn vectors() -> Vec<Value> {
    let file: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/protocol-r4-outcome-vectors.json"
    ))
    .expect("the vectors are JSON");
    file["cases"].as_array().expect("cases").clone()
}

fn target(case: &Value) -> TargetState {
    let target = &case["target"];
    TargetState {
        instance: target["instance"].as_str().unwrap().into(),
        op: target["op"].as_str().unwrap().into(),
        held_since: target["heldSince"].as_u64(),
        ops: target["ops"]
            .as_array()
            .unwrap()
            .iter()
            .map(|record| Logged {
                id: record["id"].as_str().unwrap().into(),
                after: record["after"].as_str().map(String::from),
            })
            .collect(),
    }
}

fn request(case: &Value) -> Request {
    let request = &case["request"];
    Request {
        id: request["id"].as_str().unwrap().into(),
        expect: request["expect"].as_str().unwrap().into(),
        instance: request["instance"].as_str().unwrap().into(),
        at: request["at"].as_u64().unwrap(),
    }
}

#[test]
fn outcomes_match_the_independent_vectors() {
    let cases = vectors();
    assert!(cases.len() >= 16);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let got = serde_json::to_value(outcome(&target(&case), &request(&case))).unwrap();
        assert_eq!(got, case["expected"], "{name}");
    }
}

#[test]
fn the_outcomes_have_the_documented_wire_shapes() {
    assert_eq!(
        serde_json::to_value(Outcome::Admitted { record: "x".into() }).unwrap(),
        json!({ "outcome": "admitted", "record": "x" })
    );
    assert_eq!(serde_json::to_value(Outcome::Superseded).unwrap(), json!({ "outcome": "superseded" }));
    assert_eq!(serde_json::to_value(Outcome::Open).unwrap(), json!({ "outcome": "open" }));
    assert_eq!(
        serde_json::to_value(Outcome::Unknown { reason: Reason::Restarted }).unwrap(),
        json!({ "outcome": "unknown", "reason": "restarted" })
    );
}

// --- from the tree the bridge publishes ------------------------------------------------

use crate::protocol::documents::{AgentInfo, AgentLaunch, AgentPhase, OperationKind, Scope};
use crate::protocol::tree::Tree;
use crate::protocol::tree_doc::Projector;
use crate::session_registry::SessionRegistry;

fn agent() -> AgentInfo {
    AgentInfo {
        phase: AgentPhase::Ready,
        error: None,
        launch: AgentLaunch::default(),
        initialize: None,
        authenticated_method_id: None,
    }
}

struct Bridge {
    registry: SessionRegistry,
    projector: Projector,
    tree: Tree,
}

impl Bridge {
    fn new() -> Self {
        let mut registry = SessionRegistry::new("epoch");
        registry.open_new("epoch", "s", "/work", json!({ "sessionId": "s" })).unwrap();
        let mut bridge = Self { registry, projector: Projector::default(), tree: Tree::new("I") };
        bridge.commit();
        bridge
    }

    fn commit(&mut self) -> Option<u64> {
        let next = self.tree.rev() + 1;
        self.tree.commit(self.projector.project(&self.registry, &agent(), 1, next))
    }

    fn state(&self, target: &str) -> TargetState {
        state_of(&self.tree, target).unwrap()
    }

    fn write(&mut self, scope: &Scope, kind: OperationKind) -> String {
        let id = self.registry.install_operation(scope, kind, json!({})).unwrap();
        self.commit();
        id
    }
}

fn asked(id: &str, expect: &str, bridge: &Bridge, target: &str) -> Outcome {
    let at = bridge.tree.get(&format!("/{target}")).unwrap().srev();
    let request = Request { id: id.into(), expect: expect.into(), instance: "I".into(), at };
    outcome(&bridge.state(target), &request)
}

#[test]
fn a_request_whose_expect_is_still_current_is_open_until_it_is_admitted() {
    let mut bridge = Bridge::new();
    let e = bridge.state("global").op;
    assert_eq!(asked("x", &e, &bridge, "global"), Outcome::Open);
    let admitted = bridge.write(&Scope::Global, OperationKind::Logout);
    assert_eq!(asked(&admitted, &e, &bridge, "global"), Outcome::Admitted { record: admitted.clone() });
    // A copy under another ID of the same input is superseded: it can never be admitted.
    assert_eq!(asked("y", &e, &bridge, "global"), Outcome::Superseded);
}

#[test]
fn an_operation_replacing_globals_op_supersedes_a_write_whose_answer_was_lost() {
    let mut bridge = Bridge::new();
    let e = bridge.state("global").op;
    // The write `x` to `global` was lost on the way and never admitted; another operation replaced `e`.
    let replacer = bridge.write(&Scope::Global, OperationKind::Logout);
    assert_eq!(asked("authenticate-x", &e, &bridge, "global"), Outcome::Superseded);
    let _ = replacer;
}

#[test]
fn a_session_released_and_held_again_is_unknown_released() {
    let mut bridge = Bridge::new();
    let held = bridge.tree.get("/sessions/s").unwrap().srev();
    let e = bridge.state("sessions/s").op;
    let lifecycle = |bridge: &mut Bridge, state| {
        bridge.registry.sessions.get_mut("s").unwrap().state.live.as_mut().unwrap().lifecycle = state;
        bridge.commit();
    };
    lifecycle(&mut bridge, crate::runtime_state::SessionLifecycle::Closed);
    lifecycle(&mut bridge, crate::runtime_state::SessionLifecycle::Active);
    let request = Request { id: "x".into(), expect: e, instance: "I".into(), at: held };
    assert_eq!(
        outcome(&bridge.state("sessions/s"), &request),
        Outcome::Unknown { reason: Reason::Released }
    );
}

#[test]
fn a_new_instance_is_unknown_restarted_and_an_old_expect_is_expired() {
    let mut bridge = Bridge::new();
    let e = bridge.state("global").op;
    let request = Request { id: "x".into(), expect: e.clone(), instance: "OTHER".into(), at: 1 };
    assert_eq!(outcome(&bridge.state("global"), &request), Outcome::Unknown { reason: Reason::Restarted });
    for _ in 0..70 {
        bridge.write(&Scope::Global, OperationKind::Logout);
        bridge.registry.complete_scope_operation(&Scope::Global, OperationKind::Logout, json!({}));
        bridge.commit();
    }
    assert_eq!(asked("x", &e, &bridge, "global"), Outcome::Unknown { reason: Reason::Expired });
}

#[test]
fn a_target_that_is_not_in_the_tree_has_no_state() {
    let bridge = Bridge::new();
    assert!(state_of(&bridge.tree, "sessions/nope").is_none());
    assert!(state_of(&bridge.tree, "nowhere").is_none());
}

#[test]
fn the_answer_to_a_read_names_the_record_of_an_admitted_request() {
    let mut bridge = Bridge::new();
    let e = bridge.state("global").op;
    let admitted = bridge.write(&Scope::Global, OperationKind::Logout);
    let at = bridge.tree.get("/global").unwrap().srev();
    let request = Request { id: admitted.clone(), expect: e.clone(), instance: "I".into(), at };
    let body = resolve(&bridge.tree, "global", &request).unwrap();
    assert_eq!(body["outcome"], "admitted");
    assert_eq!(body["record"]["id"], admitted.as_str());
    assert_eq!(body["record"]["after"], e.as_str());
    assert_eq!(body["record"]["kind"], "logout");
    let open = Request { id: "other".into(), expect: admitted, instance: "I".into(), at };
    assert_eq!(resolve(&bridge.tree, "global", &open).unwrap(), json!({ "outcome": "open" }));
}

#[test]
fn a_session_the_bridge_does_not_hold_is_released_or_restarted_and_a_missing_target_is_none() {
    let bridge = Bridge::new();
    let request = |instance: &str| Request { id: "x".into(), expect: "e".into(), instance: instance.into(), at: 1 };
    assert_eq!(
        resolve(&bridge.tree, "sessions/gone", &request("I")).unwrap(),
        json!({ "outcome": "unknown", "reason": "released" })
    );
    assert_eq!(
        resolve(&bridge.tree, "sessions/gone", &request("OTHER")).unwrap(),
        json!({ "outcome": "unknown", "reason": "restarted" })
    );
    assert!(resolve(&bridge.tree, "nowhere", &request("I")).is_none());
}

// --- an occupation never gives the session's `op` back ----------------------------------

/// A global `delete` occupies the held session `s`, then fails by `fail`.
fn delete_that_fails(fail: impl FnOnce(&mut SessionRegistry, &str)) -> (Bridge, String, String) {
    let mut bridge = Bridge::new();
    let before = bridge.state("sessions/s").op;
    let delete = bridge
        .registry
        .install_operation(&Scope::Global, OperationKind::Delete, json!({ "sessionId": "s" }))
        .unwrap();
    bridge.commit();
    fail(&mut bridge.registry, &delete);
    bridge.commit();
    (bridge, before, delete)
}

fn failure() -> crate::protocol::documents::ProtocolError {
    crate::protocol::documents::ProtocolError::new(crate::protocol::documents::ErrorCode::Unavailable, "failed")
}

/// The session keeps showing the failed delete; the request that expected the operation the delete
/// replaced is superseded for good, and the log never names an ID twice.
fn assert_never_gives_the_op_back((bridge, before, delete): (Bridge, String, String)) {
    let state = bridge.state("sessions/s");
    assert_eq!(state.op, delete, "the session shows the delete that occupied it");
    let ids: Vec<&str> = state.ops.iter().map(|record| record.id.as_str()).collect();
    let unique: std::collections::BTreeSet<&&str> = ids.iter().collect();
    assert_eq!(ids.len(), unique.len(), "no ID is recorded twice: {ids:?}");
    assert_eq!(asked("x", &before, &bridge, "sessions/s"), Outcome::Superseded);
    assert_eq!(asked("y", &delete, &bridge, "sessions/s"), Outcome::Open);
    assert_eq!(bridge.tree.get("/sessions/s/op").unwrap().as_value().unwrap().0["status"], "failed");
}

#[test]
fn a_delete_failed_by_its_operation_leaves_its_op_in_the_occupied_session() {
    assert_never_gives_the_op_back(delete_that_fails(|registry, delete| {
        registry.fail_protocol_operation(delete, failure());
    }));
}

#[test]
fn a_delete_failed_in_its_scope_leaves_its_op_in_the_occupied_session() {
    assert_never_gives_the_op_back(delete_that_fails(|registry, _| {
        registry.fail_scope_operation(&Scope::Global, OperationKind::Delete, failure());
    }));
}

#[test]
fn a_delete_the_session_operation_failed_leaves_its_op_in_the_occupied_session() {
    assert_never_gives_the_op_back(delete_that_fails(|registry, _| {
        registry.fail_session_operation(
            "s",
            crate::runtime_state::SessionOperationKind::Delete,
            &json!({ "code": -32000, "message": "refused" }),
        );
    }));
}

#[test]
fn a_connection_that_ends_during_a_delete_leaves_its_op_in_the_occupied_session() {
    assert_never_gives_the_op_back(delete_that_fails(|registry, _| {
        registry.end_connection(failure());
    }));
}
