//! Running admitted operations and their responses (protocol §6.2, §6.3;
//! acceptance 2, 9, 18): each kind takes its ACP path; `create`, `fork` and
//! `list` answer with their own result when they end, the others with `202`.
use super::*;

#[tokio::test]
async fn a_prompt_answers_202_with_its_op_and_commits_its_result() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create(&mut link, "s", json!({})).await;
    let mut watch = bench.watch(&scopes(&["session/s"])).await;
    watch
        .until("the alignment", |watch| watch.has("session/s"))
        .await;
    let open = bench.expect_of("session/s").await;
    let (status, body) = bench
        .op(
            "session/s",
            &open,
            "prompt",
            json!({"prompt": [text("hello")]}),
        )
        .await;
    let op = assert_accepted(status, &body, "a prompt");
    let request = link.request("session/prompt").await;
    assert_eq!(request["params"]["prompt"], json!([text("hello")]));
    watch
        .until_live("session/s", "the prompt is shown", |live| {
            live["op"]["id"] == op
        })
        .await;
    let live = watch.live("session/s");
    assert_eq!(live["op"]["kind"], "prompt");
    assert_eq!(live["op"]["input"], json!({"prompt": [text("hello")]}));
    link.update(
        "s",
        json!({"sessionUpdate": "agent_message_chunk", "messageId": "m", "content": text("hi")}),
    )
    .await;
    link.reply(
        &request,
        json!({"stopReason": "end_turn", "usage": {"inputTokens": 1, "outputTokens": 2, "totalTokens": 3}}),
    )
    .await;
    watch.until_op("session/s", &op, "completed").await;
    let live = watch.live("session/s");
    assert_eq!(live["op"]["result"]["stopReason"], "end_turn");
    assert_eq!(live["op"]["result"]["usage"]["totalTokens"], 3);
    assert_eq!(watch.log("session/s").len(), 1);
    watch.converged(&bench).await;
}

/// `create`, `fork` and `list` answer `200` when they end: with their result,
/// or with the Agent's error.
#[tokio::test]
async fn create_fork_and_list_answer_with_their_result_or_error_when_they_end() {
    let (bench, mut link) = Bench::ws(json!({
        "sessionCapabilities": {"fork": {}, "list": {}},
    }))
    .await;
    let mut watch = bench.watch(&scopes(&["global", "catalog"])).await;
    watch
        .until("the alignments", |watch| {
            watch.has("global") && watch.has("catalog")
        })
        .await;

    // A failed create.
    let none = bench.expect_of("global").await;
    let created = bench.spawn_op("global", &none, "create", json!({"cwd": CWD}));
    let request = link.request("session/new").await;
    link.refuse(&request).await;
    let (status, body) = created.await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "an Agent failure is the operation's end: {body}"
    );
    assert_eq!(body["error"]["code"], "agent_error", "{body}");
    assert_eq!(body["error"]["data"]["code"], -32600, "{body}");
    assert!(body.get("result").is_none(), "{body}");
    let failed = body["op"].clone();
    watch
        .until_live("global", "the create fails", |live| {
            live["op"]["id"] == failed && live["op"]["status"] == "failed"
        })
        .await;
    assert_eq!(watch.live("global")["op"]["error"], body["error"]);

    // A failed operation has ended: the next create is admitted on it.
    assert_eq!(bench.expect_of("global").await, failed);
    let created = bench.create(&mut link, "s", json!({})).await;
    assert_ne!(created, failed);
    let open = bench.expect_of("session/s").await;
    let forked = bench.spawn_op("session/s", &open, "fork", json!({}));
    let request = link.request("session/fork").await;
    link.refuse(&request).await;
    let (status, body) = forked.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["error"]["code"], "agent_error", "{body}");
    let failed_fork = body["op"].clone();
    // The source's failed fork has ended: a fork is admitted on it.
    let forked = bench.spawn_op("session/s", &failed_fork, "fork", json!({}));
    let request = link.request("session/fork").await;
    link.reply(&request, json!({"sessionId": "forked"})).await;
    let (status, body) = forked.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"], json!({"sessionId": "forked"}));
    watch
        .until_live("global", "the fork's target is held", |live| {
            live["sessions"].get("forked").is_some()
        })
        .await;
    let (_, target) = bench.read("session/forked").await;
    assert_eq!(target["live"]["op"]["kind"], "open");
    assert_eq!(target["live"]["op"]["status"], "completed");

    // A failed list.
    let catalog = bench.expect_of("catalog").await;
    let listed = bench.spawn_op("catalog", &catalog, "list", json!({}));
    let request = link.request("session/list").await;
    link.refuse(&request).await;
    let (status, body) = listed.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["error"]["code"], "agent_error", "{body}");
    let failed_list = body["op"].clone();
    watch.until_op("catalog", &failed_list, "failed").await;

    // The failed list has ended: the next list is admitted on it.
    let listed = bench.spawn_op("catalog", &failed_list, "list", json!({}));
    let request = link.request("session/list").await;
    link.reply(&request, json!({"sessions": [], "nextCursor": null}))
        .await;
    let (status, body) = listed.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_ne!(body["op"], failed_list);
    watch.until_op("catalog", &body["op"], "completed").await;
    watch.quiet(QUIET).await;
    watch.converged(&bench).await;
}

