//! Admission (protocol S5, §6.4; acceptance 6): `expect`, the operation's
//! status and the occupied scopes decide, then capability, arguments and the
//! Agent. A refused write changes no slot and sends nothing to the Agent. Every operation is
//! answered `202` when it is admitted; a held `create`, `fork` or `list` is waited for in the
//! document.
use super::*;

/// Every session write that starts an operation, with valid arguments.
fn session_writes() -> Vec<(&'static str, Value)> {
    vec![
        ("prompt", json!({"prompt": [text("again")]})),
        ("fork", json!({})),
    ]
}

#[tokio::test]
async fn session_writes_need_the_current_ended_op_and_replace_it() {
    let (bench, mut link) = Bench::ws(json!({"sessionCapabilities": {"fork": {}}})).await;
    bench.create_with(&mut link, "s", controls()).await;
    let mut watch = bench.watch(&scopes(&["session/s"])).await;
    watch
        .until("the alignment", |watch| watch.has("session/s"))
        .await;
    let open = watch.live("session/s")["op"]["id"].clone();
    assert_eq!(watch.live("session/s")["op"]["kind"], "open");

    let (status, body) = bench
        .op(
            "session/s",
            &open,
            "prompt",
            json!({"prompt": [text("first")]}),
        )
        .await;
    let first = assert_accepted(status, &body, "a prompt on the ended open");
    let prompt = link.request("session/prompt").await;
    assert_eq!(prompt["params"]["sessionId"], "s");
    watch
        .until_live("session/s", "the prompt runs", |live| {
            live["op"]["id"] == first
        })
        .await;
    assert_eq!(watch.live("session/s")["op"]["status"], "running");

    // While it runs, every start is refused: on its ID as not allowed, on
    // the replaced ID as stale.
    let before = bench.document("session/s").await;
    for (kind, args) in session_writes() {
        let (status, body) = bench.op("session/s", &first, kind, args.clone()).await;
        not_allowed(status, &body, &format!("{kind} while the prompt runs"));
        let (status, body) = bench.op("session/s", &open, kind, args).await;
        stale(status, &body, &format!("{kind} on the replaced open"));
    }
    link.quiet(QUIET).await;
    watch.quiet(QUIET).await;
    assert_eq!(
        bench.document("session/s").await,
        before,
        "refused writes change nothing"
    );
    link.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    watch.until_op("session/s", &first, "completed").await;

    // Each kind is admitted on the ended operation before it, which is then
    // stale.
    let mut expect = first;
    let (status, body) = bench
        .op(
            "session/s",
            &expect,
            "prompt",
            json!({"prompt": [text("second")]}),
        )
        .await;
    let second = assert_accepted(status, &body, "a prompt on the ended prompt");
    let request = link.request("session/prompt").await;
    link.reply(&request, json!({"stopReason": "end_turn"})).await;
    watch.until_op("session/s", &second, "completed").await;
    let (status, body) = bench
        .op(
            "session/s",
            &expect,
            "prompt",
            json!({"prompt": [text("again")]}),
        )
        .await;
    stale(status, &body, "a prompt on the replaced prompt");
    expect = second;

    let forked = bench.spawn_op("session/s", &expect, "fork", json!({}));
    let request = link.request("session/fork").await;
    link.reply(&request, json!({"sessionId": "forked"})).await;
    let (status, body) = forked.await.unwrap();
    assert_eq!(status, StatusCode::OK, "fork: {body}");
    assert_eq!(body["result"], json!({"sessionId": "forked"}));
    let fork = body["op"].clone();
    watch
        .until_live("session/s", "the fork completes", |live| {
            live["op"]["id"] == fork && live["op"]["status"] == "completed"
        })
        .await;
    let (status, body) = bench.op("session/s", &expect, "fork", json!({})).await;
    stale(status, &body, "fork on the replaced prompt");
    expect = fork;

    let (status, body) = bench
        .op(
            "session/s",
            &expect,
            "prompt",
            json!({"prompt": [text("last")]}),
        )
        .await;
    let last = assert_accepted(status, &body, "a prompt on the ended fork");
    let request = link.request("session/prompt").await;
    link.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    watch.until_op("session/s", &last, "completed").await;
    watch.converged(&bench).await;
}

