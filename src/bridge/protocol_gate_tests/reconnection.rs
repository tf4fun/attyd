//! The held sessions one Agent connection leaves for the next (protocol S6, §5.4), through the
//! real coordinator and ACP transport: the session waits with the entries it has, and the next
//! connection loads or resumes it, or the session ends with `load_failed`.
use super::stream_harness::*;
use super::*;

/// A loaded `saved`, followed with `global`, on a connection that then ends with an EOF.
async fn loaded_then_eof(replay: &[Value]) -> (Peer, Stream) {
    let mut peer = Peer::start(json!({"loadSession": true})).await;
    let scope = session(SAVED);
    let mut stream = Stream::open(&peer, &["global", scope.as_str()], Some(CWD));
    peer.serve_load(SAVED, replay).await;
    stream
        .until(&mut peer, "the load ends", |stream| {
            stream.live(&scope)["op"]["status"] == "completed"
        })
        .await;
    peer.settled().await;
    peer.eof().await.unwrap();
    stream
        .until(&mut peer, "the waiting load", |stream| {
            let live = stream.live(&scope);
            live["op"]["kind"] == "load" && live["op"]["status"] == "running"
        })
        .await;
    assert_ne!(
        stream.live("global")["agent"]["phase"],
        "ready",
        "global shows why"
    );
    (peer, stream)
}

async fn next_generation(peer: &mut Peer, capabilities: Value) -> Peer {
    let publication = peer.handover();
    Peer::start_on(publication, capabilities).await
}

#[tokio::test]
async fn the_next_connections_equal_replay_ends_the_waiting_load() {
    let replay = turns(&[("one", "1"), ("two", "2")]);
    let (mut old, mut stream) = loaded_then_eof(&replay).await;
    let scope = session(SAVED);
    let cached = stream.entries(&scope);
    assert_eq!(cached.len(), 4);
    let mut peer = next_generation(&mut old, json!({"loadSession": true})).await;
    let load = peer.serve_load(SAVED, &replay).await;
    assert_eq!(load["params"]["cwd"], CWD, "the session's earlier directory");
    stream
        .until(&mut peer, "the load ends", |stream| {
            stream.live(&scope)["op"]["status"] == "completed"
        })
        .await;
    assert_eq!(stream.entries(&scope), cached, "the same history keeps the entries");
    assert_eq!(stream.live("global")["agent"]["phase"], "ready");
    peer.settled().await;
    stream.converged(&peer);
}

#[tokio::test]
async fn the_next_connections_diverging_replay_replaces_what_differs() {
    let (mut old, mut stream) = loaded_then_eof(&turns(&[("one", "1"), ("two", "2")])).await;
    let scope = session(SAVED);
    let cached = stream.entries(&scope);
    let mut peer = next_generation(&mut old, json!({"loadSession": true})).await;
    peer.serve_load(SAVED, &turns(&[("one", "1"), ("other", "3")]))
        .await;
    stream
        .until(&mut peer, "the load ends", |stream| {
            stream.live(&scope)["op"]["status"] == "completed"
        })
        .await;
    let entries = stream.entries(&scope);
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[..2], cached[..2], "the common prefix is the same");
    assert_ne!(entries[2..], cached[2..], "what the Agent replays now replaces the rest");
    peer.settled().await;
    stream.converged(&peer);
}

#[tokio::test]
async fn review_an_unfollowed_session_is_released_when_the_agent_connection_ends() {
    let mut peer = Peer::start(json!({})).await;
    peer.create(CREATED, json!({})).await;
    let scope = session(CREATED);
    let mut stream = Stream::open(&peer, &["global"], None);
    stream
        .until(&mut peer, "the global alignment", |stream| {
            stream.live("global")["sessions"].get(CREATED).is_some()
        })
        .await;
    assert_eq!(peer.followers(CREATED), 0);
    assert!(peer.published(&scope));

    // EOF completes the final commit; this test starts no next generation.
    peer.eof().await.unwrap();
    stream.take_all(&mut peer);
    assert_eq!(stream.live("global")["agent"]["phase"], "stopped");
    assert!(
        !peer.published(&scope),
        "an unneeded session must leave the feed at EOF, without waiting for another connection"
    );
    let global = peer.publication.lock().feed.live(&Scope::Global).unwrap();
    assert!(
        global["sessions"].get(CREATED).is_none(),
        "the final global document must no longer list the released session"
    );
    assert!(stream.live("global")["sessions"].get(CREATED).is_none());
    stream.converged(&peer);
}

