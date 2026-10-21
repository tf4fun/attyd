//! The publisher against a scenario computed by an independent implementation
//! (tests/fixtures/protocol_vectors.py): the document after each step, its
//! digest, the rev at which each entry last changed, and the snapshot a
//! subscriber holding a given version is sent. The ops the engine emits are
//! checked by applying them to the previous document with the Rust patcher.

use serde_json::{Value, json};

use super::*;
use crate::protocol::diff::apply;
use crate::protocol::timeline::PromptEnd;

fn scenario() -> Value {
    serde_json::from_str(include_str!("../../../tests/fixtures/protocol-engine-scenario.json"))
        .expect("the scenario is JSON")
}

/// The document a replica holds: `epoch`, `rev` and the body, entries included.
fn document(event: &StreamEvent) -> Value {
    let StreamEvent::Snapshot { snapshot, .. } = event else {
        panic!("not a snapshot: {event:?}");
    };
    let mut document = snapshot.doc.clone();
    document["epoch"] = json!(snapshot.epoch);
    document["rev"] = json!(snapshot.rev);
    if let Some(entries) = &snapshot.entries {
        document["entries"] = Value::Array(entries.changed.iter().map(|(_, entry)| entry.clone()).collect());
    }
    document
}

fn input(timeline: &Timeline, live: &Value) -> Input {
    Input {
        epoch: "E1".into(),
        history: Some(History::Complete),
        timeline: Some(Arc::new(timeline.clone())),
        live: live.clone(),
    }
}

/// Mutates `timeline` as the bridge does for one step of the scenario.
fn mutate(timeline: &mut Timeline, mutation: &Value) {
    if let Some(prompt) = mutation.get("prompt") {
        timeline.begin_prompt(prompt.as_array().unwrap());
    }
    for update in mutation.get("updates").and_then(Value::as_array).into_iter().flatten() {
        timeline.reduce(update, None);
    }
    if let Some(end) = mutation.get("end") {
        timeline.end_prompt(PromptEnd::Response(end.clone()));
    }
}

#[test]
fn a_session_is_published_as_the_independent_scenario_says() {
    let scenario = scenario();
    let session = &scenario["session"];
    let scope = session["scope"].as_str().unwrap();
    let mut engine = Engine::default();
    let mut timeline = Timeline::default();
    let mut published: Option<Value> = None;

    for (n, step) in session["steps"].as_array().unwrap().iter().enumerate() {
        mutate(&mut timeline, &step["mutation"]);
        let commit = engine.commit(scope, &input(&timeline, &step["live"]));
        match (&step["doc"], &commit) {
            (Value::Null, Commit::Unchanged) => {}
            (expected, Commit::Rebuilt) => {
                assert_eq!(n, 0, "only the first publication is a rebuild");
                published = Some(expected.clone());
            }
            (expected, Commit::Ops(StreamEvent::Ops { rev, ops, .. })) => {
                assert_eq!(*rev, expected["rev"].as_u64().unwrap(), "step {n}");
                let mut replica = published.clone().expect("a document before the ops");
                for op in ops {
                    apply(&mut replica, op).unwrap_or_else(|error| panic!("step {n}: {op:?}: {error:?}"));
                }
                replica["rev"] = json!(rev);
                assert_eq!(&replica, expected, "step {n}: the ops take the document to the expected one");
                published = Some(replica);
            }
            (expected, commit) => panic!("step {n}: expected {expected} but the commit was {commit:?}"),
        }
        let checkpoint = engine.checkpoint(scope).unwrap();
        let StreamEvent::Checkpoint { rev, digest, .. } = checkpoint else {
            panic!("not a checkpoint");
        };
        assert_eq!(digest, step["digest"].as_str().unwrap(), "step {n}: the digest");
        assert_eq!(rev, published.as_ref().unwrap()["rev"].as_u64().unwrap());
    }
}

