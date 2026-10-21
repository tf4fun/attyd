//! Direct cases for the memory model's timeline (docs/bridge-memory-model.md §3–§5,
//! §10). Updates are written as the Agent sends them and expected entries are
//! written by hand from them, never computed by the reducer under test.

use serde_json::{Value, json};

use super::*;

fn text(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn chunk_update(kind: &str, text_: &str, message_id: Option<&str>) -> Value {
    let mut update = json!({ "sessionUpdate": kind, "content": text(text_) });
    if let Some(id) = message_id {
        update["messageId"] = json!(id);
    }
    update
}

fn user(text: &str, id: Option<&str>) -> Value {
    chunk_update("user_message_chunk", text, id)
}

fn agent(text: &str, id: Option<&str>) -> Value {
    chunk_update("agent_message_chunk", text, id)
}

fn thought(text: &str, id: Option<&str>) -> Value {
    chunk_update("agent_thought_chunk", text, id)
}

fn reduce_all(timeline: &mut Timeline, updates: &[Value]) {
    for update in updates {
        timeline.reduce(update, None);
    }
}

fn replay(updates: &[Value]) -> Timeline {
    let mut timeline = Timeline::default();
    reduce_all(&mut timeline, updates);
    timeline
}

fn entries(timeline: &Timeline) -> Value {
    serde_json::to_value(timeline.entries()).expect("entries serialize")
}

fn message(role: &str, id: Option<&str>, chunks: Value) -> Value {
    let mut entry = json!({ "kind": "message", "role": role, "chunks": chunks });
    if let Some(id) = id {
        entry["messageId"] = json!(id);
    }
    entry
}

fn chunk(text_: &str) -> Value {
    json!({ "content": text(text_) })
}

// --- Messages (rule 1, rule 3) ----------------------------------------------------

#[test]
fn chunks_of_one_message_join_and_a_changed_id_starts_a_message() {
    let timeline = replay(&[
        agent("Hel", Some("m1")),
        agent("lo", Some("m1")),
        agent("Bye", Some("m2")),
    ]);
    assert_eq!(
        entries(&timeline),
        json!([
            message("agent", Some("m1"), json!([chunk("Hello")])),
            message("agent", Some("m2"), json!([chunk("Bye")])),
        ])
    );
}

#[test]
fn a_chunk_without_an_id_continues_the_run_and_the_entry_takes_the_first_id() {
    let timeline = replay(&[agent("a", None), agent("b", Some("m1")), agent("c", None)]);
    assert_eq!(
        entries(&timeline),
        json!([message("agent", Some("m1"), json!([chunk("abc")]))])
    );
}

#[test]
fn a_message_resumed_after_a_tool_is_a_new_entry_with_the_same_id() {
    let timeline = replay(&[
        agent("before", Some("m1")),
        json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Run" }),
        agent("after", Some("m1")),
    ]);
    let entries = entries(&timeline);
    assert_eq!(entries.as_array().unwrap().len(), 3);
    assert_eq!(entries[0], message("agent", Some("m1"), json!([chunk("before")])));
    assert_eq!(entries[2], message("agent", Some("m1"), json!([chunk("after")])));
}

#[test]
fn roles_do_not_join() {
    let timeline = replay(&[agent("a", None), thought("t", None), agent("b", None)]);
    assert_eq!(timeline.entries().len(), 3);
}

#[test]
fn text_joins_only_when_every_other_field_is_equal() {
    let mut first = agent("a", Some("m"));
    first["_meta"] = json!({ "n": 1 });
    let mut second = agent("b", Some("m"));
    second["_meta"] = json!({ "n": 2 });
    let mut third = agent("c", Some("m"));
    third["_meta"] = json!({ "n": 2 });
    let timeline = replay(&[first, second, third]);
    assert_eq!(
        entries(&timeline),
        json!([message(
            "agent",
            Some("m"),
            json!([
                { "content": text("a"), "_meta": { "n": 1 } },
                { "content": text("bc"), "_meta": { "n": 2 } },
            ])
        )])
    );
}

#[test]
fn the_notification_envelope_is_kept_on_its_chunk_and_splits_unequal_text() {
    let mut timeline = Timeline::default();
    timeline.reduce(&agent("a", Some("m")), Some(&json!({ "n": 1 })));
    timeline.reduce(&agent("b", Some("m")), Some(&json!({ "n": 1 })));
    timeline.reduce(&agent("c", Some("m")), Some(&json!({ "n": 2 })));
    assert_eq!(
        entries(&timeline),
        json!([message(
            "agent",
            Some("m"),
            json!([
                { "content": text("ab"), "notificationMeta": { "n": 1 } },
                { "content": text("c"), "notificationMeta": { "n": 2 } },
            ])
        )])
    );
}

#[test]
fn blocks_are_kept_as_received_and_malformed_content_is_dropped() {
    let image = json!({ "type": "image", "data": "AAAA", "mimeType": "image/png", "_meta": { "k": 1 } });
    let timeline = replay(&[
        json!({ "sessionUpdate": "agent_message_chunk", "content": image }),
        json!({ "sessionUpdate": "agent_message_chunk", "content": "not a block" }),
        json!({ "sessionUpdate": "agent_message_chunk" }),
    ]);
    assert_eq!(
        entries(&timeline),
        json!([message("agent", None, json!([{ "content": image }]))])
    );
}

// --- Tools (rule 1, rule 2) -------------------------------------------------------

#[test]
fn a_sparse_first_update_keeps_exactly_its_fields() {
    let timeline = replay(&[json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t" })]);
    assert_eq!(
        entries(&timeline),
        json!([{ "kind": "tool", "call": { "toolCallId": "t" } }])
    );
}

#[test]
fn a_tool_call_announces_the_schema_defaults_and_updates_patch_by_field() {
    let timeline = replay(&[
        json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "Run" }),
        json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t", "status": "in_progress",
                "rawInput": { "cmd": "ls" } }),
        json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t", "title": null,
                "content": [{ "type": "content", "content": text("out") }] }),
    ]);
    assert_eq!(
        entries(&timeline),
        json!([{ "kind": "tool", "call": {
            "toolCallId": "t", "title": "Run", "kind": "other", "status": "in_progress",
            "rawInput": { "cmd": "ls" }, "locations": [],
            "content": [{ "type": "content", "content": text("out") }],
        } }])
    );
}

