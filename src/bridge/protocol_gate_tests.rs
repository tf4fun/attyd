//! The per-event convergence gate through the real coordinator and ACP
//! transport (docs/state-sync.md §4.1, migration step 4b).
//!
//! A scripted Agent drives `run_connection` over an in-memory transport. The
//! bridge state's protocol tap hands every commit to the test with the
//! documents it published; one replica per scope applies each event at the
//! wire level and is compared with the published document after every event.
//! Operations are installed through the registry's internal entry points and
//! then run on the existing v1 execution path; admission is step 6.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::sync::Weak;

use agent_client_protocol::Lines;
use clap::Parser;
use futures::{SinkExt, StreamExt};

use super::*;
use crate::protocol::Op;
use crate::protocol::canon::ChainHash;
use crate::protocol::diff;
use crate::protocol::documents::{OperationKind, Scope};

const WAIT: Duration = Duration::from_secs(5);

/// A client's copy of one scope, built from wire events only.
#[derive(Debug, Clone, Default)]
struct Replica {
    live: Value,
    log: Vec<Value>,
}

impl Replica {
    fn aligned(document: &crate::protocol::publish::Document) -> Self {
        Self {
            live: document.live.clone(),
            log: document
                .log
                .as_ref()
                .map(|log| {
                    serde_json::to_value(log)
                        .unwrap()
                        .as_array()
                        .unwrap()
                        .clone()
                })
                .unwrap_or_default(),
        }
    }

    fn hash_at(&self, index: usize) -> Option<String> {
        if index == 0 {
            return Some(ChainHash::ZERO.wire());
        }
        self.log
            .get(index - 1)
            .and_then(|entry| entry["hash"].as_str())
            .map(String::from)
    }

    fn apply(&mut self, event: &Value) -> Result<(), String> {
        if let Some(log) = event.get("log") {
            let from = log["from"].as_u64().ok_or("log.from")? as usize;
            if from > self.log.len() || self.hash_at(from).as_deref() != log["prev"].as_str() {
                return Err(format!("log change does not follow the replica: {event}"));
            }
            self.log.truncate(from);
            self.log
                .extend(log["entries"].as_array().ok_or("entries")?.iter().cloned());
        }
        if let Some(live) = event.get("live") {
            if let Some(replace) = live.get("replace") {
                self.live = replace.clone();
            } else {
                let ops: Vec<Op> = serde_json::from_value(live["patch"].clone())
                    .map_err(|error| format!("patch: {error}"))?;
                for op in &ops {
                    diff::apply(&mut self.live, op)
                        .map_err(|error| format!("{op:?} does not apply: {error:?}"))?;
                }
            }
        }
        Ok(())
    }
}

struct Peer {
    commands: mpsc::UnboundedSender<BridgeInput>,
    requests: futures::channel::mpsc::Receiver<String>,
    responses: futures::channel::mpsc::Sender<io::Result<String>>,
    buffered: VecDeque<Value>,
    _events: mpsc::UnboundedReceiver<String>,
    cancellation: CancellationToken,
    task: Option<tokio::task::JoinHandle<Result<(), Error>>>,
    state: Weak<BridgeLock>,
    commits: mpsc::UnboundedReceiver<PublishedCommit>,
    replicas: BTreeMap<String, Replica>,
    /// Every commit's events, in order.
    history: Vec<Vec<Value>>,
    /// Scopes the bridge stopped publishing without an `end` (a release).
    released: BTreeSet<String>,
}

fn options(configure: impl FnOnce(&mut Options)) -> Options {
    let mut options =
        Options::try_parse_from(["attyd", "--transport", "ws", "--", "ws://127.0.0.1:1/acp"])
            .unwrap()
            .normalized()
            .unwrap();
    configure(&mut options);
    options
}

impl Peer {
    /// A connection whose `initialize` the test has not answered yet.
    async fn connect(configure: impl FnOnce(&mut Options)) -> Self {
        let (outgoing, requests) = futures::channel::mpsc::channel::<String>(64);
        let (responses, incoming) = futures::channel::mpsc::channel::<io::Result<String>>(64);
        let transport = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let probe = super::memory_retention_tests::probe_coordinator_state();
        let task = tokio::spawn(run_connection(
            transport,
            Arc::new(options(configure)),
            command_rx,
            EventSink { tx: events.into() },
            cancellation.clone(),
        ));
        let state = probe.await.unwrap();
        let (tap, commits) = mpsc::unbounded_channel();
        // The tap and the cut are taken under one guard, so no commit falls
        // between the aligned replicas and the first tapped event.
        let cut = {
            let lock = state.upgrade().unwrap();
            let mut guard = lock.lock().await;
            guard.protocol.tap(tap)
        };
        let replicas = cut
            .iter()
            .map(|(scope, document)| (scope.clone(), Replica::aligned(document)))
            .collect();
        Self {
            commands,
            requests,
            responses,
            buffered: VecDeque::new(),
            _events: event_rx,
            cancellation,
            task: Some(task),
            state,
            commits,
            replicas,
            history: Vec::new(),
            released: BTreeSet::new(),
        }
    }

    async fn start(capabilities: Value) -> Self {
        Self::start_with(capabilities, json!([]), |_| {}).await
    }

    async fn start_with(
        capabilities: Value,
        auth_methods: Value,
        configure: impl FnOnce(&mut Options),
    ) -> Self {
        let mut peer = Self::connect(configure).await;
        let initialize = peer.next_request().await;
        assert_eq!(initialize["method"], "initialize");
        peer.reply(
            &initialize,
            json!({
                "protocolVersion": 1, "agentCapabilities": capabilities,
                "authMethods": auth_methods,
            }),
        )
        .await;
        peer.until("the agent is ready", |peer| {
            peer.live("global")["agent"]["phase"] == "ready"
        })
        .await;
        peer
    }

    // -- Agent side ----------------------------------------------------------

    /// The next message the bridge wrote to the Agent: a request, a
    /// notification or a response to an Agent request.
    async fn next_request(&mut self) -> Value {
        tokio::time::timeout(WAIT, async {
            loop {
                if let Some(request) = self.buffered.pop_front() {
                    return request;
                }
                let raw = self.requests.next().await.expect("ACP connection ended");
                match serde_json::from_str(&raw).unwrap() {
                    Value::Array(batch) => self.buffered.extend(batch),
                    request => self.buffered.push_back(request),
                }
            }
        })
        .await
        .expect("expected message did not reach the Agent")
    }

    /// The next request with `method`, skipping notifications such as
    /// `session/cancel`.
    async fn request(&mut self, method: &str) -> Value {
        loop {
            let message = self.next_request().await;
            if message["method"] == method {
                return message;
            }
            assert!(
                message.get("id").is_none() || message.get("method").is_none(),
                "unexpected Agent request {message} while waiting for {method}"
            );
        }
    }

