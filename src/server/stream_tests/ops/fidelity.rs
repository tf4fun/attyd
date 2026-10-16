//! Content fidelity of the log (protocol §3; acceptance 19, 20, 25, 26, 27,
//! 30, 31, 36): a scripted Agent sends a fixed sequence of ACP updates, and
//! each case asserts, from that input, the items and fields the settled log
//! holds, live and after a `load` replays them. Every replica converges.
use super::*;

/// A prompt on `session_id`, admitted with the expectation the session shows.
async fn prompt(bench: &Bench, link: &mut Link, session_id: &str, words: &str) -> (Value, Value) {
    let scope = format!("session/{session_id}");
    let expect = bench.expect_of(&scope).await;
    let (status, body) = bench
        .op(&scope, &expect, "prompt", json!({"prompt": [text(words)]}))
        .await;
    let op = assert_accepted(status, &body, words);
    (op, link.request("session/prompt").await)
}

fn user_item(words: &str) -> Value {
    json!({"type": "message", "role": "user", "chunks": [{"content": text(words)}]})
}

fn message(role: &str, message_id: &str, chunks: Value) -> Value {
    json!({"type": "message", "role": role, "messageId": message_id, "chunks": chunks})
}

fn diff() -> Value {
    json!({"type": "diff", "path": "/work/a.txt", "oldText": "x", "newText": "y"})
}

fn plan_entries() -> Value {
    json!([{"content": "step", "priority": "high", "status": "completed"}])
}

fn id_plan(plan_id: &str, content: &str) -> Value {
    json!({
        "type": "items", "planId": plan_id,
        "entries": [{"content": content, "priority": "medium", "status": "pending"}],
    })
}

/// A turn with thoughts, one message on both sides of a tool call, a second
/// message with chunk `_meta`, a tool call patched by sparse updates, a
/// legacy and an ID-addressed plan, a compaction with summary chunks, and a
/// user chunk the Agent echoes, in that order.
fn turn_updates() -> Vec<Value> {
    vec![
        json!({"sessionUpdate": "agent_thought_chunk", "messageId": "th", "content": text("think")}),
        json!({"sessionUpdate": "agent_message_chunk", "messageId": "m1", "content": text("a")}),
        json!({
            "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit",
            "kind": "edit", "status": "in_progress", "_meta": {"tool": 1},
        }),
        json!({"sessionUpdate": "agent_message_chunk", "messageId": "m1", "content": text("b")}),
        json!({
            "sessionUpdate": "agent_message_chunk", "messageId": "m2",
            "content": text("c"), "_meta": {"chunk": 1},
        }),
        json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
            "content": [diff()], "locations": [{"path": "/work/a.txt", "line": 1}],
            "rawInput": {"path": "/work/a.txt"}, "rawOutput": {"ok": true},
        }),
        // `null` keeps.
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "title": null, "content": null, "rawOutput": null}),
        json!({"sessionUpdate": "plan", "entries": plan_entries(), "_meta": {"plan": 1}}),
        json!({"sessionUpdate": "plan_update", "plan": id_plan("p", "draft")}),
        json!({"sessionUpdate": "compaction_update", "compactionId": "c", "status": "in_progress", "_meta": {"compaction": 1}}),
        json!({
            "sessionUpdate": "compaction_summary_chunk", "compactionId": "c",
            "content": text("sum"), "_meta": {"summary": 1},
        }),
        json!({
            "sessionUpdate": "compaction_summary_chunk", "compactionId": "c",
            "content": text("mary"), "_meta": {"summary": 2},
        }),
        json!({"sessionUpdate": "compaction_update", "compactionId": "c", "status": "completed"}),
        json!({"sessionUpdate": "agent_message_chunk", "messageId": "m2", "content": text("d")}),
    ]
}

/// The items `turn_updates` reduce to, after the turn's first item.
fn turn_items() -> Vec<Value> {
    vec![
        message("thought", "th", json!([{"content": text("think")}])),
        message("agent", "m1", json!([{"content": text("a")}])),
        json!({"type": "tool", "call": {
            "toolCallId": "t1", "title": "Edit", "kind": "edit", "status": "completed",
            "content": [diff()], "locations": [{"path": "/work/a.txt", "line": 1}],
            "rawInput": {"path": "/work/a.txt"}, "rawOutput": {"ok": true},
            "_meta": {"tool": 1},
        }}),
        message("agent", "m1", json!([{"content": text("b")}])),
        message(
            "agent",
            "m2",
            json!([{"content": text("c"), "_meta": {"chunk": 1}}]),
        ),
        json!({"type": "plan", "update": {"entries": plan_entries(), "_meta": {"plan": 1}}}),
        json!({"type": "plan", "update": {"plan": id_plan("p", "draft")}}),
        json!({
            "type": "compaction", "compactionId": "c", "status": "completed",
            "summary": [
                {"content": text("sum"), "_meta": {"summary": 1}},
                {"content": text("mary"), "_meta": {"summary": 2}},
            ],
            "_meta": {"compaction": 1},
        }),
        message("agent", "m2", json!([{"content": text("d")}])),
    ]
}