#[tokio::test]
async fn global_writes_need_the_current_ended_op_and_replace_it() {
    let (bench, mut link) = Bench::ws_initialized(json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "auth": {"logout": {}},
            "sessionCapabilities": {"close": {}, "delete": {}},
        },
        "authMethods": [{"id": "agent-login", "name": "Login"}],
    }))
    .await;
    let mut watch = bench.watch(&scopes(&["global"])).await;
    watch
        .until("the alignment", |watch| watch.has("global"))
        .await;
    let none = watch.live("global")["op"]["id"].clone();
    assert_eq!(watch.live("global")["op"]["kind"], "none");
    let created = bench.create_with(&mut link, "victim", json!({})).await;
    let (status, body) = bench
        .op("global", &none, "create", json!({"cwd": CWD}))
        .await;
    stale(status, &body, "a create on the replaced none");

    let (status, body) = bench
        .op(
            "global",
            &created,
            "authenticate",
            json!({"methodId": "agent-login"}),
        )
        .await;
    let auth = assert_accepted(status, &body, "authenticate");
    let request = link.request("authenticate").await;
    assert_eq!(request["params"]["methodId"], "agent-login");
    watch
        .until_live("global", "the authentication runs", |live| {
            live["op"]["id"] == auth
        })
        .await;
    let before = bench.document("global").await;
    for (kind, args) in [
        ("create", json!({"cwd": CWD})),
        ("delete", json!({"sessionId": "victim"})),
        ("authenticate", json!({"methodId": "agent-login"})),
        ("logout", json!({})),
        (
            "terminal_login",
            json!({"methodId": "agent-login", "cols": 80, "rows": 24}),
        ),
    ] {
        let (status, body) = bench.op("global", &auth, kind, args.clone()).await;
        not_allowed(status, &body, &format!("{kind} while authenticate runs"));
        let (status, body) = bench.op("global", &created, kind, args).await;
        stale(status, &body, &format!("{kind} on the replaced create"));
    }
    link.quiet(QUIET).await;
    assert_eq!(bench.document("global").await, before);
    link.reply(&request, json!({})).await;
    watch.until_op("global", &auth, "completed").await;

    let (status, body) = bench.op("global", &auth, "logout", json!({})).await;
    let logout = assert_accepted(status, &body, "logout");
    let request = link.request("logout").await;
    link.reply(&request, json!({})).await;
    watch
        .until_live("global", "the logout completes", |live| {
            live["op"]["id"] == logout && live["op"]["status"] == "completed"
        })
        .await;
    let (status, body) = bench.op("global", &auth, "logout", json!({})).await;
    stale(status, &body, "logout on the replaced authenticate");

    let (status, body) = bench
        .op("global", &logout, "delete", json!({"sessionId": "victim"}))
        .await;
    let delete = assert_accepted(status, &body, "delete");
    let close = link.request("session/close").await;
    link.reply(&close, json!({})).await;
    let request = link.request("session/delete").await;
    assert_eq!(request["params"]["sessionId"], "victim");
    link.reply(&request, json!({})).await;
    watch
        .until_live("global", "the delete completes", |live| {
            live["op"]["id"] == delete && live["op"]["status"] == "completed"
        })
        .await;
    assert!(watch.live("global")["sessions"].get("victim").is_none());
    let (status, body) = bench
        .op("global", &logout, "delete", json!({"sessionId": "victim"}))
        .await;
    stale(status, &body, "delete on the replaced logout");
    link.quiet(QUIET).await;
    watch.converged(&bench).await;
}

/// A fork excludes writes in `global` while it runs (protocol §6.2, memory model §9): a
/// session being installed has one installing operation, so what it receives
/// before its ID is known has one owner.
fn fork_capabilities() -> Value {
    json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "auth": {"logout": {}},
            "sessionCapabilities": {"close": {}, "delete": {}, "fork": {}},
        },
        "authMethods": [{"id": "agent-login", "name": "Login"}],
    })
}

