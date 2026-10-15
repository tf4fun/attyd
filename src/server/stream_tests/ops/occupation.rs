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

/// A failed delete keeps its error on `global.op`. A refused close leaves
/// the session usable; a successful close needs a real load before the same
/// subscription can use the session again (S5, S6; acceptance 14).
#[tokio::test]
async fn a_failed_held_delete_keeps_its_error_and_the_subscription() {
    for (close_succeeds, supplied_cwd) in [
        (false, None),
        (true, None),
        (true, Some("/workspace/ignored-subscription-directory")),
    ] {
        let (bench, mut link) = Bench::ws(deletable()).await;
        bench.create(&mut link, "s", json!({})).await;
        // A held session needs no cwd, and ignores a supplied directory.
        // Neither case may change the directory of a later recovery load.
        let mut query = scopes(&["global", "session/s"]);
        if let Some(cwd) = supplied_cwd {
            query.push_str(&format!("&cwd={}", encoded(&format!("session/s:{cwd}"))));
        }
        let mut watch = bench.watch(&query).await;
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
        watch.until_op("global", &delete, "running").await;
        watch.until_op("session/s", &delete, "running").await;
        if close_succeeds {
            link.reply(&close, json!({})).await;
            let request = link.request("session/delete").await;
            link.refuse(&request).await;
        } else {
            link.refuse(&close).await;
        }
        watch.until_op("global", &delete, "failed").await;
        let error = watch.live("global")["op"]["error"].clone();
        assert_eq!(error["code"], "agent_error", "{error}");
        assert_eq!(error["data"]["code"], -32600, "{error}");

        if close_succeeds {
            // The failed delete cannot undo the Agent's successful close.
            // Its occupation ends, so the existing subscription resumes by
            // loading in the directory the bridge already knew.
            let load = link.request("session/load").await;
            assert_eq!(load["params"]["sessionId"], "s");
            assert_eq!(load["params"]["cwd"], CWD);
            watch
                .until_live("session/s", "the recovery load runs", |live| {
                    live["op"]["kind"] == "load" && live["op"]["status"] == "running"
                })
                .await;
            let loading = watch.live("session/s")["op"]["id"].clone();
            for (kind, content) in [
                ("user_message_chunk", "earlier prompt"),
                ("agent_message_chunk", "earlier reply"),
            ] {
                link.update(
                    "s",
                    json!({"sessionUpdate": kind, "content": text(content)}),
                )
                .await;
            }
            link.reply(&load, json!({})).await;
            watch.until_op("session/s", &loading, "completed").await;
            watch
                .until_live("global", "the restored session's summary", |live| {
                    live["sessions"]["s"]["op"] == json!({"kind": "load", "status": "completed"})
                })
                .await;
            let log = watch.log("session/s");
            assert_eq!(log.len(), 1, "the Agent's replay is installed: {log:?}");
            assert_eq!(
                log[0]["turn"],
                json!({"prompt": [text("earlier prompt")], "reply": [text("earlier reply")]})
            );
        } else {
            // A refused close never retired the original session. It is
            // usable without another load, once the delete frees it.
            watch
                .until_live("session/s", "the session is released", |live| {
                    live["op"]["status"] != "running"
                })
                .await;
            assert!(watch.live("global")["sessions"].get("s").is_some());
        }
        assert!(
            watch.ended("session/s").is_none(),
            "a failed delete leaves the existing subscription following the session"
        );
        assert_eq!(watch.live("global")["op"]["id"], delete);
        assert_eq!(watch.live("global")["op"]["error"], error);
        watch.converged(&bench).await;

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
        // In the refused-close branch this also rejects an unexpected
        // session/load or session/delete before the prompt.
        let turn = link.request("session/prompt").await;
        link.reply(&turn, json!({"stopReason": "end_turn"})).await;
        watch.until_op("session/s", &prompt, "completed").await;
        watch
            .until_live("global", "the prompt's summary", |live| {
                live["sessions"]["s"]["op"] == json!({"kind": "prompt", "status": "completed"})
            })
            .await;
        // Neither the recovery load nor the prompt replaces the global error.
        assert_eq!(watch.live("global")["op"]["id"], delete);
        assert_eq!(watch.live("global")["op"]["status"], "failed");
        assert_eq!(watch.live("global")["op"]["error"], error);
        watch.quiet(QUIET).await;
        watch.converged(&bench).await;
    }
}

/// Remembering an existing subscription's directory does not supply a
/// directory to a new subscription of an unheld session (§5.1).
#[tokio::test]
async fn a_new_subscription_after_a_partially_failed_delete_still_needs_cwd() {
    let (bench, mut link) = Bench::ws(deletable()).await;
    bench.create(&mut link, "s", json!({})).await;
    // There is no session subscription to retain a recovery directory.
    let mut watch = bench.watch(&scopes(&["global"])).await;
    watch
        .until("the global alignment", |watch| watch.has("global"))
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
    let (status, body) = bench.read("session/s").await;
    not_found(status, &body, "the Agent closed the session");

    let mut missing = bench.watch(&scopes(&["session/s"])).await;
    missing
        .until("the missing-directory error", |watch| {
            watch.ended("session/s").is_some()
        })
        .await;
    let end = missing.ended("session/s").unwrap();
    assert_eq!(end["reason"], "load_failed", "{end}");
    assert_eq!(end["error"]["code"], "invalid_request", "{end}");
    assert!(!missing.has("session/s"));
    link.quiet(QUIET).await;
    assert_eq!(watch.live("global")["op"]["id"], delete);
    assert_eq!(watch.live("global")["op"]["error"], error);
    watch.quiet(QUIET).await;
    missing.quiet(QUIET).await;
    watch.converged(&bench).await;
    missing.converged(&bench).await;
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
