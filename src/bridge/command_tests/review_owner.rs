//! Hold one accepted browser input before coordinator delivery to exercise
//! the admission/delivery race against a real v1 close and recreation.
use super::*;

#[tokio::test]
async fn an_admitted_mode_command_cannot_follow_a_replaced_owner() {
    let mut peer = Peer::start(-1, json!({"sessionCapabilities": {"close": {}}})).await;
    create(&mut peer).await;
    let original_tag = tag(&mut peer).await;
    let original_incarnation = session(&peer.check().await)["incarnation"]
        .as_u64()
        .unwrap();

    // The HTTP route still admits and commits the real command, but the
    // channel makes its delayed delivery deterministic. No registry owner
    // or operation is synthesized by the fixture.
    let (captured, mut pending) = mpsc::unbounded_channel();
    let real_events = {
        let state = peer.state.upgrade().unwrap();
        let mut state = state.lock().await;
        state.observer_events.replace(captured).unwrap()
    };
    let (status, accepted) = command(
        &peer,
        "PUT",
        &mode_path(),
        Some("delayed-old-owner-mode"),
        Some(&original_tag),
        json!({"modeId": "code"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let delayed = pending.recv().await.expect("the accepted mode was queued");
    {
        let state = peer.state.upgrade().unwrap();
        state.lock().await.observer_events = Some(real_events.clone());
    }
    let record = find(&peer.check().await, "delayed-old-owner-mode").unwrap();
    assert_eq!(record["status"], "running");
    assert_eq!(record["target"]["incarnation"], original_incarnation);
    assert_eq!(record["requestId"], accepted["result"]["requestId"]);

    // A v1 client can win this race before the v2 input reaches the
    // coordinator. Close the old owner and let the Agent return the same
    // session ID to a new creation, producing a different incarnation.
    let closed = peer.submit(json!({
        "type": "session/close", "requestId": "review-owner-close",
        "sessionId": SESSION,
    }));
    let close = peer.next_request().await;
    assert_eq!(close["method"], "session/close");
    peer.reply(&close, json!({})).await;
    tokio::time::timeout(Duration::from_secs(3), closed)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    peer.until("the original owner is closed", |documents| !held(documents))
        .await;
    create(&mut peer).await;
    let replacement = peer.check().await;
    let replacement_incarnation = session(&replacement)["incarnation"].as_u64().unwrap();
    assert_ne!(replacement_incarnation, original_incarnation);
    assert_eq!(
        session(&replacement)["controls"]["modes"]["currentModeId"],
        "ask"
    );

    // The accepted If-Match owner must travel with the input. Looking up
    // SESSION again here would send the old command to the new owner.
    assert!(real_events.send(delayed).is_ok());
    peer.no_request().await;
    let failed = finished(&mut peer, "delayed-old-owner-mode").await;
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["target"]["incarnation"], original_incarnation);
    let documents = peer.check().await;
    assert_eq!(session(&documents)["incarnation"], replacement_incarnation);
    assert_eq!(
        session(&documents)["controls"]["modes"]["currentModeId"],
        "ask"
    );
    assert!(session(&documents)["operation"].is_null());
    peer.stop().await;
}
