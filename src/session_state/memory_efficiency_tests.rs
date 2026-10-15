//! Allocation gates for the real registry -> canonical turn overlay fold path.
//! Inputs, held views, test-side validation and serialization stay outside
//! measurement; production folding is measured.
//! These gates constrain allocation traffic, not CPU time or process RSS.

use super::{MirrorError, MirrorPhase, TurnOverlay};
use crate::session_registry::SessionRegistry;
use crate::test_allocations::{AllocationStats, measure_allocations};
use serde_json::{Value, json};

const SESSION: &str = "allocation-session";
const STEPS: usize = 8;
const BODY_SIZES: [usize; 3] = [8 * 1024, 256 * 1024, 1024 * 1024];
const ALLOCATION_SLACK: usize = 64 * 1024;
const MESSAGE_PREFIX: &str = "small message:";
const MESSAGE_PARTS: [&str; STEPS] = [" α", "\\", "\"", "\n", "five", "六", "七", " done"];

struct Fixture {
    registry: SessionRegistry,
    operation_id: String,
    held_turn: TurnOverlay,
    held_wire: Value,
}

fn tool(tool_call_id: &str, title: &str, text: &str) -> Value {
    json!({
        "sessionUpdate": "tool_call",
        "toolCallId": tool_call_id,
        "title": title,
        "status": "in_progress",
        "content": [{ "type": "content", "content": { "type": "text", "text": text } }],
        "_meta": { "preserve": "tool metadata" },
    })
}

fn message(text: &str) -> Value {
    json!({
        "sessionUpdate": "agent_message_chunk",
        "messageId": "small-message",
        "content": { "type": "text", "text": text },
        "_meta": { "preserve": "message metadata" },
    })
}

fn fixture(body_size: usize) -> Fixture {
    let mut registry = SessionRegistry::new("allocation-epoch");
    registry.register_new(SESSION);
    let operation_id = registry
        .start_turn(
            SESSION,
            "allocation-intent",
            vec![json!({ "type": "text", "text": "keep the full output" })],
        )
        .unwrap();
    // Include UTF-8 and escaped characters. Entity count is identical at every
    // size.
    let body = format!("start 界\\\"\n{}\nend", "x".repeat(body_size));
    for update in [
        tool("tool-a", "Unchanged large tool", &body),
        tool("tool-b", "Small tool", "original small output"),
        message(MESSAGE_PREFIX),
    ] {
        registry
            .append_turn_update(SESSION, &operation_id, update)
            .unwrap();
    }
    let held_turn = registry
        .state(SESSION)
        .unwrap()
        .active_turn
        .clone()
        .unwrap();
    let held_wire = serde_json::to_value(&held_turn).unwrap();
    assert_eq!(
        held_wire["updates"][0]["content"][0]["content"]["text"].as_str(),
        Some(body.as_str())
    );
    Fixture {
        registry,
        operation_id,
        held_turn,
        held_wire,
    }
}

fn assert_turns(fixture: &Fixture, expected_current: &Value) {
    let state = fixture.registry.state(SESSION).unwrap();
    assert_eq!(state.phase, MirrorPhase::Running);
    let overlay = state.active_turn.as_ref().unwrap();
    assert!(
        serde_json::to_value(overlay).unwrap() == *expected_current,
        "the running turn must preserve all bodies, identities and metadata"
    );
    assert!(
        serde_json::to_value(&fixture.held_turn).unwrap() == fixture.held_wire,
        "a previously read turn must remain complete and immutable"
    );
    assert_eq!(overlay.operation_id, fixture.operation_id);
    assert_eq!(
        overlay.execution.as_ref().unwrap().rpc_operation_id,
        "allocation-intent"
    );
    assert!(overlay.terminal.is_none());
}

