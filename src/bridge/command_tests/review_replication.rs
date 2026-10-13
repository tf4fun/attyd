use super::*;

/// A coalesced cut can put global's snapshot before the session's reset.
/// Reading the first message must not satisfy S7 for a command on that session.
#[tokio::test]
async fn read_your_writes_waits_for_the_whole_coalesced_cut() {
    let mut peer = Peer::start(-1, json!({})).await;
    create(&mut peer).await;
    let scopes = [GLOBAL.to_string(), owner(&mut peer, SESSION).await];
    let mut stream = HttpStream::open(&peer.router, &scopes).await;
    let (seq, documents) = peer.cut().await;
    assert!(stream.check(&peer.router, seq, &documents).await);
    assert!(session(&stream.replica.documents())["turn"].is_null());

    // Consume the one chunk the reader was already awaiting, then leave
    // subsequent commits in the server's slow-subscriber queue.
    stream.pause();
    peer.update(json!({"sessionUpdate": "session_info_update", "title": "Before"}))
        .await;
    stream.next().await;
    {
        let state = peer.state.upgrade().unwrap();
        let mut state = state.lock_without_commit().await;
        let epoch = state.sessions.epoch().to_string();
        state.replication.set_subscriber_budget(&epoch, 1);
    }
    let (accepted, prompt) = start_turn(&mut peer, "coalesced-turn").await;
    let at = accepted["at"]["seq"].as_u64().unwrap();
    assert_eq!(
        session(&peer.check().await)["turn"]["operationId"],
        accepted["result"]["operationId"]
    );

    stream.resume();
    let first = stream.next().await;
    assert_eq!(first["type"], "snapshot", "{first}");
    assert_eq!(first["scope"], GLOBAL, "{first}");
    assert!(first["at"]["seq"].as_u64().unwrap() >= at);
    assert!(
        session(&stream.replica.documents())["turn"].is_null(),
        "the session's replacement has not arrived"
    );
    assert!(
        stream.replica.applied() < at,
        "S7 must not report this command applied before the same-cut session reset arrives"
    );

    let reset = stream.next().await;
    assert_eq!(reset["type"], "reset", "{reset}");
    assert_eq!(reset["scope"], session_scope(SESSION), "{reset}");
    assert!(stream.replica.applied() < at);
    stream.fetch(&peer.router).await;
    assert!(stream.replica.applied() >= at);
    assert_eq!(
        session(&stream.replica.documents())["turn"]["operationId"],
        accepted["result"]["operationId"]
    );
    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    peer.stop().await;
}

#[tokio::test]
async fn missing_attachment_returns_a_protocol_error_body() {
    let mut peer = Peer::start(-1, json!({})).await;
    create(&mut peer).await;
    let tag = tag(&mut peer).await;
    let (status, body) = send(
        &peer.router,
        "GET",
        &format!("/api/v2/sessions/{SESSION}/attachments/{}", "0".repeat(64)),
        &[("if-match", tag)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body["error"]["code"], "not_found",
        "every non-2xx v2 response must carry the structured protocol error: {body}"
    );
    assert!(body["error"]["message"].is_string());
    peer.no_request().await;
    peer.stop().await;
}

/// Terminal output can keep arriving after the turn which references it
/// finished. An immutable process URL must not acquire different bytes.
#[tokio::test]
async fn immutable_process_page_does_not_follow_later_terminal_output() {
    let mut peer = Peer::start(-1, json!({})).await;
    create(&mut peer).await;
    let (_, prompt) = start_turn(&mut peer, "terminal-turn").await;
    let incarnation = session(&peer.check().await)["incarnation"]
        .as_u64()
        .unwrap();
    // Real terminal/create publishes this owner-scoped handle before the
    // Agent can refer to it. Memory ACP transport has no physical terminal
    // manager, so install its normal snapshot first, in the same order.
    {
        let state = peer.state.upgrade().unwrap();
        let mut state = state.lock().await;
        assert!(apply_terminal_snapshot(
            &mut state,
            TerminalSnapshot {
                incarnation,
                value: json!({
                    "sessionId": SESSION, "terminalId": "background-terminal",
                    "output": "first output", "released": false,
                }),
            }
        ));
    }
    peer.update(json!({
        "sessionUpdate": "tool_call", "toolCallId": "background-tool",
        "title": "Background command", "kind": "execute", "status": "in_progress",
        "content": [{"type": "terminal", "terminalId": "background-terminal"}],
    }))
    .await;
    peer.until("the terminal reference arrived", |documents| {
        session(documents)["turn"]["updates"]
            .as_array()
            .is_some_and(|updates| !updates.is_empty())
    })
    .await;
    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    let documents = peer
        .until("the turn is complete", |documents| {
            session(documents)["turn"].is_null()
                && session(documents)["history"]["phase"] == "ready"
        })
        .await;
    let revision = session(&documents)["history"]["revision"].as_str().unwrap();
    let turn_id = session(&documents)["history"]["collapsedTurns"][0]["turnId"]
        .as_str()
        .unwrap();
    let path = format!("/api/v2/sessions/{SESSION}/history/{revision}/turns/{turn_id}/process");
    let (status, before) = get(&peer.router, &path).await;
    assert_eq!(status, StatusCode::OK, "{before}");
    assert_eq!(
        before["terminals"]["background-terminal"]["output"],
        "first output"
    );

    {
        let state = peer.state.upgrade().unwrap();
        let mut state = state.lock().await;
        assert!(apply_terminal_snapshot(
            &mut state,
            TerminalSnapshot {
                incarnation,
                value: json!({
                    "sessionId": SESSION, "terminalId": "background-terminal",
                    "output": "later output", "released": false,
                }),
            }
        ));
    }
    let (status, after) = get(&peer.router, &path).await;
    match status {
        StatusCode::OK => assert_eq!(
            after, before,
            "a cache-forever process URL must return the same content while retained"
        ),
        StatusCode::GONE => assert_eq!(after["error"]["code"], "gone"),
        other => panic!("immutable process lookup returned {other}: {after}"),
    }
    peer.stop().await;
}