#[test]
fn a_subscriber_is_sent_the_entries_changed_after_the_version_it_holds() {
    let scenario = scenario();
    let session = &scenario["session"];
    let scope = session["scope"].as_str().unwrap();
    let mut engine = Engine::default();
    let mut timeline = Timeline::default();
    for step in session["steps"].as_array().unwrap() {
        mutate(&mut timeline, &step["mutation"]);
        engine.commit(scope, &input(&timeline, &step["live"]));
    }

    for expected in session["snapshots"].as_array().unwrap() {
        let have = expected["have"].as_object().map(|have| Have {
            epoch: have["epoch"].as_str().unwrap().to_string(),
            rev: have["rev"].as_u64().unwrap(),
        });
        let event = engine.snapshot(scope, have.as_ref()).unwrap();
        let StreamEvent::Snapshot { snapshot, .. } = &event else {
            panic!("not a snapshot");
        };
        let entries = snapshot.entries.as_ref().unwrap();
        let what = format!("have {have:?}");
        assert_eq!(
            serde_json::to_value(entries.base).unwrap(),
            expected["base"],
            "{what}"
        );
        assert_eq!(entries.length as u64, expected["length"].as_u64().unwrap(), "{what}");
        let indices: Vec<u64> = entries.changed.iter().map(|(index, _)| *index as u64).collect();
        assert_eq!(
            Value::from(indices),
            expected["changed"],
            "{what}: the entries listed"
        );
        // Whatever the subscriber holds, the digest is that of the whole document.
        assert_eq!(snapshot.digest, session["finalDigest"].as_str().unwrap(), "{what}");
        assert_eq!(snapshot.rev, session["final"]["rev"].as_u64().unwrap(), "{what}");
    }

    // A full snapshot is the document.
    let full = document(&engine.snapshot(scope, None).unwrap());
    assert_eq!(full, session["final"]);
}

#[test]
fn a_new_epoch_rebuilds_the_document_at_rev_zero() {
    let scenario = scenario();
    let session = &scenario["session"];
    let scope = session["scope"].as_str().unwrap();
    let mut engine = Engine::default();
    let mut timeline = Timeline::default();
    for step in session["steps"].as_array().unwrap() {
        mutate(&mut timeline, &step["mutation"]);
        engine.commit(scope, &input(&timeline, &step["live"]));
    }
    assert_eq!(engine.version(scope), Some(("E1", 5)));

    // A load replaced the history.
    let replayed = Timeline::from_replay(
        [json!({ "sessionUpdate": "agent_message_chunk", "messageId": "r", "content": { "type": "text", "text": "back" } })]
            .iter()
            .map(|update| (update, None)),
    );
    let reloaded = Input {
        epoch: "E2".into(),
        history: Some(History::Partial),
        timeline: Some(Arc::new(replayed)),
        live: json!({ "sessionId": "S" }),
    };
    assert_eq!(engine.commit(scope, &reloaded), Commit::Rebuilt);
    assert_eq!(engine.version(scope), Some(("E2", 0)));

    let held = Have { epoch: "E1".into(), rev: 5 };
    let StreamEvent::Snapshot { snapshot, .. } = engine.snapshot(scope, Some(&held)).unwrap() else {
        panic!("not a snapshot");
    };
    assert_eq!(snapshot.entries.as_ref().unwrap().base, EntriesBase::Replace, "another epoch holds nothing of this one");
    assert_eq!(snapshot.doc["history"], "partial");
}

#[test]
fn an_unknown_history_has_no_entries_and_becoming_known_is_a_new_epoch() {
    let mut engine = Engine::default();
    let scope = "session/S";
    let unknown = Input {
        epoch: "E1".into(),
        history: None,
        timeline: None,
        live: json!({ "sessionId": "S" }),
    };
    assert_eq!(engine.commit(scope, &unknown), Commit::Rebuilt);
    let StreamEvent::Snapshot { snapshot, .. } = engine.snapshot(scope, None).unwrap() else {
        panic!("not a snapshot");
    };
    assert!(snapshot.entries.is_none());
    assert_eq!(snapshot.doc, json!({ "prompts": [], "live": { "sessionId": "S" } }));

    let known = Input {
        epoch: "E1".into(),
        history: Some(History::Complete),
        timeline: Some(Arc::new(Timeline::default())),
        live: json!({ "sessionId": "S" }),
    };
    assert_eq!(engine.commit(scope, &known), Commit::Rebuilt, "entries do not appear within an epoch");
}

