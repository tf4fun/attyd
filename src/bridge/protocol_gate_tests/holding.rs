//! When the bridge holds a session and when it lets it go (bridge memory model §8.4, protocol
//! S6), through the real coordinator and ACP transport. A stream is a subscription of the feed
//! read the way a client reads it, with its own replicas; the published documents are only
//! compared with.
use super::stream_harness::*;
use super::*;

#[tokio::test]
async fn subscribers_hold_a_loadable_session_and_the_last_one_leaving_releases_it() {
    let replay = turns(&[("one", "1")]);
    let mut peer = Peer::start_with(loadable(), json!([]), |options| {
        options.session_unobserved_timeout = 0;
    })
    .await;
    let scope = session(SAVED);
    let mut first = Stream::open(&peer, &[scope.as_str()], Some(CWD));
    peer.serve_load(SAVED, &replay).await;
    first
        .until(&mut peer, "the load ends", |stream| {
            stream.live(&scope)["op"]["status"] == "completed"
        })
        .await;
    let mut second = Stream::open(&peer, &["global", scope.as_str()], None);
    second
        .until(&mut peer, "the alignment", |stream| {
            stream.replicas.contains_key(&scope)
        })
        .await;
    let globals = Stream::open(&peer, &["global", "catalog"], None);
    assert_eq!(peer.followers(SAVED), 2, "global and catalog hold nothing");

    first.close();
    peer.until_followers(SAVED, 1).await;
    peer.quiet(Duration::from_millis(300)).await;
    second.close();
    peer.until_followers(SAVED, 0).await;
    let close = peer.request("session/close").await;
    assert_eq!(close["params"]["sessionId"], SAVED);
    peer.reply(&close, json!({})).await;
    peer.until("the session is released", |peer| peer.released(&scope))
        .await;
    assert!(
        peer.events_of(&scope).iter().all(|event| event.get("end").is_none()),
        "a release sends no end"
    );
    assert!(
        peer.events_of(&scope).last().is_some_and(|change| {
            patch_paths(change).iter().any(|(op, path)| op == "remove" && path.is_empty())
        }),
        "R4 removes the released session from the followed sessions map"
    );
    assert!(!peer.published(&scope));
    let cached = first.entries(&scope);
    assert_eq!(cached.len(), 2, "the client keeps its copy");
    drop(globals);

    // A later subscription loads it again; the same history gives the same entries.
    let seen = first.events.len();
    first.reopen(&peer, Some(CWD));
    peer.serve_load(SAVED, &replay).await;
    first
        .until(&mut peer, "the reload ends", |stream| {
            stream.events.len() > seen
                && stream.live(&scope)["op"]["status"] == "completed"
                && stream.live(&scope)["op"]["kind"] == "load"
        })
        .await;
    assert_eq!(first.entries(&scope), cached);
    peer.settled().await;
    first.converged(&peer);
}

#[tokio::test]
async fn a_loadable_session_without_close_is_released_locally() {
    let replay = turns(&[("one", "1")]);
    let mut peer = Peer::start_with(json!({"loadSession": true}), json!([]), |options| {
        options.session_unobserved_timeout = 0;
    })
    .await;
    let scope = session(SAVED);
    let mut stream = Stream::open(&peer, &[scope.as_str()], Some(CWD));
    peer.serve_load(SAVED, &replay).await;
    stream
        .until(&mut peer, "the load ends", |stream| {
            stream.live(&scope)["op"]["status"] == "completed"
        })
        .await;
    let cached = stream.entries(&scope);
    stream.close();
    peer.until("the session is released", |peer| peer.released(&scope))
        .await;
    peer.quiet(Duration::from_millis(200)).await;
    assert!(
        peer.events_of(&scope).iter().all(|event| event.get("end").is_none()),
        "a release sends no end"
    );
    assert!(
        peer.events_of(&scope).last().is_some_and(|change| {
            patch_paths(change).iter().any(|(op, path)| op == "remove" && path.is_empty())
        }),
        "R4 removes the released session from the followed sessions map"
    );
    assert!(!peer.published(&scope));

    let seen = stream.events.len();
    stream.reopen(&peer, Some(CWD));
    peer.serve_load(SAVED, &replay).await;
    stream
        .until(&mut peer, "the reload ends", |stream| {
            stream.events.len() > seen
                && stream.live(&scope)["op"]["status"] == "completed"
                && stream.live(&scope)["op"]["kind"] == "load"
        })
        .await;
    assert_eq!(stream.entries(&scope), cached);
    peer.settled().await;
    stream.converged(&peer);
}

#[tokio::test]
async fn a_session_released_before_its_first_subscription_is_loaded_by_it() {
    let mut peer = Peer::start_with(loadable(), json!([]), |options| {
        options.session_unobserved_timeout = 0;
    })
    .await;
    peer.create(CREATED, json!({})).await;
    let close = peer.request("session/close").await;
    peer.reply(&close, json!({})).await;
    let scope = session(CREATED);
    peer.until("the unneeded session is released", |peer| peer.released(&scope))
        .await;
    let mut stream = Stream::open(&peer, &[scope.as_str()], Some(CWD));
    peer.serve_load(CREATED, &[]).await;
    stream
        .until(&mut peer, "the load ends", |stream| {
            stream.live(&scope)["op"]["status"] == "completed"
        })
        .await;
    assert!(stream.entries(&scope).is_empty());
    peer.settled().await;
    stream.converged(&peer);
}