/// A fork passes an explicit `cwd` to the Agent and its target uses it; an
/// omitted one inherits the source's.
#[tokio::test]
async fn a_fork_uses_its_explicit_cwd_and_otherwise_the_source_cwd() {
    let (bench, mut link) = Bench::ws(json!({"sessionCapabilities": {"fork": {}}})).await;
    bench
        .create_in(&mut link, "s", "/workspace/source", json!({}))
        .await;
    let open = bench.expect_of("session/s").await;
    let forked = bench.spawn_op(
        "session/s",
        &open,
        "fork",
        json!({"cwd": "/workspace/other"}),
    );
    let request = link.request("session/fork").await;
    assert_eq!(request["params"]["sessionId"], "s");
    assert_eq!(request["params"]["cwd"], "/workspace/other");
    link.reply(&request, json!({"sessionId": "elsewhere"}))
        .await;
    let (status, body) = forked.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    let target = bench.document("session/elsewhere").await;
    assert_eq!(target["live"]["cwd"], "/workspace/other");
    let global = bench.document("global").await;
    assert_eq!(
        global["live"]["sessions"]["elsewhere"]["cwd"],
        "/workspace/other"
    );

    let forked = bench.spawn_op("session/s", &body["op"], "fork", json!({}));
    let request = link.request("session/fork").await;
    assert_eq!(request["params"]["cwd"], "/workspace/source");
    link.reply(&request, json!({"sessionId": "beside"})).await;
    let (status, body) = forked.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        bench.document("session/beside").await["live"]["cwd"],
        "/workspace/source"
    );
}

/// The response carries the operation's own result even when the scope's
/// `op` was already replaced by the time it is written (acceptance 9).
#[tokio::test]
async fn a_fork_response_survives_its_replacement_by_an_agent_request() {
    let (bench, mut link) = Bench::ws(json!({"sessionCapabilities": {"fork": {}}})).await;
    bench.create(&mut link, "s", json!({})).await;
    let mut watch = bench.watch(&scopes(&["session/s"])).await;
    watch
        .until("the alignment", |watch| watch.has("session/s"))
        .await;
    let open = bench.expect_of("session/s").await;
    // The response is held, once admitted, until the fork ended and its
    // successor was installed.
    let mut held = bench.hold();
    let forked = bench.spawn_op("session/s", &open, "fork", json!({}));
    let fork = held.admitted().await;
    let request = link.request("session/fork").await;
    watch.until_op("session/s", &fork, "running").await;
    link.reply(&request, json!({"sessionId": "forked"})).await;
    watch.until_op("session/s", &fork, "completed").await;
    link.agent_request(
        "after",
        "session/request_permission",
        permission("s", "after"),
    )
    .await;
    watch
        .until_live("session/s", "the agent request replaces the fork", |live| {
            live["op"]["kind"] == "agent_request"
        })
        .await;
    assert!(!forked.is_finished(), "the response is held");
    held.resume();
    let (status, body) = forked.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["op"], fork,
        "the response names the fork, not its successor"
    );
    assert_eq!(body["result"], json!({"sessionId": "forked"}));
    assert_ne!(watch.live("session/s")["op"]["id"], fork);
    watch.converged(&bench).await;
}

