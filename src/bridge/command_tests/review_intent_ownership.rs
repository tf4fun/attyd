//! v1 turn keys belong to their original admission; sharing a client key
//! with a v2 command cannot settle or withdraw that command's intent.
use super::*;

const OTHER: &str = "review-other-intent-owner";
const KEY: &str = "shared-v1-v2-turn-key";

async fn create_other(peer: &mut Peer) {
    let created = peer.submit(json!({
        "type": "session/new", "requestId": "review-other-session-new",
        "cwd": "/workspace",
    }));
    let request = peer.next_request().await;
    assert_eq!(request["method"], "session/new");
    peer.reply(&request, json!({"sessionId": OTHER})).await;
    tokio::time::timeout(Duration::from_secs(3), created)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    peer.until("the other session is held", |documents| {
        documents.contains_key(&session_scope(OTHER))
    })
    .await;
}

fn v1_turn(peer: &Peer, revision: String) -> oneshot::Receiver<Result<Value, String>> {
    let (response, result) = oneshot::channel();
    // This is the same input the production v1 HTTP handler sends.
    peer.commands
        .send(BridgeInput::TurnRequest {
            session_id: OTHER.to_string(),
            history_revision: revision,
            client_intent_id: KEY.to_string(),
            prompt: vec![json!({"type": "text", "text": "A distinct v1 turn"})],
            response,
        })
        .unwrap();
    result
}

async fn turn_tag(peer: &mut Peer) -> String {
    let documents = peer.check().await;
    format!(
        "\"{}:{}:{}\"",
        epoch(peer).await,
        session(&documents)["incarnation"],
        session(&documents)["history"]["revision"].as_str().unwrap(),
    )
}

#[tokio::test]
async fn a_v1_turn_cannot_overwrite_another_sessions_finished_v2_intent() {
    let mut peer = Peer::start(-1, json!({})).await;
    create(&mut peer).await;
    create_other(&mut peer).await;
    let original_tag = turn_tag(&mut peer).await;
    let (accepted, prompt) = start_turn(&mut peer, KEY).await;
    let original_operation = accepted["result"]["operationId"].clone();
    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    peer.until("the original v2 turn has a public outcome", |documents| {
        session(documents)["turn"].is_null()
            && session(documents)["outcomes"]
                .as_object()
                .is_some_and(|outcomes| outcomes.contains_key(original_operation.as_str().unwrap()))
    })
    .await;

    // Running turns already share the bridge's RPC-operation collision
    // guard. After A finishes, v1 may legitimately reuse the key on B.
    let documents = peer.check().await;
    let revision = documents[&session_scope(OTHER)]["history"]["revision"]
        .as_str()
        .unwrap()
        .to_string();
    let v1 = v1_turn(&peer, revision);
    let other_prompt = peer.next_request().await;
    assert_eq!(other_prompt["method"], "session/prompt");
    assert_eq!(other_prompt["params"]["sessionId"], OTHER);
    let other = tokio::time::timeout(Duration::from_secs(3), v1)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_ne!(other["operationId"], original_operation);

    let (status, retried) = command(
        &peer,
        "POST",
        &format!("/api/v2/sessions/{SESSION}/turns"),
        Some(KEY),
        Some(&original_tag),
        json!({"prompt": [{"type": "text", "text": "Run it"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    assert_eq!(
        retried["result"]["operationId"], original_operation,
        "the v1 admission on B must not replace A's v2 operation: {retried}"
    );
    assert_eq!(retried["result"]["disposition"], "duplicate");
    assert!(retried["result"].get("resultExpired").is_none());
    peer.no_request().await;
    peer.reply(&other_prompt, json!({"stopReason": "end_turn"}))
        .await;
    peer.until("the v1 turn finishes", |documents| {
        documents[&session_scope(OTHER)]["turn"].is_null()
    })
    .await;
    peer.settle().await;
    peer.stop().await;
}

#[tokio::test]
async fn a_refused_v1_turn_cannot_withdraw_another_sessions_queued_v2_intent() {
    let mut peer = Peer::start(-1, json!({})).await;
    create(&mut peer).await;
    create_other(&mut peer).await;
    let original_tag = turn_tag(&mut peer).await;
    let (captured, mut pending) = mpsc::unbounded_channel();
    let real_events = {
        let state = peer.state.upgrade().unwrap();
        let mut state = state.lock().await;
        state.observer_events.replace(captured).unwrap()
    };
    let router = peer.router.clone();
    let path = format!("/api/v2/sessions/{SESSION}/turns");
    let headers = [
        ("idempotency-key", KEY.to_string()),
        ("if-match", original_tag),
    ];
    let body = json!({"prompt": [{"type": "text", "text": "The queued v2 turn"}]});
    let mut submitted = Box::pin(send(&router, "POST", &path, &headers, Some(body.clone())));
    assert!(futures::poll!(&mut submitted).is_pending());
    let delayed = pending.recv().await.expect("the v2 turn was queued");
    {
        let state = peer.state.upgrade().unwrap();
        state.lock().await.observer_events = Some(real_events.clone());
    }

    let refused = v1_turn(&peer, "not-the-current-history-revision".to_string());
    let refused = tokio::time::timeout(Duration::from_secs(3), refused)
        .await
        .unwrap()
        .unwrap();
    assert!(refused.is_err(), "the stale v1 turn must be refused");
    peer.no_request().await;
    {
        let state = peer.state.upgrade().unwrap();
        let state = state.lock().await;
        let intent = state.sessions.commands.intent(KEY);
        assert!(
            intent.is_some(),
            "rejecting B's v1 turn must not withdraw A's queued v2 intent"
        );
        assert!(matches!(
            &intent.unwrap().subject,
            crate::protocol::commands::Subject::Turn {
                session_id,
                operation_id: None,
                ..
            } if session_id == SESSION
        ));
    }

    // The original input, not a new submission, still owns this intent.
    assert!(real_events.send(delayed).is_ok());
    let (status, accepted) = submitted.await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let prompt = peer.next_request().await;
    assert_eq!(prompt["method"], "session/prompt");
    assert_eq!(prompt["params"]["sessionId"], SESSION);
    let (status, retried) = send(&router, "POST", &path, &headers, Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    assert_eq!(retried["result"]["disposition"], "duplicate");
    assert_eq!(
        retried["result"]["operationId"],
        accepted["result"]["operationId"]
    );
    peer.no_request().await;
    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    peer.until("the queued v2 turn finishes", |documents| {
        session(documents)["turn"].is_null()
    })
    .await;
    peer.settle().await;
    peer.stop().await;
}