#[tokio::test]
async fn a_session_the_agent_can_neither_load_nor_resume_stays_held() {
    let mut peer = Peer::start_with(
        json!({"sessionCapabilities": {"close": {}}}),
        json!([]),
        |options| options.session_unobserved_timeout = 0,
    )
    .await;
    peer.create(CREATED, json!({})).await;
    let scope = session(CREATED);
    let mut stream = Stream::open(&peer, &[scope.as_str()], None);
    stream
        .until(&mut peer, "the alignment", |stream| {
            stream.replicas.contains_key(&scope)
        })
        .await;
    stream.close();
    peer.until_followers(CREATED, 0).await;
    // Neither an ACP close nor a local retirement.
    peer.quiet(Duration::from_millis(500)).await;
    peer.settled().await;
    assert!(!peer.released(&scope), "the session stays held");
    let mut late = Stream::open(&peer, &[scope.as_str()], None);
    late.until(&mut peer, "the late alignment", |stream| {
        stream.replicas.contains_key(&scope)
    })
    .await;
    assert!(late.entries(&scope).is_empty());
    late.converged(&peer);
}

#[tokio::test]
async fn a_session_the_agent_can_only_resume_is_released() {
    let mut peer = Peer::start_with(
        json!({"sessionCapabilities": {"resume": {}, "close": {}}}),
        json!([]),
        |options| options.session_unobserved_timeout = 0,
    )
    .await;
    let scope = session(SAVED);
    let mut stream = Stream::open(&peer, &[scope.as_str()], Some(CWD));
    let resume = peer.request("session/resume").await;
    assert_eq!(resume["params"]["cwd"], CWD);
    peer.reply(&resume, json!({})).await;
    stream
        .until(&mut peer, "the resume ends", |stream| {
            stream.live(&scope)["op"]["status"] == "completed"
        })
        .await;
    stream.close();
    let close = peer.request("session/close").await;
    peer.reply(&close, json!({})).await;
    peer.until("the session is released", |peer| peer.released(&scope))
        .await;
}

/// A loadable session, created and then followed by one stream, with a release timer of one
/// second.
async fn followed(capabilities: Value) -> (Peer, Stream) {
    let mut peer = Peer::start_with(capabilities, json!([]), |options| {
        options.session_unobserved_timeout = 1;
    })
    .await;
    peer.create(CREATED, json!({})).await;
    let scope = session(CREATED);
    let mut stream = Stream::open(&peer, &[scope.as_str()], None);
    stream
        .until(&mut peer, "the alignment", |stream| {
            stream.replicas.contains_key(&scope)
        })
        .await;
    (peer, stream)
}

