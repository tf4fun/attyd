use super::*;

#[tokio::test]
async fn a_dropped_turn_http_future_does_not_strand_the_intent() {
    let mut peer = Peer::start(-1, json!({})).await;
    create(&mut peer).await;
    let documents = peer.check().await;
    let tag = format!(
        "\"{}:{}:{}\"",
        epoch(&peer).await,
        session(&documents)["incarnation"],
        session(&documents)["history"]["revision"].as_str().unwrap(),
    );
    let path = format!("/api/v2/sessions/{SESSION}/turns");
    let headers = [
        ("idempotency-key", "disconnected-turn".to_string()),
        ("if-match", tag),
    ];
    let body = json!({"prompt": [{"type": "text", "text": "Run once"}]});
    // Poll the real HTTP handler through enqueue, then disconnect before the
    // coordinator answers admission. The bridge owns the submitted command.
    let mut request = Box::pin(send(
        &peer.router,
        "POST",
        &path,
        &headers,
        Some(body.clone()),
    ));
    assert!(futures::poll!(&mut request).is_pending());
    drop(request);

    let prompt = peer.next_request().await;
    assert_eq!(prompt["method"], "session/prompt");
    let documents = peer.check().await;
    assert_eq!(
        session(&documents)["turn"]["clientIntentId"],
        "disconnected-turn"
    );
    let operation_id = session(&documents)["turn"]["operationId"].clone();
    let (status, retried) = send(&peer.router, "POST", &path, &headers, Some(body.clone())).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an accepted turn must remain recoverable: {retried}"
    );
    assert_eq!(retried["result"]["disposition"], "duplicate");
    assert_eq!(retried["result"]["operationId"], operation_id);
    peer.no_request().await;

    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    peer.until("the disconnected turn finishes", |documents| {
        session(documents)["turn"].is_null()
    })
    .await;
    let (status, retried) = send(&peer.router, "POST", &path, &headers, Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    assert_eq!(retried["result"]["disposition"], "duplicate");
    assert_eq!(retried["result"]["operationId"], operation_id);
    peer.no_request().await;
    peer.settle().await;
    peer.stop().await;
}