/// Acceptance 19, 20, 25, 26: a completed and a cancelled prompt settle every
/// item of their turn, in order, with message boundaries, `_meta` and the
/// prompt's response as `end`; the user chunks the Agent sends are items of
/// the turn and start none.
#[tokio::test]
async fn a_completed_and_a_cancelled_turn_keep_their_items_and_end() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create(&mut link, "s", json!({})).await;
    let mut watch = bench.watch(&scopes(&["session/s"])).await;
    watch
        .until("the alignment", |watch| watch.has("session/s"))
        .await;

    let (first, request) = prompt(&bench, &mut link, "s", "go").await;
    for update in turn_updates() {
        link.update("s", update).await;
    }
    // The Agent echoes the prompt as a user chunk: an item of this turn.
    link.update(
        "s",
        json!({"sessionUpdate": "user_message_chunk", "content": text("go")}),
    )
    .await;
    let response = json!({"stopReason": "end_turn", "_meta": {"response": 1}});
    link.reply(&request, response.clone()).await;
    watch.until_op("session/s", &first, "completed").await;

    let log = watch.log("session/s");
    assert_eq!(log.len(), 1, "{log:#?}");
    let mut items = vec![user_item("go")];
    items.extend(turn_items());
    items.push(user_item("go"));
    assert_eq!(
        log[0]["turn"],
        json!({"items": items, "end": {"response": response}}),
        "the settled turn"
    );

    // A cancelled prompt settles its content with its response.
    let (second, request) = prompt(&bench, &mut link, "s", "stop").await;
    link.update(
        "s",
        json!({"sessionUpdate": "agent_message_chunk", "content": text("partial")}),
    )
    .await;
    link.update(
        "s",
        json!({"sessionUpdate": "tool_call", "toolCallId": "t2", "title": "Run"}),
    )
    .await;
    let (status, body) = bench.cancel("session/s", &second).await;
    assert_eq!(status, StatusCode::ACCEPTED, "the cancel: {body}");
    link.request("session/cancel").await;
    link.update(
        "s",
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "t2", "status": "failed"}),
    )
    .await;
    link.reply(&request, json!({"stopReason": "cancelled"}))
        .await;
    watch.until_op("session/s", &second, "completed").await;
    let log = watch.log("session/s");
    assert_eq!(log.len(), 2, "{log:#?}");
    assert_eq!(
        log[1]["turn"],
        json!({
            "items": [
                user_item("stop"),
                {"type": "message", "role": "agent", "chunks": [{"content": text("partial")}]},
                {"type": "tool", "call": {
                    "toolCallId": "t2", "title": "Run", "kind": "other", "status": "failed",
                    "content": [], "locations": [],
                }},
            ],
            "end": {"response": {"stopReason": "cancelled"}},
        }),
        "the cancelled turn"
    );
    assert_ne!(log[0]["hash"], log[1]["hash"]);
    link.quiet(QUIET).await;
    watch.quiet(QUIET).await;
    watch.converged(&bench).await;
}