#[tokio::test]
async fn an_accepted_url_flow_alone_keeps_the_session_needed() {
    for request_scoped in [false, true] {
        let (mut peer, mut stream) = followed(loadable()).await;
        let scope = session(CREATED);
        let prompt = vec![text("hi")];
        let (_, request) = peer.prompt(CREATED, prompt).await;
        let elicitation_scope = if request_scoped {
            json!({"requestId": request["id"]})
        } else {
            json!({"sessionId": CREATED})
        };
        peer.agent_request(
            "sign-in",
            "elicitation/create",
            url_elicitation(elicitation_scope, "flow"),
        )
        .await;
        stream
            .until(&mut peer, "the URL elicitation", |stream| {
                interaction_ids(&stream.live(&scope)).len() == 1
            })
            .await;
        let interaction = interaction_ids(&stream.live(&scope)).remove(0);
        peer.answer(
            &scope,
            &interaction,
            json!({"response": {"action": "accept"}}),
        )
        .await;
        peer.response("sign-in").await;
        peer.reply(&request, json!({"stopReason": "end_turn"})).await;
        stream
            .until(&mut peer, "the prompt ends with its flow open", |stream| {
                let live = stream.live(&scope);
                live["op"]["status"] == "completed" && live["urlFlows"].get(&interaction).is_some()
            })
            .await;
        stream.close();
        peer.until_followers(CREATED, 0).await;
        peer.quiet(BEYOND_TIMER).await;
        peer.settled().await;
        assert!(!peer.released(&scope), "request_scoped={request_scoped}");

        peer.send(json!({
            "jsonrpc": "2.0", "method": "elicitation/complete",
            "params": {"elicitationId": "flow"},
        }))
        .await;
        let close = peer.request("session/close").await;
        peer.reply(&close, json!({})).await;
        peer.until("the session is released", |peer| peer.released(&scope))
            .await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn an_exited_terminal_not_yet_released_keeps_the_session_needed() {
    // The bridge runs Agent terminals for a stdio Agent only, in a real workspace.
    let workspace = tempfile::tempdir().unwrap();
    let cwd = workspace.path().to_string_lossy().into_owned();
    let mut peer = Peer::start_with(loadable(), json!([]), |options| {
        options.transport = Transport::Stdio;
        options.command = vec!["/bin/sh".into()];
        options.cwd = workspace.path().to_path_buf();
        options.session_unobserved_timeout = 1;
    })
    .await;
    peer.create_in(CREATED, &cwd, json!({})).await;
    let scope = session(CREATED);
    let mut stream = Stream::open(&peer, &[scope.as_str()], None);
    stream
        .until(&mut peer, "the alignment", |stream| {
            stream.replicas.contains_key(&scope)
        })
        .await;
    let prompt = vec![text("run")];
    let (_, request) = peer.prompt(CREATED, prompt).await;
    peer.agent_request(
        "terminal",
        "terminal/create",
        json!({"sessionId": CREATED, "command": "/bin/sh", "args": ["-c", "exit 0"]}),
    )
    .await;
    let created = peer.response("terminal").await;
    let terminal = created["result"]["terminalId"].as_str().unwrap().to_string();
    stream
        .until(&mut peer, "the terminal exits", |stream| {
            stream.live(&scope)["terminals"]
                .as_object()
                .is_some_and(|terminals| {
                    terminals
                        .values()
                        .any(|terminal| !terminal["exitStatus"].is_null())
                })
        })
        .await;
    peer.reply(&request, json!({"stopReason": "end_turn"})).await;
    stream
        .until(&mut peer, "the prompt ends", |stream| {
            stream.live(&scope)["op"]["status"] == "completed"
        })
        .await;
    stream.close();
    peer.until_followers(CREATED, 0).await;
    peer.quiet(BEYOND_TIMER).await;
    peer.settled().await;
    assert!(!peer.released(&scope), "an unreleased terminal keeps the need");

    peer.agent_request(
        "release",
        "terminal/release",
        json!({"sessionId": CREATED, "terminalId": terminal}),
    )
    .await;
    peer.response("release").await;
    let close = peer.request("session/close").await;
    peer.reply(&close, json!({})).await;
    peer.until("the session is released", |peer| peer.released(&scope))
        .await;
}

#[tokio::test]
async fn a_new_subscription_or_an_occupation_cancels_the_release_timer() {
    let (mut peer, mut stream) = followed(loadable()).await;
    let scope = session(CREATED);
    stream.close();
    peer.until_followers(CREATED, 0).await;
    stream.reopen(&peer, None);
    peer.quiet(BEYOND_TIMER).await;
    peer.settled().await;
    assert!(!peer.released(&scope), "a new subscription cancels the timer");

    stream.close();
    peer.until_followers(CREATED, 0).await;
    peer.install(
        "global",
        OperationKind::Delete,
        json!({"sessionId": CREATED}),
    )
    .await;
    peer.quiet(BEYOND_TIMER).await;
    peer.settled().await;
    assert!(!peer.released(&scope), "an occupation keeps the need");
}

#[tokio::test]
async fn review_a_sessions_own_admitted_operation_keeps_it_held_before_dispatch() {
    let (mut peer, mut stream) = followed(loadable()).await;
    let scope = session(CREATED);
    let op = peer
        .install(&scope, OperationKind::Prompt, json!({"prompt": [text("waiting")]}))
        .await;
    stream
        .until(&mut peer, "the admitted operation", |stream| {
            stream.live(&scope)["op"]["id"] == op
        })
        .await;
    // The operation owns this session before its ACP task starts. Dropping the last observer
    // must not let a retirement timer close its target.
    stream.close();
    peer.until_followers(CREATED, 0).await;
    peer.quiet(BEYOND_TIMER).await;
    peer.settled().await;
    assert!(!peer.released(&scope), "the session's own operation holds it");
    assert_eq!(peer.live(&scope)["op"]["id"], op);
}

#[tokio::test]
async fn a_client_that_only_polls_opens_a_session_holds_it_while_it_polls_and_lets_it_go_after() {
    let replay = turns(&[("one", "1")]);
    let mut peer = Peer::start_with(loadable(), json!([]), |options| {
        options.session_unobserved_timeout = 1;
    })
    .await;
    let scope = session(SAVED);
    let path = format!("/sessions/{SAVED}/entries");

    // The first poll finds nothing yet and starts the load with the directory it gave.
    let first = peer.poll(&path, Some(CWD)).await;
    assert_eq!(first["changes"][0]["end"]["reason"], "absent");
    peer.serve_load(SAVED, &replay).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let polled = peer.poll(&path, Some(CWD)).await;
        if polled["changes"][0]["snapshot"]["node"]["v"].as_array().is_some_and(|entries| entries.len() == 2) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "never loaded: {polled}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Polling every half second, within the timer of one second, keeps the session for longer
    // than the timer.
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        peer.poll(&path, None).await;
        assert!(!peer.released(&scope), "a poll restarts the unobserved timer");
    }

    // Once the client stops, the existing rule releases the session after the timeout.
    let close = peer.request("session/close").await;
    assert_eq!(close["params"]["sessionId"], SAVED);
    peer.reply(&close, json!({})).await;
    peer.until("the session is released", |peer| peer.released(&scope)).await;
}