#[tokio::test]
async fn forks_of_different_sessions_cannot_install_together() {
    let (bench, mut link) = Bench::ws_initialized(fork_capabilities()).await;
    bench.create_with(&mut link, "s", json!({})).await;
    bench.create_with(&mut link, "t", json!({})).await;
    let source = bench.expect_of("session/s").await;
    let other = bench.expect_of("session/t").await;
    let forked = bench.spawn_op("session/s", &source, "fork", json!({}));
    let request = link.request("session/fork").await;
    let before = bench.document("session/t").await;
    let (status, body) = bench.op("session/t", &other, "fork", json!({})).await;
    not_allowed(status, &body, "a second fork while one installation is running");
    assert_eq!(bench.document("session/t").await, before);
    link.quiet(QUIET).await;
    link.reply(&request, json!({"sessionId": "forked-s"})).await;
    assert_eq!(forked.await.unwrap().0, StatusCode::OK);
    let next = bench.spawn_op("session/t", &other, "fork", json!({}));
    let request = link.request("session/fork").await;
    link.reply(&request, json!({"sessionId": "forked-t"})).await;
    assert_eq!(next.await.unwrap().0, StatusCode::OK);
}

#[tokio::test]
async fn a_fork_is_refused_while_a_global_operation_runs() {
    let (bench, mut link) = Bench::ws_initialized(fork_capabilities()).await;
    let created = bench.create_with(&mut link, "s", json!({})).await;
    let open = bench.expect_of("session/s").await;
    let mut watch = bench.watch(&scopes(&["global"])).await;
    watch
        .until("the alignment", |watch| watch.has("global"))
        .await;

    let (status, body) = bench
        .op(
            "global",
            &created,
            "authenticate",
            json!({"methodId": "agent-login"}),
        )
        .await;
    let auth = assert_accepted(status, &body, "authenticate");
    let request = link.request("authenticate").await;
    watch
        .until_live("global", "the authentication runs", |live| {
            live["op"]["id"] == auth
        })
        .await;

    let before = bench.document("session/s").await;
    let (status, body) = bench.op("session/s", &open, "fork", json!({})).await;
    not_allowed(status, &body, "a fork while global is not free");
    link.quiet(QUIET).await;
    assert_eq!(
        bench.document("session/s").await,
        before,
        "a refused fork changes nothing"
    );

    link.reply(&request, json!({})).await;
    watch.until_op("global", &auth, "completed").await;
    let forked = bench.spawn_op("session/s", &open, "fork", json!({}));
    let request = link.request("session/fork").await;
    link.reply(&request, json!({"sessionId": "forked"})).await;
    let (status, body) = forked.await.unwrap();
    assert_eq!(status, StatusCode::OK, "a fork once global is free: {body}");
}

#[tokio::test]
async fn a_global_write_is_refused_while_a_fork_runs() {
    let (bench, mut link) = Bench::ws_initialized(fork_capabilities()).await;
    let created = bench.create_with(&mut link, "s", json!({})).await;
    let open = bench.expect_of("session/s").await;

    let forked = bench.spawn_op("session/s", &open, "fork", json!({}));
    let request = link.request("session/fork").await;
    let before = bench.document("global").await;
    for (kind, args) in [
        ("create", json!({"cwd": CWD})),
        ("delete", json!({"sessionId": "s"})),
        ("authenticate", json!({"methodId": "agent-login"})),
        ("logout", json!({})),
        (
            "terminal_login",
            json!({"methodId": "agent-login", "cols": 80, "rows": 24}),
        ),
    ] {
        let (status, body) = bench.op("global", &created, kind, args).await;
        not_allowed(status, &body, &format!("{kind} while a fork installs"));
    }
    link.quiet(QUIET).await;
    assert_eq!(
        bench.document("global").await,
        before,
        "refused writes change nothing"
    );

    link.reply(&request, json!({"sessionId": "forked"})).await;
    let (status, body) = forked.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    // The fork has ended: global is free again and a create is admitted.
    bench.create_with(&mut link, "next", json!({})).await;
}

/// 128 random bits in unpadded base64url (protocol §6.5): 22 characters.
const CLIENT_ID: &str = "Zm9yLXRoZS1jbGllbnQtMQ";
const OTHER_ID: &str = "Zm9yLXRoZS1jbGllbnQtMg";

