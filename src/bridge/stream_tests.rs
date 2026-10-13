//! Protocol v2 snapshot reads and the replication stream, end to end over
//! the real routes, coordinator and ACP transport (issue #22 acceptance).
use axum::http::StatusCode;

use super::convergence_tests::{Peer, SESSION, create, create_unobserved, held, session};
use super::stream_client::{HttpStream, get, owner_scope};
use super::*;
use crate::protocol::{EndReason, GLOBAL, session_scope};

/// The owner tag of the held [`SESSION`].
pub(super) async fn owner(peer: &Peer) -> String {
    let (_, documents) = peer.cut().await;
    let (status, global) = get(&peer.router, "/api/v2/global").await;
    assert_eq!(status, StatusCode::OK);
    owner_scope(
        SESSION,
        global["at"]["epoch"].as_str().unwrap(),
        session(&documents)["incarnation"].as_u64().unwrap(),
    )
}

async fn close(peer: &mut Peer) {
    let closed = peer.submit(json!({
        "type": "session/close", "requestId": "stream-close", "sessionId": SESSION,
    }));
    let close = peer.next_request().await;
    assert_eq!(close["method"], "session/close");
    peer.reply(&close, json!({})).await;
    tokio::time::timeout(Duration::from_secs(3), closed)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    peer.until("the closed session leaves", |documents| !held(documents))
        .await;
}

/// Starts a turn from another client; returns the Agent's prompt request.
async fn start_turn(peer: &mut Peer) -> Value {
    let revision = session(&peer.check().await)["history"]["revision"].clone();
    let (response, admitted) = oneshot::channel();
    peer.commands
        .send(BridgeInput::TurnRequest {
            session_id: SESSION.into(),
            history_revision: revision.as_str().unwrap().into(),
            client_intent_id: "stream-turn".into(),
            prompt: vec![json!({"type": "text", "text": "Run it"})],
            response,
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), admitted)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let prompt = peer.next_request().await;
    assert_eq!(prompt["method"], "session/prompt");
    peer.until("the turn runs", |documents| {
        !session(documents)["turn"].is_null()
    })
    .await;
    prompt
}

/// An Agent that could load the session again, so a load would be visible.
fn close_and_load() -> Value {
    json!({"loadSession": true, "sessionCapabilities": {"close": {}, "resume": {}}})
}