#[test]
fn an_empty_collection_clears_and_null_keeps() {
    let timeline = replay(&[
        json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "Run",
                "locations": [{ "path": "/a" }] }),
        json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t", "locations": [] }),
        json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t", "status": null }),
    ]);
    let call = &entries(&timeline)[0]["call"];
    assert_eq!(call["locations"], json!([]));
    assert_eq!(call["status"], json!("pending"));
}

#[test]
fn a_permission_tool_call_is_reduced_like_an_update_without_a_notification() {
    let mut timeline = Timeline::default();
    timeline.reduce_permission_tool_call(&json!({ "toolCallId": "t" }));
    timeline.reduce(
        &json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "Edit" }),
        None,
    );
    assert_eq!(timeline.entries().len(), 1);
    assert_eq!(entries(&timeline)[0]["call"]["title"], json!("Edit"));
}

#[test]
fn a_tool_update_after_the_prompt_ended_changes_its_entry_in_place() {
    let mut timeline = Timeline::default();
    timeline.begin_prompt(&[text("go")]);
    timeline.reduce(
        &json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "Run" }),
        None,
    );
    timeline.end_prompt(PromptEnd::Response(json!({ "stopReason": "end_turn" })));
    timeline.begin_prompt(&[text("next")]);
    timeline.reduce(
        &json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t", "status": "completed" }),
        None,
    );
    assert_eq!(timeline.entries().len(), 3);
    assert_eq!(entries(&timeline)[1]["call"]["status"], json!("completed"));
}

#[test]
fn the_tool_envelope_follows_the_patch_rule() {
    let mut timeline = Timeline::default();
    let call = json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "x" });
    let update = json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t" });
    timeline.reduce(&call, Some(&json!({ "v": 1 })));
    timeline.reduce(&update, None);
    assert_eq!(entries(&timeline)[0]["notificationMeta"], json!({ "v": 1 }));
    timeline.reduce(&update, Some(&json!({ "v": 2 })));
    assert_eq!(entries(&timeline)[0]["notificationMeta"], json!({ "v": 2 }));
}

// --- Plans (§3.2, §3.4) -----------------------------------------------------------

fn legacy_plan(step: &str) -> Value {
    json!({ "sessionUpdate": "plan", "entries": [{ "content": step, "priority": "high", "status": "pending" }] })
}

fn plan_state(step: &str) -> Value {
    json!({ "entries": [{ "content": step, "priority": "high", "status": "pending" }] })
}

