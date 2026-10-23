//! Running admitted operations (protocol §6.2, §6.3; acceptance 2, 9, 18): each kind takes its
//! ACP path; every operation is answered `202` and its result is in the document, which
//! `spawn_op` waits for in `create`, `fork` and `list`.
use super::*;

#[tokio::test]
async fn a_prompt_answers_202_with_its_op_and_commits_its_result() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create_with(&mut link, "s", json!({})).await;
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
    assert_eq!(watch.entries("session/s").len(), 2, "the prompt and the reply");
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
    let created = bench.create_with(&mut link, "s", json!({})).await;
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
    let (_, target) = bench.read_document("session/forked").await;
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






/// A prompt's attachment references are restored to the blocks they name
/// before `session/prompt`; one unknown reference refuses the whole prompt.
#[tokio::test]
async fn prompt_attachment_references_are_resolved_and_an_unknown_one_is_refused() {
    let (bench, mut link) = Bench::ws(json!({"promptCapabilities": {"image": true}})).await;
    bench.create_with(&mut link, "s", json!({})).await;
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
    let set = bench.spawn_action(
        "s",
        "set_config",
        json!({"configId": "from-response", "value": true}),
    );
    // An admitted action must reach the Agent with the same options admission used.
    let request = link.request("session/set_config_option").await;
    assert_eq!(request["params"]["sessionId"], "s");
    assert_eq!(request["params"]["configId"], "from-response");
    assert_eq!(request["params"]["value"], true);
    let confirmed = json!([{
        "type": "boolean", "id": "from-response", "name": "Response", "currentValue": true,
    }]);
    link.reply(&request, json!({"configOptions": confirmed}))
        .await;
    let (status, body) = set.await.unwrap();
    assert_eq!((status, &body), (StatusCode::OK, &json!({})));
    watch
        .until_live("session/s", "the options are confirmed", |live| {
            live["controls"]["configOptions"] == confirmed
        })
        .await;
    watch.converged(&bench).await;
}

/// A successful `set_mode` or `set_config` action folds its acknowledgement into `controls` in one
/// event and touches no operation; a failed one changes nothing (acceptance 18).
#[tokio::test]
async fn control_acknowledgements_fold_into_one_event_and_failures_change_nothing() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create_with(&mut link, "s", controls()).await;
    let mut watch = bench.watch(&scopes(&["session/s"])).await;
    watch
        .until("the alignment", |watch| watch.has("session/s"))
        .await;
    let op = watch.live("session/s")["op"].clone();

    let set = bench.spawn_action("s", "set_config", json!({"configId": "verbose", "value": true}));
    let request = link.request("session/set_config_option").await;
    link.reply(&request, json!({"configOptions": verbose(true)}))
        .await;
    assert_eq!(set.await.unwrap(), (StatusCode::OK, json!({})));
    watch
        .until_live("session/s", "the acknowledgement folds", |live| {
            live["controls"]["configOptions"] == verbose(true)
        })
        .await;
    let event = watch.events.last().unwrap().clone();
    assert!(
        patch_paths(&event).iter().all(|path| path.starts_with("/live/controls")),
        "the acknowledgement is one change of controls and of nothing else: {event}"
    );

    let set = bench.spawn_action("s", "set_config", json!({"configId": "verbose", "value": false}));
    let request = link.request("session/set_config_option").await;
    link.refuse(&request).await;
    let (status, body) = set.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["error"]["code"], "agent_error", "{body}");
    assert_eq!(
        watch.live("session/s")["controls"]["configOptions"],
        verbose(true),
        "a failure folds nothing"
    );

    let set = bench.spawn_action("s", "set_mode", json!({"modeId": "plan"}));
    let request = link.request("session/set_mode").await;
    link.reply(&request, json!({})).await;
    assert_eq!(set.await.unwrap(), (StatusCode::OK, json!({})));
    watch
        .until_live("session/s", "the mode folds", |live| {
            live["controls"]["modes"]["currentModeId"] == "plan"
        })
        .await;

    let set = bench.spawn_action("s", "set_mode", json!({"modeId": "ask"}));
    let request = link.request("session/set_mode").await;
    link.refuse(&request).await;
    let (status, body) = set.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["error"]["code"], "agent_error", "{body}");
    assert_eq!(
        watch.live("session/s")["controls"]["modes"]["currentModeId"],
        "plan"
    );
    assert_eq!(watch.live("session/s")["op"], op, "no control touched the operation");
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
    bench.create_with(&mut link, "s", json!({})).await;
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
        json!({"sessions": sessions, "nextCursor": "page-3", "_meta": {"total": 3}}),
    )
    .await;
    let (status, body) = listed.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["revision"], revision);
    assert_eq!(body["result"]["_meta"], json!({"total": 3}), "the Agent's page _meta");
    assert_eq!(body["result"]["sessions"], sessions);
    assert_eq!(body["result"]["nextCursor"], "page-3");
    watch.until_op("catalog", &body["op"], "completed").await;
    assert_eq!(watch.live("catalog")["op"]["result"], body["result"]);
    watch.converged(&bench).await;
}