#[test]
fn an_entry_the_memory_replaced_without_changing_it_is_not_published_again() {
    let mut engine = Engine::default();
    let scope = "session/S";
    let mut timeline = Timeline::default();
    timeline.begin_prompt(&[json!({ "type": "text", "text": "hi" })]);
    let live = json!({ "sessionId": "S" });
    assert_eq!(engine.commit(scope, &input(&timeline, &live)), Commit::Rebuilt);

    // The same content in new allocations: an equal timeline cloned and rebuilt.
    let mut again = Timeline::default();
    again.begin_prompt(&[json!({ "type": "text", "text": "hi" })]);
    assert_eq!(engine.commit(scope, &input(&again, &live)), Commit::Unchanged);
    assert_eq!(engine.version(scope), Some(("E1", 0)));
}

#[test]
fn a_scope_without_entries_publishes_its_live_document_alone() {
    let scenario = scenario();
    let global = &scenario["global"];
    let scope = global["scope"].as_str().unwrap();
    let mut engine = Engine::default();
    let mut published: Option<Value> = None;
    for (n, step) in global["steps"].as_array().unwrap().iter().enumerate() {
        let commit = engine.commit(
            scope,
            &Input {
                epoch: global["epoch"].as_str().unwrap().into(),
                history: None,
                timeline: None,
                live: step["live"].clone(),
            },
        );
        match (&step["doc"], &commit) {
            (Value::Null, Commit::Unchanged) => {}
            (expected, Commit::Rebuilt) => published = Some(expected.clone()),
            (expected, Commit::Ops(StreamEvent::Ops { rev, ops, .. })) => {
                let mut replica = published.clone().unwrap();
                for op in ops {
                    apply(&mut replica, op).unwrap_or_else(|error| panic!("step {n}: {error:?}"));
                }
                replica["rev"] = json!(rev);
                assert_eq!(&replica, expected, "step {n}");
                published = Some(replica);
            }
            (expected, commit) => panic!("step {n}: expected {expected} but got {commit:?}"),
        }
        let StreamEvent::Checkpoint { digest, .. } = engine.checkpoint(scope).unwrap() else {
            panic!("not a checkpoint");
        };
        assert_eq!(digest, step["digest"].as_str().unwrap(), "step {n}");
    }
    let StreamEvent::Snapshot { snapshot, .. } = engine.snapshot(scope, None).unwrap() else {
        panic!("not a snapshot");
    };
    assert!(snapshot.entries.is_none(), "a scope without a timeline has no entries");
    assert_eq!(snapshot.doc.as_object().unwrap().keys().collect::<Vec<_>>(), ["live"]);
}

#[test]
fn a_terminals_output_rolls_as_a_bounded_tail() {
    let mut engine = Engine::default();
    let scope = "global";
    let live = |output: &str| json!({ "terminals": { "t": { "terminalId": "t", "output": output } } });
    let input = |output: &str| Input {
        epoch: "G".into(),
        history: None,
        timeline: None,
        live: live(output),
    };
    engine.commit(scope, &input("abcd"));
    let Commit::Ops(StreamEvent::Ops { ops, .. }) = engine.commit(scope, &input("cdefgh")) else {
        panic!("not ops");
    };
    let kinds: Vec<&str> = ops
        .iter()
        .map(|op| match op {
            Op::Trim { .. } => "trim",
            Op::Append { .. } => "append",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["trim", "append"]);
}

#[test]
fn an_attachment_is_served_while_a_published_entry_references_it() {
    let mut engine = Engine::default();
    let scope = "session/S";
    let image = json!({ "type": "image", "data": "AAAA", "mimeType": "image/png" });
    let timeline = Timeline::from_replay(
        [json!({ "sessionUpdate": "agent_message_chunk", "content": image })]
            .iter()
            .map(|update| (update, None)),
    );
    engine.commit(scope, &input(&timeline, &json!({})));
    let id = "caf76d0a16892b770bf06da2da02838e5294dd0d4be88c85ff2efb98a0591f79";
    assert_eq!(engine.attachment(scope, id), Some(&image));
    assert_eq!(engine.attachment(scope, "unknown"), None);
    assert_eq!(engine.attachment("session/other", id), None);
}