#[test]
fn a_legacy_plan_is_replaced_in_its_turn_and_snapshotted_per_turn() {
    let mut timeline = Timeline::default();
    timeline.begin_prompt(&[text("one")]);
    timeline.reduce(&legacy_plan("a"), None);
    timeline.reduce(&agent("work", None), None);
    timeline.reduce(&legacy_plan("b"), None);
    timeline.begin_prompt(&[text("two")]);
    timeline.reduce(&legacy_plan("c"), None);
    let entries = entries(&timeline);
    assert_eq!(entries.as_array().unwrap().len(), 5);
    assert_eq!(entries[1], json!({ "kind": "plan", "legacy": plan_state("b") }));
    assert_eq!(entries[4], json!({ "kind": "plan", "legacy": plan_state("c") }));
}

#[test]
fn an_id_plan_is_upserted_removed_and_revived_in_one_entry() {
    let update = |step: &str| json!({ "sessionUpdate": "plan_update", "plan": { "planId": "p", "entries": [step] } });
    let removed = json!({ "sessionUpdate": "plan_removed", "planId": "p" });
    let mut timeline = replay(&[update("a"), agent("x", None), update("b")]);
    assert_eq!(timeline.entries().len(), 2);
    assert_eq!(
        entries(&timeline)[0],
        json!({ "kind": "plan", "update": { "plan": { "planId": "p", "entries": ["b"] } } })
    );
    timeline.reduce(&removed, None);
    assert_eq!(
        entries(&timeline)[0],
        json!({ "kind": "plan", "update": { "planId": "p" } })
    );
    timeline.reduce(&update("c"), None);
    assert_eq!(timeline.entries().len(), 2);
    assert_eq!(entries(&timeline)[0]["update"]["plan"]["entries"], json!(["c"]));
}

#[test]
fn removing_an_unknown_plan_starts_a_removal_entry() {
    let timeline = replay(&[json!({ "sessionUpdate": "plan_removed", "planId": "gone" })]);
    assert_eq!(
        entries(&timeline),
        json!([{ "kind": "plan", "update": { "planId": "gone" } }])
    );
}

// --- Compactions (§3.2) -----------------------------------------------------------

#[test]
fn a_compaction_is_upserted_and_its_summary_chunks_append() {
    let timeline = replay(&[
        json!({ "sessionUpdate": "compaction_update", "compactionId": "c", "status": "in_progress",
                "_meta": { "a": 1 } }),
        json!({ "sessionUpdate": "compaction_summary_chunk", "compactionId": "c",
                "content": text("Sum"), "_meta": { "s": 1 } }),
        json!({ "sessionUpdate": "compaction_summary_chunk", "compactionId": "c",
                "content": text("mary"), "_meta": { "s": 1 } }),
        agent("later", None),
        json!({ "sessionUpdate": "compaction_update", "compactionId": "c", "status": "completed" }),
    ]);
    assert_eq!(timeline.entries().len(), 2);
    assert_eq!(
        entries(&timeline)[0],
        json!({ "kind": "compaction", "compactionId": "c", "status": "completed",
                "summary": [{ "content": text("Summary"), "_meta": { "s": 1 } }],
                "_meta": { "a": 1 } })
    );
}

#[test]
fn compaction_summary_error_and_meta_are_three_state() {
    let mut timeline = replay(&[json!({
        "sessionUpdate": "compaction_update", "compactionId": "c", "status": "failed",
        "summary": [text("s")], "error": "boom", "_meta": { "k": 1 },
    })]);
    timeline.reduce(
        &json!({ "sessionUpdate": "compaction_update", "compactionId": "c", "status": "failed" }),
        None,
    );
    let kept = entries(&timeline)[0].clone();
    assert_eq!(kept["summary"], json!([chunk("s")]));
    assert_eq!(kept["error"], json!("boom"));
    assert_eq!(kept["_meta"], json!({ "k": 1 }));
    timeline.reduce(
        &json!({ "sessionUpdate": "compaction_update", "compactionId": "c",
                "summary": null, "error": null, "_meta": null }),
        None,
    );
    let cleared = entries(&timeline)[0].clone();
    assert_eq!(cleared["summary"], json!([]));
    assert!(cleared.get("error").is_none() && cleared.get("_meta").is_none());
}

#[test]
fn a_summary_chunk_for_an_unknown_compaction_is_dropped() {
    let timeline = replay(&[json!({ "sessionUpdate": "compaction_summary_chunk",
        "compactionId": "nope", "content": text("x") })]);
    assert!(timeline.entries().is_empty());
}