/// A create returns its own session even after a successor replaced it;
/// a client that first reads those events after the replacement converges.
#[tokio::test]
async fn a_create_response_survives_its_replacement_and_a_late_reader_converges() {
    let (bench, mut link) = Bench::ws_initialized(json!({
        "protocolVersion": 1,
        "agentCapabilities": {},
        "authMethods": [{"id": "agent-login", "name": "Login"}],
    }))
    .await;
    let mut watch = bench.watch(&scopes(&["global"])).await;
    watch
        .until("the control stream aligns", |watch| watch.has("global"))
        .await;
    let mut late = bench.watch(&scopes(&["global"])).await;
    late.until("the late reader aligns", |watch| watch.has("global"))
        .await;
    let none = late.live("global")["op"]["id"].clone();

    let mut held = bench.hold();
    let created = bench.spawn_op("global", &none, "create", json!({"cwd": CWD}));
    let create = held.admitted().await;
    let request = link.request("session/new").await;
    watch.until_op("global", &create, "running").await;
    link.reply(&request, json!({"sessionId": "created"})).await;
    watch.until_op("global", &create, "completed").await;

    let (status, body) = bench
        .op(
            "global",
            &create,
            "authenticate",
            json!({"methodId": "agent-login"}),
        )
        .await;
    let successor = assert_accepted(status, &body, "the create's successor");
    let authenticate = link.request("authenticate").await;
    watch.until_op("global", &successor, "running").await;
    assert_ne!(successor, create);
    assert_eq!(
        late.live("global")["op"]["id"],
        none,
        "the late reader has consumed neither the create nor its successor"
    );
    assert!(!created.is_finished(), "the create response is still held");

    held.resume();
    let (status, body) = created.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["op"], create, "the response names its own operation");
    assert_eq!(body["result"], json!({"sessionId": "created"}));
    assert!(watch.live("global")["sessions"].get("created").is_some());

    link.reply(&authenticate, json!({})).await;
    watch.until_op("global", &successor, "completed").await;
    late.until_op("global", &successor, "completed").await;
    assert_eq!(late.live("global"), watch.live("global"));
    watch.quiet(QUIET).await;
    late.quiet(QUIET).await;
    watch.converged(&bench).await;
    late.converged(&bench).await;
}