fn assert_independent_of_unrelated_body(samples: &[(usize, AllocationStats)]) {
    let baseline = samples[0].1.requested_bytes;
    for &(body_size, stats) in &samples[1..] {
        assert!(
            stats.requested_bytes <= baseline.saturating_add(ALLOCATION_SLACK),
            "eight small updates must not allocate in proportion to unchanged body size: \
             body={body_size}, requested={}, 8KiB baseline={baseline}, slack={ALLOCATION_SLACK}; \
             all samples={samples:?}",
            stats.requested_bytes,
        );
    }
}

#[test]
fn memory_efficiency_other_tool_updates_do_not_copy_unrelated_large_body() {
    let mut samples = Vec::new();
    for body_size in BODY_SIZES {
        let mut fixture = fixture(body_size);
        let updates: Vec<Value> = (0..STEPS)
            .map(|step| {
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "tool-b",
                    "title": format!("Small tool step {step:02}"),
                    "status": if step + 1 == STEPS { "completed" } else { "in_progress" },
                    "content": [{ "type": "content", "content": {
                        "type": "text", "text": format!("small output {step:02}")
                    } }],
                })
            })
            .collect();
        let mut expected = fixture.held_wire.clone();
        let last = updates.last().unwrap();
        for field in ["title", "status", "content"] {
            expected["updates"][1][field] = last[field].clone();
        }

        let (result, stats) = measure_allocations(|| {
            updates.into_iter().try_for_each(|update| {
                fixture
                    .registry
                    .append_turn_update(SESSION, &fixture.operation_id, update)
            })
        });
        result.unwrap();
        assert_turns(&fixture, &expected);
        samples.push((body_size, stats));
    }
    assert_independent_of_unrelated_body(&samples);
}

#[test]
fn memory_efficiency_small_message_appends_do_not_copy_unrelated_large_body() {
    let mut samples = Vec::new();
    for body_size in BODY_SIZES {
        let mut fixture = fixture(body_size);
        let updates: Vec<Value> = MESSAGE_PARTS.into_iter().map(message).collect();
        let mut expected = fixture.held_wire.clone();
        expected["updates"][2]["content"]["text"] =
            json!(format!("{MESSAGE_PREFIX}{}", MESSAGE_PARTS.concat()));

        let (result, stats) = measure_allocations(|| {
            updates.into_iter().try_for_each(|update| {
                fixture
                    .registry
                    .append_turn_update(SESSION, &fixture.operation_id, update)
            })
        });
        result.unwrap();
        assert_turns(&fixture, &expected);
        samples.push((body_size, stats));
    }
    assert_independent_of_unrelated_body(&samples);
}

#[test]
fn memory_efficiency_rejected_folds_preserve_the_running_turn() {
    let mut fixture = fixture(BODY_SIZES[2]);
    let before = fixture.registry.state(SESSION).unwrap().clone();
    let rejections = [
        (
            "wrong-operation".to_string(),
            message("wrong owner"),
            MirrorError::OperationMismatch,
        ),
        (
            fixture.operation_id.clone(),
            json!({ "sessionUpdate": "plan", "entries": 7 }),
            MirrorError::InconsistentHistory,
        ),
        (
            fixture.operation_id.clone(),
            json!({
                "sessionUpdate": "agent_message_chunk",
                "messageId": 7,
                "content": { "type": "text", "text": "invalid message identity" },
            }),
            MirrorError::InconsistentHistory,
        ),
    ];
    for (operation, update, error) in rejections {
        assert_eq!(
            fixture
                .registry
                .append_turn_update(SESSION, &operation, update),
            Err(error)
        );
        assert!(
            fixture.registry.state(SESSION).unwrap() == &before,
            "a rejected fold must not change owner, execution or contents"
        );
        let unchanged = fixture.held_wire.clone();
        assert_turns(&fixture, &unchanged);
    }

    // Rejection must not poison the still-running owner.
    fixture
        .registry
        .append_turn_update(SESSION, &fixture.operation_id, message(" after rejection"))
        .unwrap();
    let mut expected = fixture.held_wire.clone();
    expected["updates"][2]["content"]["text"] = json!(format!("{MESSAGE_PREFIX} after rejection"));
    assert_turns(&fixture, &expected);
}
