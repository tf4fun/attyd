//! Operations across scopes (protocol S5, §6.2 Delete, §6.4; acceptance 10,
//! 11): a delete occupies `global` and its session, is the `op` of both with
//! one ID until it ends, and holds back the load of an unheld session.
use super::*;

fn deletable() -> Value {
    json!({"loadSession": true, "sessionCapabilities": {"close": {}, "delete": {}}})
}

/// A held session's prompt and a global delete of that session: whichever is
/// admitted first, the other is refused, and both never run.
#[tokio::test]
async fn a_held_sessions_prompt_and_its_delete_are_never_both_admitted() {
    let (bench, mut link) = Bench::ws(deletable()).await;
    for id in ["first", "second", "third"] {
        bench.create(&mut link, id, json!({})).await;
    }
    let mut watch = bench
        .watch(&scopes(&[
            "global",
            "session/first",
            "session/second",
            "session/third",
        ]))
        .await;
    watch
        .until("the alignments", |watch| {
            ["global", "session/first", "session/second", "session/third"]
                .iter()
                .all(|scope| watch.has(scope))
        })
        .await;

    // The delete first: it is the `op` of both scopes, and the prompt is
    // refused on either ID.
    let open = watch.live("session/first")["op"]["id"].clone();
    let global = watch.live("global")["op"]["id"].clone();
    let (status, body) = bench
        .op("global", &global, "delete", json!({"sessionId": "first"}))
        .await;
    let delete = assert_accepted(status, &body, "the delete");
    let close = link.request("session/close").await;
    assert_eq!(close["params"]["sessionId"], "first");
    watch.until_op("global", &delete, "running").await;
    watch.until_op("session/first", &delete, "running").await;
    assert_eq!(watch.live("session/first")["op"]["kind"], "delete");
    let before = bench.document("session/first").await;
    let (status, body) = bench
        .op(
            "session/first",
            &open,
            "prompt",
            json!({"prompt": [text("x")]}),
        )
        .await;
    stale(status, &body, "a prompt on the open the delete replaced");
    let (status, body) = bench
        .op(
            "session/first",
            &delete,
            "prompt",
            json!({"prompt": [text("x")]}),
        )
        .await;
    not_allowed(status, &body, "a prompt while the delete occupies");
    link.quiet(QUIET).await;
    assert_eq!(bench.document("session/first").await, before);
    link.reply(&close, json!({})).await;
    let request = link.request("session/delete").await;
    assert_eq!(request["params"]["sessionId"], "first");
    link.reply(&request, json!({})).await;
    watch.until_op("global", &delete, "completed").await;
    watch
        .until("the deleted scope ends", |watch| {
            watch.ended("session/first").is_some()
        })
        .await;
    assert_eq!(
        watch.ended("session/first").unwrap(),
        json!({"reason": "deleted"})
    );
    watch
        .until_live("global", "the deleted session leaves global", |live| {
            live["sessions"].get("first").is_none()
        })
        .await;

    // The prompt first: the delete is refused while the session is occupied.
    let open = watch.live("session/second")["op"]["id"].clone();
    let (status, body) = bench
        .op(
            "session/second",
            &open,
            "prompt",
            json!({"prompt": [text("x")]}),
        )
        .await;
    let prompt = assert_accepted(status, &body, "the prompt");
    let turn = link.request("session/prompt").await;
    let before = bench.document("global").await;
    let (status, body) = bench
        .op("global", &delete, "delete", json!({"sessionId": "second"}))
        .await;
    not_allowed(status, &body, "a delete of the prompting session");
    link.quiet(QUIET).await;
    assert_eq!(bench.document("global").await, before);
    link.reply(&turn, json!({"stopReason": "end_turn"})).await;
    watch.until_op("session/second", &prompt, "completed").await;
    let (status, body) = bench
        .op("global", &delete, "delete", json!({"sessionId": "second"}))
        .await;
    let second = assert_accepted(status, &body, "the delete once the session is free");
    let close = link.request("session/close").await;
    link.reply(&close, json!({})).await;
    let request = link.request("session/delete").await;
    link.reply(&request, json!({})).await;
    watch.until_op("global", &second, "completed").await;
    watch
        .until("the second scope ends", |watch| {
            watch.ended("session/second").is_some()
        })
        .await;

    // Both at once: exactly one is admitted.
    let open = watch.live("session/third")["op"]["id"].clone();
    let (prompted, deleted) = tokio::join!(
        bench.op(
            "session/third",
            &open,
            "prompt",
            json!({"prompt": [text("x")]})
        ),
        bench.op("global", &second, "delete", json!({"sessionId": "third"})),
    );
    match (prompted.0, deleted.0) {
        (StatusCode::ACCEPTED, _) => {
            not_allowed(deleted.0, &deleted.1, "the delete that lost the race");
            let turn = link.request("session/prompt").await;
            link.quiet(QUIET).await;
            link.reply(&turn, json!({"stopReason": "end_turn"})).await;
            watch
                .until_op("session/third", &prompted.1["op"], "completed")
                .await;
            watch
                .until_live("global", "the prompt's summary", |live| {
                    live["sessions"]["third"]["op"]
                        == json!({"kind": "prompt", "status": "completed"})
                })
                .await;
        }
        (_, StatusCode::ACCEPTED) => {
            // The running delete replaced the open the prompt names.
            stale(prompted.0, &prompted.1, "the prompt that lost the race");
            let close = link.request("session/close").await;
            link.quiet(QUIET).await;
            link.reply(&close, json!({})).await;
            let request = link.request("session/delete").await;
            link.reply(&request, json!({})).await;
            watch
                .until_op("global", &deleted.1["op"], "completed")
                .await;
            watch
                .until("the third scope ends", |watch| {
                    watch.ended("session/third").is_some()
                })
                .await;
            assert_eq!(
                watch.ended("session/third").unwrap(),
                json!({"reason": "deleted"})
            );
            watch
                .until_live("global", "the third leaves global", |live| {
                    live["sessions"].get("third").is_none()
                })
                .await;
        }
        _ => panic!("one of the two is admitted: {prompted:?} {deleted:?}"),
    }
    watch.quiet(QUIET).await;
    watch.converged(&bench).await;
}

