use super::*;

const TERMINAL: &str = "review-process-terminal";

async fn terminal_output(peer: &Peer, incarnation: u64, output: &str) {
    let state = peer.state.upgrade().unwrap();
    let mut state = state.lock().await;
    assert!(apply_terminal_snapshot(
        &mut state,
        TerminalSnapshot {
            incarnation,
            value: json!({
                "sessionId": SESSION, "terminalId": TERMINAL,
                "output": output, "released": false,
            }),
        }
    ));
}

fn process_path(revision: &str, turn: &str) -> String {
    format!("/api/v2/sessions/{SESSION}/history/{revision}/turns/{turn}/process")
}

/// A real completed turn containing a terminal reference. The memory ACP
/// transport has no terminal manager, so publish the manager's normal state
/// snapshot before the Agent refers to its handle.
async fn terminal_history(peer: &mut Peer) -> (u64, String, String) {
    create(peer).await;
    let (_, prompt) = start_turn(peer, "process-owner-turn").await;
    let incarnation = session(&peer.check().await)["incarnation"]
        .as_u64()
        .unwrap();
    terminal_output(peer, incarnation, "process-output-before-retirement").await;
    peer.update(json!({
        "sessionUpdate": "tool_call", "toolCallId": "process-owner-tool",
        "title": "Background command", "kind": "execute", "status": "in_progress",
        "content": [{"type": "terminal", "terminalId": TERMINAL}],
    }))
    .await;
    peer.until("the registered terminal reference arrives", |documents| {
        session(documents)["turn"]["updates"]
            .as_array()
            .is_some_and(|updates| {
                updates
                    .iter()
                    .any(|update| update["toolCallId"] == "process-owner-tool")
            })
    })
    .await;
    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    let documents = peer
        .until("the terminal's turn completes", |documents| {
            session(documents)["turn"].is_null()
                && session(documents)["history"]["phase"] == "ready"
        })
        .await;
    let history = &session(&documents)["history"];
    (
        incarnation,
        history["revision"].as_str().unwrap().to_string(),
        history["collapsedTurns"][0]["turnId"]
            .as_str()
            .unwrap()
            .to_string(),
    )
}

async fn retirement_releases_process_freeze(operation: &str) {
    // Advertise only this retirement method; delete therefore takes its
    // supported direct path rather than first issuing session/close.
    let capabilities = json!({"sessionCapabilities": {(operation): {}}});
    let mut peer = Peer::start(-1, capabilities).await;
    let (_, revision, turn) = terminal_history(&mut peer).await;
    let (status, page) = get(&peer.router, &process_path(&revision, &turn)).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(
        page["terminals"][TERMINAL]["output"],
        "process-output-before-retirement"
    );
    {
        let state = peer.state.upgrade().unwrap();
        let state = state.lock_without_commit().await;
        assert!(state.process_terminals.contains_key(SESSION));
    }

    let tag = tag(&mut peer).await;
    let (method, path) = if operation == "close" {
        ("POST", format!("/api/v2/sessions/{SESSION}/close"))
    } else {
        ("DELETE", format!("/api/v2/sessions/{SESSION}"))
    };
    let (status, accepted) = command(
        &peer,
        method,
        &path,
        Some("retire-process-owner"),
        Some(&tag),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let request = peer.next_request().await;
    assert_eq!(request["method"], format!("session/{operation}"));
    peer.reply(&request, json!({})).await;
    let record = finished(&mut peer, "retire-process-owner").await;
    assert_eq!(record["status"], "succeeded", "{record}");
    peer.until("the process owner retires", |documents| !held(documents))
        .await;

    // No later process read is required to release an unreachable revision.
    // In particular, a read of the now-unheld owner cannot clean this cache
    // because the route returns gone before entering process_terminals.
    {
        let state = peer.state.upgrade().unwrap();
        let state = state.lock_without_commit().await;
        assert!(
            !state.process_terminals.contains_key(SESSION),
            "{operation} left the retired owner's frozen terminal body in BridgeState"
        );
    }
    peer.stop().await;
}

#[tokio::test]
async fn closing_an_owner_releases_its_frozen_process_terminals() {
    retirement_releases_process_freeze("close").await;
}

#[tokio::test]
async fn deleting_an_owner_releases_its_frozen_process_terminals() {
    retirement_releases_process_freeze("delete").await;
}

/// Reproduce the process route's two separate bridge-lock acquisitions:
/// an old read waits after owner_view while a newer revision is read/frozen.
#[tokio::test]
async fn an_overtaken_process_read_cannot_replace_a_newer_freeze() {
    let mut peer = Peer::start(-1, json!({})).await;
    let (incarnation, revision_a, turn) = terminal_history(&mut peer).await;
    let source = ReplicationSource::default();
    source.attach(&peer.state.upgrade().unwrap());

    // HTTP read A has captured its immutable history and live terminal view,
    // but has not reacquired the bridge lock in process_terminals yet.
    let raw_a = source.owner_view(SESSION, None).await.unwrap();
    assert_eq!(raw_a["session"]["historyRevision"], revision_a);
    let terminals_a = raw_a["live"]["terminals"].clone();
    assert_eq!(
        terminals_a[TERMINAL]["output"],
        "process-output-before-retirement"
    );

    let (_, prompt) = start_turn(&mut peer, "next-process-revision").await;
    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    let documents = peer
        .until("revision B replaces revision A", |documents| {
            session(documents)["turn"].is_null()
                && session(documents)["history"]["revision"] != revision_a
        })
        .await;
    let revision_b = session(&documents)["history"]["revision"]
        .as_str()
        .unwrap()
        .to_string();
    terminal_output(&peer, incarnation, "revision-B-frozen-output").await;
    let path_b = process_path(&revision_b, &turn);
    let (status, before) = get(&peer.router, &path_b).await;
    assert_eq!(status, StatusCode::OK, "{before}");
    assert_eq!(
        before["terminals"][TERMINAL]["output"],
        "revision-B-frozen-output"
    );

    // Resume A at the exact second phase the route calls. Rejecting the old
    // revision is valid; returning its captured data must not replace B's
    // already published freeze either.
    if let Err(error) = source
        .process_terminals(SESSION, &revision_a, terminals_a)
        .await
    {
        assert_eq!(error.code, crate::protocol::ErrorCode::Gone);
    }
    terminal_output(&peer, incarnation, "revision-B-later-output").await;
    let (status, after) = get(&peer.router, &path_b).await;
    match status {
        StatusCode::OK => assert_eq!(
            after, before,
            "an older process read replaced the newer revision's immutable terminal snapshot"
        ),
        StatusCode::GONE => assert_eq!(after["error"]["code"], "gone"),
        other => panic!("immutable process lookup returned {other}: {after}"),
    }
    peer.stop().await;
}
