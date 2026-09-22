//! Allocation gates for the real registry -> canonical turn overlay fold path.
//! Inputs, held views, test-side validation and serialization stay outside
//! measurement; production folding and byte accounting are both measured.
//! These gates constrain allocation traffic, not CPU time or process RSS.

use super::{MirrorError, MirrorPhase, TurnAdmission};
use crate::session_mirror::SessionView;
use crate::session_registry::SessionRegistry;
use crate::test_allocations::{AllocationStats, measure_allocations};
use serde_json::{Value, json};

const SESSION: &str = "allocation-session";
const INCARNATION: u64 = 1;
const STEPS: usize = 8;
const BODY_SIZES: [usize; 3] = [8 * 1024, 256 * 1024, 1024 * 1024];
const ALLOCATION_SLACK: usize = 64 * 1024;
const MESSAGE_PREFIX: &str = "small message:";
const MESSAGE_PARTS: [&str; STEPS] = [" α", "\\", "\"", "\n", "five", "六", "七", " done"];

struct Fixture {
    registry: SessionRegistry,
    operation_id: String,
    held_view: SessionView,
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
    registry.register_new(SESSION, INCARNATION);
    let revision = registry
        .state(SESSION)
        .unwrap()
        .history_revision
        .clone()
        .unwrap();
    let TurnAdmission::Accepted { operation_id } = registry
        .start_turn(
            SESSION,
            INCARNATION,
            &revision,
            "allocation-intent",
            vec![json!({ "type": "text", "text": "keep the full output" })],
        )
        .unwrap()
    else {
        panic!("a fresh turn must be admitted");
    };
    // Include UTF-8 and escaped characters so accounting cannot assume text byte
    // length equals serialized length. Entity count is identical at every size.
    let body = format!("start 界\\\"\n{}\nend", "x".repeat(body_size));
    for update in [
        tool("tool-a", "Unchanged large tool", &body),
        tool("tool-b", "Small tool", "original small output"),
        message(MESSAGE_PREFIX),
    ] {
        registry
            .append_turn_update(SESSION, INCARNATION, &operation_id, update)
            .unwrap();
    }
    let held_view = registry.view(SESSION, INCARNATION).unwrap();
    let held_wire = serde_json::to_value(&held_view.session).unwrap();
    assert_eq!(
        held_wire["activeTurn"]["updates"][0]["content"][0]["content"]["text"].as_str(),
        Some(body.as_str())
    );
    Fixture {
        registry,
        operation_id,
        held_view,
        held_wire,
    }
}

fn assert_views_and_accounting(fixture: &mut Fixture, expected_current: &Value) {
    let current = fixture.registry.view(SESSION, INCARNATION).unwrap();
    assert!(
        serde_json::to_value(&current.session).unwrap() == *expected_current,
        "latest view must preserve all bodies, identities, metadata and revisions"
    );
    assert!(
        serde_json::to_value(&fixture.held_view.session).unwrap() == fixture.held_wire,
        "a previously published view must remain complete and immutable"
    );
    assert_eq!(current.session.phase, MirrorPhase::Running);
    assert_eq!(
        current.baseline.revision(),
        fixture.held_view.baseline.revision()
    );
    assert_eq!(
        current.baseline.updates(),
        fixture.held_view.baseline.updates()
    );

    let state = fixture.registry.state(SESSION).unwrap();
    let overlay = state.active_turn.as_ref().unwrap();
    assert_eq!(overlay.operation_id, fixture.operation_id);
    assert_eq!(
        overlay.execution.as_ref().unwrap().rpc_operation_id,
        "allocation-intent"
    );
    assert!(overlay.terminal.is_none());
    let actual_bytes = serde_json::to_vec(overlay).unwrap().len();
    assert_eq!(state.active_overlay_bytes, actual_bytes);
    assert_eq!(fixture.registry.overlay_bytes, actual_bytes);
}

fn expected_after(fixture: &Fixture, update_count: usize) -> Value {
    let mut expected = fixture.held_wire.clone();
    expected["viewRevision"] = json!(fixture.held_view.session.view_revision + update_count as u64);
    expected
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
        let mut expected = expected_after(&fixture, STEPS);
        let last = updates.last().unwrap();
        for field in ["title", "status", "content"] {
            expected["activeTurn"]["updates"][1][field] = last[field].clone();
        }

        let (result, stats) = measure_allocations(|| {
            updates.into_iter().try_for_each(|update| {
                fixture.registry.append_turn_update(
                    SESSION,
                    INCARNATION,
                    &fixture.operation_id,
                    update,
                )
            })
        });
        result.unwrap();
        assert_views_and_accounting(&mut fixture, &expected);
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
        let mut expected = expected_after(&fixture, STEPS);
        expected["activeTurn"]["updates"][2]["content"]["text"] =
            json!(format!("{MESSAGE_PREFIX}{}", MESSAGE_PARTS.concat()));

        let (result, stats) = measure_allocations(|| {
            updates.into_iter().try_for_each(|update| {
                fixture.registry.append_turn_update(
                    SESSION,
                    INCARNATION,
                    &fixture.operation_id,
                    update,
                )
            })
        });
        result.unwrap();
        assert_views_and_accounting(&mut fixture, &expected);
        samples.push((body_size, stats));
    }
    assert_independent_of_unrelated_body(&samples);
}

#[test]
fn memory_efficiency_rejected_folds_preserve_snapshot_owner_revision_and_accounting() {
    let mut fixture = fixture(BODY_SIZES[2]);
    let before = fixture.registry.state(SESSION).unwrap().clone();
    let before_overlay_bytes = fixture.registry.overlay_bytes;
    let rejections = [
        (
            INCARNATION + 1,
            fixture.operation_id.clone(),
            message("stale incarnation"),
            MirrorError::StaleIncarnation,
        ),
        (
            INCARNATION,
            "wrong-operation".to_string(),
            message("wrong owner"),
            MirrorError::OperationMismatch,
        ),
        (
            INCARNATION,
            fixture.operation_id.clone(),
            json!({ "sessionUpdate": "plan", "entries": 7 }),
            MirrorError::InconsistentHistory,
        ),
        (
            INCARNATION,
            fixture.operation_id.clone(),
            json!({
                "sessionUpdate": "agent_message_chunk",
                "messageId": 7,
                "content": { "type": "text", "text": "invalid message identity" },
            }),
            MirrorError::InconsistentHistory,
        ),
    ];
    for (incarnation, operation, update, error) in rejections {
        assert_eq!(
            fixture
                .registry
                .append_turn_update(SESSION, incarnation, &operation, update),
            Err(error)
        );
        assert!(
            fixture.registry.state(SESSION).unwrap() == &before,
            "a rejected fold must not change owner, execution, contents or revision"
        );
        assert_eq!(fixture.registry.overlay_bytes, before_overlay_bytes);
        let unchanged = fixture.held_wire.clone();
        assert_views_and_accounting(&mut fixture, &unchanged);
    }

    // Rejection must not poison the still-running owner or consume a revision.
    fixture
        .registry
        .append_turn_update(
            SESSION,
            INCARNATION,
            &fixture.operation_id,
            message(" after rejection"),
        )
        .unwrap();
    let mut expected = expected_after(&fixture, 1);
    expected["activeTurn"]["updates"][2]["content"]["text"] =
        json!(format!("{MESSAGE_PREFIX} after rejection"));
    assert_views_and_accounting(&mut fixture, &expected);
}
