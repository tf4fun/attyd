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
