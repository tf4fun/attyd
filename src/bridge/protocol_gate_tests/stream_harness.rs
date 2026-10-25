//! The harness of the stream tests: a client's subscription of the feed with its own replicas,
//! and the scripted Agent's side of a load.
use super::*;


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
    pub(super) subscription: Option<TreeSubscription>,
    pub(super) client: Client,
    pub(super) replicas: BTreeMap<String, Document>,
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
            client: Client::following(&scopes.iter().map(|scope| path_of(scope)).collect::<Vec<_>>().iter().map(String::as_str).collect::<Vec<_>>()),
            replicas: BTreeMap::new(),
            ended: BTreeMap::new(),
            events: Vec::new(),
        };
        stream.subscribe(peer, cwd);
        stream
    }

    /// Subscribes with the versions the replicas hold (protocol §5.1).
    pub(super) fn subscribe(&mut self, peer: &Peer, cwd: Option<&str>) {
        let query = StreamQuery {
            instance: self.client.instance.clone(),
            follow: self.scopes.iter().map(|scope| {
                let path = path_of(&scope.to_string());
                let have = self.client.value(&path).map(|_| self.client.version(&path));
                Follow { path, have, window: None }
            }).collect(),
            cwd: self.scopes.iter().filter_map(|scope| match scope {
                Scope::Session(id) => cwd.map(|cwd| (id.clone(), cwd.to_string())),
                _ => None,
            }).collect(),
        };
        self.subscription = Some(peer.subscribe(query));
    }

    /// Closes the stream and subscribes again with the versions it holds.
    pub(super) fn reopen(&mut self, peer: &Peer, cwd: Option<&str>) {
        self.subscription = None;
        self.subscribe(peer, cwd);
    }

    pub(super) fn close(&mut self) {
        self.subscription = None;
    }

    fn take(&mut self, event: Event) {
        use crate::protocol::tree_stream::documents::scope_of;
        self.client.apply(&event).unwrap_or_else(|error| panic!("{error}: {event:?}"));
        self.replicas.clear();
        for (path, held) in &self.client.paths {
            let Some(scope) = scope_of(path) else { continue };
            if let Some(doc) = held.value.as_ref().and_then(|value| document(&scope, value)) {
                self.replicas.insert(scope, Document { doc, rev: held.version });
            }
        }
        for change in event.changes.iter().flatten() {
            let Some(scope) = scope_of(change.path()) else { continue };
            let change = serde_json::to_value(change).unwrap();
            if let Some(end) = change.get("end").filter(|end| end["reason"] != "absent") {
                let mut end = end.clone();
                if let Some(agent) = end["error"].get("agentCode").cloned() {
                    end["error"]["data"] = json!({ "code": agent });
                }
                self.ended.insert(scope, end);
            } else if self.replicas.contains_key(&scope) {
                self.ended.remove(&scope);
            }
        }
        self.events.push(serde_json::to_value(event).unwrap());
    }

    /// Takes every queued event.
    pub(super) fn take_all(&mut self, peer: &mut Peer) {
        peer.drain();
        while let Some(event) = self.subscription.as_ref().and_then(TreeSubscription::try_next) {
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
        self.replicas.get(scope).map_or(Value::Null, Document::live)
    }

    pub(super) fn entries(&self, scope: &str) -> Vec<Value> {
        self.replicas.get(scope).map_or_else(Vec::new, Document::entries)
    }

    pub(super) fn events_of(&self, scope: &str) -> Vec<Value> {
        self.events.iter().flat_map(|event| event["changes"].as_array().into_iter().flatten())
            .filter(|change| change["path"] == path_of(scope)).cloned().collect()
    }

    /// Every followed scope equals what the bridge publishes now.
    pub(super) fn converged(&self, peer: &Peer) {
        let documents = peer.publication.lock().feed.documents();
        for (scope, replica) in &self.replicas {
            assert_eq!(self.client.value(&path_of(scope)), documents.get(scope), "{scope}: native node");
            assert_eq!(replica.doc, document(scope, &documents[scope]).unwrap(), "{scope}");
        }
    }
}

impl Peer {
    /// A read of `path` the way `GET /api/v2/nodes/{path}` serves it (protocol revision 4 §7.1): a
    /// short subscription that asks the bridge to serve the streams, answers with its first event
    /// and is dropped.
    pub(super) async fn poll(&self, path: &str, cwd: Option<&str>) -> Value {
        use crate::protocol::tree_stream::params::{Follow, StreamQuery, session_of};
        let query = StreamQuery {
            instance: None,
            follow: vec![Follow { path: path.to_string(), have: None, window: None }],
            cwd: session_of(path).zip(cwd.map(String::from)).into_iter().collect(),
        };
        let subscription = self.publication.lock().feed.subscribe_tree_reading(&query);
        self.commands.send(BridgeInput::ServeStreams).unwrap();
        let first = subscription.next_json().await.expect("the feed is open");
        serde_json::from_str(&first).expect("an event is JSON")
    }

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

    /// What the next generation takes over once this connection ended.
    pub(super) fn handover(&mut self) -> Publication {
        assert!(self.task.is_none(), "the connection ended");
        self.publication.clone()
    }

    /// The next generation, publishing into `publication`: started and ready.
    pub(super) async fn start_on(publication: Publication, capabilities: Value) -> Self {
        let mut peer = Self::connect_on(publication, |_| {}).await;
        let initialize = peer.next_request().await;
        assert_eq!(initialize["method"], "initialize");
        peer.reply(
            &initialize,
            json!({
                "protocolVersion": 1, "agentCapabilities": capabilities, "authMethods": [],
            }),
        )
        .await;
        peer.until("the agent is ready", |peer| {
            peer.live("global")["agent"]["phase"] == "ready"
        })
        .await;
        peer
    }

    /// Runs one whole turn of `session_id` with `reply`.
    pub(super) async fn whole_turn(&mut self, session_id: &str, prompt: &str, reply: &str) {
        let scope = session(session_id);
        let (op, request) = self.prompt(session_id, vec![text(prompt)]).await;
        self.update(session_id, chunk(&Uuid::new_v4().to_string(), text(reply)))
            .await;
        self.reply(&request, json!({"stopReason": "end_turn"})).await;
        self.until("the turn completes", |peer| {
            peer.live(&scope)["op"]["id"] == op && peer.live(&scope)["op"]["status"] == "completed"
        })
        .await;
    }

    /// Whether the feed publishes `scope`.
    pub(super) fn published(&self, scope: &str) -> bool {
        self.publication.lock().feed.with_tree(|tree| tree.get(&path_of(scope)).is_some())
    }
}

