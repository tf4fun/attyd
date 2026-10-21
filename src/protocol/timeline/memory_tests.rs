//! Allocation gates for the timeline's copy-on-write sharing (docs/bridge-memory-model.md
//! §1, §2): a held view of the timeline must not make an update allocate in
//! proportion to content the update does not touch. Inputs, the held view and test
//! side validation stay outside measurement; the production reducer is measured.
//! These gates constrain allocation traffic, not CPU time or process RSS.

use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::test_allocations::{AllocationStats, measure_allocations};

const STEPS: usize = 8;
const BODY_SIZES: [usize; 3] = [8 * 1024, 256 * 1024, 1024 * 1024];
const ALLOCATION_SLACK: usize = 64 * 1024;
const MESSAGE_PREFIX: &str = "small message:";
const MESSAGE_PARTS: [&str; STEPS] = [" α", "\\", "\"", "\n", "five", "六", "七", " done"];

fn tool(tool_call_id: &str, title: &str, text: &str) -> Value {
    json!({
        "sessionUpdate": "tool_call",
        "toolCallId": tool_call_id,
        "title": title,
        "content": [{ "type": "content", "content": { "type": "text", "text": text } }],
    })
}

fn message(text: &str) -> Value {
    json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text } })
}

/// A prompt, a large tool, a small tool and a message: entries 0 to 3.
fn fixture(body_size: usize) -> Timeline {
    // UTF-8 and escaped characters; the entry count is the same at every size.
    let body = format!("start 界\\\"\n{}\nend", "x".repeat(body_size));
    let mut timeline = Timeline::default();
    timeline.begin_prompt(&[json!({ "type": "text", "text": "keep the full output" })]);
    for update in [
        tool("tool-a", "Unchanged large tool", &body),
        tool("tool-b", "Small tool", "original small output"),
        message(MESSAGE_PREFIX),
    ] {
        timeline.reduce(&update, None);
    }
    timeline
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

/// The held view still shows what it showed, and the large entry is the same
/// allocation in both: it was shared, not copied.
fn assert_unrelated_entry_shared(timeline: &Timeline, held: &Timeline) {
    assert!(
        Arc::ptr_eq(&timeline.entries()[1], &held.entries()[1]),
        "the unchanged large tool entry must stay shared with the held view"
    );
    assert_eq!(held.entries().len(), 4);
    assert_eq!(
        serde_json::to_value(&held.entries()[2]).unwrap()["call"]["title"],
        json!("Small tool")
    );
    assert_eq!(
        serde_json::to_value(&held.entries()[3]).unwrap()["chunks"][0]["content"]["text"],
        json!(MESSAGE_PREFIX)
    );
}

#[test]
fn memory_efficiency_other_tool_updates_do_not_copy_unrelated_large_body() {
    let mut samples = Vec::new();
    for body_size in BODY_SIZES {
        let mut timeline = fixture(body_size);
        let held = timeline.clone();
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

        let ((), stats) = measure_allocations(|| {
            for update in &updates {
                timeline.reduce(update, None);
            }
        });

        let call = serde_json::to_value(&timeline.entries()[2]).unwrap()["call"].clone();
        assert_eq!(call["title"], json!("Small tool step 07"));
        assert_eq!(call["status"], json!("completed"));
        assert_eq!(
            call["content"],
            json!([{ "type": "content", "content": { "type": "text", "text": "small output 07" } }])
        );
        assert_unrelated_entry_shared(&timeline, &held);
        samples.push((body_size, stats));
    }
    assert_independent_of_unrelated_body(&samples);
}

#[test]
fn memory_efficiency_small_message_appends_do_not_copy_unrelated_large_body() {
    let mut samples = Vec::new();
    for body_size in BODY_SIZES {
        let mut timeline = fixture(body_size);
        let held = timeline.clone();
        let updates: Vec<Value> = MESSAGE_PARTS.into_iter().map(message).collect();

        let ((), stats) = measure_allocations(|| {
            for update in &updates {
                timeline.reduce(update, None);
            }
        });

        assert_eq!(
            serde_json::to_value(&timeline.entries()[3]).unwrap()["chunks"][0]["content"]["text"],
            json!(format!("{MESSAGE_PREFIX}{}", MESSAGE_PARTS.concat()))
        );
        assert_unrelated_entry_shared(&timeline, &held);
        samples.push((body_size, stats));
    }
    assert_independent_of_unrelated_body(&samples);
}

#[test]
fn a_rejected_update_leaves_the_timeline_and_its_sharing_untouched() {
    let mut timeline = fixture(8 * 1024);
    let held = timeline.clone();
    // No `toolCallId`, no `type` on the block: neither can be reduced.
    timeline.reduce(&json!({ "sessionUpdate": "tool_call_update", "title": "x" }), None);
    timeline.reduce(
        &json!({ "sessionUpdate": "agent_message_chunk", "content": { "text": "no type" } }),
        None,
    );
    assert_eq!(timeline.entries().len(), 4);
    for index in 0..4 {
        assert!(Arc::ptr_eq(&timeline.entries()[index], &held.entries()[index]));
    }
}