#[tokio::test]
async fn a_write_with_a_client_id_is_installed_under_that_id() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create_with(&mut link, "s", json!({})).await;
    let open = bench.expect_of("session/s").await;
    let (status, body) = bench
        .op_with_id("session/s", &open, CLIENT_ID, "prompt", json!({"prompt": [text("hi")]}))
        .await;
    assert_eq!(assert_accepted(status, &body, "a prompt with an ID"), CLIENT_ID);
    let request = link.request("session/prompt").await;
    assert_eq!(bench.expect_of("session/s").await, CLIENT_ID);
    link.reply(&request, json!({"stopReason": "end_turn"})).await;
}

#[tokio::test]
async fn a_malformed_client_id_is_refused_before_anything_happens() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create_with(&mut link, "s", json!({})).await;
    let open = bench.expect_of("session/s").await;
    let before = bench.document("session/s").await;
    for id in ["", "short", "Zm9yLXRoZS1jbGllbnQtMQ==", "has spaces in it 22chr", "Zm9yLXRoZS1jbGllbnQtMQx"] {
        let (status, body) = bench
            .op_with_id("session/s", &open, id, "prompt", json!({"prompt": [text("hi")]}))
            .await;
        invalid(status, &body, &format!("the ID {id:?}"));
    }
    link.quiet(QUIET).await;
    assert_eq!(bench.document("session/s").await, before);
}

#[tokio::test]
async fn a_repeated_write_is_accepted_again_and_does_nothing_more() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create_with(&mut link, "s", json!({})).await;
    let open = bench.expect_of("session/s").await;
    let write = json!({"prompt": [text("once")]});
    let (status, body) = bench
        .op_with_id("session/s", &open, CLIENT_ID, "prompt", write.clone())
        .await;
    assert_accepted(status, &body, "the write");
    let request = link.request("session/prompt").await;

    // The answer was lost: the client sends the same write again while it
    // runs, and again once it has ended.
    let (status, body) = bench
        .op_with_id("session/s", &open, CLIENT_ID, "prompt", write.clone())
        .await;
    assert_eq!(assert_accepted(status, &body, "a repeat while it runs"), CLIENT_ID);
    link.reply(&request, json!({"stopReason": "end_turn"})).await;
    let mut watch = bench.watch(&scopes(&["session/s"])).await;
    watch
        .until_op("session/s", &json!(CLIENT_ID), "completed")
        .await;
    let (status, body) = bench
        .op_with_id("session/s", &open, CLIENT_ID, "prompt", write.clone())
        .await;
    assert_eq!(assert_accepted(status, &body, "a repeat after it ended"), CLIENT_ID);
    link.quiet(QUIET).await;
}

#[tokio::test]
async fn a_write_that_is_not_a_repeat_is_judged_by_expect_and_status_as_before() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create_with(&mut link, "s", json!({})).await;
    let open = bench.expect_of("session/s").await;
    let (status, body) = bench
        .op_with_id("session/s", &open, CLIENT_ID, "prompt", json!({"prompt": [text("a")]}))
        .await;
    assert_accepted(status, &body, "the first write");
    let request = link.request("session/prompt").await;

    // Another ID on the replaced `expect` is stale; on the current one the
    // running operation refuses it.
    let (status, body) = bench
        .op_with_id("session/s", &open, OTHER_ID, "prompt", json!({"prompt": [text("b")]}))
        .await;
    stale(status, &body, "another ID on the replaced expect");
    let (status, body) = bench
        .op_with_id("session/s", &json!(CLIENT_ID), OTHER_ID, "prompt", json!({"prompt": [text("b")]}))
        .await;
    not_allowed(status, &body, "another ID while the first runs");
    link.reply(&request, json!({"stopReason": "end_turn"})).await;
}