#[test]
fn control_updates_are_not_entries() {
    let timeline = replay(&[
        json!({ "sessionUpdate": "current_mode_update", "currentModeId": "ask" }),
        json!({ "sessionUpdate": "usage_update", "used": 1, "size": 2 }),
        json!({ "sessionUpdate": "session_info_update", "title": "t" }),
        json!({ "sessionUpdate": "available_commands_update", "availableCommands": [] }),
        json!({ "sessionUpdate": "config_option_update", "configOptions": [] }),
    ]);
    assert!(timeline.entries().is_empty());
}

// --- The sent prompt and its echo (§4.1) ------------------------------------------

#[test]
fn a_sent_prompt_is_a_user_entry_without_an_id_and_joins_its_text_like_a_replay() {
    let mut timeline = Timeline::default();
    timeline.begin_prompt(&[text("A"), text("B")]);
    assert_eq!(
        entries(&timeline),
        json!([message("user", None, json!([chunk("AB")]))])
    );
}

#[test]
fn an_exact_echo_adds_nothing_and_adopts_the_id_and_metadata() {
    let mut timeline = Timeline::default();
    timeline.begin_prompt(&[text("hello")]);
    let mut echo = user("hello", Some("u1"));
    echo["_meta"] = json!({ "c": 1 });
    timeline.reduce(&echo, Some(&json!({ "n": 1 })));
    assert_eq!(
        entries(&timeline),
        json!([message(
            "user",
            Some("u1"),
            json!([{ "content": text("hello"), "_meta": { "c": 1 }, "notificationMeta": { "n": 1 } }])
        )])
    );
}

#[test]
fn an_echo_with_metadata_that_differs_between_blocks_splits_the_joined_text() {
    let mut timeline = Timeline::default();
    timeline.begin_prompt(&[text("A"), text("B")]);
    let mut first = user("A", Some("u"));
    first["_meta"] = json!({ "i": 1 });
    let mut second = user("B", Some("u"));
    second["_meta"] = json!({ "i": 2 });
    timeline.reduce(&first, None);
    timeline.reduce(&second, None);
    assert_eq!(
        entries(&timeline),
        json!([message(
            "user",
            Some("u"),
            json!([
                { "content": text("A"), "_meta": { "i": 1 } },
                { "content": text("B"), "_meta": { "i": 2 } },
            ])
        )])
    );
}

#[test]
fn a_second_echo_with_another_id_is_not_an_echo() {
    let mut timeline = Timeline::default();
    timeline.begin_prompt(&[text("A"), text("B")]);
    timeline.reduce(&user("A", Some("u1")), None);
    timeline.reduce(&user("B", Some("u2")), None);
    assert_eq!(
        entries(&timeline),
        json!([
            // The sent entry still holds the whole prompt; only block A was echoed.
            message("user", Some("u1"), json!([chunk("AB")])),
            message("user", Some("u2"), json!([chunk("B")])),
        ])
    );
}

#[test]
fn an_echo_split_differently_from_the_sent_blocks_duplicates_never_concatenates() {
    let mut timeline = Timeline::default();
    timeline.begin_prompt(&[text("hello")]);
    timeline.reduce(&user("hel", None), None);
    timeline.reduce(&user("lo", None), None);
    assert_eq!(
        entries(&timeline),
        json!([
            message("user", None, json!([chunk("hello")])),
            message("user", None, json!([chunk("hello")])),
        ])
    );
}

#[test]
fn an_agent_that_does_not_echo_leaves_the_sent_entry_alone() {
    let mut timeline = Timeline::default();
    timeline.begin_prompt(&[text("hi")]);
    timeline.reduce(&agent("yo", Some("a1")), None);
    timeline.reduce(&user("later", None), None);
    assert_eq!(timeline.entries().len(), 3);
    assert_eq!(entries(&timeline)[0], message("user", None, json!([chunk("hi")])));
}

// --- Prompt outcomes (§4.2) -------------------------------------------------------

#[test]
fn every_sent_prompt_keeps_its_outcome_and_content() {
    let mut timeline = Timeline::default();
    timeline.begin_prompt(&[text("one")]);
    timeline.reduce(&agent("partial", Some("a")), None);
    timeline.end_prompt(PromptEnd::Error(json!({ "code": -32603, "message": "boom" })));
    timeline.begin_prompt(&[text("two")]);
    timeline.reduce(&agent("cut", Some("b")), None);
    timeline.end_prompt(PromptEnd::Interrupted(true));
    timeline.begin_prompt(&[text("three")]);
    timeline.end_prompt(PromptEnd::Response(json!({ "stopReason": "cancelled" })));
    timeline.begin_prompt(&[text("four")]);
    assert_eq!(timeline.entries().len(), 6);
    assert_eq!(
        serde_json::to_value(timeline.prompts()).unwrap(),
        json!([
            { "entry": 0, "end": { "error": { "code": -32603, "message": "boom" } } },
            { "entry": 2, "end": { "interrupted": true } },
            { "entry": 4, "end": { "response": { "stopReason": "cancelled" } } },
            { "entry": 5 },
        ])
    );
}

