//! Sharing a session and a client key does not make a v1 input the owner
//! of a pending v2 admission. The input must carry its original admission.
use super::*;

const KEY: &str = "same-session-v1-v2-key";

struct QueuedTurn {
    peer: Peer,
    path: String,
    headers: Vec<(&'static str, String)>,
    body: Value,
    revision: String,
    submitted: futures::future::BoxFuture<'static, (StatusCode, Value)>,
    delayed: BridgeInput,
    real_events: mpsc::UnboundedSender<BridgeInput>,
}

async fn queued_turn() -> QueuedTurn {
    let mut peer = Peer::start(-1, json!({})).await;
    create(&mut peer).await;
    let documents = peer.check().await;
    let revision = session(&documents)["history"]["revision"]
        .as_str()
        .unwrap()
        .to_string();
    let tag = format!(
        "\"{}:{}:{revision}\"",
        epoch(&peer).await,
        session(&documents)["incarnation"],
    );
    let (captured, mut pending) = mpsc::unbounded_channel();
    let real_events = {
        let state = peer.state.upgrade().unwrap();
        let mut state = state.lock().await;
        state.observer_events.replace(captured).unwrap()
    };
    let path = format!("/api/v2/sessions/{SESSION}/turns");
    let headers = vec![("idempotency-key", KEY.to_string()), ("if-match", tag)];
    let body = json!({"prompt": [{"type": "text", "text": "Original v2 prompt A"}]});
    let router = peer.router.clone();
    let request_path = path.clone();
    let request_headers = headers.clone();
    let request_body = body.clone();
    let mut submitted: futures::future::BoxFuture<'static, (StatusCode, Value)> =
        Box::pin(async move {
            send(
                &router,
                "POST",
                &request_path,
                &request_headers,
                Some(request_body),
            )
            .await
        });
    // Only input delivery is delayed: the actual HTTP handler reserves the
    // intent, and the actual coordinator later admits or refuses the turn.
    assert!(futures::poll!(&mut submitted).is_pending());
    let delayed = pending.recv().await.expect("the v2 input was queued");
    {
        let state = peer.state.upgrade().unwrap();
        state.lock().await.observer_events = Some(real_events.clone());
    }
    QueuedTurn {
        peer,
        path,
        headers,
        body,
        revision,
        submitted,
        delayed,
        real_events,
    }
}

fn v1_turn(peer: &Peer, revision: String) -> oneshot::Receiver<Result<Value, String>> {
    let (response, result) = oneshot::channel();
    // The production v1 route sends this input, without a v2 admission.
    peer.commands
        .send(BridgeInput::TurnRequest {
            session_id: SESSION.to_string(),
            history_revision: revision,
            client_intent_id: KEY.to_string(),
            prompt: vec![json!({"type": "text", "text": "Different v1 prompt B"})],
            response,
        })
        .unwrap();
    result
}

#[tokio::test]
async fn same_session_v1_admission_cannot_resolve_a_different_v2_prompt() {
    let QueuedTurn {
        mut peer,
        path,
        headers,
        body,
        revision,
        submitted,
        delayed,
        real_events,
    } = queued_turn().await;
    let v1 = v1_turn(&peer, revision);
    let prompt = peer.next_request().await;
    assert_eq!(prompt["method"], "session/prompt");
    assert_eq!(prompt["params"]["sessionId"], SESSION);
    assert_eq!(
        prompt["params"]["prompt"][0]["text"],
        "Different v1 prompt B"
    );
    let v1 = tokio::time::timeout(Duration::from_secs(3), v1)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(v1["disposition"], "accepted");

    assert!(real_events.send(delayed).is_ok());
    let (status, original) = submitted.await;
    assert_eq!(status, StatusCode::CONFLICT, "{original}");
    assert_eq!(original["error"]["code"], "intent_conflict");

    // Prompt A was refused and never reached the Agent. Retrying A cannot
    // claim that B's different payload was its successful duplicate.
    let (status, retried) = send(&peer.router, "POST", &path, &headers, Some(body)).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a refused v2 prompt cannot recover the unrelated v1 operation {}: {retried}",
        v1["operationId"],
    );
    assert_eq!(retried["error"]["code"], "intent_conflict");
    peer.no_request().await;
    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    peer.until("the v1 turn finishes", |documents| {
        session(documents)["turn"].is_null()
    })
    .await;
    peer.settle().await;
    peer.stop().await;
}

#[tokio::test]
async fn same_session_v1_refusal_cannot_release_a_queued_v2_intent() {
    let QueuedTurn {
        mut peer,
        path,
        headers,
        body,
        submitted,
        delayed,
        real_events,
        ..
    } = queued_turn().await;
    let refused = v1_turn(&peer, "stale-v1-history-revision".to_string());
    let refused = tokio::time::timeout(Duration::from_secs(3), refused)
        .await
        .unwrap()
        .unwrap();
    assert!(refused.is_err());
    peer.no_request().await;

    // The original v2 input is still pending. Its reserved fingerprint must
    // reject a different body, even though a v1 input with this key failed.
    let different = json!({"prompt": [{"type": "text", "text": "Replacement v2 prompt C"}]});
    let (status, conflicting) = send(&peer.router, "POST", &path, &headers, Some(different)).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "the v1 refusal must not make A's reserved key available again: {conflicting}"
    );
    assert_eq!(conflicting["error"]["code"], "intent_conflict");
    peer.no_request().await;

    assert!(real_events.send(delayed).is_ok());
    let (status, accepted) = submitted.await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let prompt = peer.next_request().await;
    assert_eq!(prompt["method"], "session/prompt");
    assert_eq!(
        prompt["params"]["prompt"][0]["text"],
        "Original v2 prompt A"
    );
    let (status, retried) = send(&peer.router, "POST", &path, &headers, Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    assert_eq!(retried["result"]["disposition"], "duplicate");
    assert_eq!(
        retried["result"]["operationId"],
        accepted["result"]["operationId"]
    );
    peer.no_request().await;
    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    peer.until("the original v2 turn finishes", |documents| {
        session(documents)["turn"].is_null()
    })
    .await;
    peer.settle().await;
    peer.stop().await;
}