/// The Agent's messages before the `session/new` response are the new
/// session's history (#8): a read and every subscription get them as one turn
/// with an empty prompt, an image among them as an attachment reference the
/// session serves, and the controls among them settle as before.
#[tokio::test]
async fn a_new_sessions_early_messages_are_its_log_for_reads_and_every_subscription() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    let expect = bench.expect_of("global").await;
    let created = bench.spawn_op("global", &expect, "create", json!({"cwd": CWD}));
    let new = link.request("session/new").await;
    let picture = image("d2VsY29tZQ==");
    let early = [
        json!({"sessionUpdate": "agent_message_chunk", "messageId": "welcome", "content": text("Session ")}),
        json!({"sessionUpdate": "session_info_update", "title": "Welcome"}),
        json!({"sessionUpdate": "agent_message_chunk", "messageId": "welcome", "content": text("initialized.")}),
        json!({"sessionUpdate": "agent_message_chunk", "messageId": "welcome", "content": picture.clone()}),
    ];
    for update in &early {
        link.update("s", update.clone()).await;
    }
    link.reply(&new, json!({"sessionId": "s"})).await;
    let (status, body) = created.await.unwrap();
    assert_eq!(status, StatusCode::OK, "create: {body}");
    assert_eq!(body["result"], json!({"sessionId": "s"}), "{body}");

    let expected = crate::protocol::stream_check::log_values(
        crate::protocol::canon::TurnLog::from_replay(&early).entries(),
    );
    let document = bench.document("session/s").await;
    assert_eq!(
        document["log"],
        json!(expected),
        "the early messages are the log, with the canon's hash"
    );
    let reply = &document["log"][0]["turn"]["reply"];
    assert_eq!(document["log"][0]["turn"]["prompt"], json!([]));
    // Adjacent plain text blocks merge (protocol §3.2).
    assert_eq!(reply[0], text("Session initialized."));
    assert_eq!(reply[1]["type"], "attachment", "{reply}");
    assert_eq!(reply[1]["block"], "image", "{reply}");
    assert_eq!(reply.as_array().unwrap().len(), 2, "{reply}");
    assert_eq!(document["live"]["controls"]["info"]["title"], "Welcome");
    let (status, served) = bench
        .get(&format!(
            "/api/v2/sessions/s/attachments/{}",
            reply[1]["id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{served}");
    assert_eq!(served, picture, "the reference names the early image");

    let mut first = bench.watch(&scopes(&["session/s"])).await;
    first
        .until("the first subscription aligns", |watch| {
            watch.has("session/s")
        })
        .await;
    assert_eq!(first.log("session/s"), expected);
    let mut later = bench.watch(&scopes(&["session/s"])).await;
    later
        .until("a later subscription aligns", |watch| {
            watch.has("session/s")
        })
        .await;
    assert_eq!(later.log("session/s"), expected);
    first.converged(&bench).await;
    later.converged(&bench).await;
}

/// Agent EOF ends admitted writes with their own errors, even when the
/// fork's source already shows its next load or has left the published scopes.
#[tokio::test]
async fn admitted_create_fork_and_list_keep_their_errors_when_the_agent_disconnects() {
    for loadable in [false, true] {
        let (bench, mut link) = Bench::ws(json!({
            "loadSession": loadable,
            "sessionCapabilities": {"fork": {}, "list": {}},
        }))
        .await;
        bench.create(&mut link, "source", json!({})).await;
        let mut watch = bench
            .watch(&scopes(&["global", "catalog", "session/source"]))
            .await;
        watch
            .until("the alignments", |watch| {
                watch.has("global") && watch.has("catalog") && watch.has("session/source")
            })
            .await;

        let mut pending = Vec::new();
        for (scope, kind, method, args) in [
            ("global", "create", "session/new", json!({"cwd": CWD})),
            ("session/source", "fork", "session/fork", json!({})),
            ("catalog", "list", "session/list", json!({})),
        ] {
            let expect = watch.live(scope)["op"]["id"].clone();
            let mut held = bench.hold();
            let response = bench.spawn_op(scope, &expect, kind, args);
            let op = held.admitted().await;
            let request = link.request(method).await;
            if kind == "fork" {
                assert_eq!(request["params"]["sessionId"], "source");
            }
            watch.until_op(scope, &op, "running").await;
            assert!(!response.is_finished(), "the {kind} response is held");
            // No ACP response is sent: the connection's end must settle it.
            pending.push((kind, op, held, response));
        }
        let fork = pending[1].1.clone();
        if loadable {
            bench.follow_next_generation().await;
        }
        link.close().await;

        // Do not require a separately published failed fork: ending the
        // connection may replace or remove it in the same commit.
        if loadable {
            watch
                .until_live(
                    "session/source",
                    "the source waits for its next load",
                    |live| {
                        live["op"]["kind"] == "load"
                            && live["op"]["status"] == "running"
                            && live["op"]["id"] != fork
                    },
                )
                .await;
            let source = bench.document("session/source").await;
            assert_eq!(source["live"]["op"]["kind"], "load");
            assert_ne!(source["live"]["op"]["id"], fork);
            // The next Agent's initialize is deliberately not answered;
            // none of the old HTTP responses may wait for this new load.
        } else {
            watch
                .until("the source scope ends", |watch| {
                    watch.ended("session/source").is_some()
                })
                .await;
            let end = watch.ended("session/source").unwrap();
            assert_eq!(end["reason"], "load_failed", "{end}");
            let (status, body) = bench.read("session/source").await;
            not_found(status, &body, "the source left the held scopes");
        }

        // Let each response take its result only after the source's old slot
        // is no longer available. This delays the HTTP waiters, not the work.
        for (kind, op, mut held, response) in pending {
            assert!(!response.is_finished(), "the {kind} response is held");
            held.resume();
            let (status, body) = tokio::time::timeout(WAIT, response)
                .await
                .unwrap_or_else(|_| {
                    panic!("{kind} did not finish after Agent EOF; loadable={loadable}")
                })
                .unwrap();
            assert_eq!(
                status,
                StatusCode::OK,
                "{kind}, loadable={loadable}: {body}"
            );
            assert_eq!(
                body["op"], op,
                "{kind} must answer for its admitted operation"
            );
            assert_eq!(body["error"]["code"], "unavailable", "{kind}: {body}");
            assert!(body.get("result").is_none(), "{kind}: {body}");
            assert!(
                body["error"]["message"]
                    .as_str()
                    .is_some_and(|message| !message.is_empty()),
                "{kind}: {body}"
            );
        }
    }
}

/// A dropped HTTP connection drops only the response: the admitted create
/// still runs to its end.
#[tokio::test]
async fn a_dropped_create_response_does_not_withdraw_the_create() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    let mut watch = bench.watch(&scopes(&["global"])).await;
    watch
        .until("the alignment", |watch| watch.has("global"))
        .await;
    let none = bench.expect_of("global").await;
    let mut held = bench.hold();
    let created = bench
        .open_op("global", &none, "create", json!({"cwd": CWD}))
        .await;
    let create = held.admitted().await;
    let request = link.request("session/new").await;
    // The connection is closed before the response may take the result and
    // before the Agent answers.
    created.close().await;
    held.resume();
    link.reply(&request, json!({"sessionId": "kept"})).await;
    watch.until_op("global", &create, "completed").await;
    let live = watch.live("global");
    assert_eq!(live["op"]["result"], json!({"sessionId": "kept"}));
    assert!(live["sessions"].get("kept").is_some());
    watch.converged(&bench).await;
}