/// The Agent's messages before the `session/new` response are the new session's first entries
/// (#8): a read and every subscription get them, an image among them as an attachment
/// reference the session serves, and the controls among them settle as before.
#[tokio::test]
async fn a_new_sessions_early_messages_are_its_entries_for_reads_and_every_subscription() {
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

    let document = bench.document("session/s").await;
    let entries = document["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "one agent message: {entries:?}");
    assert_eq!(entries[0]["messageId"], "welcome");
    let chunks = entries[0]["chunks"].as_array().unwrap();
    // Adjacent plain text chunks merge; the image is an attachment reference.
    assert_eq!(chunks[0]["content"], text("Session initialized."));
    let reference = &chunks[1]["content"];
    assert_eq!(reference["type"], "attachment", "{reference}");
    assert_eq!(reference["block"], "image", "{reference}");
    assert_eq!(chunks.len(), 2, "{chunks:?}");
    assert_eq!(document["live"]["controls"]["info"]["title"], "Welcome");
    let (status, served) = bench
        .get(&format!(
            "/api/v2/sessions/s/attachments/{}",
            reference["id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{served}");
    assert_eq!(served, picture, "the reference names the early image");

    let mut first = bench.watch(&scopes(&["session/s"])).await;
    first
        .until("the first subscription aligns", |watch| watch.has("session/s"))
        .await;
    let mut later = bench.watch(&scopes(&["session/s"])).await;
    later
        .until("a later subscription aligns", |watch| watch.has("session/s"))
        .await;
    assert_eq!(first.entries("session/s"), document["entries"].as_array().unwrap().clone());
    assert_eq!(later.entries("session/s"), first.entries("session/s"));
    first.converged(&bench).await;
    later.converged(&bench).await;
}

/// Agent EOF ends admitted writes with their own errors, even when the fork's source already
/// shows its next load or has left the published scopes.
#[tokio::test]
async fn admitted_create_fork_and_list_keep_their_errors_when_the_agent_disconnects() {
    // A create and a fork both install a session, one at a time (protocol §6.2): each runs
    // beside a list, which is the catalog's own scope.
    for (loadable, with_fork) in [(false, false), (false, true), (true, false), (true, true)] {
        let (bench, mut link) = Bench::ws(json!({
            "loadSession": loadable,
            "sessionCapabilities": {"fork": {}, "list": {}},
        }))
        .await;
        bench.create_with(&mut link, "source", json!({})).await;
        let mut watch = bench
            .watch(&scopes(&["global", "catalog", "session/source"]))
            .await;
        watch
            .until("the alignments", |watch| {
                watch.has("global") && watch.has("catalog") && watch.has("session/source")
            })
            .await;

        let installing = if with_fork {
            ("session/source", "fork", "session/fork", json!({}))
        } else {
            ("global", "create", "session/new", json!({"cwd": CWD}))
        };
        let mut pending = Vec::new();
        for (scope, kind, method, args) in [installing, ("catalog", "list", "session/list", json!({}))] {
            let expect = watch.live(scope)["op"]["id"].clone();
            let (status, body) = bench.op(scope, &expect, kind, args).await;
            let op = assert_accepted(status, &body, kind);
            let request = link.request(method).await;
            if kind == "fork" {
                assert_eq!(request["params"]["sessionId"], "source");
            }
            watch.until_op(scope, &op, "running").await;
            // No ACP response is sent: the connection's end must settle it.
            pending.push((kind, scope, op));
        }
        let fork = pending
            .iter()
            .find(|(kind, ..)| *kind == "fork")
            .map_or(Value::Null, |(_, _, op)| op.clone());
        link.close().await;

        // The source's slot may be replaced or gone by the time the fork's end would show, so
        // only the operations whose scope stays are read; the source itself shows its next
        // load or ends.
        if loadable {
            watch
                .until_live("session/source", "the source waits for its next load", |live| {
                    live["op"]["kind"] == "load"
                        && live["op"]["status"] == "running"
                        && live["op"]["id"] != fork
                })
                .await;
        } else {
            watch
                .until("the source scope ends", |watch| watch.ended("session/source").is_some())
                .await;
            let end = watch.ended("session/source").unwrap();
            assert_eq!(end["reason"], "load_failed", "{end}");
            let (status, body) = bench.read_document("session/source").await;
            not_found(status, &body, "the source left the held scopes");
        }
        for (kind, scope, op) in pending {
            if kind == "fork" {
                continue;
            }
            watch.until_op(scope, &op, "failed").await;
            let error = watch.live(scope)["op"]["error"].clone();
            assert_eq!(error["code"], "unavailable", "{kind}, loadable={loadable}: {error}");
            assert!(
                error["message"].as_str().is_some_and(|message| !message.is_empty()),
                "{kind}: {error}"
            );
        }
    }
}

/// A create runs to its end whatever the client does with the response: it was answered `202`
/// when it was admitted, and the result comes to the document.
#[tokio::test]
async fn a_create_runs_to_its_end_without_its_response_being_read() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    let mut watch = bench.watch(&scopes(&["global"])).await;
    watch
        .until("the alignment", |watch| watch.has("global"))
        .await;
    let none = bench.expect_of("global").await;
    let (status, body) = bench.op("global", &none, "create", json!({"cwd": CWD})).await;
    let create = assert_accepted(status, &body, "the create");
    let request = link.request("session/new").await;
    link.reply(&request, json!({"sessionId": "kept"})).await;
    watch.until_op("global", &create, "completed").await;
    let live = watch.live("global");
    assert_eq!(live["op"]["result"], json!({"sessionId": "kept"}));
    assert!(live["sessions"].get("kept").is_some());
    watch.converged(&bench).await;
}

/// Audit Q5: a notification the bridge does not know, an extension (`_`-prefixed) one among
/// them, is ignored. It tears down nothing: the connection stays up and the next update of the
/// same Agent still reaches the session.
#[tokio::test]
async fn unknown_agent_notifications_are_ignored_and_the_connection_goes_on() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create_with(&mut link, "s", json!({})).await;
    let mut watch = bench.watch(&scopes(&["global", "session/s"])).await;
    watch
        .until("the alignments", |watch| {
            watch.has("global") && watch.has("session/s")
        })
        .await;

    link.notify("_vendor/event", json!({"anything": [1, 2, 3]})).await;
    link.notify("test/unknown_notification", json!({})).await;
    link.update(
        "s",
        json!({"sessionUpdate": "agent_message_chunk", "messageId": "m", "content": text("still here")}),
    )
    .await;
    watch
        .until("the update after them arrives", |watch| {
            json!(watch.entries("session/s")).to_string().contains("still here")
        })
        .await;
    assert_eq!(watch.live("global")["agent"]["phase"], "ready");
    assert_eq!(bench.accepted(), 1, "no second connection was needed");
    watch.converged(&bench).await;
}