/// Acceptance 19, 27, 30: a `load` replays the same updates; the log holds
/// the same items without `end`, one turn per user message (a user message
/// split by a tool call is one turn), and nothing the replay did not provide.
#[tokio::test]
async fn a_load_replays_every_item_into_turns_by_user_message() {
    let (bench, mut link) =
        Bench::ws(json!({"loadSession": true, "promptCapabilities": {"image": true}})).await;
    let mut watch = bench.watch(&cold("saved")).await;
    let mut replay = vec![json!({"sessionUpdate": "user_message_chunk", "messageId": "u1", "content": text("go")})];
    replay.extend(turn_updates());
    replay.extend([
        json!({"sessionUpdate": "user_message_chunk", "messageId": "u2", "content": text("look")}),
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "t3", "status": "pending"}),
        json!({"sessionUpdate": "user_message_chunk", "messageId": "u2", "content": image("aGk=")}),
        json!({"sessionUpdate": "user_message_chunk", "messageId": "u3", "content": text("next")}),
    ]);
    link.serve_load("saved", &replay).await;
    watch
        .until_live("session/saved", "the load completes", |live| {
            live["op"]["kind"] == "load" && live["op"]["status"] == "completed"
        })
        .await;

    let log = watch.log("session/saved");
    assert_eq!(log.len(), 3, "{log:#?}");
    let mut items = vec![message("user", "u1", json!([{"content": text("go")}]))];
    items.extend(turn_items());
    assert_eq!(log[0]["turn"], json!({"items": items}), "the first turn");
    let turn = &log[1]["turn"];
    assert_eq!(turn.get("end"), None, "a replayed turn has no end");
    assert_eq!(turn["items"].as_array().unwrap().len(), 3, "{turn:#}");
    assert_eq!(
        turn["items"][0],
        message("user", "u2", json!([{"content": text("look")}]))
    );
    assert_eq!(
        turn["items"][1],
        json!({"type": "tool", "call": {"toolCallId": "t3", "status": "pending"}}),
        "a first sparse update keeps only its fields"
    );
    assert_eq!(turn["items"][2]["messageId"], "u2");
    assert_eq!(turn["items"][2]["chunks"][0]["content"]["type"], "attachment");
    assert_eq!(
        log[2]["turn"],
        json!({"items": [message("user", "u3", json!([{"content": text("next")}]))]})
    );
    watch.converged(&bench).await;
}

/// Acceptance 31: plans keep one item per `planId` in the session. Turn A
/// creates P and Q; turn B updates and then removes P. P's item stays at its
/// position in A, in its removed state; Q stays current; B adds no plan item.
/// A legacy plan in each turn leaves one snapshot in each.
#[tokio::test]
async fn plans_keep_one_item_per_id_across_turns() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create(&mut link, "s", json!({})).await;
    let mut watch = bench.watch(&scopes(&["session/s"])).await;
    watch
        .until("the alignment", |watch| watch.has("session/s"))
        .await;

    let (a, request) = prompt(&bench, &mut link, "s", "a").await;
    for update in [
        json!({"sessionUpdate": "plan_update", "plan": id_plan("P", "p1")}),
        json!({"sessionUpdate": "plan_update", "plan": id_plan("Q", "q1")}),
        json!({"sessionUpdate": "plan", "entries": plan_entries()}),
        json!({"sessionUpdate": "plan", "entries": []}),
    ] {
        link.update("s", update).await;
    }
    link.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    watch.until_op("session/s", &a, "completed").await;
    let settled_a = watch.log("session/s")[0].clone();

    let (b, request) = prompt(&bench, &mut link, "s", "b").await;
    for update in [
        json!({"sessionUpdate": "plan_update", "plan": id_plan("P", "p2")}),
        json!({"sessionUpdate": "plan_removed", "planId": "P", "_meta": {"removed": 1}}),
        json!({"sessionUpdate": "plan", "entries": plan_entries()}),
    ] {
        link.update("s", update).await;
    }
    link.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    watch.until_op("session/s", &b, "completed").await;

    let log = watch.log("session/s");
    assert_eq!(log.len(), 2, "{log:#?}");
    assert_eq!(
        log[0]["turn"]["items"],
        json!([
            user_item("a"),
            {"type": "plan", "update": {"planId": "P", "_meta": {"removed": 1}}},
            {"type": "plan", "update": {"plan": id_plan("Q", "q1")}},
            {"type": "plan", "update": {"entries": []}},
        ]),
        "turn A"
    );
    assert_ne!(log[0]["hash"], settled_a["hash"], "turn A was rewritten");
    assert_eq!(
        log[1]["turn"]["items"],
        json!([
            user_item("b"),
            {"type": "plan", "update": {"entries": plan_entries()}},
        ]),
        "turn B"
    );
    link.quiet(QUIET).await;
    watch.quiet(QUIET).await;
    watch.converged(&bench).await;
}