/// A prompt's attachment references are restored to the blocks they name
/// before `session/prompt`; one unknown reference refuses the whole prompt.
#[tokio::test]
async fn prompt_attachment_references_are_resolved_and_an_unknown_one_is_refused() {
    let (bench, mut link) = Bench::ws(json!({"promptCapabilities": {"image": true}})).await;
    bench.create(&mut link, "s", json!({})).await;
    let mut watch = bench.watch(&scopes(&["session/s"])).await;
    watch
        .until("the alignment", |watch| watch.has("session/s"))
        .await;
    let open = bench.expect_of("session/s").await;
    let picture = image("cGljdHVyZQ==");
    let (status, body) = bench
        .op(
            "session/s",
            &open,
            "prompt",
            json!({"prompt": [text("look"), picture.clone()]}),
        )
        .await;
    let first = assert_accepted(status, &body, "a prompt with an image");
    let request = link.request("session/prompt").await;
    assert_eq!(request["params"]["prompt"], json!([text("look"), picture]));
    link.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    watch.until_op("session/s", &first, "completed").await;
    let reference = watch.live("session/s")["op"]["input"]["prompt"][1].clone();
    assert_eq!(reference["type"], "attachment", "{reference}");
    assert_eq!(reference["block"], "image");

    let (status, body) = bench
        .op(
            "session/s",
            &first,
            "prompt",
            json!({"prompt": [text("again"), reference.clone()]}),
        )
        .await;
    let second = assert_accepted(status, &body, "a prompt that references the image");
    let request = link.request("session/prompt").await;
    assert_eq!(
        request["params"]["prompt"],
        json!([text("again"), picture]),
        "the reference is restored to its block"
    );
    link.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    watch
        .until_live("session/s", "the second prompt completes", |live| {
            live["op"]["id"] == second && live["op"]["status"] == "completed"
        })
        .await;
    assert_eq!(
        watch.live("session/s")["op"]["input"]["prompt"][1],
        reference,
        "the input keeps the normalized reference"
    );

    let before = bench.document("session/s").await;
    let mut unknown = reference;
    unknown["id"] = json!("0".repeat(64));
    let (status, body) = bench
        .op(
            "session/s",
            &second,
            "prompt",
            json!({"prompt": [text("valid"), unknown]}),
        )
        .await;
    invalid(status, &body, "a prompt with an unknown attachment");
    link.quiet(QUIET).await;
    assert_eq!(bench.document("session/s").await, before);
    watch.converged(&bench).await;
}