/// The close's interaction joins the delete; either scope that shows it
/// answers it.
#[tokio::test]
async fn a_close_interaction_is_answered_through_either_scope_that_shows_it() {
    let (bench, mut link) = Bench::ws(deletable()).await;
    for id in ["a", "b"] {
        bench.create(&mut link, id, json!({})).await;
    }
    let mut watch = bench
        .watch(&scopes(&["global", "session/a", "session/b"]))
        .await;
    watch
        .until("the alignments", |watch| {
            watch.has("global") && watch.has("session/a") && watch.has("session/b")
        })
        .await;
    let mut expect = watch.live("global")["op"]["id"].clone();
    for (id, through) in [("a", "session/a"), ("b", "global")] {
        let scope = format!("session/{id}");
        let (status, body) = bench
            .op("global", &expect, "delete", json!({"sessionId": id}))
            .await;
        let delete = assert_accepted(status, &body, "the delete");
        let close = link.request("session/close").await;
        let question = format!("close-{id}");
        link.agent_request(
            &question,
            "elicitation/create",
            form(json!({"sessionId": id}), &question),
        )
        .await;
        watch
            .until("the interaction in both scopes", |watch| {
                [scope.as_str(), "global"].iter().all(|scope| {
                    watch.live(scope)["op"]["id"] == delete
                        && !interaction_ids(&watch.live(scope)).is_empty()
                })
            })
            .await;
        let interaction = interaction_for(&watch.live("global"), &question);
        assert_eq!(
            interaction_ids(&watch.live(&scope)),
            vec![interaction.clone()],
            "the occupied session shows the delete's interaction"
        );
        assert_eq!(
            watch.live(&scope)["op"]["interactions"],
            watch.live("global")["op"]["interactions"]
        );
        let mut answers = serde_json::Map::new();
        answers.insert(interaction.clone(), accept());
        let (status, body) = bench.answer(through, &delete, Value::Object(answers)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{through}: {body}");
        assert_eq!(body["op"], delete);
        assert_eq!(link.response(&question).await["result"]["action"], "accept");
        watch
            .until("the answer leaves both scopes", |watch| {
                interaction_ids(&watch.live("global")).is_empty()
                    && interaction_ids(&watch.live(&scope)).is_empty()
            })
            .await;
        link.reply(&close, json!({})).await;
        let request = link.request("session/delete").await;
        link.reply(&request, json!({})).await;
        watch.until_op("global", &delete, "completed").await;
        watch
            .until("the scope ends", |watch| watch.ended(&scope).is_some())
            .await;
        expect = delete;
    }
    watch.converged(&bench).await;
}

/// A held session's failed delete keeps the session; `global.op.error`
/// carries the Agent's error, and the session is free again.
#[tokio::test]
async fn a_failed_held_delete_keeps_its_session_and_the_error() {
    let (bench, mut link) = Bench::ws(deletable()).await;
    bench.create(&mut link, "s", json!({})).await;
    let mut watch = bench.watch(&scopes(&["global", "session/s"])).await;
    watch
        .until("the alignments", |watch| {
            watch.has("global") && watch.has("session/s")
        })
        .await;
    let global = watch.live("global")["op"]["id"].clone();
    let (status, body) = bench
        .op("global", &global, "delete", json!({"sessionId": "s"}))
        .await;
    let delete = assert_accepted(status, &body, "the delete");
    let close = link.request("session/close").await;
    link.reply(&close, json!({})).await;
    let request = link.request("session/delete").await;
    link.refuse(&request).await;
    watch.until_op("global", &delete, "failed").await;
    let error = watch.live("global")["op"]["error"].clone();
    assert_eq!(error["code"], "agent_error", "{error}");
    assert_eq!(error["data"]["code"], -32600, "{error}");
    assert!(watch.live("global")["sessions"].get("s").is_some());

    // The delete's occupation of the session has ended; whatever `op` the
    // session shows now is not running, and the session takes a prompt on it.
    watch
        .until_live("session/s", "the session is released", |live| {
            live["op"]["status"] != "running"
        })
        .await;
    assert!(watch.ended("session/s").is_none(), "the session stays");
    let released = watch.live("session/s")["op"]["id"].clone();
    let (status, body) = bench
        .op(
            "session/s",
            &released,
            "prompt",
            json!({"prompt": [text("still here")]}),
        )
        .await;
    let prompt = assert_accepted(status, &body, "a prompt after the failed delete");
    let turn = link.request("session/prompt").await;
    link.reply(&turn, json!({"stopReason": "end_turn"})).await;
    watch.until_op("session/s", &prompt, "completed").await;
    watch
        .until_live("global", "the prompt's summary", |live| {
            live["sessions"]["s"]["op"] == json!({"kind": "prompt", "status": "completed"})
        })
        .await;
    // `global` keeps the failed delete and its error.
    assert_eq!(watch.live("global")["op"]["id"], delete);
    assert_eq!(watch.live("global")["op"]["status"], "failed");
    assert_eq!(watch.live("global")["op"]["error"], error);
    watch.quiet(QUIET).await;
    watch.converged(&bench).await;
}

/// An unheld session's delete loads nothing: a subscription to it waits, and
/// ends with the delete; after a failed delete the subscription's load
/// starts.
#[tokio::test]
async fn a_cold_delete_holds_back_the_load_until_it_ends() {
    let (bench, mut link) = Bench::ws(deletable()).await;
    let mut watch = bench.watch(&scopes(&["global"])).await;
    watch
        .until("the alignment", |watch| watch.has("global"))
        .await;
    let global = watch.live("global")["op"]["id"].clone();
    let (status, body) = bench
        .op("global", &global, "delete", json!({"sessionId": "gone"}))
        .await;
    let delete = assert_accepted(status, &body, "a cold delete");
    let request = link.request("session/delete").await;
    assert_eq!(request["params"]["sessionId"], "gone");
    watch.until_op("global", &delete, "running").await;
    let (status, body) = bench.read("session/gone").await;
    not_found(status, &body, "an unheld occupied session is not held");

    let mut waiting = bench.watch(&cold("gone")).await;
    link.quiet(QUIET).await;
    waiting.quiet(QUIET).await;
    assert!(!waiting.has("session/gone"), "the subscription waits");
    link.reply(&request, json!({})).await;
    watch.until_op("global", &delete, "completed").await;
    waiting
        .until("the deleted session ends", |watch| {
            watch.ended("session/gone").is_some()
        })
        .await;
    assert_eq!(
        waiting.ended("session/gone").unwrap(),
        json!({"reason": "deleted"})
    );
    assert!(!waiting.has("session/gone"));
    link.quiet(QUIET).await;

    // A failed cold delete releases the held-back load.
    let (status, body) = bench
        .op("global", &delete, "delete", json!({"sessionId": "kept"}))
        .await;
    let failing = assert_accepted(status, &body, "a cold delete that fails");
    let request = link.request("session/delete").await;
    let mut waiting = bench.watch(&cold("kept")).await;
    link.quiet(QUIET).await;
    link.refuse(&request).await;
    watch.until_op("global", &failing, "failed").await;
    let load = link.request("session/load").await;
    assert_eq!(load["params"]["sessionId"], "kept");
    assert_eq!(load["params"]["cwd"], CWD);
    waiting
        .until_live("session/kept", "the load runs", |live| {
            live["op"]["kind"] == "load"
        })
        .await;
    let loading = waiting.live("session/kept")["op"]["id"].clone();
    assert_ne!(loading, failing);
    link.reply(&load, json!({})).await;
    waiting
        .until_op("session/kept", &loading, "completed")
        .await;
    watch
        .until_live("global", "the kept session's summary", |live| {
            live["sessions"]["kept"]["op"] == json!({"kind": "load", "status": "completed"})
        })
        .await;
    watch.quiet(QUIET).await;
    waiting.quiet(QUIET).await;
    watch.converged(&bench).await;
    waiting.converged(&bench).await;
}