#[tokio::test]
async fn a_session_the_agent_cannot_load_again_ends_with_load_failed_at_the_connection_end() {
    let mut peer = Peer::start(json!({})).await;
    peer.create(CREATED, json!({})).await;
    let scope = session(CREATED);
    let mut stream = Stream::open(&peer, &["global", scope.as_str()], None);
    stream
        .until(&mut peer, "the alignment", |stream| stream.replicas.len() == 2)
        .await;
    peer.eof().await.unwrap();
    stream
        .until(&mut peer, "the end", |stream| stream.ended.contains_key(&scope))
        .await;
    assert_eq!(stream.ended[&scope]["reason"], "load_failed");
    assert_eq!(stream.live("global")["agent"]["phase"], "stopped");
    stream.converged(&peer);
}

// Resume is a valid way to attach after a connection ends, but ACP does not promise that it
// replays history. The bridge must keep the entries it already owns.
async fn resume_on_the_next_connection_keeps_the_known_entries(
    old: &mut Peer,
    stream: &mut Stream,
    session_id: &str,
    cached: &[Value],
) {
    let scope = session(session_id);
    let capabilities = json!({"sessionCapabilities": {"resume": {}}});
    let mut peer = next_generation(old, capabilities).await;
    stream.take_all(&mut peer);
    assert!(
        !stream.ended.contains_key(&scope),
        "resume support keeps the original subscription alive across connections"
    );
    assert_eq!(stream.entries(&scope), cached);
    let resume = peer.request("session/resume").await;
    assert_eq!(resume["params"]["sessionId"], session_id);
    assert_eq!(resume["params"]["cwd"], CWD);
    // A conforming resume response sends no historical message notifications.
    peer.reply(&resume, json!({})).await;
    stream
        .until(&mut peer, "the resume completes on the original stream", |stream| {
            stream.live(&scope)["op"]["status"] == "completed"
        })
        .await;
    peer.settled().await;
    stream.take_all(&mut peer);
    assert!(stream.ended.is_empty());
    assert_eq!(
        stream.entries(&scope),
        cached,
        "no resume replay is not evidence of an empty history"
    );
    stream.converged(&peer);
    peer.quiet(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn review_resume_only_session_waits_across_eof_and_preserves_its_known_entries() {
    let capabilities = json!({"sessionCapabilities": {"resume": {}}});
    let mut old = Peer::start(capabilities).await;
    old.create_in(CREATED, CWD, json!({})).await;
    let scope = session(CREATED);
    let mut stream = Stream::open(&old, &[scope.as_str()], None);
    old.whole_turn(CREATED, "keep this prompt", "keep this answer").await;
    stream
        .until(&mut old, "the nonempty history", |stream| {
            stream.entries(&scope).len() == 2
        })
        .await;
    let cached = stream.entries(&scope);
    old.eof().await.unwrap();
    stream.take_all(&mut old);
    assert!(
        !stream.ended.contains_key(&scope),
        "an Agent that supports resume can attach this session again"
    );
    assert_eq!(stream.live(&scope)["op"]["kind"], "load");
    assert_eq!(stream.live(&scope)["op"]["status"], "running");
    assert_eq!(stream.entries(&scope), cached);
    resume_on_the_next_connection_keeps_the_known_entries(&mut old, &mut stream, CREATED, &cached)
        .await;
}

#[tokio::test]
async fn review_waiting_session_uses_the_new_connections_resume_capability() {
    let mut old = Peer::start(json!({"loadSession": true})).await;
    let scope = session(SAVED);
    let mut stream = Stream::open(&old, &[scope.as_str()], Some(CWD));
    old.serve_load(SAVED, &turns(&[("one", "1"), ("two", "2")])).await;
    stream
        .until(&mut old, "the initial load completes", |stream| {
            stream.live(&scope)["op"]["status"] == "completed"
        })
        .await;
    let cached = stream.entries(&scope);
    assert_eq!(cached.len(), 4);
    old.eof().await.unwrap();
    stream.take_all(&mut old);
    assert_eq!(stream.live(&scope)["op"]["kind"], "load");
    assert_eq!(stream.live(&scope)["op"]["status"], "running");
    resume_on_the_next_connection_keeps_the_known_entries(&mut old, &mut stream, SAVED, &cached)
        .await;
}

/// Automatic release is still an ordinary ACP close: a refusal keeps the session usable and
/// must not turn an idle timer into a retry loop.
#[tokio::test(start_paused = true)]
async fn review_refused_automatic_close_waits_for_a_new_subscription_cycle() {
    for timeout_seconds in [0, 1] {
        let mut peer = Peer::start_with(loadable(), json!([]), |options| {
            options.session_unobserved_timeout = timeout_seconds;
        })
        .await;
        let scope = session(SAVED);
        let mut stream = Stream::open(&peer, &[scope.as_str()], Some(CWD));
        peer.serve_load(SAVED, &turns(&[("before close", "kept reply")]))
            .await;
        stream
            .until(&mut peer, "the initial load", |stream| {
                stream.live(&scope)["op"]["status"] == "completed"
            })
            .await;
        let history = stream.entries(&scope);
        let before_reopen = stream.events_of(&scope).len();

        stream.close();
        peer.until_followers(SAVED, 0).await;
        let refused = peer.request("session/close").await;
        assert_eq!(refused["params"]["sessionId"], SAVED);
        peer.refuse(&refused).await;
        // Cross several full intervals, including the immediate-release case. No other command
        // or subscription may reset the timer during this wait.
        let several_intervals = Duration::from_secs(timeout_seconds.max(1) as u64 * 3);
        peer.quiet(several_intervals).await;
        peer.settled().await;
        assert!(!peer.released(&scope), "a refused close keeps the session");

        stream.reopen(&peer, None);
        stream
            .until(&mut peer, "the returning subscription", |stream| {
                stream.events_of(&scope).len() > before_reopen
            })
            .await;
        assert_eq!(stream.entries(&scope), history);
        peer.quiet(several_intervals).await;
        let (op, prompt) = peer.prompt(SAVED, vec![text("still usable")]).await;
        peer.update(SAVED, chunk("after-refusal", text("still here")))
            .await;
        peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
        stream
            .until(&mut peer, "the new prompt completes", |stream| {
                stream.live(&scope)["op"]["id"] == op
                    && stream.live(&scope)["op"]["status"] == "completed"
            })
            .await;
        let entries = stream.entries(&scope);
        assert_eq!(entries.len(), history.len() + 2);
        assert_eq!(entries[history.len()]["chunks"][0]["content"], text("still usable"));
        assert_eq!(entries[history.len() + 1]["chunks"][0]["content"], text("still here"));
        peer.settled().await;
        stream.converged(&peer);

        stream.close();
        peer.until_followers(SAVED, 0).await;
        let close = peer.request("session/close").await;
        assert_eq!(close["params"]["sessionId"], SAVED);
        assert_ne!(close["id"], refused["id"]);
        peer.reply(&close, json!({})).await;
        peer.until("the next close releases the session", |peer| peer.released(&scope))
            .await;
        peer.quiet(several_intervals).await;
        peer.settled().await;
    }
}

/// A creation's early controls, sent before its response, stay with the new session after an
/// earlier session was locally released.
#[tokio::test(start_paused = true)]
async fn a_new_sessions_early_controls_survive_its_creation_response() {
    let mut peer = Peer::start_with(json!({"loadSession": true}), json!([]), |options| {
        options.session_unobserved_timeout = 1;
    })
    .await;
    peer.create("old", json!({})).await;
    peer.until("the old session is locally released", |peer| {
        peer.released(&session("old"))
    })
    .await;

    let (_, created) = peer.start_op("global", "create", json!({"cwd": CWD})).await;
    let request = peer.request("session/new").await;
    let commands = json!([{"name": "new-command", "description": "For the new session"}]);
    peer.send(json!([
        {
            "jsonrpc": "2.0", "method": "session/update",
            "params": {"sessionId": "new", "update": {
                "sessionUpdate": "available_commands_update", "availableCommands": commands,
            }},
        },
        {"jsonrpc": "2.0", "id": request["id"], "result": {"sessionId": "new"}},
    ]))
    .await;
    assert_eq!(Peer::ended(created).await.unwrap(), json!({"sessionId": "new"}));

    let scope = session("new");
    let mut stream = Stream::open(&peer, &[scope.as_str()], None);
    stream
        .until(&mut peer, "the newly created session's alignment", |stream| {
            stream.replicas.contains_key(&scope)
        })
        .await;
    assert_eq!(
        stream.live(&scope)["controls"]["commands"],
        commands,
        "the new session keeps its own early controls"
    );
    assert!(peer.live("global")["sessions"].get("old").is_none());
    peer.quiet(Duration::from_secs(2)).await;
    peer.settled().await;
    stream.converged(&peer);
}