/// The setup response replaces early config options for both the published
/// controls and the ACP execution of a later, legally admitted write (§4.5).
#[tokio::test]
async fn review_setup_config_options_are_used_for_admission_and_execution() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    let early = json!([{
        "type": "boolean", "id": "only-early", "name": "Early", "currentValue": false,
    }]);
    let response = json!([{
        "type": "boolean", "id": "from-response", "name": "Response", "currentValue": false,
    }]);
    let expect = bench.expect_of("global").await;
    let created = bench.spawn_op("global", &expect, "create", json!({"cwd": CWD}));
    let new = link.request("session/new").await;
    // These two messages use the same ordered ACP connection. The option
    // offered before the response must not override the response's options.
    link.update(
        "s",
        json!({"sessionUpdate": "config_option_update", "configOptions": early}),
    )
    .await;
    link.reply(&new, json!({"sessionId": "s", "configOptions": response}))
        .await;
    let (status, body) = created.await.unwrap();
    assert_eq!(status, StatusCode::OK, "create: {body}");
    assert_eq!(body["result"], json!({"sessionId": "s"}), "{body}");

    let mut watch = bench.watch(&scopes(&["session/s"])).await;
    watch
        .until("the session alignment", |watch| watch.has("session/s"))
        .await;
    let live = watch.live("session/s");
    assert_eq!(live["controls"]["configOptions"], response);
    let (status, body) = bench
        .op(
            "session/s",
            &live["op"]["id"],
            "set_config",
            json!({"configId": "from-response", "value": true}),
        )
        .await;
    let op = assert_accepted(status, &body, "the option offered by the setup response");
    // A 202 alone is insufficient: execution must use the same options as
    // admission and actually send the accepted write to the Agent.
    let request = link.request("session/set_config_option").await;
    assert_eq!(request["params"]["sessionId"], "s");
    assert_eq!(request["params"]["configId"], "from-response");
    assert_eq!(request["params"]["value"], true);
    let confirmed = json!([{
        "type": "boolean", "id": "from-response", "name": "Response", "currentValue": true,
    }]);
    link.reply(&request, json!({"configOptions": confirmed}))
        .await;
    watch.until_op("session/s", &op, "completed").await;
    assert_eq!(
        watch.live("session/s")["controls"]["configOptions"],
        confirmed
    );
    watch.converged(&bench).await;
}

