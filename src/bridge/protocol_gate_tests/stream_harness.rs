//! The harness of the stream tests: a client's subscription of the feed with its own replicas,
//! and the scripted Agent's side of a load.
use super::*;
use crate::protocol::subscription::Have;

pub(super) const SAVED: &str = "saved";
pub(super) const CWD: &str = "/workspace";
pub(super) const BEYOND_TIMER: Duration = Duration::from_millis(1500);

pub(super) fn session(session_id: &str) -> String {
    format!("session/{session_id}")
}

pub(super) fn user(content: Value) -> Value {
    json!({"sessionUpdate": "user_message_chunk", "content": content})
}

/// The replay of turns `(prompt, reply)`.
pub(super) fn turns(turns: &[(&str, &str)]) -> Vec<Value> {
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

pub(super) fn loadable() -> Value {
    json!({"loadSession": true, "sessionCapabilities": {"close": {}}})
}

/// One client's stream: a subscription of the feed and the replicas it keeps. The replicas
/// outlive the subscription, as a client keeps its copies across streams.
pub(super) struct Stream {
    scopes: Vec<Scope>,
    pub(super) subscription: Option<FeedSubscription>,
    pub(super) replicas: BTreeMap<String, Replica>,
    /// How each scope ended, if it did.
    pub(super) ended: BTreeMap<String, Value>,
    /// Every event taken, in order.
    pub(super) events: Vec<Value>,
}

impl Stream {
    pub(super) fn open(peer: &Peer, scopes: &[&str], cwd: Option<&str>) -> Self {
        let mut stream = Self {
            scopes: scopes.iter().map(|scope| Scope::parse(scope).unwrap()).collect(),
            subscription: None,
            replicas: BTreeMap::new(),
            ended: BTreeMap::new(),
            events: Vec::new(),
        };
        stream.subscribe(peer, cwd);
        stream
    }

    /// Subscribes with the versions the replicas hold (protocol §5.1).
    pub(super) fn subscribe(&mut self, peer: &Peer, cwd: Option<&str>) {
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
    pub(super) fn reopen(&mut self, peer: &Peer, cwd: Option<&str>) {
        self.subscription = None;
        self.subscribe(peer, cwd);
    }

    pub(super) fn close(&mut self) {
        self.subscription = None;
    }

    fn take(&mut self, event: crate::protocol::engine::StreamEvent) {
        let event = serde_json::to_value(event).unwrap();
        let scope = event["scope"].as_str().unwrap().to_string();
        if let Some(end) = event.get("end") {
            self.replicas.remove(&scope);
            self.ended.insert(scope, end.clone());
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
    pub(super) fn take_all(&mut self, peer: &mut Peer) {
        peer.drain();
        while let Some(event) = self.subscription.as_ref().and_then(FeedSubscription::try_next) {
            self.take(event);
        }
    }

    /// Takes events until `condition` holds of the stream.
    pub(super) async fn until(&mut self, peer: &mut Peer, description: &str, condition: impl Fn(&Self) -> bool) {
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

    pub(super) fn live(&self, scope: &str) -> Value {
        self.replicas.get(scope).map_or(Value::Null, Replica::live)
    }

    pub(super) fn entries(&self, scope: &str) -> Vec<Value> {
        self.replicas.get(scope).map_or_else(Vec::new, Replica::entries)
    }

    pub(super) fn events_of(&self, scope: &str) -> Vec<Value> {
        self.events.iter().filter(|event| event["scope"] == scope).cloned().collect()
    }

    /// Every followed scope equals what the bridge publishes now.
    pub(super) fn converged(&self, peer: &Peer) {
        let documents = peer.publication.lock().feed.documents();
        for (scope, replica) in &self.replicas {
            let published = Replica::from_snapshot(&documents[scope]);
            assert_eq!(replica.doc, published.doc, "{scope}");
        }
    }
}

impl Peer {
    /// Nothing reaches the Agent for `period`.
    pub(super) async fn quiet(&mut self, period: Duration) {
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
    pub(super) fn followers(&self, session_id: &str) -> usize {
        self.publication.lock().feed.followers(&session(session_id))
    }

    pub(super) async fn until_followers(&self, session_id: &str, count: usize) {
        tokio::time::timeout(WAIT, async {
            while self.followers(session_id) != count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{session_id} did not reach {count} followers"));
    }

    /// Answers the `session/load` the bridge sends for `session_id` with `replay`.
    pub(super) async fn serve_load(&mut self, session_id: &str, replay: &[Value]) -> Value {
        let load = self.request("session/load").await;
        assert_eq!(load["params"]["sessionId"], session_id);
        for update in replay {
            self.update(session_id, update.clone()).await;
        }
        self.reply(&load, json!({})).await;
        load
    }

    pub(super) fn released(&self, scope: &str) -> bool {
        self.gate.released.contains(scope)
    }

    /// Whether the feed publishes `scope`.
    pub(super) fn published(&self, scope: &str) -> bool {
        self.publication
            .lock()
            .feed
            .live(&Scope::parse(scope).unwrap())
            .is_some()
    }
}

