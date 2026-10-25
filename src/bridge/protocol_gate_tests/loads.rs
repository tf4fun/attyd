//! The loads a subscription starts, and what a subscription to a session that is not held sees
//! (protocol §5.1, B6), through the real coordinator and ACP transport.
use super::stream_harness::*;
use super::*;

/// The entries a load of `replay` gives `saved`, as a client that followed it holds them.
async fn history(replay: &[Value]) -> Stream {
    let mut peer = Peer::start(json!({"loadSession": true})).await;
    let scope = session(SAVED);
    let mut stream = Stream::open(&peer, &[scope.as_str()], Some(CWD));
    peer.serve_load(SAVED, replay).await;
    stream
        .until(&mut peer, "the load completes", |stream| {
            stream.live(&scope)["op"]["status"] == "completed"
        })
        .await;
    peer.stop().await.unwrap();
    stream
}

#[tokio::test]
async fn a_held_session_ignores_the_load_directory() {
    let mut peer = Peer::start(json!({"loadSession": true})).await;
    peer.create(CREATED, json!({})).await;
    let scope = session(CREATED);
    let mut stream = Stream::open(&peer, &[scope.as_str()], Some("/elsewhere"));
    stream
        .until(&mut peer, "the alignment", |stream| {
            stream.replicas.contains_key(&scope)
        })
        .await;
    assert!(stream.entries(&scope).is_empty());
    peer.quiet(Duration::from_millis(200)).await;
    stream.converged(&peer);
}

#[tokio::test]
async fn two_subscriptions_share_one_load_and_both_end_with_the_replayed_entries() {
    let replay = turns(&[("one", "1"), ("two", "2")]);
    let earlier = history(&replay).await;
    let scope = session(SAVED);
    let cached = earlier.entries(&scope);
    assert_eq!(cached.len(), 4);
    let mut peer = Peer::start(json!({"loadSession": true})).await;
    let mut fresh = Stream::open(&peer, &[scope.as_str()], Some(CWD));
    let load = peer.request("session/load").await;
    // A client that holds the session from an earlier run of the bridge.
    let mut remembering = Stream::open(&peer, &[scope.as_str()], Some(CWD));
    remembering.client = earlier.client.clone();
    remembering.replicas = earlier.replicas.clone();
    remembering.reopen(&peer, Some(CWD));
    for stream in [&mut fresh, &mut remembering] {
        stream
            .until(&mut peer, "the load runs", |stream| {
                stream.live(&scope)["op"]["kind"] == "load"
                    && stream.live(&scope)["op"]["status"] == "running"
            })
            .await;
    }
    assert_eq!(peer.followers(SAVED), 2);
    for update in &replay {
        peer.update(SAVED, update.clone()).await;
    }
    peer.reply(&load, json!({})).await;
    for stream in [&mut fresh, &mut remembering] {
        stream
            .until(&mut peer, "the load ends", |stream| {
                stream.live(&scope)["op"]["status"] == "completed"
                    && stream.entries(&scope).len() == 4
            })
            .await;
        assert_eq!(stream.entries(&scope), cached);
    }
    peer.quiet(Duration::from_millis(200)).await;
    peer.settled().await;
    fresh.converged(&peer);
    remembering.converged(&peer);
}

#[tokio::test]
async fn reconnecting_before_during_and_after_a_load_keeps_one_load_and_converges() {
    let replay = turns(&[("one", "1"), ("two", "2")]);
    let scope = session(SAVED);
    let mut peer = Peer::start(json!({"loadSession": true})).await;
    let mut stream = Stream::open(&peer, &[scope.as_str()], Some(CWD));
    let load = peer.request("session/load").await;
    stream
        .until(&mut peer, "the alignment", |stream| {
            stream.live(&scope)["op"]["kind"] == "load"
        })
        .await;

    // Before the replay: the running load keeps the session; a new stream starts no second load.
    stream.reopen(&peer, Some(CWD));
    stream
        .until(&mut peer, "the second alignment", |stream| {
            stream.live(&scope)["op"]["status"] == "running"
        })
        .await;

    // During the replay.
    peer.update(SAVED, replay[0].clone()).await;
    peer.update(SAVED, replay[1].clone()).await;
    // A barrier: the Agent's next request shows only once the bridge has handled the replay
    // before it.
    peer.agent_request(
        "barrier",
        "elicitation/create",
        form(json!({"sessionId": SAVED})),
    )
    .await;
    stream
        .until(&mut peer, "the barrier", |stream| {
            interaction_ids(&stream.live(&scope)).len() == 1
        })
        .await;
    stream.reopen(&peer, Some(CWD));
    stream
        .until(&mut peer, "the third alignment", |stream| {
            interaction_ids(&stream.live(&scope)).len() == 1
        })
        .await;
    let barrier = interaction_ids(&stream.live(&scope)).remove(0);
    peer.answer(
        &scope,
        &barrier,
        json!({"response": {"action": "accept", "content": {}}}),
    )
    .await;
    peer.response("barrier").await;
    for update in &replay[2..] {
        peer.update(SAVED, update.clone()).await;
    }

    // After the load ended, before its end was delivered.
    peer.reply(&load, json!({})).await;
    peer.until("the load ends", |peer| {
        peer.live(&scope)["op"]["status"] == "completed"
    })
    .await;
    stream.reopen(&peer, Some(CWD));
    stream
        .until(&mut peer, "the aligned load", |stream| {
            stream.live(&scope)["op"]["status"] == "completed" && stream.entries(&scope).len() == 4
        })
        .await;
    peer.quiet(Duration::from_millis(200)).await;
    peer.settled().await;
    stream.converged(&peer);
}