#[tokio::test]
async fn snapshot_reads_serve_held_sessions_and_never_load() {
    let mut peer = Peer::start(-1, close_and_load()).await;
    let (status, global) = get(&peer.router, "/api/v2/global").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(global["state"]["agent"]["phase"], "ready");
    // Launch metadata is static per epoch and comes with the snapshot.
    let launch = &global["state"]["agent"]["launch"];
    assert_eq!(launch["transport"], "ws");
    assert_eq!(launch["additionalDirectories"], json!([]));
    assert_eq!(launch["mcpServers"], json!([]));
    let (seq, _) = peer.cut().await;
    assert_eq!(global["at"]["seq"], seq);

    let (status, missing) = get(&peer.router, &format!("/api/v2/sessions/{SESSION}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["error"]["code"], "not_found");
    peer.no_request().await;

    create(&mut peer).await;
    let (status, snapshot) = get(&peer.router, &format!("/api/v2/sessions/{SESSION}")).await;
    assert_eq!(status, StatusCode::OK);
    let (seq, documents) = peer.cut().await;
    assert_eq!(snapshot["at"]["seq"], seq);
    assert_eq!(snapshot["state"], documents[&session_scope(SESSION)]);

    let (status, invalid) = get(&peer.router, "/api/v2/stream?scope=session/x").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid["error"]["code"], "invalid_request");
    peer.stop().await;
}

/// v2's form of `view_refresh_and_sse_resume_return_the_retired_owner_without_fresh_open`:
/// pages that come back after another page closed the session get `end`,
/// whether they had no event yet, still have the close in the log, or
/// resume past compaction, and nothing loads the session again.
#[tokio::test]
async fn sse_resume_returns_the_retired_owner_without_fresh_open() {
    let mut peer = Peer::start(-1, close_and_load()).await;
    create(&mut peer).await;
    let owner = owner(&peer).await;
    let scopes = [GLOBAL.to_string(), owner.clone()];

    // Page A disconnects before its first event.
    let mut first_event = HttpStream::open(&peer.router, &[owner.clone()]).await;
    first_event.disconnect();
    // Pages C and D read their cut, then disconnect.
    let mut in_log = HttpStream::open(&peer.router, &scopes).await;
    let mut compacted = HttpStream::open(&peer.router, &scopes).await;
    let (seq, documents) = peer.cut().await;
    for page in [&mut in_log, &mut compacted] {
        assert!(page.check(&peer.router, seq, &documents).await);
        page.disconnect();
    }

    // Page B closes the session.
    close(&mut peer).await;
    let (seq, _) = peer.cut().await;

    first_event.reconnect(&peer.router).await;
    let end = first_event.next().await;
    assert_eq!(
        (end["type"].as_str(), end["reason"].as_str()),
        (Some("end"), Some("closed"))
    );

    let read = in_log.entries.len();
    in_log.reconnect(&peer.router).await;
    assert!(in_log.pump(seq).await);
    assert_eq!(
        in_log.replica.ended(&session_scope(SESSION)),
        Some(EndReason::Closed)
    );
    assert!(
        in_log.entries[read..]
            .iter()
            .all(|entry| entry["type"] == "ops" || entry["type"] == "end"),
        "the close is still in the log, so global resumes without a reset: {:?}",
        &in_log.entries[read..]
    );

    {
        let state = peer.state.upgrade().unwrap();
        let mut state = state.lock_without_commit().await;
        state.replication.log_mut_for_test().compact_through(seq);
    }
    compacted.reconnect(&peer.router).await;
    while compacted.replica.ended(&session_scope(SESSION)).is_none() {
        compacted.next().await;
    }
    let (seq, documents) = peer.cut().await;
    assert!(compacted.check(&peer.router, seq, &documents).await);
    assert_eq!(
        compacted.replica.ended(&session_scope(SESSION)),
        Some(EndReason::Closed),
        "the retired table outlives compaction"
    );

    peer.no_request().await;
    assert!(peer.cut().await.1.get(&session_scope(SESSION)).is_none());
    peer.stop().await;
}

#[tokio::test]
async fn an_old_epoch_owner_is_replaced_while_the_new_epoch_holds_the_same_incarnation() {
    let mut old = Peer::start(-1, json!({})).await;
    create(&mut old).await;
    let old_owner = owner(&old).await;
    old.stop().await;

    let mut peer = Peer::start(-1, json!({})).await;
    create(&mut peer).await;
    let new_owner = owner(&peer).await;
    assert_eq!(
        old_owner.rsplit_once(':').unwrap().1,
        new_owner.rsplit_once(':').unwrap().1,
        "both epochs hold incarnation 1"
    );
    assert_ne!(old_owner, new_owner);

    let mut page = HttpStream::open(&peer.router, &[old_owner]).await;
    let end = page.next().await;
    assert_eq!(
        (end["type"].as_str(), end["reason"].as_str()),
        (Some("end"), Some("replaced"))
    );
    peer.no_request().await;
    assert!(held(&peer.check().await));
    peer.stop().await;
}

#[tokio::test]
async fn an_http_snapshot_that_overtakes_sse_entries_is_not_reset_again() {
    let mut peer = Peer::start(-1, json!({})).await;
    create(&mut peer).await;
    let owner = owner(&peer).await;
    let mut page = HttpStream::open(&peer.router, &[owner]).await;
    assert_eq!(page.next().await["type"], "reset");
    for title in ["One", "Two", "Three"] {
        peer.update(json!({"sessionUpdate": "session_info_update", "title": title}))
            .await;
        peer.until(title, |documents| {
            session(documents)["controls"]["info"]["title"] == title
        })
        .await;
    }
    // The snapshot is fetched before the page read the entries after its
    // reset; those entries are covered.
    page.fetch(&peer.router).await;
    let (seq, documents) = peer.cut().await;
    assert!(page.check(&peer.router, seq, &documents).await);
    assert_eq!(page.replica.resets, 1, "no second reset");
    assert!(page.replica.covered >= 3, "{}", page.replica.covered);
    peer.stop().await;
}

#[tokio::test]
async fn coalescing_a_slow_subscriber_gives_the_same_final_replica() {
    let mut peer = Peer::start(-1, json!({})).await;
    create(&mut peer).await;
    let owner = owner(&peer).await;
    {
        let state = peer.state.upgrade().unwrap();
        let mut state = state.lock_without_commit().await;
        let epoch = state.sessions.epoch().to_string();
        state.replication.set_subscriber_budget(&epoch, 4_096);
    }
    let scopes = [GLOBAL.to_string(), owner];
    let mut slow = HttpStream::open_paused(&peer.router, &scopes).await;
    peer.streams
        .push(HttpStream::open(&peer.router, &scopes).await);
    let prompt = start_turn(&mut peer).await;
    for index in 0..120 {
        peer.update(json!({
            "sessionUpdate": "agent_message_chunk", "messageId": "answer",
            "content": {"type": "text", "text": format!("chunk {index} ")},
        }))
        .await;
    }
    peer.until("every chunk is streamed", |documents| {
        session(documents)["turn"]["updates"][0]["content"]["text"]
            .as_str()
            .is_some_and(|text| text.ends_with("chunk 119 "))
    })
    .await;
    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    peer.until("the turn completes", |documents| {
        session(documents)["turn"].is_null()
    })
    .await;
    peer.settle().await;

    slow.resume();
    let (seq, documents) = peer.cut().await;
    assert!(slow.check(&peer.router, seq, &documents).await);
    assert!(
        slow.entries.len() < 60,
        "the slow subscriber's queue was coalesced: {} entries for 120 chunks",
        slow.entries.len()
    );
    assert_eq!(
        slow.replica.documents(),
        peer.streams[0].replica.documents()
    );
    peer.stop().await;
}

#[tokio::test]
async fn global_subscribers_never_keep_a_session_observed() {
    let mut peer = Peer::start(1, json!({})).await;
    peer.streams
        .push(HttpStream::open(&peer.router, &[GLOBAL.to_string()]).await);
    create_unobserved(&mut peer).await;
    peer.until("the unobserved session retires", |documents| {
        !held(documents)
    })
    .await;
    let (status, _) = get(&peer.router, &format!("/api/v2/sessions/{SESSION}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    peer.no_request().await;
    peer.settle().await;
    assert!(
        peer.streams[0].replica.documents()[GLOBAL]["sessions"]
            .get(SESSION)
            .is_none()
    );
    peer.stop().await;
}

#[tokio::test]
async fn an_owner_subscription_observes_its_session_until_the_stream_drops() {
    let mut peer = Peer::start(1, json!({})).await;
    create(&mut peer).await;
    let owner = owner(&peer).await;
    let mut page = HttpStream::open(&peer.router, &[owner]).await;
    assert_eq!(page.next().await["type"], "reset");
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(held(&peer.check().await), "the stream's lease observes it");
    page.disconnect();
    peer.until("the unobserved session retires", |documents| {
        !held(documents)
    })
    .await;
    peer.no_request().await;
    peer.stop().await;
}

#[tokio::test]
async fn the_connection_end_closes_streams_and_keeps_the_final_global_readable() {
    let mut peer = Peer::start(-1, json!({})).await;
    let router = peer.router.clone();
    let mut stream = HttpStream::open(&router, &[GLOBAL.to_string()]).await;
    let (seq, documents) = peer.cut().await;
    assert!(stream.check(&router, seq, &documents).await);
    peer.stop().await;
    while !stream.ended().await {}
    assert_eq!(
        stream.replica.documents()[GLOBAL]["agent"]["phase"],
        "stopped"
    );
    let (status, global) = get(&router, "/api/v2/global").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(global["state"]["agent"]["phase"], "stopped");
    let (status, _) = get(&router, &format!("/api/v2/sessions/{SESSION}")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}