#[tokio::test]
async fn a_list_needs_the_current_ended_catalog_op() {
    let (bench, mut link) = Bench::ws(json!({"sessionCapabilities": {"list": {}}})).await;
    let mut watch = bench.watch(&scopes(&["catalog"])).await;
    watch
        .until("the alignment", |watch| watch.has("catalog"))
        .await;
    let none = watch.live("catalog")["op"]["id"].clone();
    let listed = bench.spawn_op("catalog", &none, "list", json!({}));
    let request = link.request("session/list").await;
    watch
        .until_live("catalog", "the list runs", |live| {
            live["op"]["kind"] == "list" && live["op"]["status"] == "running"
        })
        .await;
    let list = watch.live("catalog")["op"]["id"].clone();
    let (status, body) = bench.op("catalog", &list, "list", json!({})).await;
    not_allowed(status, &body, "a list while one runs");
    let (status, body) = bench.op("catalog", &none, "list", json!({})).await;
    stale(status, &body, "a list on the replaced none");
    link.quiet(QUIET).await;
    link.reply(&request, json!({"sessions": [], "nextCursor": null}))
        .await;
    let (status, body) = listed.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["op"], list);
    watch.until_op("catalog", &list, "completed").await;

    let listed = bench.spawn_op("catalog", &list, "list", json!({}));
    let request = link.request("session/list").await;
    link.reply(&request, json!({"sessions": [], "nextCursor": null}))
        .await;
    let (status, body) = listed.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    let next = body["op"].clone();
    assert_ne!(next, list);
    watch.until_op("catalog", &next, "completed").await;
    let (status, body) = bench.op("catalog", &list, "list", json!({})).await;
    stale(status, &body, "a list on the replaced list");
    link.quiet(QUIET).await;
    watch.quiet(QUIET).await;
    watch.converged(&bench).await;
}