#[tokio::test]
async fn an_unheld_session_without_cwd_or_whose_load_fails_ends_with_load_failed() {
    let mut peer = Peer::start(json!({"loadSession": true})).await;
    let scope = session(SAVED);
    let mut missing = Stream::open(&peer, &["global", scope.as_str()], None);
    missing
        .until(&mut peer, "the end", |stream| stream.ended.contains_key(&scope))
        .await;
    let end = &missing.ended[&scope];
    assert_eq!(end["reason"], "load_failed");
    assert_eq!(end["error"]["code"], "invalid_request");
    assert_eq!(
        missing.events_of(&scope).len(),
        2,
        "R4 first reports absent, then the load failure"
    );
    assert_eq!(missing.events_of(&scope)[0]["end"]["reason"], "absent");
    peer.quiet(Duration::from_millis(200)).await;
    let before = missing.events_of("global").len();
    let op = peer.install("global", OperationKind::Logout, json!({})).await;
    missing
        .until(&mut peer, "a later global change", |stream| {
            stream.live("global")["op"]["id"] == op
        })
        .await;
    assert!(
        missing.events_of("global").len() > before,
        "the stream goes on for its other scopes"
    );

    let mut failing = Stream::open(&peer, &[scope.as_str()], Some(CWD));
    let load = peer.request("session/load").await;
    failing
        .until(&mut peer, "the alignment", |stream| {
            stream.live(&scope)["op"]["kind"] == "load"
        })
        .await;
    peer.refuse(&load).await;
    failing
        .until(&mut peer, "the end", |stream| stream.ended.contains_key(&scope))
        .await;
    let end = &failing.ended[&scope];
    assert_eq!(end["reason"], "load_failed");
    assert_eq!(end["error"]["code"], "agent_error");
    peer.settled().await;
    let failure = peer.publication.lock().feed.with_tree(|tree| {
        tree.get("/global/loadFailures/saved").expect("the failed load is recorded").to_value()
    });
    assert_eq!(failure["code"], "agent_error", "materialization cleanup keeps the Agent's original failure");
}

fn deletable() -> Value {
    json!({"loadSession": true, "sessionCapabilities": {"delete": {}}})
}

#[tokio::test]
async fn review_a_subscription_after_delete_admission_waits_for_its_end() {
    let mut peer = Peer::start(deletable()).await;
    let scope = session(SAVED);
    peer.start_op("global", "delete", json!({"sessionId": SAVED}))
        .await;

    // An admitted delete that has not ended occupies the session: a subscription opened
    // meanwhile starts no load and waits for its end.
    let mut stream = Stream::open(&peer, &[scope.as_str()], Some(CWD));
    let delete = peer.request("session/delete").await;
    peer.quiet(Duration::from_millis(200)).await;
    stream.take_all(&mut peer);
    assert_eq!(stream.events_of(&scope).len(), 1, "the occupied scope only reports absence");
    assert_eq!(stream.events_of(&scope)[0]["end"]["reason"], "absent");
    assert!(!stream.replicas.contains_key(&scope) && stream.ended.is_empty(), "the occupied scope waits");
    peer.reply(&delete, json!({})).await;
    stream
        .until(&mut peer, "the admitted delete ends", |stream| {
            stream.ended.contains_key(&scope)
        })
        .await;
    assert_eq!(stream.events_of(&scope).len(), 2, "deleted follows the initial absence");
    assert_eq!(stream.ended[&scope]["reason"], "deleted");
    peer.quiet(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn a_subscription_waits_for_a_cold_delete_and_ends_with_deleted() {
    let mut peer = Peer::start(deletable()).await;
    let scope = session(SAVED);
    peer.start_op("global", "delete", json!({"sessionId": SAVED}))
        .await;
    let delete = peer.request("session/delete").await;
    let mut stream = Stream::open(&peer, &[scope.as_str()], Some(CWD));
    peer.quiet(Duration::from_millis(200)).await;
    stream.take_all(&mut peer);
    assert_eq!(stream.events_of(&scope).len(), 1, "the stream only reports absence");
    assert_eq!(stream.events_of(&scope)[0]["end"]["reason"], "absent");
    assert!(!stream.replicas.contains_key(&scope) && stream.ended.is_empty(), "the stream waits");
    peer.reply(&delete, json!({})).await;
    stream
        .until(&mut peer, "the end", |stream| stream.ended.contains_key(&scope))
        .await;
    assert_eq!(stream.events_of(&scope).len(), 2, "deleted follows the initial absence");
    assert_eq!(stream.ended[&scope]["reason"], "deleted");
    peer.quiet(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn a_subscription_waiting_for_a_failed_cold_delete_loads_the_session() {
    let mut peer = Peer::start(deletable()).await;
    let scope = session(SAVED);
    peer.start_op("global", "delete", json!({"sessionId": SAVED}))
        .await;
    let delete = peer.request("session/delete").await;
    let mut stream = Stream::open(&peer, &[scope.as_str()], Some(CWD));
    peer.refuse(&delete).await;
    peer.serve_load(SAVED, &turns(&[("one", "1")])).await;
    stream
        .until(&mut peer, "the load ends", |stream| {
            stream.live(&scope)["op"]["kind"] == "load"
                && stream.live(&scope)["op"]["status"] == "completed"
        })
        .await;
    assert!(stream.ended.is_empty());
    peer.settled().await;
    stream.converged(&peer);
}
