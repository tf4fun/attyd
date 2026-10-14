//! Request scope must retain the lifetime and visible parent of its ACP request.
use super::*;

/// Keep this setup entirely on v2: no mixed-client intent compatibility is
/// required to expose request-scoped interactions during a held-session command.
async fn create_with_modes(peer: &mut Peer) {
    let (status, accepted) = command(
        peer,
        "POST",
        SESSIONS,
        Some("parent-review-create"),
        None,
        json!({"cwd": "/workspace"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let request = peer.next_request().await;
    assert_eq!(request["method"], "session/new");
    peer.reply(
        &request,
        json!({"sessionId": SESSION, "modes": {
            "currentModeId": "ask",
            "availableModes": [{"id": "ask", "name": "Ask"}, {"id": "code", "name": "Code"}],
        }}),
    )
    .await;
    assert_eq!(
        finished(peer, "parent-review-create").await["status"],
        "succeeded"
    );
}

async fn set_mode(peer: &mut Peer, key: &str) -> (Value, Value) {
    let documents = peer.check().await;
    let tag = format!(
        "\"{}:{}\"",
        epoch(peer).await,
        documents[&session_scope(SESSION)]["incarnation"],
    );
    let (status, accepted) = command(
        peer,
        "PUT",
        &format!("/api/v2/sessions/{SESSION}/mode"),
        Some(key),
        Some(&tag),
        json!({"modeId": "code"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let request = peer.next_request().await;
    assert_eq!(request["method"], "session/set_mode");
    (accepted["result"]["requestId"].clone(), request)
}

#[tokio::test]
async fn a_request_scoped_elicitation_during_v2_set_mode_has_its_visible_parent() {
    let mut peer = Peer::start(-1, json!({})).await;
    create_with_modes(&mut peer).await;
    let (request_id, mode) = set_mode(&mut peer, "mode-with-parent").await;
    peer.send(scoped_elicitation(790, &mode["id"], form_mode()))
        .await;
    let (interaction, pending) = global_interaction(&mut peer).await;
    let delivered = answer(
        &mut peer,
        &interaction,
        "answer-mode-parent",
        json!({"action": "decline"}),
    )
    .await;
    assert_eq!(delivered["id"], 790);
    peer.reply(&mode, json!({})).await;
    assert_eq!(
        finished(&mut peer, "mode-with-parent").await["status"],
        "succeeded"
    );
    peer.settle().await;
    peer.stop().await;
    assert_eq!(
        pending["requestId"], request_id,
        "the existing set_mode record must parent a request-scoped elicitation: {pending}"
    );
}

#[tokio::test]
async fn a_request_scoped_elicitation_ends_when_its_v2_session_command_ends() {
    let mut peer = Peer::start(-1, json!({})).await;
    create_with_modes(&mut peer).await;
    let (_, mode) = set_mode(&mut peer, "mode-child-lifetime").await;
    peer.send(scoped_elicitation(791, &mode["id"], form_mode()))
        .await;
    let (interaction, _) = global_interaction(&mut peer).await;
    peer.reply(&mode, json!({})).await;
    assert_eq!(
        finished(&mut peer, "mode-child-lifetime").await["status"],
        "succeeded"
    );
    peer.settle().await;
    let documents = peer.check().await;
    if documents[GLOBAL]["interactions"]
        .get(&interaction)
        .is_some()
    {
        peer.stop().await;
        panic!(
            "an ended session/set_mode must not leave an answerable global interaction: {}",
            documents[GLOBAL]["interactions"]
        );
    }
    let cancelled = peer.next_request().await;
    peer.settle().await;
    peer.stop().await;
    assert_eq!(cancelled["id"], 791);
    assert_eq!(cancelled["result"]["action"], "cancel");
}

#[tokio::test]
async fn an_elicitation_naming_an_already_finished_global_request_is_cancelled() {
    let mut peer = Peer::start_with(-1, json!({}), agent_login(), |_| {}).await;
    let (_, request) = authenticate(&mut peer, "finished-parent").await;
    peer.reply(&request, json!({})).await;
    assert_eq!(
        finished(&mut peer, "finished-parent").await["status"],
        "succeeded"
    );

    // Finish another request after the elicitation on the same ACP wire. Its
    // ordered completion proves the late child was processed before inspection.
    let (_, barrier) = authenticate(&mut peer, "late-child-barrier").await;
    // The JSON-RPC parent is no longer outstanding. Its absence must not turn
    // an explicitly request-scoped interaction into an independent global one.
    peer.send(scoped_elicitation(792, &request["id"], form_mode()))
        .await;
    peer.reply(&barrier, json!({})).await;
    let barrier_finished = finished(&mut peer, "late-child-barrier").await;
    peer.settle().await;
    let documents = peer.check().await;
    if !documents[GLOBAL]["interactions"]
        .as_object()
        .unwrap()
        .is_empty()
    {
        peer.stop().await;
        panic!(
            "an elicitation naming an ended request must be cancelled, not published without a parent: {}",
            documents[GLOBAL]["interactions"]
        );
    }
    let cancelled = peer.next_request().await;
    peer.stop().await;
    assert_eq!(barrier_finished["status"], "succeeded");
    assert_eq!(cancelled["id"], 792);
    assert_eq!(cancelled["result"]["action"], "cancel");
}
