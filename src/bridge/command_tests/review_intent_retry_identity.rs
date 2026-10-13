//! Equal logical fingerprints do not make two independently queued inputs
//! the same admission attempt. A recoverable refusal must settle its input.
use super::*;

const KEY: &str = "same-fingerprint-distinct-inputs";
type HttpAnswer = futures::future::BoxFuture<'static, (StatusCode, Value)>;

struct QueuedTurn {
    peer: Peer,
    path: String,
    headers: Vec<(&'static str, String)>,
    body: Value,
    submitted: HttpAnswer,
    delayed: BridgeInput,
    real_events: mpsc::UnboundedSender<BridgeInput>,
}

fn request(peer: &Peer, path: &str, headers: &[(&'static str, String)], body: Value) -> HttpAnswer {
    let router = peer.router.clone();
    let path = path.to_string();
    let headers = headers.to_vec();
    Box::pin(async move { send(&router, "POST", &path, &headers, Some(body)).await })
}

async fn after_identical_v1_refusal() -> QueuedTurn {
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
    let path = format!("/api/v2/sessions/{SESSION}/turns");
    let headers = vec![("idempotency-key", KEY.to_string()), ("if-match", tag)];
    let body = json!({"prompt": [{"type": "text", "text": "Original queued prompt A"}]});

    // Admit the real HTTP input but delay its coordinator delivery. Keep
    // its HTTP future alive: it still awaits its own admission answer.
    let (captured, mut pending) = mpsc::unbounded_channel();
    let real_events = {
        let state = peer.state.upgrade().unwrap();
        let mut state = state.lock().await;
        state.observer_events.replace(captured).unwrap()
    };
    let mut submitted = request(&peer, &path, &headers, body.clone());
    assert!(futures::poll!(&mut submitted).is_pending());
    let delayed = pending.recv().await.expect("the original turn was queued");
    {
        let state = peer.state.upgrade().unwrap();
        state.lock().await.observer_events = Some(real_events.clone());
    }

    // A real v1 mode RPC provides the temporary admission conflict. No
    // runtime state, history revision or intent is changed by the fixture.
    let mode = peer.submit(json!({
        "type": "session/set_mode", "requestId": "review-busy-mode",
        "sessionId": SESSION, "modeId": "code",
    }));
    let mode_request = peer.next_request().await;
    assert_eq!(mode_request["method"], "session/set_mode");
    assert!(!session(&peer.check().await)["operation"].is_null());
    let (response, refused) = oneshot::channel();
    peer.commands
        .send(BridgeInput::TurnRequest {
            session_id: SESSION.to_string(),
            history_revision: revision.clone(),
            client_intent_id: KEY.to_string(),
            prompt: body["prompt"].as_array().unwrap().clone(),
            response,
        })
        .unwrap();
    let refused = tokio::time::timeout(Duration::from_secs(3), refused)
        .await
        .unwrap()
        .unwrap();
    assert!(refused.is_err(), "the mode still owns admission");
    peer.no_request().await;

    // Completing this control restores turn eligibility without changing
    // the owner, history revision, key or prompt of the queued v2 input.
    peer.reply(&mode_request, json!({})).await;
    tokio::time::timeout(Duration::from_secs(3), mode)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    peer.until("the temporary mode operation finishes", |documents| {
        session(documents)["operation"].is_null()
    })
    .await;
    assert_eq!(
        session(&peer.check().await)["history"]["revision"],
        revision
    );
    QueuedTurn {
        peer,
        path,
        headers,
        body,
        submitted,
        delayed,
        real_events,
    }
}

async fn deliver_original(
    peer: &mut Peer,
    real_events: &mpsc::UnboundedSender<BridgeInput>,
    delayed: BridgeInput,
    submitted: HttpAnswer,
) -> (Value, Value) {
    assert!(real_events.send(delayed).is_ok());
    let (status, accepted) = submitted.await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let prompt = peer.next_request().await;
    assert_eq!(prompt["method"], "session/prompt");
    assert_eq!(
        prompt["params"]["prompt"][0]["text"],
        "Original queued prompt A"
    );
    (accepted, prompt)
}

async fn complete_turn(peer: &mut Peer, prompt: &Value) {
    peer.no_request().await;
    peer.reply(prompt, json!({"stopReason": "end_turn"})).await;
    peer.until("the accepted turn finishes", |documents| {
        session(documents)["turn"].is_null()
    })
    .await;
    peer.settle().await;
}

#[tokio::test]
async fn a_late_turn_answer_cannot_bind_a_different_retry_to_its_operation() {
    let QueuedTurn {
        mut peer,
        path,
        headers,
        body,
        submitted,
        delayed,
        real_events,
    } = after_identical_v1_refusal().await;
    let different = json!({"prompt": [{"type": "text", "text": "Different prompt C"}]});
    let (captured, mut pending) = mpsc::unbounded_channel();
    {
        let state = peer.state.upgrade().unwrap();
        state.lock().await.observer_events = Some(captured);
    }
    let mut later = request(&peer, &path, &headers, different.clone());
    // Both safe strategies are allowed: retain A's reservation and reject
    // C, or invalidate A before reusing the key for C. Retain any queued C
    // input so the responses can be checked against the actual ACP prompt.
    let queued = tokio::select! {
        response = &mut later => Err(response),
        input = pending.recv() => Ok(input.expect("the later turn was queued")),
    };
    {
        let state = peer.state.upgrade().unwrap();
        state.lock().await.observer_events = Some(real_events.clone());
    }
    match queued {
        Err((status, rejected)) => {
            assert_eq!(status, StatusCode::CONFLICT, "{rejected}");
            assert_eq!(rejected["error"]["code"], "intent_conflict");
            let (accepted, prompt) =
                deliver_original(&mut peer, &real_events, delayed, submitted).await;
            let (status, retried) = request(&peer, &path, &headers, body).await;
            assert_eq!(status, StatusCode::OK, "{retried}");
            assert_eq!(retried["result"]["disposition"], "duplicate");
            assert_eq!(
                retried["result"]["operationId"],
                accepted["result"]["operationId"]
            );
            complete_turn(&mut peer, &prompt).await;
        }
        Ok(later_input) => {
            assert!(real_events.send(delayed).is_ok());
            let (status, original) = submitted.await;
            if status == StatusCode::CONFLICT {
                // Reusing the key is safe if the previous attempt was
                // invalidated first and cannot send its old prompt.
                peer.no_request().await;
                assert!(real_events.send(later_input).is_ok());
                let (status, accepted) = later.await;
                assert_eq!(status, StatusCode::OK, "{accepted}");
                let prompt = peer.next_request().await;
                assert_eq!(prompt["method"], "session/prompt");
                assert_eq!(prompt["params"]["prompt"][0]["text"], "Different prompt C");
                let (status, retried) = request(&peer, &path, &headers, different).await;
                assert_eq!(status, StatusCode::OK, "{retried}");
                assert_eq!(retried["result"]["disposition"], "duplicate");
                assert_eq!(
                    retried["result"]["operationId"],
                    accepted["result"]["operationId"]
                );
                let (status, obsolete) = request(&peer, &path, &headers, body).await;
                assert_eq!(status, StatusCode::CONFLICT, "{obsolete}");
                assert_eq!(obsolete["error"]["code"], "intent_conflict");
                complete_turn(&mut peer, &prompt).await;
            } else {
                assert_eq!(status, StatusCode::OK, "{original}");
                let prompt = peer.next_request().await;
                assert_eq!(prompt["method"], "session/prompt");
                assert_eq!(
                    prompt["params"]["prompt"][0]["text"],
                    "Original queued prompt A"
                );
                assert!(real_events.send(later_input).is_ok());
                let (status, rejected) = later.await;
                assert_eq!(status, StatusCode::CONFLICT, "{rejected}");
                assert_eq!(rejected["error"]["code"], "intent_conflict");
                let (status, retried) = request(&peer, &path, &headers, different).await;
                assert_eq!(
                    status,
                    StatusCode::CONFLICT,
                    "C was refused and never sent; it cannot recover A's operation {}: {retried}",
                    original["result"]["operationId"],
                );
                assert_eq!(retried["error"]["code"], "intent_conflict");
                let (status, recovered) = request(&peer, &path, &headers, body).await;
                assert_eq!(status, StatusCode::OK, "{recovered}");
                assert_eq!(recovered["result"]["disposition"], "duplicate");
                assert_eq!(
                    recovered["result"]["operationId"],
                    original["result"]["operationId"]
                );
                complete_turn(&mut peer, &prompt).await;
            }
        }
    }
    peer.stop().await;
}
