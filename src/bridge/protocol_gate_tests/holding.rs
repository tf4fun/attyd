//! When the bridge holds a session and when it lets it go (bridge memory model §8.4, protocol
//! S6), through the real coordinator and ACP transport. A stream is a subscription of the feed
//! read the way a client reads it, with its own replicas; the published documents are only
//! compared with.
use super::*;
use crate::protocol::subscription::Have;

const SAVED: &str = "saved";
const CWD: &str = "/workspace";
const BEYOND_TIMER: Duration = Duration::from_millis(1500);

fn session(session_id: &str) -> String {
    format!("session/{session_id}")
}

fn user(content: Value) -> Value {
    json!({"sessionUpdate": "user_message_chunk", "content": content})
}

/// The replay of turns `(prompt, reply)`.
fn turns(turns: &[(&str, &str)]) -> Vec<Value> {
    turns
        .iter()
        .enumerate()
        .flat_map(|(index, (prompt, reply))| {
            [
                user(text(prompt)),
                chunk(&format!("reply-{index}"), text(reply)),
            ]
        })
        .collect()
}

fn loadable() -> Value {
    json!({"loadSession": true, "sessionCapabilities": {"close": {}}})
}

/// One client's stream: a subscription of the feed and the replicas it keeps. The replicas
/// outlive the subscription, as a client keeps its copies across streams.
struct Stream {
    scopes: Vec<Scope>,
    subscription: Option<FeedSubscription>,
    replicas: BTreeMap<String, Replica>,
    /// Every event taken, in order.
    events: Vec<Value>,
}

impl Stream {
    fn open(peer: &Peer, scopes: &[&str], cwd: Option<&str>) -> Self {
        let mut stream = Self {
            scopes: scopes.iter().map(|scope| Scope::parse(scope).unwrap()).collect(),
            subscription: None,
            replicas: BTreeMap::new(),
            events: Vec::new(),
        };
        stream.subscribe(peer, cwd);
        stream
    }

    /// Subscribes with the versions the replicas hold (protocol §5.1).
    fn subscribe(&mut self, peer: &Peer, cwd: Option<&str>) {
        let follow = self
            .scopes
            .iter()
            .map(|scope| Subscribed {
                scope: scope.clone(),
                have: self.replicas.get(&scope.to_string()).map(|replica| Have {
                    epoch: replica.epoch.clone(),
                    rev: replica.rev,
                }),
                cwd: cwd.map(String::from),
            })
            .collect::<Vec<_>>();
        self.subscription = Some(peer.subscribe(follow));
    }

    /// Closes the stream and subscribes again with the versions it holds.
    fn reopen(&mut self, peer: &Peer, cwd: Option<&str>) {
        self.subscription = None;
        self.subscribe(peer, cwd);
    }

    fn close(&mut self) {
        self.subscription = None;
    }

    fn take(&mut self, event: crate::protocol::engine::StreamEvent) {
        let event = serde_json::to_value(event).unwrap();
        let scope = event["scope"].as_str().unwrap().to_string();
        if event.get("end").is_some() {
            self.replicas.remove(&scope);
        } else {
            self.replicas
                .entry(scope)
                .or_default()
                .apply(&event)
                .unwrap_or_else(|error| panic!("{error}"));
        }
        self.events.push(event);
    }

    /// Takes every queued event.
    fn take_all(&mut self, peer: &mut Peer) {
        peer.drain();
        while let Some(event) = self.subscription.as_ref().and_then(FeedSubscription::try_next) {
            self.take(event);
        }
    }

    /// Takes events until `condition` holds of the stream.
    async fn until(&mut self, peer: &mut Peer, description: &str, condition: impl Fn(&Self) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            self.take_all(peer);
            if condition(self) {
                return;
            }
            let subscription = self.subscription.clone().expect("an open stream");
            match tokio::time::timeout_at(deadline, subscription.next()).await {
                Ok(Some(event)) => self.take(event),
                Ok(None) => {
                    self.take_all(peer);
                    assert!(condition(self), "the stream closed before {description}");
                    return;
                }
                Err(_) => panic!(
                    "timed out waiting for {description}; replicas: {:#?}",
                    self.replicas
                ),
            }
        }
    }

    fn live(&self, scope: &str) -> Value {
        self.replicas.get(scope).map_or(Value::Null, Replica::live)
    }

    fn entries(&self, scope: &str) -> Vec<Value> {
        self.replicas.get(scope).map_or_else(Vec::new, Replica::entries)
    }

    fn events_of(&self, scope: &str) -> Vec<Value> {
        self.events.iter().filter(|event| event["scope"] == scope).cloned().collect()
    }

    /// Every followed scope equals what the bridge publishes now.
    fn converged(&self, peer: &Peer) {
        let documents = peer.publication.lock().feed.documents();
        for (scope, replica) in &self.replicas {
            let published = Replica::from_snapshot(&documents[scope]);
            assert_eq!(replica.doc, published.doc, "{scope}");
        }
    }
}

impl Peer {
    /// Nothing reaches the Agent for `period`.
    async fn quiet(&mut self, period: Duration) {
        assert!(
            self.buffered.is_empty(),
            "unexpected Agent messages: {:?}",
            self.buffered
        );
        if let Ok(message) = tokio::time::timeout(period, self.requests.next()).await {
            panic!("unexpected Agent message {message:?}");
        }
    }

    /// How many subscriptions follow `session_id`.
    fn followers(&self, session_id: &str) -> usize {
        self.publication.lock().feed.followers(&session(session_id))
    }

    async fn until_followers(&self, session_id: &str, count: usize) {
        tokio::time::timeout(WAIT, async {
            while self.followers(session_id) != count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{session_id} did not reach {count} followers"));
    }

    /// Answers the `session/load` the bridge sends for `session_id` with `replay`.
    async fn serve_load(&mut self, session_id: &str, replay: &[Value]) -> Value {
        let load = self.request("session/load").await;
        assert_eq!(load["params"]["sessionId"], session_id);
        for update in replay {
            self.update(session_id, update.clone()).await;
        }
        self.reply(&load, json!({})).await;
        load
    }

    fn released(&self, scope: &str) -> bool {
        self.gate.released.contains(scope)
    }

    /// Whether the feed publishes `scope`.
    fn published(&self, scope: &str) -> bool {
        self.publication
            .lock()
            .feed
            .live(&Scope::parse(scope).unwrap())
            .is_some()
    }
}

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
