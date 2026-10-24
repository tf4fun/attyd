//! The Agent connection and the recovery that follows its end (protocol §5.1, §5.4; Q2): a
//! stream that needs a connection gets the next one, one that does not gets none, and what
//! the stream shows survives the gap. Over real HTTP and a scripted Agent.
use super::*;

/// A server whose Agent holds `saved`, loaded with `replay` through a stream that stays open.
async fn loaded(replay: &[Value]) -> (Bench, Link, Watch) {
    let (server, mut link) = Bench::start(json!({"loadSession": true})).await;
    let mut stream = server
        .watch(&format!(
            "scope=global&scope=session%2Fsaved&cwd={}",
            query_scope(&format!("session/saved:{CWD}"))
        ))
        .await;
    link.serve_load("saved", replay).await;
    stream
        .until("the load ends", |stream| {
            stream.live("session/saved")["op"]["status"] == "completed"
        })
        .await;
    (server, link, stream)
}

/// A dropped body has actually left the feed, not merely had time to do so.
async fn wait_followers(hub: &BridgeHub, session_id: &str, expected: usize) {
    tokio::time::timeout(WAIT, async {
        loop {
            if hub.publication.lock().feed.followers(&format!("session/{session_id}")) == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the HTTP subscriber count did not settle");
}

/// Start the negative observation only after the old runtime really stopped, then cross its
/// actual next-attempt backoff, with a scheduling margin.
async fn cross_recovery_window(hub: &BridgeHub) {
    let backoff = tokio::time::timeout(WAIT, async {
        loop {
            let state = hub.state.lock().await;
            if state.input.is_none() {
                return state.recovery_backoff;
            }
            drop(state);
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the old Agent runtime did not finish");
    tokio::time::sleep(backoff + Duration::from_secs(1)).await;
}

pub(super) fn initialized(capabilities: Value) -> Value {
    json!({"protocolVersion": 1, "agentCapabilities": capabilities, "authMethods": []})
}

/// Events until `global` shows the Agent `phase`.
async fn until_phase(stream: &mut Watch, phases: &[&str]) {
    let wanted: Vec<String> = phases.iter().map(|phase| phase.to_string()).collect();
    stream
        .until(&format!("the Agent is {phases:?}"), move |stream| {
            stream.live("global")["agent"]["phase"]
                .as_str()
                .is_some_and(|phase| wanted.iter().any(|wanted| wanted == phase))
        })
        .await;
}

/// A server launched with the fake Agent through a shell that records each launch, as the
/// terminal-login cases need.
async fn recording_server(
    root: &std::path::Path,
    pid: bool,
) -> (Bench, std::path::PathBuf, std::path::PathBuf) {
    let auth = root.join("authenticated");
    let launches = root.join("launches");
    let manifest = env!("CARGO_MANIFEST_DIR");
    let fixture = std::path::Path::new(manifest).join("tests/fixtures/fake-agent.ts");
    let record = if pid { "echo \"$$ $*\"" } else { "echo \"$*\"" };
    let script = format!(
        "export ATTYD_FAKE_AUTH_FILE='{}'; {} >> '{}'; exec node --import tsx '{}' \"$@\"",
        auth.display(),
        record,
        launches.display(),
        fixture.display(),
    );
    let options = Options::try_parse_from([
        "attyd",
        "--cwd",
        manifest,
        "--",
        "sh",
        "-c",
        &script,
        "agent",
        "--terminal-auth-required",
    ])
    .unwrap()
    .normalized()
    .unwrap();
    (Bench::serve(options, Agents::listen().await).await, auth, launches)
}

/// Runs a real terminal login to success through a `terminal_login`. Its prompt and its exit are
/// read from a global stream of its own, opened while the connection runs and dropped once the
/// login completed. The caller's stream alone then follows the next connection.
async fn sign_in(address: SocketAddr) {
    let mut events = watch_at(address, "scope=global").await;
    events.event().await;
    let expect = events.live("global")["op"]["id"].clone();
    let started = send(
        address,
        Method::POST,
        "/api/v2/targets/global/ops",
        Some(json!({
            "expect": expect,
            "kind": "terminal_login",
            "args": {"methodId": "terminal-login", "cols": 80, "rows": 24},
        })),
    )
    .await;
    assert_eq!(started.status(), StatusCode::ACCEPTED);
    let login = json_of(started).await["op"].as_str().unwrap().to_string();
    // Input waits for the prompt the login prints.
    let prompt = login.clone();
    events
        .until("the login prompts", move |events| {
            events.live("global")["terminals"][&prompt]["output"]
                .as_str()
                .unwrap_or_default()
                .contains("Enter access code")
        })
        .await;
    let input = send(
        address,
        Method::POST,
        &format!("/api/v2/targets/global/resources/{login}/input"),
        Some(json!({"data": "open-sesame\r"})),
    )
    .await;
    assert!(input.status().is_success(), "{}", input.status());
    let ended = login.clone();
    events
        .until("the login ends", move |events| {
            let op = events.live("global")["op"].clone();
            op["id"] == ended.as_str() && op["status"] != "running"
        })
        .await;
    let op = events.live("global")["op"].clone();
    assert_eq!(op["status"], "completed", "{op}");
    assert_eq!(op["result"], json!({"exitCode": 0, "signal": null}), "{op}");
}

/// Events until the Agent has left `ready` and come back to it.
async fn until_ready_again(stream: &mut Watch) {
    let mut ended = false;
    loop {
        stream.event().await;
        let phase = stream.live("global")["agent"]["phase"].clone();
        ended |= phase != "ready";
        if ended && phase == "ready" {
            return;
        }
    }
}

#[tokio::test]
async fn after_an_agent_eof_reads_serve_the_last_documents_and_a_global_stream_restarts_nothing() {
    let (server, link) = Bench::start(json!({})).await;
    let mut stream = server.watch("scope=global&scope=catalog").await;
    stream.event().await;
    stream.event().await;
    link.close().await;
    until_phase(&mut stream, &["stopped", "error"]).await;
    let global = server.document("global").await;
    assert_ne!(global["live"]["agent"]["phase"], "ready");
    server.document("catalog").await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        server.agents.accepted(),
        1,
        "neither a read nor a global stream starts a connection"
    );
    let (status, body) = server.read_document("session/unheld").await;
    not_found(status, &body, "an unheld session");
    assert_eq!(server.agents.accepted(), 1);
}

/// Q2: a successful terminal login refreshes the connection. With only a global stream open,
/// and no read or new subscription, the bridge starts the next connection once the old one
/// ends; the original stream follows it.
#[tokio::test]
async fn a_successful_terminal_login_starts_the_next_connection_for_a_global_only_stream() {
    let root = tempfile::tempdir().unwrap();
    let (server, auth, launches) = recording_server(root.path(), false).await;
    let launched = || {
        let lines = std::fs::read_to_string(&launches).unwrap_or_default();
        let (logins, agents): (Vec<&str>, Vec<&str>) = lines
            .lines()
            .partition(|line| line.contains("--terminal-login"));
        (agents.len(), logins.len())
    };

    let mut stream = server.watch("scope=global").await;
    until_phase(&mut stream, &["ready"]).await;
    assert_eq!(launched(), (1, 0));

    // The login runs to success; then the old connection ends, and the next one becomes ready
    // on the same stream.
    sign_in(server.address).await;
    assert!(auth.exists(), "the login signed in");
    until_ready_again(&mut stream).await;
    assert_eq!(launched(), (2, 1), "one login and exactly one next connection");
    stream.converged(&server).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(launched(), (2, 1), "the next connection is started once");
    server.hub.shutdown().await;
}

/// Q2: the login authorizes one next connection. When that connection ends with an ordinary
/// EOF, a global-only stream starts no third.
#[tokio::test]
async fn a_login_restart_is_consumed_and_the_next_ordinary_eof_starts_nothing() {
    let root = tempfile::tempdir().unwrap();
    let (server, _auth, launches) = recording_server(root.path(), true).await;
    let agents = || {
        std::fs::read_to_string(&launches)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.contains("--terminal-login"))
            .map(|line| line.split(' ').next().unwrap().to_string())
            .collect::<Vec<_>>()
    };

    let mut stream = server.watch("scope=global").await;
    until_phase(&mut stream, &["ready"]).await;
    sign_in(server.address).await;
    until_ready_again(&mut stream).await;
    let launched = agents();
    assert_eq!(launched.len(), 2, "the login's one next connection");

    // The next connection's Agent exits: an ordinary EOF.
    let killed = std::process::Command::new("kill")
        .arg(&launched[1])
        .status()
        .unwrap();
    assert!(killed.success());
    until_phase(&mut stream, &["stopped", "error"]).await;
    cross_recovery_window(&server.hub).await;
    assert_eq!(agents().len(), 2, "the login authorized one connection only");
    server.hub.shutdown().await;
}

#[tokio::test]
async fn a_new_global_stream_aligns_while_initialize_is_unanswered() {
    let mut server = Bench::launch().await;
    let mut link = server.agents.connect().await;
    let initialize = link.request("initialize").await;

    let mut stream = server.watch("scope=global").await;
    let aligned = stream.event().await;
    assert_eq!(aligned["scope"], "global", "{aligned}");
    let agent = &aligned["snapshot"]["doc"]["live"]["agent"];
    assert_eq!(agent["phase"], "initializing", "{aligned}");
    assert!(agent["initialize"].is_null(), "{aligned}");

    link.reply(&initialize, initialized(json!({}))).await;
    until_phase(&mut stream, &["ready"]).await;
    stream.converged(&server).await;
}

#[tokio::test]
async fn a_known_session_aligns_on_a_new_stream_while_the_next_initialize_is_unanswered() {
    let replay = turns(&[("one", "1"), ("two", "2")]);
    let (mut server, link, mut first) = loaded(&replay).await;
    let before = server.document("session/saved").await;
    link.close().await;
    first
        .until("the session waits for its load", |stream| {
            stream.live("session/saved")["op"]["status"] == "running"
        })
        .await;

    let mut next = server.agents.connect().await;
    let initialize = next.request("initialize").await;
    // A held session needs no cwd; its known entries align at once.
    let mut second = server.watch("scope=session%2Fsaved").await;
    let aligned = second.event().await;
    assert_eq!(aligned["scope"], "session/saved", "{aligned}");
    assert_eq!(second.entries("session/saved"), before["entries"].as_array().unwrap().clone());
    assert_eq!(second.live("session/saved")["op"]["kind"], "load");
    assert_eq!(second.live("session/saved")["op"]["status"], "running");

    next.reply(&initialize, initialized(json!({"loadSession": true})))
        .await;
    next.serve_load("saved", &replay).await;
    second
        .until("the load ends", |stream| {
            stream.live("session/saved")["op"]["status"] == "completed"
        })
        .await;
    let after = server.document("session/saved").await;
    second.converged(&server).await;
    first
        .until("the load ends", |stream| {
            stream.live("session/saved")["op"]["status"] == "completed"
        })
        .await;
    first.converged(&server).await;
    assert_eq!(after["entries"], before["entries"]);
    assert_eq!(server.agents.accepted(), 2);
}

#[tokio::test]
async fn an_initialization_failure_keeps_recovering_while_a_session_stream_needs_it() {
    let replay = turns(&[("one", "1")]);
    let (mut server, link, mut stream) = loaded(&replay).await;
    let before = server.document("session/saved").await;
    link.close().await;
    stream
        .until("the session waits for its load", |stream| {
            stream.live("session/saved")["op"]["status"] == "running"
        })
        .await;

    // The next connection ends before it answers initialize.
    let mut failed = server.agents.connect().await;
    failed.request("initialize").await;
    failed.close().await;

    let mut next = server.agents.accept(json!({"loadSession": true})).await;
    next.serve_load("saved", &replay).await;
    stream
        .until("the load ends", |stream| {
            stream.live("session/saved")["op"]["kind"] == "load"
                && stream.live("session/saved")["op"]["status"] == "completed"
        })
        .await;
    assert!(stream.ended.is_empty(), "the stream keeps the session");
    assert_eq!(stream.entries("session/saved"), before["entries"].as_array().unwrap().clone());
    stream.converged(&server).await;
    assert_eq!(server.agents.accepted(), 3);
}

#[tokio::test]
async fn recovery_continues_for_a_remaining_subscriber_and_stops_after_the_last_leaves() {
    let replay = turns(&[("one", "1")]);
    let (mut server, link, first) = loaded(&replay).await;
    let mut second = server.watch("scope=session%2Fsaved").await;
    second.event().await;
    // A global stream only watches the attempts; it needs no connection.
    let mut watcher = server.watch("scope=global").await;
    watcher.event().await;

    link.close().await;
    until_phase(&mut watcher, &["stopped", "error"]).await;
    drop(first);
    wait_followers(&server.hub, "saved", 1).await;

    // One subscriber left: a failed attempt is followed by another.
    let mut failed = server.agents.connect().await;
    failed.request("initialize").await;
    until_phase(&mut watcher, &["initializing"]).await;
    failed.close().await;
    until_phase(&mut watcher, &["stopped", "error"]).await;
    let mut pending = server.agents.connect().await;
    pending.request("initialize").await;
    until_phase(&mut watcher, &["initializing"]).await;

    // The last subscriber leaves while that attempt runs; once it ends, no further attempt
    // starts.
    drop(second);
    wait_followers(&server.hub, "saved", 0).await;
    pending.close().await;
    until_phase(&mut watcher, &["stopped", "error"]).await;
    cross_recovery_window(&server.hub).await;
    assert_eq!(server.agents.accepted(), 3, "no attempt after the last subscriber");
}

#[tokio::test]
async fn a_cold_load_cut_by_an_eof_keeps_the_session_followed_and_converges_on_the_next_connection() {
    let replay = turns(&[("one", "1"), ("two", "2"), ("three", "3")]);
    let (mut server, mut link) = Bench::start(json!({"loadSession": true})).await;
    let mut stream = server
        .watch(&format!(
            "scope=session%2Fsaved&cwd={}",
            query_scope(&format!("session/saved:{CWD}"))
        ))
        .await;

    // The interrupted load only replays part of the history. Publishing it would show a
    // truncated session as complete: while the first load runs the history is unknown.
    let load = link.request("session/load").await;
    assert_eq!(load["params"]["sessionId"], "saved");
    let aligned = stream.event().await;
    assert_eq!(aligned["scope"], "session/saved", "{aligned}");
    assert_eq!(stream.live("session/saved")["op"]["kind"], "load", "{aligned}");
    assert_eq!(stream.live("session/saved")["op"]["status"], "running", "{aligned}");
    for update in &replay[..2] {
        link.update("saved", update.clone()).await;
    }
    // This real Agent request follows the partial replay in ordered ingress. Seeing it in the
    // same stream proves the coordinator processed that replay.
    let barrier = "cold-load-partial-replay-barrier";
    link.send(json!({
        "jsonrpc": "2.0", "id": barrier, "method": "elicitation/create",
        "params": {
            "sessionId": "saved", "mode": "form", "message": barrier,
            "requestedSchema": {"type": "object", "properties": {}},
        },
    }))
    .await;
    stream
        .until("the barrier", |stream| {
            interaction_ids(&stream.live("session/saved")).len() == 1
        })
        .await;
    assert!(stream.ended.is_empty());
    assert!(
        stream.entries("session/saved").is_empty(),
        "an unfinished first load shows no history"
    );
    let loading = server.document("session/saved").await;
    assert!(
        loading.get("entries").is_none(),
        "the unfinished first load still has an unknown history: {loading}"
    );
    // Leave the elicitation unanswered. EOF cancels it with the first load.
    link.close().await;

    let mut next = server.agents.accept(json!({"loadSession": true})).await;
    let load = next.request("session/load").await;
    assert_eq!(load["params"]["sessionId"], "saved");
    assert_eq!(load["params"]["cwd"], CWD);
    for update in &replay {
        next.update("saved", update.clone()).await;
    }
    next.reply(&load, json!({})).await;

    stream
        .until("the load ends", |stream| {
            stream.live("session/saved")["op"]["kind"] == "load"
                && stream.live("session/saved")["op"]["status"] == "completed"
                && stream.entries("session/saved").len() == 6
        })
        .await;
    assert!(stream.ended.is_empty(), "the stream keeps the session");
    stream.converged(&server).await;
    assert_eq!(server.agents.accepted(), 2);
}

#[tokio::test]
async fn a_session_ended_with_load_failed_is_no_recovery_need_behind_a_global_stream() {
    let (server, mut link) = Bench::start(json!({"loadSession": true})).await;
    let mut stream = server
        .watch(&format!(
            "scope=global&scope=session%2Fsaved&cwd={}",
            query_scope(&format!("session/saved:{CWD}"))
        ))
        .await;
    let load = link.request("session/load").await;
    link.send(json!({
        "jsonrpc": "2.0", "id": load["id"],
        "error": {"code": -32002, "message": "no such session"},
    }))
    .await;
    stream
        .until("the end", |stream| stream.ended.contains_key("session/saved"))
        .await;
    assert_eq!(stream.ended["session/saved"]["reason"], "load_failed");
    wait_followers(&server.hub, "saved", 0).await;

    link.close().await;
    until_phase(&mut stream, &["stopped", "error"]).await;
    cross_recovery_window(&server.hub).await;
    assert_eq!(
        server.agents.accepted(),
        1,
        "an ended session scope needs no connection"
    );
}

/// A peer that never answers initialize must not leave the published Agent running after
/// shutdown has drained and closed the protocol streams.
#[tokio::test]
async fn review_shutdown_during_initialize_publishes_a_final_agent_before_closing_streams() {
    let mut server = Bench::launch().await;
    let mut link = server.agents.connect().await;
    link.request("initialize").await;
    let mut stream = server.watch("scope=global").await;
    stream.event().await;
    assert_eq!(stream.live("global")["agent"]["phase"], "initializing");

    tokio::time::timeout(WAIT, server.hub.shutdown())
        .await
        .expect("shutdown must finish even when initialize is unanswered");
    // Observe the actual HTTP body ending, not a quiet timeout, keeping every queued event.
    let mut delivered = std::mem::take(&mut stream.sse.buffer).into_bytes();
    tokio::time::timeout(WAIT, async {
        while let Some(chunk) = stream.sse.body.next().await {
            delivered.extend_from_slice(&chunk.unwrap());
        }
    })
    .await
    .expect("shutdown must drain and close the SSE body");
    let (status, document) = server.read_document("global").await;
    let runtime_remains = server.hub.state.lock().await.input.is_some();
    // Release the scripted peer before asserting.
    link.close().await;
    for message in String::from_utf8(delivered).unwrap().split("\n\n") {
        if let Some(event) = Watch::parse(message) {
            for change in event["changes"].as_array().into_iter().flatten() {
                assert_eq!(change["path"], "/global", "{event}");
                assert!(change.get("end").is_none(), "{event}");
            }
            stream.take(&event);
        }
    }
    assert_eq!(status, StatusCode::OK);
    assert!(
        matches!(
            document["live"]["agent"]["phase"].as_str(),
            Some("stopped" | "error")
        ),
        "the final published cut must precede SSE EOF: {document}"
    );
    assert_eq!(stream.replicas["global"].doc, document);
    assert!(!runtime_remains, "shutdown left initialize running after closing SSE");
}

/// Options' URI parser accepts this authority, but the HTTP transport's URL parser rejects its
/// port before it has a connection. That startup failure still belongs in the always-existing
/// global and catalog documents.
#[tokio::test]
async fn review_an_http_transport_construction_error_is_published() {
    let options = Options::try_parse_from([
        "attyd",
        "--transport",
        "http",
        "--",
        "http://127.0.0.1:99999/acp",
    ])
    .unwrap()
    .normalized()
    .expect("the configured endpoint passes the current CLI validation");
    let bench = Bench::serve(options, Agents::listen().await).await;
    tokio::time::timeout(WAIT, async {
        loop {
            let stopped = bench.hub.stopped.notified();
            if bench.hub.state.lock().await.input.is_none() {
                break;
            }
            stopped.await;
        }
    })
    .await
    .expect("the rejected transport construction ends the generation");

    let global = bench.document("global").await;
    bench.document("catalog").await;
    assert_eq!(global["live"]["agent"]["phase"], "error", "{global}");
    assert!(
        !global["live"]["agent"]["error"]["message"]
            .as_str()
            .unwrap_or("")
            .is_empty()
    );
    bench.hub.shutdown().await;
}

/// Readiness must serve cold scopes registered during initialize as well as the held sessions
/// inherited from the previous Agent connection.
#[tokio::test]
async fn review_a_cold_subscription_registered_during_initialize_is_served_after_reloads() {
    let replay = turns(&[("one", "1")]);
    let (mut server, old, mut keeper) = loaded(&replay).await;
    old.close().await;
    keeper
        .until("the session waits for its load", |stream| {
            stream.live("session/saved")["op"]["status"] == "running"
        })
        .await;

    let mut next = server.agents.connect().await;
    let initialize = next.request("initialize").await;
    let mut cold = server.watch(&cold("new-cold")).await;
    // The HTTP handshake only completes after the subscription is registered. Observe that
    // registration before answering initialize.
    wait_followers(&server.hub, "new-cold", 1).await;
    assert_eq!(
        server.read_document("session/new-cold").await.0,
        StatusCode::NOT_FOUND
    );

    next.reply(&initialize, initialized(json!({"loadSession": true})))
        .await;
    next.serve_load("saved", &replay).await;
    keeper
        .until("the load ends", |stream| {
            stream.live("session/saved")["op"]["status"] == "completed"
        })
        .await;
    let saved = server.document("session/saved").await;
    assert_eq!(saved["live"]["op"]["status"], "completed");
    keeper.converged(&server).await;

    // Even after the inherited reload has succeeded, the new cold scope must start without
    // another HTTP request, subscription or Agent event.
    let load = next.request("session/load").await;
    assert_eq!(load["params"]["sessionId"], "new-cold", "{load}");
    assert_eq!(load["params"]["cwd"], CWD, "{load}");
    next.reply(&load, json!({})).await;
    cold.until("the load ends", |stream| {
        stream.live("session/new-cold")["op"]["status"] == "completed"
    })
    .await;
    let document = server.document("session/new-cold").await;
    assert_eq!(document["entries"], json!([]));
    cold.converged(&server).await;
    assert_eq!(server.agents.accepted(), 2);
    server.hub.shutdown().await;
}