/// A successful `set_mode` or `set_config` folds its acknowledgement into
/// `controls` in the event that ends it; a failed one changes nothing
/// (acceptance 18).
#[tokio::test]
async fn control_acknowledgements_fold_into_the_ending_event_and_failures_change_nothing() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create(&mut link, "s", controls()).await;
    let mut watch = bench.watch(&scopes(&["session/s"])).await;
    watch
        .until("the alignment", |watch| watch.has("session/s"))
        .await;
    let open = bench.expect_of("session/s").await;

    let (status, body) = bench
        .op(
            "session/s",
            &open,
            "set_config",
            json!({"configId": "verbose", "value": true}),
        )
        .await;
    let config = assert_accepted(status, &body, "set_config");
    let request = link.request("session/set_config_option").await;
    link.reply(&request, json!({"configOptions": verbose(true)}))
        .await;
    watch.until_op("session/s", &config, "completed").await;
    assert_eq!(
        watch.live("session/s")["controls"]["configOptions"],
        verbose(true)
    );
    let end = watch.events.last().unwrap().clone();
    let paths = patch_paths(&end);
    assert!(
        paths.iter().any(|path| path.starts_with("/controls"))
            && paths.iter().any(|path| path.starts_with("/op")),
        "the acknowledgement and the end are one event: {end}"
    );

    let (status, body) = bench
        .op(
            "session/s",
            &config,
            "set_config",
            json!({"configId": "verbose", "value": false}),
        )
        .await;
    let refused = assert_accepted(status, &body, "a set_config the Agent refuses");
    let request = link.request("session/set_config_option").await;
    link.refuse(&request).await;
    watch.until_op("session/s", &refused, "failed").await;
    let live = watch.live("session/s");
    assert_eq!(live["op"]["error"]["code"], "agent_error");
    assert_eq!(
        live["controls"]["configOptions"],
        verbose(true),
        "a failure folds nothing"
    );

    let (status, body) = bench
        .op("session/s", &refused, "set_mode", json!({"modeId": "plan"}))
        .await;
    let mode = assert_accepted(status, &body, "set_mode");
    let request = link.request("session/set_mode").await;
    link.reply(&request, json!({})).await;
    watch.until_op("session/s", &mode, "completed").await;
    assert_eq!(
        watch.live("session/s")["controls"]["modes"]["currentModeId"],
        "plan"
    );
    let end = watch.events.last().unwrap().clone();
    let paths = patch_paths(&end);
    assert!(
        paths.iter().any(|path| path.starts_with("/controls"))
            && paths.iter().any(|path| path.starts_with("/op")),
        "the acknowledgement and the end are one event: {end}"
    );

    let (status, body) = bench
        .op("session/s", &mode, "set_mode", json!({"modeId": "ask"}))
        .await;
    let refused = assert_accepted(status, &body, "a set_mode the Agent refuses");
    let request = link.request("session/set_mode").await;
    link.refuse(&request).await;
    watch.until_op("session/s", &refused, "failed").await;
    assert_eq!(
        watch.live("session/s")["controls"]["modes"]["currentModeId"],
        "plan"
    );
    watch.converged(&bench).await;
}

/// A list answers its page with the catalog revision; a stale revision is
/// refused before the Agent is asked.
#[tokio::test]
async fn a_list_returns_its_page_and_a_stale_revision_is_catalog_changed() {
    let (bench, mut link) = Bench::ws(json!({"sessionCapabilities": {"list": {}}})).await;
    let mut watch = bench.watch(&scopes(&["catalog"])).await;
    watch
        .until("the alignment", |watch| watch.has("catalog"))
        .await;
    let old = watch.live("catalog")["revision"].clone();
    bench.create(&mut link, "s", json!({})).await;
    watch
        .until_live("catalog", "the create advances the revision", |live| {
            live["revision"] != old
        })
        .await;
    let revision = watch.live("catalog")["revision"].clone();
    let none = bench.expect_of("catalog").await;

    let before = bench.document("catalog").await;
    let (status, body) = bench
        .op("catalog", &none, "list", json!({"revision": old}))
        .await;
    assert_refused(
        status,
        &body,
        StatusCode::CONFLICT,
        "catalog_changed",
        "a list on a stale revision",
    );
    link.quiet(QUIET).await;
    assert_eq!(bench.document("catalog").await, before);

    let listed = bench.spawn_op(
        "catalog",
        &none,
        "list",
        json!({"revision": revision, "cursor": "page-2"}),
    );
    let request = link.request("session/list").await;
    assert_eq!(request["params"]["cursor"], "page-2");
    let sessions = json!([{"sessionId": "saved", "cwd": CWD}]);
    link.reply(
        &request,
        json!({"sessions": sessions, "nextCursor": "page-3"}),
    )
    .await;
    let (status, body) = listed.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["revision"], revision);
    assert_eq!(body["result"]["sessions"], sessions);
    assert_eq!(body["result"]["nextCursor"], "page-3");
    watch.until_op("catalog", &body["op"], "completed").await;
    assert_eq!(watch.live("catalog")["op"]["result"], body["result"]);
    watch.converged(&bench).await;
}