/// Acceptance 36: a permission's sparse `toolCall` and a first sparse
/// `tool_call_update` each start an item holding exactly their fields; later
/// updates and a `tool_call` fill them in by the patch rule.
#[tokio::test]
async fn first_sparse_tool_states_are_kept_as_received() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create(&mut link, "s", json!({})).await;
    let mut watch = bench.watch(&scopes(&["session/s"])).await;
    watch
        .until("the alignment", |watch| watch.has("session/s"))
        .await;

    let (op, request) = prompt(&bench, &mut link, "s", "go").await;
    link.agent_request(
        "ask",
        "session/request_permission",
        json!({
            "sessionId": "s",
            "toolCall": {"toolCallId": "t"},
            "options": [{"optionId": "allow", "name": "Allow once", "kind": "allow_once"}],
        }),
    )
    .await;
    link.update(
        "s",
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "u", "status": "pending"}),
    )
    .await;
    watch
        .until_live("session/s", "the permission", |live| {
            !interaction_ids(live).is_empty()
        })
        .await;
    let interaction = interaction_for(&watch.live("session/s"), "t");
    let (status, body) = bench
        .answer("session/s", &op, answers(&[(interaction.as_str(), allow())]))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "the answer: {body}");
    link.response("ask").await;
    for update in [
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "u", "title": "Read", "kind": "read"}),
        json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "u",
            "content": [{"type": "content", "content": text("out")}], "rawInput": {"n": 1},
        }),
        json!({"sessionUpdate": "tool_call", "toolCallId": "t", "title": "Run", "rawInput": {"cmd": "ls"}}),
    ] {
        link.update("s", update).await;
    }
    link.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    watch.until_op("session/s", &op, "completed").await;

    let log = watch.log("session/s");
    assert_eq!(
        log[0]["turn"]["items"],
        json!([
            user_item("go"),
            {"type": "tool", "call": {
                "toolCallId": "t", "title": "Run", "kind": "other", "status": "pending",
                "content": [], "locations": [], "rawInput": {"cmd": "ls"},
            }},
            {"type": "tool", "call": {
                "toolCallId": "u", "status": "pending", "title": "Read", "kind": "read",
                "content": [{"type": "content", "content": text("out")}], "rawInput": {"n": 1},
            }},
        ]),
    );
    link.quiet(QUIET).await;
    watch.quiet(QUIET).await;
    watch.converged(&bench).await;
}

/// Acceptance 13, 25: an attachment keeps the block as received, `_meta` and
/// annotations included, in the log, in a tool call's content and in the
/// read; a text block keeps its `_meta` and annotations inline.
#[tokio::test]
async fn attachments_and_text_keep_their_meta_and_annotations() {
    let (bench, mut link) = Bench::ws(json!({"promptCapabilities": {"image": true}})).await;
    bench.create(&mut link, "s", json!({})).await;
    let mut watch = bench.watch(&scopes(&["session/s"])).await;
    watch
        .until("the alignment", |watch| watch.has("session/s"))
        .await;
    let mut picture = image("bWV0YQ==");
    picture["_meta"] = json!({"block": 1});
    picture["annotations"] = json!({"priority": 0.5});
    let mut tool_picture = image("dG9vbA==");
    tool_picture["_meta"] = json!({"tool": 1});
    let noted = json!({"type": "text", "text": "noted", "annotations": {"audience": ["user"]}, "_meta": {"text": 1}});

    let (op, request) = prompt(&bench, &mut link, "s", "show").await;
    link.update(
        "s",
        json!({"sessionUpdate": "agent_message_chunk", "content": picture.clone()}),
    )
    .await;
    link.update(
        "s",
        json!({"sessionUpdate": "agent_message_chunk", "content": noted.clone()}),
    )
    .await;
    link.update(
        "s",
        json!({
            "sessionUpdate": "tool_call", "toolCallId": "look", "title": "Look",
            "content": [{"type": "content", "content": tool_picture.clone()}],
        }),
    )
    .await;
    link.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    watch.until_op("session/s", &op, "completed").await;

    let items = watch.log("session/s")[0]["turn"]["items"].clone();
    let chunks = &items[1]["chunks"];
    assert_eq!(chunks[0]["content"]["type"], "attachment", "{items:#}");
    assert_eq!(chunks[1]["content"], noted, "text keeps its fields inline");
    let in_tool = items[2]["call"]["content"][0]["content"].clone();
    assert_eq!(in_tool["type"], "attachment", "{items:#}");
    let path = |reference: &Value| {
        format!(
            "/api/v2/sessions/s/attachments/{}",
            reference["id"].as_str().unwrap()
        )
    };
    let (status, body) = bench.get(&path(&chunks[0]["content"])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, picture, "the block as received");
    let (status, body) = bench.get(&path(&in_tool)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, tool_picture, "the tool's block as received");
    link.quiet(QUIET).await;
    watch.quiet(QUIET).await;
    watch.converged(&bench).await;
}
