//! A named request scope ends at its actual parent boundary, including an
//! early published fork result and a prompt that has no Request record.
use super::*;

async fn create_source(peer: &mut Peer) {
    let (status, accepted) = command(
        peer,
        "POST",
        SESSIONS,
        Some("completion-source"),
        None,
        json!({"cwd": "/workspace"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let request = peer.next_request().await;
    assert_eq!(request["method"], "session/new");
    peer.reply(&request, json!({"sessionId": SESSION})).await;
    assert_eq!(
        finished(peer, "completion-source").await["status"],
        "succeeded"
    );
}

#[tokio::test]
async fn a_succeeded_fork_ends_its_children_before_optional_history_finishes() {
    let mut peer = Peer::start(
        -1,
        json!({"loadSession": true, "sessionCapabilities": {"fork": {}}}),
    )
    .await;
    create_source(&mut peer).await;
    let documents = peer.check().await;
    let tag = format!(
        "\"{}:{}\"",
        epoch(&peer).await,
        documents[&session_scope(SESSION)]["incarnation"],
    );
    let (status, accepted) = command(
        &peer,
        "POST",
        &format!("/api/v2/sessions/{SESSION}/fork"),
        Some("fork-with-child"),
        Some(&tag),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let fork = peer.next_request().await;
    assert_eq!(fork["method"], "session/fork");
    peer.send(scoped_elicitation(810, &fork["id"], form_mode()))
        .await;
    let (interaction, pending) = global_interaction(&mut peer).await;
    assert_eq!(pending["requestId"], accepted["result"]["requestId"]);
    peer.reply(&fork, json!({"sessionId": "fork-completion-target"}))
        .await;

    // The optional load is a separate ACP request, and can wait indefinitely.
    // Cancellation of the original fork's child must not depend on that reply.
    let mut cancelled = None;
    let load = loop {
        let message = peer.next_request().await;
        if message["method"] == "session/load" {
            break message;
        }
        assert_eq!(message["id"], 810, "{message}");
        cancelled = Some(message);
    };
    assert_eq!(load["params"]["sessionId"], "fork-completion-target");
    let record = finished(&mut peer, "fork-with-child").await;
    peer.settle().await;
    let at_success = peer.check().await;
    let child_at_success = at_success[GLOBAL]["interactions"]
        .get(&interaction)
        .cloned();

    // Complete the optional work so a failure below leaves no background task.
    peer.reply(&load, json!({})).await;
    peer.until("the fork's pending child is cleaned up", |documents| {
        documents[GLOBAL]["interactions"]
            .get(&interaction)
            .is_none()
    })
    .await;
    let cancelled = match cancelled {
        Some(cancelled) => cancelled,
        None => peer.next_request().await,
    };
    peer.settle().await;
    peer.stop().await;
    assert_eq!(record["status"], "succeeded");
    assert_eq!(record["result"]["sessionId"], "fork-completion-target");
    assert!(
        child_at_success.is_none(),
        "a succeeded fork record must have ended its pending request-scoped child before history load finishes: {child_at_success:?}"
    );
    assert_eq!(cancelled["id"], 810);
    assert_eq!(cancelled["result"]["action"], "cancel");
}

#[tokio::test]
async fn a_named_prompt_elicitation_ends_with_its_prompt_without_needing_a_request_record() {
    let mut peer = Peer::start(-1, json!({})).await;
    create_source(&mut peer).await;
    let documents = peer.check().await;
    let session = &documents[&session_scope(SESSION)];
    let tag = format!(
        "\"{}:{}:{}\"",
        epoch(&peer).await,
        session["incarnation"],
        session["history"]["revision"].as_str().unwrap(),
    );
    let (status, accepted) = command(
        &peer,
        "POST",
        &format!("/api/v2/sessions/{SESSION}/turns"),
        Some("prompt-with-child"),
        Some(&tag),
        json!({"prompt": [{"type": "text", "text": "Ask before finishing"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let prompt = peer.next_request().await;
    assert_eq!(prompt["method"], "session/prompt");
    peer.send(scoped_elicitation(811, &prompt["id"], form_mode()))
        .await;
    let (interaction, _) = global_interaction(&mut peer).await;
    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    peer.until("the prompt finishes", |documents| {
        documents[&session_scope(SESSION)]["turn"].is_null()
    })
    .await;
    peer.settle().await;
    let documents = peer.check().await;
    let child_after_prompt = documents[GLOBAL]["interactions"].get(&interaction).cloned();
    let cancelled = if child_after_prompt.is_none() {
        Some(peer.next_request().await)
    } else {
        None
    };
    peer.stop().await;
    assert!(
        child_after_prompt.is_none(),
        "a named request-scoped interaction must not remain answerable after its prompt ended: {child_after_prompt:?}"
    );
    let cancelled = cancelled.unwrap();
    assert_eq!(cancelled["id"], 811);
    assert_eq!(cancelled["result"]["action"], "cancel");
}