/// Two writes with the same `expect`: exactly one is admitted; resending it
/// after the operation ended is still stale, and a fork does not run twice.
#[tokio::test]
async fn of_two_writes_with_one_expect_only_one_is_admitted() {
    let (bench, mut link) = Bench::ws(json!({"sessionCapabilities": {"fork": {}}})).await;
    let mut watch = bench.watch(&scopes(&["global"])).await;
    watch
        .until("the alignment", |watch| watch.has("global"))
        .await;

    // Two creates on one expect.
    let none = bench.expect_of("global").await;
    let first = bench.spawn_op("global", &none, "create", json!({"cwd": CWD}));
    let second = bench.spawn_op("global", &none, "create", json!({"cwd": CWD}));
    let new = link.request("session/new").await;
    // The refused one answers while the admitted one waits for its end.
    let first_refused = tokio::time::timeout(WAIT, async {
        loop {
            if first.is_finished() {
                return true;
            }
            if second.is_finished() {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("one of the two creates is refused at once");
    let (refused, admitted) = if first_refused {
        (first, second)
    } else {
        (second, first)
    };
    let (status, body) = refused.await.unwrap();
    stale(status, &body, "the second create on one expect");
    link.quiet(QUIET).await;
    link.reply(&new, json!({"sessionId": "s"})).await;
    let (status, body) = admitted.await.unwrap();
    assert_eq!(status, StatusCode::OK, "the admitted create: {body}");
    watch.until_op("global", &body["op"], "completed").await;
    let (status, body) = bench
        .op("global", &none, "create", json!({"cwd": CWD}))
        .await;
    stale(status, &body, "a resent create after its end");
    link.quiet(QUIET).await;
    assert_eq!(
        watch.live("global")["sessions"].as_object().unwrap().len(),
        1,
        "one session was created"
    );

    // Two prompts on one expect.
    let open = bench.expect_of("session/s").await;
    let (a, b) = tokio::join!(
        bench.op("session/s", &open, "prompt", json!({"prompt": [text("a")]})),
        bench.op("session/s", &open, "prompt", json!({"prompt": [text("b")]})),
    );
    let mut outcomes = [a, b];
    outcomes.sort_by_key(|(status, _)| *status);
    let prompt = assert_accepted(outcomes[0].0, &outcomes[0].1, "one prompt");
    stale(outcomes[1].0, &outcomes[1].1, "the other prompt");
    let request = link.request("session/prompt").await;
    link.quiet(QUIET).await;
    link.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    let mut session = bench.watch(&scopes(&["session/s"])).await;
    session
        .until_live("session/s", "the prompt completes", |live| {
            live["op"]["id"] == prompt && live["op"]["status"] == "completed"
        })
        .await;
    assert_eq!(session.entries("session/s").len(), 1, "one prompt reached the session");

    // A fork resent after its end does not fork again.
    let forked = bench.spawn_op("session/s", &prompt, "fork", json!({}));
    let request = link.request("session/fork").await;
    link.reply(&request, json!({"sessionId": "forked"})).await;
    let (status, body) = forked.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    let fork = body["op"].clone();
    let (status, body) = bench.op("session/s", &prompt, "fork", json!({})).await;
    stale(status, &body, "a resent fork");
    link.quiet(QUIET).await;
    session.until_op("session/s", &fork, "completed").await;
    // The fork's target reaches the global stream after the prompt's and the
    // fork's summaries of `s`.
    watch
        .until_live("global", "the fork's target is listed", |live| {
            live["sessions"].get("forked").is_some()
        })
        .await;
    assert_eq!(
        watch.live("global")["sessions"].as_object().unwrap().len(),
        2,
        "one fork"
    );
    watch.quiet(QUIET).await;
    session.quiet(QUIET).await;
    watch.converged(&bench).await;
    session.converged(&bench).await;
}

/// A definite refusal does not consume `expect`: the same write is admitted
/// once its condition holds.
#[tokio::test]
async fn a_refused_write_can_be_submitted_again_on_the_same_expect() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create_with(&mut link, "s", controls()).await;
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
            json!({"prompt": [text("one")]}),
        )
        .await;
    let first = assert_accepted(status, &body, "the first prompt");
    let request = link.request("session/prompt").await;
    let queued = json!({"prompt": [text("two")]});
    let (status, body) = bench
        .op("session/s", &first, "prompt", queued.clone())
        .await;
    not_allowed(status, &body, "the queued prompt while the first runs");
    link.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    watch.until_op("session/s", &first, "completed").await;
    let (status, body) = bench.op("session/s", &first, "prompt", queued).await;
    let second = assert_accepted(status, &body, "the queued prompt, submitted again");
    let request = link.request("session/prompt").await;
    assert_eq!(request["params"]["prompt"], json!([text("two")]));
    link.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    watch
        .until_live("session/s", "the second prompt completes", |live| {
            live["op"]["id"] == second && live["op"]["status"] == "completed"
        })
        .await;

    // An argument refusal does not consume it either.
    let (status, body) = bench.op("session/s", &second, "prompt", json!({})).await;
    invalid(status, &body, "a prompt without blocks");
    let (status, body) = bench
        .op(
            "session/s",
            &second,
            "prompt",
            json!({"prompt": [text("corrected")]}),
        )
        .await;
    let third = assert_accepted(status, &body, "the corrected prompt");
    let request = link.request("session/prompt").await;
    link.reply(&request, json!({"stopReason": "end_turn"})).await;
    watch.until_op("session/s", &third, "completed").await;
    watch.converged(&bench).await;
}

/// §6.4's order: `expect`, then status and occupation, then capability,
/// arguments and the Agent.
#[tokio::test]
async fn refusals_follow_the_admission_order() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create_with(&mut link, "s", controls()).await;
    let mut watch = bench
        .watch(&scopes(&["global", "catalog", "session/s"]))
        .await;
    watch
        .until("the alignments", |watch| {
            watch.has("global") && watch.has("catalog") && watch.has("session/s")
        })
        .await;
    let open = bench.expect_of("session/s").await;
    let (status, body) = bench
        .op(
            "session/s",
            &open,
            "prompt",
            json!({"prompt": [text("run")]}),
        )
        .await;
    let running = assert_accepted(status, &body, "a prompt");
    let request = link.request("session/prompt").await;
    let before = bench.document("session/s").await;
    let malformed = json!({"prompt": 5});
    let (status, body) = bench
        .op(
            "session/s",
            &json!("not-the-op"),
            "prompt",
            malformed.clone(),
        )
        .await;
    stale(
        status,
        &body,
        "a wrong expect is stale before arguments are checked",
    );
    let (status, body) = bench.op("session/s", &running, "prompt", malformed).await;
    not_allowed(
        status,
        &body,
        "a running op refuses before arguments are checked",
    );
    let (status, body) = bench
        .op("session/s", &running, "fork", json!({"cwd": 5}))
        .await;
    not_allowed(
        status,
        &body,
        "a running op refuses before the capability is checked",
    );
    assert_eq!(bench.document("session/s").await, before);
    link.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    watch.until_op("session/s", &running, "completed").await;
    watch
        .until_live("global", "the completed prompt's summary", |live| {
            live["sessions"]["s"]["op"] == json!({"kind": "prompt", "status": "completed"})
        })
        .await;

    // Not advertised: unsupported, before the arguments.
    let (status, body) = bench
        .op("session/s", &running, "fork", json!({"cwd": 5}))
        .await;
    assert_refused(
        status,
        &body,
        StatusCode::NOT_IMPLEMENTED,
        "unsupported",
        "fork",
    );
    let global = bench.expect_of("global").await;
    let (status, body) = bench
        .op("global", &global, "delete", json!({"sessionId": 5}))
        .await;
    assert_refused(
        status,
        &body,
        StatusCode::NOT_IMPLEMENTED,
        "unsupported",
        "delete",
    );
    let catalog = bench.expect_of("catalog").await;
    let (status, body) = bench.op("catalog", &catalog, "list", json!({})).await;
    assert_refused(
        status,
        &body,
        StatusCode::NOT_IMPLEMENTED,
        "unsupported",
        "list",
    );
    let (status, body) = bench.op("global", &global, "logout", json!({})).await;
    assert_refused(
        status,
        &body,
        StatusCode::NOT_IMPLEMENTED,
        "unsupported",
        "logout",
    );

    // A known content capability is checked before the content's arguments.
    let (status, body) = bench
        .op(
            "session/s",
            &running,
            "prompt",
            json!({"prompt": [{"type": "image", "data": "!", "mimeType": "image/png"}]}),
        )
        .await;
    assert_refused(
        status,
        &body,
        StatusCode::NOT_IMPLEMENTED,
        "unsupported",
        "image support before malformed image data",
    );

    // Arguments, including ACP block structure.
    for (kind, args) in [
        ("prompt", json!({})),
        ("prompt", json!({"prompt": [{"type": "nope"}]})),
        ("prompt", json!({"prompt": [{"type": "text"}]})),
        ("prompt", json!({"prompt": [{"type": "text", "text": 7}]})),
    ] {
        let (status, body) = bench.op("session/s", &running, kind, args.clone()).await;
        invalid(status, &body, &format!("{kind} {args}"));
    }
    for (kind, args) in [
        ("create", json!({})),
        ("create", json!({"cwd": 5})),
        ("create", json!({"cwd": "relative"})),
        ("create", json!({"cwd": "/workspace/\u{0}bad"})),
        ("authenticate", json!({"methodId": "unknown"})),
        (
            "terminal_login",
            json!({"methodId": "unknown", "cols": 80, "rows": 24}),
        ),
    ] {
        let (status, body) = bench.op("global", &global, kind, args.clone()).await;
        invalid(status, &body, &format!("{kind} {args}"));
    }
    link.quiet(QUIET).await;
    watch.quiet(QUIET).await;
    watch.converged(&bench).await;
}

/// Without an Agent connection, a write is refused with `unavailable`, after
/// `expect` and its arguments.
#[tokio::test]
async fn without_a_connection_writes_are_unavailable_after_expect_and_arguments() {
    let (bench, link) = Bench::ws(json!({})).await;
    let mut watch = bench.watch(&scopes(&["global"])).await;
    watch
        .until_live("global", "the agent is ready", |live| {
            live["agent"]["phase"] == "ready"
        })
        .await;
    link.close().await;
    watch
        .until_live("global", "the connection ended", |live| {
            live["agent"]["phase"] != "ready"
        })
        .await;
    let before = bench.document("global").await;
    let none = before["live"]["op"]["id"].clone();
    let (status, body) = bench
        .op(
            "global",
            &json!("not-the-op"),
            "create",
            json!({"cwd": CWD}),
        )
        .await;
    stale(status, &body, "a wrong expect without a connection");
    let (status, body) = bench.op("global", &none, "create", json!({})).await;
    invalid(
        status,
        &body,
        "a create without cwd and without a connection",
    );
    let (status, body) = bench
        .op("global", &none, "create", json!({"cwd": CWD}))
        .await;
    assert_refused(
        status,
        &body,
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
        "a create without a connection",
    );
    assert_eq!(bench.document("global").await, before, "nothing changed");
    tokio::time::sleep(QUIET).await;
    assert_eq!(bench.accepted(), 1, "a refused write starts no connection");
}

/// Bridge-started kinds, kinds of another scope and malformed requests are
/// refused with the protocol's error body; an unheld session is not found.
#[tokio::test]
async fn malformed_and_foreign_writes_are_refused_with_the_protocol_error_body() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create_with(&mut link, "s", json!({})).await;
    let global = bench.expect_of("global").await;
    let catalog = bench.expect_of("catalog").await;
    let session = bench.expect_of("session/s").await;
    for (scope, expect, kind) in [
        ("session/s", &session, "load"),
        ("session/s", &session, "open"),
        ("global", &global, "none"),
        ("catalog", &catalog, "none"),
        ("global", &global, "prompt"),
        ("global", &global, "list"),
        ("catalog", &catalog, "create"),
        ("session/s", &session, "create"),
        ("session/s", &session, "delete"),
        ("session/s", &session, "no_such_kind"),
    ] {
        let (status, body) = bench.op(scope, expect, kind, json!({})).await;
        invalid(status, &body, &format!("{kind} on {scope}"));
    }
    let (status, body) = bench
        .op(
            "session/unheld",
            &session,
            "prompt",
            json!({"prompt": [text("x")]}),
        )
        .await;
    not_found(status, &body, "a prompt on an unheld session");
    let (status, body) = bench
        .op("nowhere", &global, "create", json!({"cwd": CWD}))
        .await;
    invalid(status, &body, "an unknown scope name");

    let path = ops_path("global");
    for (content_type, raw, what) in [
        (
            Some("application/json"),
            "{not json".to_string(),
            "invalid JSON",
        ),
        (
            None,
            json!({"expect": global, "kind": "create", "args": {"cwd": CWD}}).to_string(),
            "no content type",
        ),
        (
            Some("application/json"),
            json!({"kind": "create", "args": {"cwd": CWD}}).to_string(),
            "no expect",
        ),
        (
            Some("application/json"),
            json!({"expect": global, "args": {"cwd": CWD}}).to_string(),
            "no kind",
        ),
        (
            Some("application/json"),
            json!({"expect": global, "kind": "create", "args": [CWD]}).to_string(),
            "args that are not an object",
        ),
        (
            Some("application/json"),
            json!({"expect": 7, "kind": "create", "args": {"cwd": CWD}}).to_string(),
            "an expect that is not a string",
        ),
    ] {
        let (status, body) = bench.raw(&path, content_type, &raw).await;
        invalid(status, &body, what);
    }
    link.quiet(QUIET).await;
    assert_eq!(bench.expect_of("global").await, global);
    assert_eq!(bench.expect_of("session/s").await, session);
}

/// A session scope is named by session ID, `/` included; the route takes it
/// URL-encoded.
#[tokio::test]
async fn a_session_id_with_slashes_is_written_through_its_encoded_scope() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create_with(&mut link, "team/one two", json!({})).await;
    let scope = "session/team/one two";
    let mut watch = bench.watch(&scopes(&[scope])).await;
    watch.until("the alignment", |watch| watch.has(scope)).await;
    let open = bench.expect_of(scope).await;
    let (status, body) = bench
        .op(scope, &open, "prompt", json!({"prompt": [text("hi")]}))
        .await;
    let op = assert_accepted(status, &body, "a prompt on an encoded scope");
    let request = link.request("session/prompt").await;
    assert_eq!(request["params"]["sessionId"], "team/one two");
    link.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    watch
        .until_live(scope, "the prompt completes", |live| {
            live["op"]["id"] == op && live["op"]["status"] == "completed"
        })
        .await;
    watch.converged(&bench).await;
}