// --- Derived turns (§5) -----------------------------------------------------------

#[test]
fn turns_start_at_user_messages_and_a_resumed_user_message_continues_its_turn() {
    let timeline = replay(&[
        agent("preface", None),
        user("q1", Some("u1")),
        json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "x" }),
        user("q1 again", Some("u1")),
        agent("a1", None),
        user("q2", Some("u2")),
        agent("a2", None),
    ]);
    assert_eq!(
        timeline.turns(),
        vec![
            Turn { start: 0, end: 1, prompt: None },
            Turn { start: 1, end: 5, prompt: None },
            Turn { start: 5, end: 7, prompt: None },
        ]
    );
}

#[test]
fn a_prompt_belongs_to_the_turn_its_entry_starts() {
    let mut timeline = Timeline::default();
    timeline.begin_prompt(&[text("a")]);
    timeline.reduce(&agent("x", None), None);
    timeline.end_prompt(PromptEnd::Response(json!({ "stopReason": "end_turn" })));
    timeline.begin_prompt(&[text("b")]);
    assert_eq!(
        timeline.turns(),
        vec![
            Turn { start: 0, end: 2, prompt: Some(0) },
            Turn { start: 2, end: 3, prompt: Some(1) },
        ]
    );
}

// --- Live and replay build the same structure (§4.3) ------------------------------

/// What the Agent streams for one prompt, as both a live run and a replay see it.
fn agent_content() -> Vec<Value> {
    vec![
        thought("thinking", Some("th1")),
        json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Run", "kind": "execute" }),
        json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
                "content": [{ "type": "content", "content": text("done") }] }),
        json!({ "sessionUpdate": "plan_update", "plan": { "planId": "p", "entries": ["x"] } }),
        agent("Hel", Some("a1")),
        agent("lo", Some("a1")),
        json!({ "sessionUpdate": "compaction_update", "compactionId": "c", "status": "completed",
                "summary": [text("short")] }),
    ]
}

fn live(echo: Option<Value>) -> Timeline {
    let mut timeline = Timeline::default();
    timeline.begin_prompt(&[text("hi")]);
    if let Some(echo) = echo {
        timeline.reduce(&echo, None);
    }
    reduce_all(&mut timeline, &agent_content());
    timeline.end_prompt(PromptEnd::Response(json!({ "stopReason": "end_turn" })));
    timeline
}

#[test]
fn a_live_prompt_with_an_exact_echo_equals_its_replay() {
    let mut updates = vec![user("hi", Some("u1"))];
    updates.extend(agent_content());
    assert_eq!(entries(&live(Some(user("hi", Some("u1"))))), entries(&replay(&updates)));
}

#[test]
fn a_live_prompt_without_an_echo_equals_a_replay_that_carries_no_id() {
    let mut updates = vec![user("hi", None)];
    updates.extend(agent_content());
    assert_eq!(entries(&live(None)), entries(&replay(&updates)));
}

#[test]
fn a_load_replaces_the_live_prompt_outcomes() {
    let mut updates = vec![user("hi", Some("u1"))];
    updates.extend(agent_content());
    let loaded = Timeline::from_replay(updates.iter().map(|update| (update, None)));
    assert!(loaded.prompts().is_empty());
    assert_eq!(loaded.entries().len(), 6);
}

#[test]
fn the_current_turn_cursor_agrees_with_the_derived_turns() {
    let mut timeline = Timeline::default();
    for round in 0..3 {
        timeline.begin_prompt(&[text(&format!("q{round}"))]);
        timeline.reduce(&legacy_plan("p"), None);
        timeline.reduce(&agent("a", None), None);
        timeline.end_prompt(PromptEnd::Response(json!({ "stopReason": "end_turn" })));
    }
    let turns = timeline.turns();
    assert_eq!(turns.len(), 3);
    // One legacy plan entry per turn, each in its own turn.
    for turn in &turns {
        let plans = (turn.start..turn.end)
            .filter(|&i| matches!(&*timeline.entries()[i], Entry::Plan(_)))
            .count();
        assert_eq!(plans, 1);
    }
}