    /// The bridge's response to the Agent request `id`.
    async fn response(&mut self, id: &str) -> Value {
        loop {
            let message = self.next_request().await;
            if message.get("method").is_none() && message["id"] == id {
                return message;
            }
        }
    }

    async fn send(&mut self, message: Value) {
        self.responses.send(Ok(message.to_string())).await.unwrap();
    }

    async fn reply(&mut self, request: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": request["id"], "result": result}))
            .await;
    }

    async fn refuse(&mut self, request: &Value) {
        self.send(json!({
            "jsonrpc": "2.0", "id": request["id"],
            "error": {"code": -32600, "message": "Refused by the Agent"},
        }))
        .await;
    }

    async fn update(&mut self, session_id: &str, update: Value) {
        self.send(json!({
            "jsonrpc": "2.0", "method": "session/update",
            "params": {"sessionId": session_id, "update": update},
        }))
        .await;
    }

    async fn agent_request(&mut self, id: &str, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
    }

    // -- Client side ---------------------------------------------------------

    fn submit(&self, command: Value) -> oneshot::Receiver<Result<Value, BridgeRequestError>> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(BridgeInput::BusinessRequest { command, response })
            .unwrap();
        result
    }

    async fn result(
        result: oneshot::Receiver<Result<Value, BridgeRequestError>>,
    ) -> Result<Value, BridgeRequestError> {
        tokio::time::timeout(WAIT, result)
            .await
            .expect("command did not settle")
            .unwrap()
    }

    /// Creates `session_id` through `session/new` and returns its history
    /// revision.
    async fn create(&mut self, session_id: &str, response: Value) -> String {
        let created = self.submit(json!({
            "type": "session/new", "requestId": format!("new-{session_id}"), "cwd": "/workspace",
        }));
        let new = self.request("session/new").await;
        let mut response = response;
        response["sessionId"] = json!(session_id);
        self.reply(&new, response).await;
        let created = Self::result(created).await.unwrap();
        let scope = format!("session/{session_id}");
        self.until("the created session is published", |peer| {
            peer.replicas.contains_key(&scope)
        })
        .await;
        created["view"]["session"]["historyRevision"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Starts a v1 turn of `session_id` and returns the Agent's
    /// `session/prompt` request.
    async fn turn(&mut self, session_id: &str, revision: &str, prompt: Vec<Value>) -> Value {
        let (response, admitted) = oneshot::channel();
        self.commands
            .send(BridgeInput::TurnRequest {
                session_id: session_id.into(),
                history_revision: revision.into(),
                client_intent_id: format!("turn-{}", Uuid::new_v4()),
                prompt,
                response,
            })
            .unwrap();
        tokio::time::timeout(WAIT, admitted)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        self.request("session/prompt").await
    }

    // -- Bridge state ----------------------------------------------------------

    fn lock(&self) -> Arc<BridgeLock> {
        self.state.upgrade().expect("the connection still runs")
    }

    /// Installs a running operation through the registry's internal entry, as
    /// admission will (step 6); the guard's release publishes it.
    async fn install(&mut self, scope: &str, kind: OperationKind, input: Value) -> String {
        let id = {
            let lock = self.lock();
            let mut state = lock.lock().await;
            state
                .sessions
                .install_operation(&Scope::parse(scope).unwrap(), kind, input)
                .unwrap()
        };
        self.until("the installed operation is published", |peer| {
            peer.live(scope)["op"]["id"] == id
        })
        .await;
        id
    }

    async fn install_prompt(&mut self, session_id: &str, prompt: &[Value]) -> String {
        let id = {
            let lock = self.lock();
            let mut state = lock.lock().await;
            state.sessions.install_prompt(session_id, prompt).unwrap()
        };
        let scope = format!("session/{session_id}");
        self.until("the prompt is published", |peer| {
            peer.live(&scope)["op"]["id"] == id
        })
        .await;
        id
    }

    async fn attachment(&self, session_id: &str, attachment_id: &str) -> Option<Value> {
        self.lock()
            .lock()
            .await
            .protocol
            .publisher
            .attachment(session_id, attachment_id)
    }

    // -- The gate ----------------------------------------------------------------

    /// Applies one commit's events, one at a time, checking the replica of each
    /// scope against the document the commit published.
    fn apply(&mut self, commit: PublishedCommit) {
        let mut seen = BTreeSet::new();
        let events: Vec<Value> = commit
            .events
            .iter()
            .map(|event| serde_json::to_value(event).unwrap())
            .collect();
        assert!(!events.is_empty(), "a commit without changes sends nothing");
        for event in &events {
            let scope = event["scope"].as_str().unwrap().to_string();
            assert!(
                seen.insert(scope.clone()),
                "two events for {scope} in one commit: {events:?}"
            );
            if event.get("end").is_some() {
                assert!(
                    self.replicas.remove(&scope).is_some(),
                    "{scope} ended without a replica"
                );
                assert!(
                    !commit.documents.contains_key(&scope),
                    "{scope} ended but is still published"
                );
                continue;
            }
            let replica = self.replicas.entry(scope.clone()).or_default();
            replica
                .apply(event)
                .unwrap_or_else(|error| panic!("{error}"));
            let document = commit
                .documents
                .get(&scope)
                .unwrap_or_else(|| panic!("{scope} changed but is not published"));
            assert_eq!(replica.live, document.live, "live of {scope} after {event}");
            if let Some(log) = &document.log {
                assert_eq!(
                    replica.log,
                    serde_json::to_value(log)
                        .unwrap()
                        .as_array()
                        .unwrap()
                        .clone(),
                    "log of {scope} after {event}"
                );
            }
        }
        self.history.push(events);
    }

    fn drain(&mut self) {
        while let Ok(commit) = self.commits.try_recv() {
            self.apply(commit);
        }
    }

    /// Waits until `condition` holds of the replicas, applying commits as they
    /// arrive.
    async fn until(&mut self, description: &str, condition: impl Fn(&Self) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            self.drain();
            if condition(self) {
                return;
            }
            match tokio::time::timeout_at(deadline, self.commits.recv()).await {
                Ok(Some(commit)) => self.apply(commit),
                Ok(None) => {
                    assert!(condition(self), "the tap closed before {description}");
                    return;
                }
                Err(_) => panic!(
                    "timed out waiting for {description}; replicas: {:#?}",
                    self.replicas
                ),
            }
        }
    }

    /// Nothing is left unpublished: under the bridge lock, every published
    /// scope's replica equals its document, and a commit now would publish
    /// nothing (I1, I2). A scope that stopped being published without `end`
    /// was released (S6).
    async fn settled(&mut self) {
        let lock = self.lock();
        let state = lock.lock().await;
        assert!(
            !state.unpublished(),
            "a transition changed what clients see without committing"
        );
        self.drain();
        let published: BTreeSet<String> = state.protocol.publisher.scopes().into_iter().collect();
        let gone: Vec<String> = self
            .replicas
            .keys()
            .filter(|scope| !published.contains(*scope))
            .cloned()
            .collect();
        for scope in gone {
            self.replicas.remove(&scope);
            self.released.insert(scope);
        }
        for scope in &published {
            let document = state.protocol.publisher.document(scope).unwrap();
            let replica = self
                .replicas
                .get(scope)
                .unwrap_or_else(|| panic!("{scope} is published but never reached the replica"));
            assert_eq!(replica.live, document.live, "live of {scope}");
        }
    }

    /// Stops the connection and applies its last commits.
    async fn stop(&mut self) -> Result<(), Error> {
        self.cancellation.cancel();
        self.finish().await
    }

    /// Waits for the connection to end on its own and applies its last commits.
    async fn finish(&mut self) -> Result<(), Error> {
        let result = tokio::time::timeout(WAIT, self.task.take().unwrap())
            .await
            .expect("the connection did not stop")
            .unwrap();
        while let Some(commit) = self.commits.recv().await {
            self.apply(commit);
        }
        result
    }

    fn live(&self, scope: &str) -> Value {
        self.replicas
            .get(scope)
            .map_or(Value::Null, |replica| replica.live.clone())
    }

    fn log(&self, scope: &str) -> Vec<Value> {
        self.replicas
            .get(scope)
            .map_or_else(Vec::new, |replica| replica.log.clone())
    }

    /// The events `scope` received, in order.
    fn events_of(&self, scope: &str) -> Vec<Value> {
        self.history
            .iter()
            .flatten()
            .filter(|event| event["scope"] == scope)
            .cloned()
            .collect()
    }

    /// Whether some event of `scope` set `path` to a value matching
    /// `predicate`, directly or within a replaced document.
    fn ever(&self, scope: &str, predicate: impl Fn(&Value) -> bool) -> bool {
        // Replay the scope's events to see every intermediate document.
        let mut replica = Replica::default();
        let mut matched = false;
        for event in self.events_of(scope) {
            if event.get("end").is_some() {
                continue;
            }
            if replica.apply(&event).is_ok() && predicate(&replica.live) {
                matched = true;
            }
        }
        matched
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

fn text(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn image(data: &str) -> Value {
    json!({ "type": "image", "mimeType": "image/png", "data": data })
}

fn chunk(message_id: &str, content: Value) -> Value {
    json!({
        "sessionUpdate": "agent_message_chunk", "messageId": message_id, "content": content,
    })
}

fn permission(session_id: &str) -> Value {
    json!({
        "sessionId": session_id,
        "toolCall": {"toolCallId": "tool", "title": "Confirm", "status": "pending"},
        "options": [{"optionId": "allow", "name": "Allow once", "kind": "allow_once"}],
    })
}

fn form(scope: Value) -> Value {
    let mut params = json!({
        "mode": "form", "message": "Confirm",
        "requestedSchema": {"type": "object", "properties": {}},
    });
    params
        .as_object_mut()
        .unwrap()
        .extend(scope.as_object().unwrap().clone());
    params
}

fn url_elicitation(scope: Value, elicitation_id: &str) -> Value {
    let mut params = json!({
        "mode": "url", "message": "Sign in", "elicitationId": elicitation_id,
        "url": "https://example.test/sign-in",
    });
    params
        .as_object_mut()
        .unwrap()
        .extend(scope.as_object().unwrap().clone());
    params
}

fn interaction_ids(live: &Value) -> Vec<String> {
    live["op"]["interactions"]
        .as_object()
        .map(|interactions| interactions.keys().cloned().collect())
        .unwrap_or_default()
}

fn patch_paths(event: &Value) -> Vec<(String, String)> {
    event["live"]["patch"]
        .as_array()
        .map(|ops| {
            ops.iter()
                .map(|op| {
                    (
                        op["op"].as_str().unwrap().to_string(),
                        op["path"].as_str().unwrap().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

const CREATED: &str = "created";

// ---------------------------------------------------------------------------
// 1. The commit runs when a mutable guard is released

#[tokio::test]
async fn a_released_mutable_guard_commits_and_an_unchanged_one_sends_nothing() {
    let mut peer = Peer::start(json!({})).await;
    peer.settled().await;
    let before = peer.history.len();

    // Mutable access that changes nothing publishes nothing.
    {
        let lock = peer.lock();
        let mut state = lock.lock().await;
        let state: &mut BridgeState = &mut state;
        state.catalog_revision += 0;
    }
    peer.settled().await;
    assert_eq!(
        peer.history.len(),
        before,
        "an unchanged guard sent a commit"
    );

    // An early return out of a guarded transition still commits it.
    let id = {
        let lock = peer.lock();
        let mut state = lock.lock().await;
        let early = (|| -> Result<String, String> {
            let id = state
                .sessions
                .install_operation(&Scope::Global, OperationKind::Logout, json!({}))
                .map_err(|_| "busy".to_string())?;
            Err(id)
        })();
        early.unwrap_err()
    };
    peer.until("the early return committed", |peer| {
        peer.live("global")["op"]["id"] == id
    })
    .await;
    assert_eq!(peer.history.len(), before + 1, "one commit");
    peer.settled().await;
}

#[tokio::test]
async fn the_gate_reports_a_transition_that_skipped_the_commit() {
    let mut peer = Peer::start(json!({})).await;
    peer.settled().await;
    let lock = peer.lock();
    {
        let mut state = lock.lock_without_commit().await;
        state
            .sessions
            .install_operation(&Scope::Global, OperationKind::Logout, json!({}))
            .unwrap();
    }
    assert!(
        lock.lock().await.unpublished(),
        "the gate missed an uncommitted transition"
    );
    // The next mutable guard publishes it.
    {
        let mut state = lock.lock().await;
        let _: &mut BridgeState = &mut state;
    }
    drop(lock);
    peer.until("the skipped change is published", |peer| {
        peer.live("global")["op"]["kind"] == "logout"
    })
    .await;
    peer.settled().await;
}

// ---------------------------------------------------------------------------
// 2. The session path

#[tokio::test]
async fn a_streamed_prompt_with_an_answered_permission_commits_in_one_event() {
    let mut peer = Peer::start(json!({})).await;
    let revision = peer.create(CREATED, json!({})).await;
    let scope = format!("session/{CREATED}");
    assert_eq!(peer.live(&scope)["op"]["kind"], "open");
    assert_eq!(peer.log(&scope), Vec::<Value>::new());

    let prompt = vec![text("hi")];
    let op = peer.install_prompt(CREATED, &prompt).await;
    let request = peer.turn(CREATED, &revision, prompt).await;
    peer.update(CREATED, chunk("answer", text("hel"))).await;
    peer.until("the first chunk streams", |peer| {
        peer.live(&scope)["op"]["process"][0]["content"]["text"] == "hel"
    })
    .await;
    peer.update(CREATED, chunk("answer", text("lo"))).await;
    peer.until("the chunk folds", |peer| {
        peer.live(&scope)["op"]["process"][0]["content"]["text"] == "hello"
    })
    .await;
    let streamed = peer.events_of(&scope).pop().unwrap();
    assert!(
        streamed["live"]["patch"]
            .as_array()
            .unwrap()
            .iter()
            .any(|op| op["op"] == "append" && op["text"] == "lo"),
        "streamed text appends its suffix: {streamed}"
    );

    peer.agent_request(
        "permission",
        "session/request_permission",
        permission(CREATED),
    )
    .await;
    peer.until("the permission joins the prompt", |peer| {
        interaction_ids(&peer.live(&scope)).len() == 1
    })
    .await;
    let live = peer.live(&scope);
    let interaction = interaction_ids(&live).remove(0);
    assert_eq!(live["op"]["id"], op);
    assert_eq!(
        live["op"]["interactions"][&interaction]["kind"],
        "permission"
    );
    assert_eq!(
        peer.live("global")["sessions"][CREATED]["pending"]["interactions"],
        1
    );
    let answered = peer.submit(json!({
        "type": "permission/respond", "requestId": "answer", "sessionId": CREATED,
        "permissionId": interaction, "outcome": {"outcome": "selected", "optionId": "allow"},
    }));
    Peer::result(answered).await.unwrap();
    let response = peer.response("permission").await;
    assert_eq!(response["result"]["outcome"]["optionId"], "allow");
    peer.until("the answered permission leaves", |peer| {
        interaction_ids(&peer.live(&scope)).is_empty()
    })
    .await;

    peer.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    peer.until("the prompt completes", |peer| {
        peer.live(&scope)["op"]["status"] == "completed"
    })
    .await;
    let commit = peer.events_of(&scope).pop().unwrap();
    assert_eq!(commit["log"]["from"], 0);
    assert_eq!(
        commit["log"]["entries"][0]["turn"],
        json!({ "prompt": [text("hi")], "reply": [text("hello")] })
    );
    let paths = patch_paths(&commit);
    assert!(
        paths.contains(&("remove".into(), "/op/process".into())),
        "the commit removes the process in the same event: {commit}"
    );
    let live = peer.live(&scope);
    assert_eq!(live["op"]["id"], op);
    assert_eq!(live["op"]["result"], json!({ "stopReason": "end_turn" }));
    assert!(live["op"].get("process").is_none());
    assert_eq!(peer.log(&scope).len(), 1);
    peer.settled().await;
}

#[tokio::test]
async fn a_failed_prompt_appends_nothing_and_keeps_its_error() {
    let mut peer = Peer::start(json!({})).await;
    let revision = peer.create(CREATED, json!({})).await;
    let scope = format!("session/{CREATED}");
    let prompt = vec![text("hi")];
    peer.install_prompt(CREATED, &prompt).await;
    let request = peer.turn(CREATED, &revision, prompt).await;
    peer.update(CREATED, chunk("answer", text("partial"))).await;
    peer.until("the chunk streams", |peer| {
        peer.live(&scope)["op"]["process"][0]["content"]["text"] == "partial"
    })
    .await;
    peer.refuse(&request).await;
    peer.until("the prompt fails", |peer| {
        peer.live(&scope)["op"]["status"] == "failed"
    })
    .await;
    let live = peer.live(&scope);
    assert_eq!(live["op"]["error"]["code"], "agent_error");
    assert_eq!(live["op"]["input"], json!({ "prompt": [text("hi")] }));
    assert!(live["op"].get("process").is_none());
    assert!(peer.log(&scope).is_empty(), "a failed turn appends nothing");
    assert!(
        peer.events_of(&scope)
            .iter()
            .all(|event| event.get("log").is_none()
                || event["log"]["entries"].as_array().unwrap().is_empty()),
        "no log change for a failed turn"
    );
    peer.settled().await;
}

#[tokio::test]
async fn a_cancelled_stop_reason_completes_the_prompt() {
    let mut peer = Peer::start(json!({})).await;
    let revision = peer.create(CREATED, json!({})).await;
    let scope = format!("session/{CREATED}");
    let prompt = vec![text("hi")];
    peer.install_prompt(CREATED, &prompt).await;
    let request = peer.turn(CREATED, &revision, prompt).await;
    peer.reply(&request, json!({"stopReason": "cancelled"}))
        .await;
    peer.until("the prompt ends", |peer| {
        peer.live(&scope)["op"]["status"] != "running"
    })
    .await;
    let live = peer.live(&scope);
    assert_eq!(live["op"]["status"], "completed");
    assert_eq!(live["op"]["result"]["stopReason"], "cancelled");
    peer.settled().await;
}

#[tokio::test]
async fn attachments_follow_the_documents_that_reference_them() {
    let mut peer = Peer::start(json!({"promptCapabilities": {"image": true}})).await;
    let revision = peer.create(CREATED, json!({})).await;
    let scope = format!("session/{CREATED}");
    let prompt = vec![text("look"), image("cHJvbXB0")];
    peer.install_prompt(CREATED, &prompt).await;
    let request = peer.turn(CREATED, &revision, prompt).await;
    let input = peer.live(&scope)["op"]["input"]["prompt"][1].clone();
    assert_eq!(
        input["type"], "attachment",
        "the input is normalized: {input}"
    );
    let prompt_attachment = input["id"].as_str().unwrap().to_string();
    assert_eq!(
        peer.attachment(CREATED, &prompt_attachment).await.unwrap()["data"],
        "cHJvbXB0"
    );

    peer.update(
        CREATED,
        json!({
            "sessionUpdate": "tool_call", "toolCallId": "shot", "title": "screenshot",
            "status": "completed",
            "content": [{"type": "content", "content": image("dG9vbA")}],
        }),
    )
    .await;
    peer.until("the tool call streams", |peer| {
        peer.live(&scope)["op"]["process"][0]["content"][0]["content"]["type"] == "attachment"
    })
    .await;
    let tool_attachment = peer.live(&scope)["op"]["process"][0]["content"][0]["content"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        peer.attachment(CREATED, &tool_attachment).await.unwrap()["data"],
        "dG9vbA"
    );
    peer.update(CREATED, chunk("answer", text("seen"))).await;
    peer.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    peer.until("the prompt completes", |peer| {
        peer.live(&scope)["op"]["status"] == "completed"
    })
    .await;
    assert!(
        peer.attachment(CREATED, &prompt_attachment).await.is_some(),
        "the log keeps the prompt's attachment"
    );
    assert!(
        peer.attachment(CREATED, &tool_attachment).await.is_none(),
        "the committed process released its attachment"
    );
    peer.settled().await;
}

#[tokio::test]
async fn a_set_mode_folds_its_acknowledgement_when_it_completes() {
    let mut peer = Peer::start(json!({})).await;
    peer.create(
        CREATED,
        json!({"modes": {"currentModeId": "ask", "availableModes": [
            {"id": "ask", "name": "Ask"}, {"id": "plan", "name": "Plan"},
        ]}}),
    )
    .await;
    let scope = format!("session/{CREATED}");
    assert_eq!(
        peer.live(&scope)["controls"]["modes"]["currentModeId"],
        "ask"
    );
    let op = peer
        .install(&scope, OperationKind::SetMode, json!({"modeId": "plan"}))
        .await;
    let done = peer.submit(json!({
        "type": "session/set_mode", "requestId": "mode", "sessionId": CREATED, "modeId": "plan",
    }));
    let request = peer.request("session/set_mode").await;
    peer.reply(&request, json!({})).await;
    Peer::result(done).await.unwrap();
    peer.until("the set_mode completes", |peer| {
        peer.live(&scope)["op"]["status"] == "completed"
    })
    .await;
    let live = peer.live(&scope);
    assert_eq!(live["op"]["id"], op);
    assert_eq!(live["controls"]["modes"]["currentModeId"], "plan");
    let end = peer.events_of(&scope).pop().unwrap();
    let paths = patch_paths(&end);
    assert!(
        paths.iter().any(|(_, path)| path.starts_with("/controls/"))
            && paths.iter().any(|(_, path)| path.starts_with("/op/")),
        "the acknowledgement and the end are one event: {end}"
    );
    peer.settled().await;
}

// ---------------------------------------------------------------------------
// 3. Global and catalog operations

#[tokio::test]
async fn a_create_completes_with_its_session_and_advances_the_catalog() {
    let mut peer = Peer::start(json!({})).await;
    let revision = peer.live("catalog")["revision"].clone();
    let op = peer
        .install(
            "global",
            OperationKind::Create,
            json!({"cwd": "/workspace"}),
        )
        .await;
    peer.create(CREATED, json!({})).await;
    peer.until("the create completes", |peer| {
        peer.live("global")["op"]["status"] == "completed"
    })
    .await;
    let global = peer.live("global");
    assert_eq!(global["op"]["id"], op);
    assert_eq!(global["op"]["result"], json!({"sessionId": CREATED}));
    assert!(global["sessions"].get(CREATED).is_some());
    let scope = format!("session/{CREATED}");
    let appeared = peer
        .history
        .iter()
        .position(|events| events.iter().any(|event| event["scope"] == scope))
        .unwrap();
    assert!(
        peer.history[appeared]
            .iter()
            .any(|event| event["scope"] == "global"
                && patch_paths(event)
                    .iter()
                    .any(|(_, path)| path.starts_with("/op"))),
        "the target appears in the commit that completes the create"
    );
    assert_ne!(peer.live("catalog")["revision"], revision);
    peer.settled().await;
}

#[tokio::test]
async fn a_failed_create_fails_its_operation() {
    let mut peer = Peer::start(json!({})).await;
    peer.install(
        "global",
        OperationKind::Create,
        json!({"cwd": "/workspace"}),
    )
    .await;
    let created = peer.submit(json!({
        "type": "session/new", "requestId": "new", "cwd": "/workspace",
    }));
    let request = peer.request("session/new").await;
    peer.refuse(&request).await;
    Peer::result(created).await.unwrap_err();
    peer.until("the create fails", |peer| {
        peer.live("global")["op"]["status"] == "failed"
    })
    .await;
    assert_eq!(peer.live("global")["op"]["error"]["code"], "agent_error");
    peer.settled().await;
}

#[tokio::test]
async fn a_fork_completes_on_its_source_and_its_target_appears() {
    let mut peer = Peer::start(json!({"sessionCapabilities": {"fork": {}}})).await;
    peer.create("source", json!({})).await;
    let op = peer
        .install("session/source", OperationKind::Fork, json!({}))
        .await;
    let forked = peer.submit(json!({
        "type": "session/fork", "requestId": "fork", "sessionId": "source",
    }));
    let request = peer.request("session/fork").await;
    peer.reply(&request, json!({"sessionId": "forked"})).await;
    Peer::result(forked).await.unwrap();
    peer.until("the fork completes", |peer| {
        peer.live("session/source")["op"]["status"] == "completed"
            && peer.replicas.contains_key("session/forked")
    })
    .await;
    let source = peer.live("session/source");
    assert_eq!(source["op"]["id"], op);
    assert_eq!(source["op"]["result"], json!({"sessionId": "forked"}));
    let target = peer.live("session/forked");
    assert_eq!(target["op"]["kind"], "open");
    assert_eq!(target["op"]["status"], "completed");
    peer.settled().await;
}

#[tokio::test]
async fn a_delete_takes_the_close_interaction_and_ends_the_scope_with_deleted() {
    let mut peer = Peer::start(json!({"sessionCapabilities": {"close": {}, "delete": {}}})).await;
    peer.create(CREATED, json!({})).await;
    let scope = format!("session/{CREATED}");
    let revision = peer.live("catalog")["revision"].clone();
    let op = peer
        .install(
            "global",
            OperationKind::Delete,
            json!({"sessionId": CREATED}),
        )
        .await;
    peer.until("the delete occupies the session", |peer| {
        peer.live(&scope)["op"]["id"] == op
    })
    .await;
    let deleted = peer.submit(json!({
        "type": "session/delete", "requestId": "delete", "sessionId": CREATED,
    }));
    let close = peer.request("session/close").await;
    peer.agent_request(
        "close-question",
        "elicitation/create",
        form(json!({"sessionId": CREATED})),
    )
    .await;
    peer.until("the close interaction joins the delete", |peer| {
        interaction_ids(&peer.live("global")).len() == 1
    })
    .await;
    let interaction = interaction_ids(&peer.live("global")).remove(0);
    assert_eq!(
        interaction_ids(&peer.live(&scope)),
        vec![interaction.clone()],
        "the occupied session shows the delete's interaction"
    );
    let answered = peer.submit(json!({
        "type": "elicitation/respond", "requestId": "answer", "sessionId": CREATED,
        "elicitationId": interaction, "response": {"action": "accept", "content": {}},
    }));
    Peer::result(answered).await.unwrap();
    assert_eq!(
        peer.response("close-question").await["result"]["action"],
        "accept"
    );
    peer.reply(&close, json!({})).await;
    let delete = peer.request("session/delete").await;
    peer.reply(&delete, json!({})).await;
    Peer::result(deleted).await.unwrap();
    peer.until("the delete completes", |peer| {
        peer.live("global")["op"]["status"] == "completed"
    })
    .await;
    let ended = peer
        .events_of(&scope)
        .into_iter()
        .find(|event| event.get("end").is_some())
        .expect("the deleted scope ends");
    assert_eq!(ended["end"]["reason"], "deleted");
    assert!(!peer.replicas.contains_key(&scope));
    assert!(peer.live("global")["sessions"].get(CREATED).is_none());
    assert_eq!(peer.live("global")["op"]["id"], op);
    assert_eq!(peer.live("global")["op"]["result"], json!({}));
    assert_ne!(peer.live("catalog")["revision"], revision);
    peer.settled().await;
}

#[tokio::test]
async fn a_failed_delete_frees_the_session_it_occupied() {
    let mut peer = Peer::start(json!({"sessionCapabilities": {"close": {}, "delete": {}}})).await;
    peer.create(CREATED, json!({})).await;
    let scope = format!("session/{CREATED}");
    let own = peer.live(&scope)["op"]["id"].clone();
    let op = peer
        .install(
            "global",
            OperationKind::Delete,
            json!({"sessionId": CREATED}),
        )
        .await;
    let deleted = peer.submit(json!({
        "type": "session/delete", "requestId": "delete", "sessionId": CREATED,
    }));
    let close = peer.request("session/close").await;
    peer.refuse(&close).await;
    Peer::result(deleted).await.unwrap_err();
    peer.until("the delete fails", |peer| {
        peer.live("global")["op"]["status"] == "failed"
    })
    .await;
    assert_eq!(peer.live("global")["op"]["id"], op);
    assert_eq!(peer.live("global")["op"]["error"]["code"], "agent_error");
    assert_eq!(
        peer.live(&scope)["op"]["id"],
        own,
        "the session shows its own operation again"
    );
    assert!(
        peer.events_of(&scope)
            .iter()
            .all(|event| event.get("end").is_none())
    );
    // The freed session takes a new operation.
    peer.install(&scope, OperationKind::SetMode, json!({"modeId": "x"}))
        .await;
    peer.settled().await;
}

#[tokio::test]
async fn a_close_releases_the_session_without_an_end() {
    let mut peer = Peer::start(json!({"sessionCapabilities": {"close": {}}})).await;
    peer.create(CREATED, json!({})).await;
    let scope = format!("session/{CREATED}");
    let closed = peer.submit(json!({
        "type": "session/close", "requestId": "close", "sessionId": CREATED,
    }));
    let close = peer.request("session/close").await;
    peer.reply(&close, json!({})).await;
    Peer::result(closed).await.unwrap();
    peer.until("the session leaves global", |peer| {
        peer.live("global")["sessions"].get(CREATED).is_none()
    })
    .await;
    peer.settled().await;
    assert!(peer.released.contains(&scope), "the session was released");
    assert!(
        peer.events_of(&scope)
            .iter()
            .all(|event| event.get("end").is_none()),
        "a release sends no end"
    );
}

#[tokio::test]
async fn a_list_completes_on_the_catalog_with_its_page() {
    let mut peer = Peer::start(json!({"sessionCapabilities": {"list": {}}})).await;
    let op = peer
        .install("catalog", OperationKind::List, json!({}))
        .await;
    let listed = peer.submit(json!({"type": "session/list", "requestId": "list"}));
    let request = peer.request("session/list").await;
    peer.reply(
        &request,
        json!({"sessions": [{"sessionId": "saved", "cwd": "/workspace"}], "nextCursor": null}),
    )
    .await;
    Peer::result(listed).await.unwrap();
    peer.until("the list completes", |peer| {
        peer.live("catalog")["op"]["status"] == "completed"
    })
    .await;
    let catalog = peer.live("catalog");
    assert_eq!(catalog["op"]["id"], op);
    assert_eq!(catalog["op"]["result"]["revision"], catalog["revision"]);
    assert_eq!(catalog["op"]["result"]["sessions"][0]["sessionId"], "saved");
    assert_eq!(catalog["op"]["result"]["nextCursor"], Value::Null);
    peer.settled().await;
}

#[tokio::test]
async fn authenticate_and_logout_settle_the_global_operation_and_the_agent() {
    let mut peer = Peer::start_with(
        json!({"auth": {"logout": {}}}),
        json!([{"id": "agent-login", "name": "Login"}]),
        |_| {},
    )
    .await;
    peer.install(
        "global",
        OperationKind::Authenticate,
        json!({"methodId": "agent-login"}),
    )
    .await;
    let done = peer.submit(json!({
        "type": "auth/authenticate", "requestId": "auth", "methodId": "agent-login",
    }));
    let request = peer.request("authenticate").await;
    peer.reply(&request, json!({})).await;
    Peer::result(done).await.unwrap();
    peer.until("the authentication completes", |peer| {
        peer.live("global")["op"]["status"] == "completed"
    })
    .await;
    assert_eq!(
        peer.live("global")["agent"]["authenticatedMethodId"],
        "agent-login"
    );

    peer.install("global", OperationKind::Logout, json!({}))
        .await;
    let done = peer.submit(json!({"type": "auth/logout", "requestId": "logout"}));
    let request = peer.request("logout").await;
    peer.reply(&request, json!({})).await;
    Peer::result(done).await.unwrap();
    peer.until("the logout completes", |peer| {
        peer.live("global")["op"]["status"] == "completed"
    })
    .await;
    assert_eq!(
        peer.live("global")["agent"]["authenticatedMethodId"],
        Value::Null
    );
    peer.settled().await;
}

// ---------------------------------------------------------------------------
// 4. Request-scoped interactions

#[tokio::test]
async fn a_request_scoped_elicitation_followed_by_its_response_joins_the_catalog() {
    let mut peer = Peer::start(json!({"sessionCapabilities": {"list": {}}})).await;
    let op = peer
        .install("catalog", OperationKind::List, json!({}))
        .await;
    let listed = peer.submit(json!({"type": "session/list", "requestId": "list"}));
    let request = peer.request("session/list").await;
    // The interaction and its parent's response arrive back to back.
    peer.agent_request(
        "list-question",
        "elicitation/create",
        form(json!({"requestId": request["id"]})),
    )
    .await;
    peer.reply(&request, json!({"sessions": [], "nextCursor": null}))
        .await;
    Peer::result(listed).await.unwrap();
    peer.until("the list completes", |peer| {
        peer.live("catalog")["op"]["status"] == "completed"
    })
    .await;
    assert!(
        peer.ever("catalog", |live| live["op"]["id"] == op
            && interaction_ids(live).len() == 1),
        "the elicitation joined the list on the catalog"
    );
    assert!(
        !peer.ever("global", |live| !interaction_ids(live).is_empty()),
        "the elicitation never reached global"
    );
    assert_eq!(peer.live("global")["op"]["kind"], "none");
    assert!(interaction_ids(&peer.live("catalog")).is_empty());
    assert_eq!(
        peer.response("list-question").await["result"]["action"],
        "cancel",
        "the operation's end cancels its pending interaction"
    );
    peer.settled().await;
}

#[tokio::test]
async fn a_request_scoped_url_flow_on_global_outlives_its_operation() {
    let mut peer = Peer::start(json!({})).await;
    let op = peer
        .install(
            "global",
            OperationKind::Create,
            json!({"cwd": "/workspace"}),
        )
        .await;
    let created = peer.submit(json!({
        "type": "session/new", "requestId": "new", "cwd": "/workspace",
    }));
    let request = peer.request("session/new").await;
    peer.agent_request(
        "sign-in",
        "elicitation/create",
        url_elicitation(json!({"requestId": request["id"]}), "flow"),
    )
    .await;
    peer.until("the elicitation joins the create", |peer| {
        interaction_ids(&peer.live("global")).len() == 1
    })
    .await;
    let interaction = interaction_ids(&peer.live("global")).remove(0);
    assert_eq!(peer.live("global")["op"]["id"], op);
    let answered = peer.submit(json!({
        "type": "elicitation/respond", "requestId": "answer",
        "elicitationId": interaction, "response": {"action": "accept"},
    }));
    Peer::result(answered).await.unwrap();
    assert_eq!(peer.response("sign-in").await["result"]["action"], "accept");
    peer.until("the accepted flow moves to urlFlows", |peer| {
        interaction_ids(&peer.live("global")).is_empty()
            && peer.live("global")["urlFlows"].get(&interaction).is_some()
    })
    .await;
    peer.reply(&request, json!({"sessionId": CREATED})).await;
    Peer::result(created).await.unwrap();
    peer.until("the create completes", |peer| {
        peer.live("global")["op"]["status"] == "completed"
    })
    .await;
    assert_eq!(
        peer.live("global")["urlFlows"][&interaction]["elicitationId"],
        "flow",
        "the flow outlives its operation"
    );
    peer.settled().await;
}

#[tokio::test]
async fn request_scoped_interactions_of_a_prompt_join_its_session() {
    let mut peer = Peer::start(json!({})).await;
    let revision = peer.create(CREATED, json!({})).await;
    let scope = format!("session/{CREATED}");
    let prompt = vec![text("hi")];
    let op = peer.install_prompt(CREATED, &prompt).await;
    let request = peer.turn(CREATED, &revision, prompt).await;

    peer.agent_request(
        "sign-in",
        "elicitation/create",
        url_elicitation(json!({"requestId": request["id"]}), "flow"),
    )
    .await;
    peer.until("the URL elicitation joins the prompt", |peer| {
        interaction_ids(&peer.live(&scope)).len() == 1
    })
    .await;
    assert_eq!(peer.live(&scope)["op"]["id"], op);
    assert!(interaction_ids(&peer.live("global")).is_empty());
    let interaction = interaction_ids(&peer.live(&scope)).remove(0);
    let answered = peer.submit(json!({
        "type": "elicitation/respond", "requestId": "answer",
        "elicitationId": interaction, "response": {"action": "accept"},
    }));
    Peer::result(answered).await.unwrap();
    assert_eq!(peer.response("sign-in").await["result"]["action"], "accept");
    peer.until("the flow is the session's", |peer| {
        peer.live(&scope)["urlFlows"].get(&interaction).is_some()
    })
    .await;
    assert!(
        peer.live("global")["urlFlows"]
            .as_object()
            .unwrap()
            .is_empty()
    );

    // A second interaction arrives right before the prompt's response.
    peer.agent_request(
        "question",
        "elicitation/create",
        form(json!({"requestId": request["id"]})),
    )
    .await;
    peer.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    peer.until("the prompt completes", |peer| {
        peer.live(&scope)["op"]["status"] == "completed"
    })
    .await;
    assert!(
        peer.ever(&scope, |live| live["op"]["id"] == op
            && interaction_ids(live).iter().any(|id| *id != interaction)),
        "the second interaction joined the prompt before it ended"
    );
    assert!(interaction_ids(&peer.live(&scope)).is_empty());
    assert_eq!(
        peer.response("question").await["result"]["action"],
        "cancel"
    );
    assert_eq!(
        peer.live(&scope)["urlFlows"][&interaction]["elicitationId"],
        "flow",
        "the accepted flow stays after the prompt"
    );
    assert!(
        !peer.ever("global", |live| !interaction_ids(live).is_empty()),
        "nothing reached global"
    );
    peer.settled().await;
}

// ---------------------------------------------------------------------------
// 5. Login terminals

#[cfg(unix)]
#[tokio::test]
async fn login_terminals_publish_output_and_exit_and_keep_their_method() {
    let script = "printf 'login %s\\n' \"$0\"; if [ \"$0\" = b ]; then read answer; exit 3; fi";
    let mut peer = Peer::start_with(
        json!({}),
        json!([
            {"id": "login-a", "name": "A", "type": "terminal", "args": ["a"]},
            {"id": "login-b", "name": "B", "type": "terminal", "args": ["b"]},
        ]),
        |options| {
            options.transport = Transport::Stdio;
            options.command = vec!["/bin/sh".into(), "-c".into(), script.into()];
        },
    )
    .await;
    let a = peer
        .install(
            "global",
            OperationKind::TerminalLogin,
            json!({"methodId": "login-a", "cols": 80, "rows": 24}),
        )
        .await;
    let started = peer.submit(json!({
        "type": "auth/terminal_start", "requestId": "term-a", "methodId": "login-a",
        "cols": 80, "rows": 24,
    }));
    Peer::result(started).await.unwrap();
    peer.until("login A exits", |peer| {
        peer.live("global")["op"]["status"] == "completed"
    })
    .await;
    let global = peer.live("global");
    assert_eq!(global["op"]["id"], a);
    assert_eq!(
        global["op"]["result"],
        json!({"exitCode": 0, "signal": null})
    );
    let terminal = &global["terminals"][&a];
    assert_eq!(terminal["terminalId"], a);
    assert_eq!(terminal["methodId"], "login-a");
    assert!(
        terminal["output"].as_str().unwrap().contains("login a"),
        "{terminal}"
    );
    assert_eq!(
        terminal["exitStatus"],
        json!({"exitCode": 0, "signal": null})
    );

    // B starts while A's terminal is still there.
    let b = peer
        .install(
            "global",
            OperationKind::TerminalLogin,
            json!({"methodId": "login-b", "cols": 80, "rows": 24}),
        )
        .await;
    let started = peer.submit(json!({
        "type": "auth/terminal_start", "requestId": "term-b", "methodId": "login-b",
        "cols": 80, "rows": 24,
    }));
    Peer::result(started).await.unwrap();
    peer.until("login B prints", |peer| {
        peer.live("global")["terminals"][&b]["output"]
            .as_str()
            .is_some_and(|output| output.contains("login b"))
    })
    .await;
    let global = peer.live("global");
    assert_eq!(global["terminals"][&b]["exitStatus"], Value::Null);
    assert_eq!(global["terminals"][&b]["methodId"], "login-b");
    assert_eq!(global["terminals"][&a]["methodId"], "login-a");
    let written = peer.submit(json!({
        "type": "auth/terminal_input", "requestId": "term-b", "data": "done\n",
    }));
    Peer::result(written).await.unwrap();
    peer.until("login B exits", |peer| {
        peer.live("global")["op"]["status"] == "completed"
    })
    .await;
    let global = peer.live("global");
    assert_eq!(global["op"]["id"], b);
    assert_eq!(
        global["op"]["result"],
        json!({"exitCode": 3, "signal": null})
    );
    assert_eq!(global["terminals"][&b]["exitStatus"]["exitCode"], 3);
    assert_eq!(
        global["terminals"].as_object().unwrap().len(),
        2,
        "an operation's end removes no terminal"
    );

    // A login that cannot start fails its operation.
    peer.install(
        "global",
        OperationKind::TerminalLogin,
        json!({"methodId": "missing", "cols": 80, "rows": 24}),
    )
    .await;
    let started = peer.submit(json!({
        "type": "auth/terminal_start", "requestId": "term-c", "methodId": "missing",
        "cols": 80, "rows": 24,
    }));
    Peer::result(started).await.unwrap_err();
    peer.until("the login fails", |peer| {
        peer.live("global")["op"]["status"] == "failed"
    })
    .await;
    assert_eq!(
        peer.live("global")["terminals"].as_object().unwrap().len(),
        2
    );
    peer.settled().await;

    // The connection's end removes them.
    peer.stop().await.unwrap();
    assert!(
        peer.live("global")["terminals"]
            .as_object()
            .unwrap()
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// 6. The end of the Agent connection

#[tokio::test]
async fn the_connection_end_fails_running_operations_and_publishes_the_final_agent() {
    let mut peer = Peer::start_with(
        json!({"sessionCapabilities": {"list": {}}}),
        json!([{"id": "agent-login", "name": "Login"}]),
        |_| {},
    )
    .await;
    let revision = peer.create(CREATED, json!({})).await;
    let scope = format!("session/{CREATED}");
    let prompt = vec![text("hi")];
    peer.install_prompt(CREATED, &prompt).await;
    let request = peer.turn(CREATED, &revision, prompt).await;
    peer.agent_request(
        "sign-in",
        "elicitation/create",
        url_elicitation(json!({"requestId": request["id"]}), "flow"),
    )
    .await;
    peer.until("the URL elicitation joins the prompt", |peer| {
        interaction_ids(&peer.live(&scope)).len() == 1
    })
    .await;
    let interaction = interaction_ids(&peer.live(&scope)).remove(0);
    let answered = peer.submit(json!({
        "type": "elicitation/respond", "requestId": "answer",
        "elicitationId": interaction, "response": {"action": "accept"},
    }));
    Peer::result(answered).await.unwrap();
    peer.agent_request(
        "permission",
        "session/request_permission",
        permission(CREATED),
    )
    .await;
    peer.until("the permission is pending", |peer| {
        interaction_ids(&peer.live(&scope)).len() == 1
            && peer.live(&scope)["urlFlows"].get(&interaction).is_some()
    })
    .await;
    peer.install(
        "global",
        OperationKind::Authenticate,
        json!({"methodId": "agent-login"}),
    )
    .await;
    let _authenticating = peer.submit(json!({
        "type": "auth/authenticate", "requestId": "auth", "methodId": "agent-login",
    }));
    peer.request("authenticate").await;
    peer.install("catalog", OperationKind::List, json!({}))
        .await;
    let _listing = peer.submit(json!({"type": "session/list", "requestId": "list"}));
    peer.request("session/list").await;
    peer.settled().await;

    peer.stop().await.unwrap();
    let global = peer.live("global");
    assert_eq!(global["agent"]["phase"], "stopped");
    for scope in ["global", "catalog", scope.as_str()] {
        let live = peer.live(scope);
        assert_eq!(live["op"]["status"], "failed", "{scope}: {live}");
        assert_eq!(live["op"]["error"]["code"], "unavailable", "{scope}");
        assert!(interaction_ids(&live).is_empty(), "{scope}");
        assert!(
            live["urlFlows"].as_object().unwrap().is_empty(),
            "{scope} keeps no URL flow"
        );
    }
    assert_eq!(global["sessions"][CREATED]["op"]["status"], "failed");
}

#[tokio::test]
async fn a_failed_connection_publishes_the_agent_error() {
    let mut peer = Peer::connect(|_| {}).await;
    let initialize = peer.next_request().await;
    peer.reply(
        &initialize,
        json!({"protocolVersion": 99, "agentCapabilities": {}, "authMethods": []}),
    )
    .await;
    peer.finish().await.unwrap_err();
    let agent = peer.live("global")["agent"].clone();
    assert_eq!(agent["phase"], "error", "{agent}");
    assert!(agent["error"]["message"].is_string(), "{agent}");
    assert_eq!(agent["launch"]["transport"], "ws");
}

#[tokio::test]
async fn the_same_history_gives_the_same_log_after_a_restart() {
    async fn load(replay: &[Value]) -> Vec<Value> {
        let mut peer = Peer::start(json!({"loadSession": true})).await;
        let loaded = peer.submit(json!({
            "type": "session/load", "requestId": "load", "sessionId": "saved",
            "cwd": "/workspace",
        }));
        let request = peer.request("session/load").await;
        for update in replay {
            peer.update("saved", update.clone()).await;
        }
        peer.reply(&request, json!({})).await;
        Peer::result(loaded).await.unwrap();
        peer.until("the load completes", |peer| {
            peer.live("session/saved")["op"]["status"] == "completed"
        })
        .await;
        peer.settled().await;
        let log = peer.log("session/saved");
        peer.stop().await.unwrap();
        log
    }
    let replay = [
        json!({"sessionUpdate": "user_message_chunk", "content": text("hi")}),
        chunk("answer", text("hel")),
        chunk("answer", text("lo")),
    ];
    let first = load(&replay).await;
    assert_eq!(first.len(), 1);
    assert_eq!(first, load(&replay).await, "a restart keeps the same log");
}
