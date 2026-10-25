//! The per-event convergence gate through the real coordinator and ACP
//! transport (docs/state-sync.md §4.1).
//!
//! A scripted Agent drives `run_connection` over an in-memory transport. The feed's tap hands
//! every commit to the test with the nodes it published; an R4 client applies each atomic event
//! and is compared with all published nodes after every commit.
//! Operations start through the bridge's admission of writes (protocol §6); a test that only
//! needs an occupied scope installs one through the registry's internal entry point.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::sync::Weak;

use agent_client_protocol::Lines;
use clap::Parser;
use futures::{SinkExt, StreamExt};

use super::*;
use crate::protocol::admission::OpWrite;
use crate::protocol::documents::{OperationKind, ProtocolError, Scope};
use crate::protocol::feed::TreeSubscription;
use crate::protocol::feed::tap::TapCommit;
use crate::protocol::publication::Publication;
use crate::protocol::tree_stream::client::Client;
use crate::protocol::tree_stream::documents::{Document, document, path_of};
use crate::protocol::tree_stream::params::{Follow, StreamQuery};
use crate::protocol::tree_stream::wire::{Change, Event};
use crate::protocol::outcome::{self, Request as OutcomeRequest};

const WAIT: Duration = Duration::from_secs(5);

type Documents = BTreeMap<String, Value>;

/// One step of a scope as a replica saw it: the document after an event (or the initial cut).
#[derive(Debug, Clone)]
struct Step {
    doc: Value,
    ended_prompts: usize,
}

impl Step {
    fn of(replica: &Document) -> Self {
        Self {
            doc: replica.doc.clone(),
            ended_prompts: replica.ended_prompts(),
        }
    }

    fn live(&self) -> &Value {
        &self.doc["live"]
    }
}

/// The real R4 client follows three roots. Scope documents are only assertion views of its
/// nodes; they never receive or apply events themselves.
#[derive(Debug)]
struct Gate {
    client: Client,
    replicas: BTreeMap<String, Document>,
    history: Vec<Vec<Event>>,
    timeline: BTreeMap<String, Vec<Step>>,
    released: BTreeSet<String>,
}

impl Gate {
    fn new(cut: &TapCommit) -> Self {
        let mut gate = Self {
            client: Client::following(&["/global", "/catalog", "/sessions"]),
            replicas: BTreeMap::new(),
            history: Vec::new(),
            timeline: BTreeMap::new(),
            released: BTreeSet::new(),
        };
        gate.apply(cut.events.clone(), &cut.documents);
        assert!(gate.client.instance.is_some(), "the initial cut names its tree instance");
        gate.history.clear();
        gate
    }

    fn nodes(&self) -> Documents {
        let mut nodes = BTreeMap::new();
        for scope in ["global", "catalog"] {
            if let Some(value) = self.client.value(&path_of(scope)) {
                nodes.insert(scope.to_string(), value.clone());
            }
        }
        if let Some(sessions) = self.client.value("/sessions").and_then(Value::as_object) {
            nodes.extend(sessions.iter().map(|(id, value)| (format!("session/{id}"), value.clone())));
        }
        nodes
    }

    /// Apply the wire event atomically, then compare every published node with the cut. A
    /// lost event or a change not published as an event cannot be hidden by the assertion view.
    fn apply(&mut self, events: Vec<Event>, documents: &Documents) {
        let mut changed = BTreeSet::new();
        for event in &events {
            for change in event.changes.iter().flatten() {
                assert!(changed.insert(change.path().to_string()), "two changes of {} in one commit: {events:?}", change.path());
            }
            self.client.apply(event).unwrap_or_else(|error| panic!("{error}: {event:?}"));
            let nodes = self.nodes();
            for scope in self.replicas.keys() {
                if !nodes.contains_key(scope) {
                    self.released.insert(scope.clone());
                }
            }
            let mut replicas = BTreeMap::new();
            for (scope, value) in nodes {
                let path = if scope.starts_with("session/") { "/sessions".into() } else { path_of(&scope) };
                let replica = Document {
                    doc: document(&scope, &value).expect("a scope node has its operation"),
                    rev: self.client.version(&path),
                };
                if self.replicas.get(&scope).is_none_or(|previous| previous.doc != replica.doc) {
                    self.timeline.entry(scope.clone()).or_default().push(Step::of(&replica));
                }
                self.released.remove(&scope);
                replicas.insert(scope, replica);
            }
            self.replicas = replicas;
        }
        self.history.push(events);
        self.check(documents);
    }

    fn check(&mut self, documents: &Documents) {
        let nodes = self.nodes();
        for (scope, value) in documents {
            assert_eq!(nodes.get(scope), Some(value), "{scope}: the R4 client matches the published node");
            assert_eq!(self.replicas[scope].doc, document(scope, value).unwrap(), "{scope}");
        }
        assert_eq!(nodes.keys().collect::<Vec<_>>(), documents.keys().collect::<Vec<_>>(), "the R4 client holds exactly the published scopes");
    }

    /// R4 changes addressing this scope, relative to its node for path assertions.
    fn events_of(&self, scope: &str) -> Vec<Value> {
        self.history.iter().flatten().flat_map(|event| scope_changes(event, scope)).collect()
    }

    /// Whether some document `scope` went through, from the cut on, matches.
    fn ever(&self, scope: &str, predicate: impl Fn(&Value) -> bool) -> bool {
        self.timeline
            .get(scope)
            .is_some_and(|steps| steps.iter().any(|step| predicate(step.live())))
    }

    /// The prompt `op` ran until one event ended it and its prompt together: the operation
    /// never completes before the prompt's end is in the document, nor the other way round.
    fn assert_ended_atomically(&self, scope: &str, op: &Value) {
        let steps = &self.timeline[scope];
        let ended = steps
            .iter()
            .position(|step| step.live()["op"]["id"] == *op && step.live()["op"]["status"] != "running")
            .expect("the prompt's operation ended");
        assert!(ended > 0, "the prompt ran before it ended");
        assert_eq!(
            steps[ended].ended_prompts,
            steps[ended - 1].ended_prompts + 1,
            "{scope}: the operation and its prompt end in one event: {:?}",
            steps[ended]
        );
        assert_eq!(steps[ended - 1].live()["op"]["id"], *op, "the prompt ran until its end");
        assert_eq!(steps[ended - 1].live()["op"]["status"], "running");
    }
}

struct Peer {
    commands: mpsc::UnboundedSender<BridgeInput>,
    requests: futures::channel::mpsc::Receiver<String>,
    responses: futures::channel::mpsc::Sender<io::Result<String>>,
    buffered: VecDeque<Value>,
    _events: mpsc::UnboundedReceiver<BridgeEvent>,
    cancellation: CancellationToken,
    task: Option<tokio::task::JoinHandle<Result<(), Error>>>,
    state: Weak<BridgeLock>,
    commits: mpsc::UnboundedReceiver<TapCommit>,
    gate: Gate,
    /// The published documents and the feed, shared with the generations before and after
    /// this connection.
    publication: Publication,
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
        Self::connect_on(Publication::default(), configure).await
    }

    /// A connection publishing into `publication`, after what earlier generations published.
    async fn connect_on(publication: Publication, configure: impl FnOnce(&mut Options)) -> Self {
        let (outgoing, requests) = futures::channel::mpsc::channel::<String>(64);
        let (responses, incoming) = futures::channel::mpsc::channel::<io::Result<String>>(64);
        let transport = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        // The tap is taken before the generation's first commit, with the cut it follows.
        let (tap, commits) = mpsc::unbounded_channel();
        let cut = publication.lock().feed.tap(tap);
        let probe = super::memory_retention_tests::probe_coordinator_state();
        let task = tokio::spawn(run_generation(
            Ok(transport),
            Arc::new(options(configure)),
            command_rx,
            EventSink { tx: events },
            cancellation.clone(),
            publication.clone(),
        ));
        let state = probe.await.unwrap();
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
            gate: Gate::new(&cut),
            publication,
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

    /// A `session/update` whose notification carries its own `_meta`.
    async fn update_with_meta(&mut self, session_id: &str, update: Value, meta: Value) {
        self.send(json!({
            "jsonrpc": "2.0", "method": "session/update",
            "params": {"sessionId": session_id, "update": update, "_meta": meta},
        }))
        .await;
    }

    /// The session's timeline entries, read from bridge memory (not from a
    /// published document).
    async fn timeline(&self, session_id: &str) -> Value {
        let lock = self.lock();
        let state = lock.lock().await;
        let timeline = state
            .sessions
            .state(session_id)
            .and_then(|session| session.timeline.as_ref())
            .expect("the session's timeline is known");
        serde_json::to_value(timeline.entries()).unwrap()
    }

    /// Waits until the session's timeline holds `len` entries. An update that
    /// changes no published document is visible only in bridge memory.
    async fn timeline_len(&self, session_id: &str, len: usize) -> Value {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let entries = self.timeline(session_id).await;
            if entries.as_array().is_some_and(|entries| entries.len() == len) {
                return entries;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {len} entries of {session_id}: {entries}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// How much history the session's timeline holds, read from bridge memory.
    async fn history(&self, session_id: &str) -> Option<crate::protocol::timeline::History> {
        let lock = self.lock();
        let state = lock.lock().await;
        state.sessions.state(session_id).and_then(|session| session.history)
    }

    async fn agent_request(&mut self, id: &str, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
    }

    // -- Client side ---------------------------------------------------------

    /// Follows `scopes` on the feed (protocol §5.1) and asks the bridge to serve the streams.
    fn subscribe(&self, query: StreamQuery) -> TreeSubscription {
        let subscription = self.publication.lock().feed.subscribe_tree(&query);
        self.commands.send(BridgeInput::ServeStreams).unwrap();
        subscription
    }

    /// A v2 operation write in `scope` through the bridge's admission
    /// (protocol §6), expecting the scope's published operation.
    async fn write(
        &self,
        scope: &str,
        kind: &str,
        args: Value,
    ) -> Result<operations::OperationAdmission, ProtocolError> {
        let scope = Scope::parse(scope).unwrap();
        let expect = {
            let lock = self.lock();
            let state = lock.lock().await;
            let published = state.protocol.publication.lock();
            published.feed.with_tree(|tree| {
                tree.get(&path_of(&scope.to_string())).expect("a published scope")
                    .child("op").unwrap().to_value()["id"].as_str().unwrap().to_string()
            })
        };
        self.write_request(
            scope,
            OpWrite {
                id: None,
                expect,
                kind: kind.to_string(),
                args,
            },
        )
        .await
    }

    /// Delivers an already prepared request without refreshing its identity or `expect`.
    async fn write_request(
        &self,
        scope: Scope,
        write: OpWrite,
    ) -> Result<operations::OperationAdmission, ProtocolError> {
        let (reply, admitted) = oneshot::channel();
        self.commands
            .send(BridgeInput::Operation {
                scope,
                write,
                reply,
            })
            .unwrap();
        tokio::time::timeout(WAIT, admitted)
            .await
            .expect("the write was not admitted")
            .expect("the bridge dropped the write")
    }

    fn prepared_prompt(&self, target: &str) -> (OpWrite, OutcomeRequest) {
        self.publication.lock().feed.with_tree(|tree| {
            let state = outcome::state_of(tree, target).unwrap();
            let request = OutcomeRequest {
                id: crate::protocol::op::operation_id(),
                expect: state.op,
                instance: state.instance,
                at: tree.get(&format!("/{target}")).unwrap().srev(),
            };
            let write = OpWrite {
                id: Some(request.id.clone()),
                expect: request.expect.clone(),
                kind: "prompt".into(),
                args: json!({"prompt": [text("only once")]}),
            };
            (write, request)
        })
    }

    fn outcome(&self, target: &str, request: &OutcomeRequest) -> Value {
        self.publication
            .lock()
            .feed
            .with_tree(|tree| outcome::resolve(tree, target, request).unwrap())
    }

    /// EOF joins the coordinator, so every remaining ACP send is available for inspection.
    async fn finish_without_another_prompt(&mut self) {
        self.eof().await.unwrap();
        tokio::time::timeout(WAIT, async {
            while let Some(raw) = self.requests.next().await {
                match serde_json::from_str(&raw).unwrap() {
                    Value::Array(batch) => self.buffered.extend(batch),
                    message => self.buffered.push_back(message),
                }
            }
        })
        .await
        .expect("the ACP sender outlived the coordinator");
        assert!(
            self.buffered
                .iter()
                .all(|message| message["method"] != "session/prompt"),
            "a duplicate reached the Agent: {:?}",
            self.buffered
        );
    }

    /// Sends a control action to `session_id` and waits until the bridge admits it; the receiver
    /// is the Agent's answer.
    async fn control(
        &mut self,
        session_id: &str,
        control: crate::bridge::controls::Control,
    ) -> crate::bridge::controls::ControlAnswer {
        let (reply, admitted) = oneshot::channel();
        self.commands
            .send(BridgeInput::Control {
                session_id: session_id.to_string(),
                control,
                reply,
            })
            .unwrap();
        tokio::time::timeout(WAIT, admitted)
            .await
            .expect("the control was not admitted")
            .expect("the bridge dropped the control")
            .unwrap_or_else(|error| panic!("the control was refused: {error:?}"))
    }

    /// Starts an operation and returns its ID and the scope/ID to observe for its end.
    async fn start_op(
        &mut self,
        scope: &str,
        kind: &str,
        args: Value,
    ) -> (String, (String, String)) {
        let admission = self
            .write(scope, kind, args)
            .await
            .unwrap_or_else(|error| panic!("{kind} in {scope} was refused: {error:?}"));
        let id = admission.op.clone();
        self.until("the operation is published", |peer| {
            peer.live(scope)["op"]["id"] == id.as_str()
        })
        .await;
        (admission.op.clone(), (scope.to_string(), admission.op))
    }

    /// Reads the final result through the same published tree as the client.
    async fn ended(&mut self, (scope, id): (String, String)) -> Result<Value, ProtocolError> {
        self.until("the operation's end is published", |peer| {
            let op = &peer.live(&scope)["op"];
            op["id"] == id && matches!(op["status"].as_str(), Some("completed" | "failed"))
        })
        .await;
        let op = &self.live(&scope)["op"];
        if op["status"] == "completed" {
            Ok(op["result"].clone())
        } else {
            Err(serde_json::from_value(op["error"].clone()).unwrap())
        }
    }

    /// Answers one interaction at its resource (protocol §7), as the route does.
    async fn answer_resource(
        &self,
        scope: &str,
        interaction: &str,
        body: Value,
    ) -> Result<(), ProtocolError> {
        let (reply, acted) = oneshot::channel();
        self.commands
            .send(BridgeInput::Resource {
                scope: Scope::parse(scope).unwrap(),
                resource: interaction.to_string(),
                action: resources::ResourceAction::Answer { body },
                reply,
            })
            .unwrap();
        tokio::time::timeout(WAIT, acted)
            .await
            .expect("the answer was not handled")
            .expect("the bridge dropped the answer")
    }

    /// Answers `interaction`, a pending interaction of `scope`, at its resource.
    async fn answer(&mut self, scope: &str, interaction: &str, answer: Value) {
        self.answer_resource(scope, interaction, answer)
            .await
            .unwrap_or_else(|error| panic!("the answer in {scope} was refused: {error:?}"));
    }

    /// Creates `session_id` through a v2 `create` and returns the create's
    /// operation.
    async fn create(&mut self, session_id: &str, response: Value) -> String {
        self.create_in(session_id, "/workspace", response).await
    }

    /// Creates `session_id` in `cwd`.
    async fn create_in(&mut self, session_id: &str, cwd: &str, response: Value) -> String {
        let (op, end) = self.start_op("global", "create", json!({"cwd": cwd})).await;
        let new = self.request("session/new").await;
        let mut response = response;
        response["sessionId"] = json!(session_id);
        self.reply(&new, response).await;
        assert_eq!(
            self.ended(end).await.unwrap(),
            json!({"sessionId": session_id})
        );
        let scope = format!("session/{session_id}");
        self.until("the created session is published", |peer| peer.has(&scope))
            .await;
        op
    }

    /// Starts a v2 prompt of `session_id`: its operation and the Agent's
    /// `session/prompt` request.
    async fn prompt(&mut self, session_id: &str, prompt: Vec<Value>) -> (String, Value) {
        let (op, _) = self
            .start_op(
                &format!("session/{session_id}"),
                "prompt",
                json!({"prompt": prompt}),
            )
            .await;
        (op, self.request("session/prompt").await)
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

    async fn attachment(&self, session_id: &str, attachment_id: &str) -> Option<Value> {
        self.lock()
            .lock()
            .await
            .protocol
            .publication
            .lock()
            .feed
            .attachment(session_id, attachment_id)
    }

    // -- The gate ----------------------------------------------------------------

    /// Applies one tapped commit to the gate.
    fn apply(&mut self, commit: TapCommit) {
        self.gate.apply(commit.events, &commit.documents);
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
                    self.gate.replicas
                ),
            }
        }
    }

    /// Nothing is left unpublished: under the bridge lock, a commit now would
    /// publish nothing, and every published scope's replica equals its
    /// document, live and known log (I1, I2).
    async fn settled(&mut self) {
        let lock = self.lock();
        let state = lock.lock().await;
        assert!(
            !state.unpublished(),
            "a transition changed what clients see without committing"
        );
        // Every commit before this guard is already in the tap.
        self.drain();
        let documents = state.protocol.publication.lock().feed.documents();
        self.gate.check(&documents);
    }

    /// Sends one JSON-RPC batch ending with the response to `parent` while the
    /// bridge lock is held, and releases the lock only once that response was
    /// received and removed its pending request. The coordinator, which needs
    /// the lock, handles nothing of the batch before then.
    async fn send_held_batch(&mut self, batch: Vec<Value>, parent: &Value) {
        let parent: agent_client_protocol::schema::v1::RequestId =
            serde_json::from_value(parent["id"].clone()).unwrap();
        let lock = self.lock();
        let guard = lock.lock().await;
        self.send(Value::Array(batch)).await;
        let ingress = guard.ingress.clone().expect("the connection's ingress");
        tokio::time::timeout(WAIT, async {
            while ingress.is_pending(&parent) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the parent's response was not received");
        drop(guard);
    }

    /// Stops the connection and applies its last commits.
    async fn stop(&mut self) -> Result<(), Error> {
        self.cancellation.cancel();
        self.finish().await
    }

    /// The Agent closes its end of the transport: the connection ends with
    /// an EOF. Applies its last commits.
    async fn eof(&mut self) -> Result<(), Error> {
        self.responses.close_channel();
        self.finish().await
    }

    /// Waits for the connection to end on its own and applies its last commits.
    async fn finish(&mut self) -> Result<(), Error> {
        let task = self.task.take().unwrap();
        let result = tokio::time::timeout(WAIT, task)
            .await
            .expect("the connection did not finish")
            .unwrap();
        // The feed outlives the connection, so its tap stays open: every commit of the
        // connection is queued by now.
        self.drain();
        result
    }

    fn live(&self, scope: &str) -> Value {
        self.gate
            .replicas
            .get(scope)
            .map_or(Value::Null, Document::live)
    }

    /// The entries the scope's replica holds.
    fn entries(&self, scope: &str) -> Vec<Value> {
        self.gate
            .replicas
            .get(scope)
            .map_or_else(Vec::new, Document::entries)
    }

    fn has(&self, scope: &str) -> bool {
        self.gate.replicas.contains_key(scope)
    }

    fn events_of(&self, scope: &str) -> Vec<Value> {
        self.gate.events_of(scope)
    }

    fn ever(&self, scope: &str, predicate: impl Fn(&Value) -> bool) -> bool {
        self.gate.ever(scope, predicate)
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

fn agent_request(id: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "elicitation/create", "params": params})
}

fn result(request: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
}

fn interaction_ids(live: &Value) -> Vec<String> {
    live["interactions"]
        .as_object()
        .map(|interactions| interactions.keys().cloned().collect())
        .unwrap_or_default()
}

fn patch_paths(event: &Value) -> Vec<(String, String)> {
    event["ops"]
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

/// Select actual R4 changes. For a tap following `/sessions`, strip only the session's
/// pointer prefix; no operation or snapshot is synthesized from the resulting document.
fn scope_changes(event: &Event, scope: &str) -> Vec<Value> {
    let path = path_of(scope);
    event.changes.iter().flatten().filter_map(|change| {
        if change.path() == path {
            return Some(serde_json::to_value(change).unwrap());
        }
        if change.path() != "/sessions" || !scope.starts_with("session/") {
            return None;
        }
        let prefix = path.strip_prefix("/sessions").unwrap();
        match change {
            Change::Ops { ops, .. } => {
                let ops: Vec<Value> = ops.iter().filter_map(|op| {
                    let mut value = serde_json::to_value(op).unwrap();
                    let relative = value["path"].as_str()?.strip_prefix(prefix)?;
                    if !relative.is_empty() && !relative.starts_with('/') { return None; }
                    value["path"] = json!(relative);
                    Some(value)
                }).collect();
                (!ops.is_empty()).then(|| json!({ "path": path, "ops": ops }))
            }
            Change::Snapshot { snapshot, .. } => {
                use crate::protocol::tree_stream::wire::SNode;
                let id = scope.strip_prefix("session/").unwrap();
                let contains = match &snapshot.node {
                    SNode::Value { v } => v.get(id).is_some(),
                    SNode::Map { m, .. } => m.iter().any(|key| key == id),
                    _ => false,
                };
                contains.then(|| serde_json::to_value(change).unwrap())
            }
            Change::End { .. } => None,
        }
    }).collect()
}

/// The text of the agent messages among `entries`, chunk after chunk.
fn agent_text(entries: &[Value]) -> String {
    entries
        .iter()
        .filter(|entry| entry["kind"] == "message" && entry["role"] == "agent")
        .flat_map(|entry| entry["chunks"].as_array().cloned().unwrap_or_default())
        .filter_map(|chunk| chunk["content"]["text"].as_str().map(str::to_owned))
        .collect()
}

const CREATED: &str = "created";

// ---------------------------------------------------------------------------
// 1. The commit runs when a mutable guard is released

#[tokio::test]
async fn a_released_mutable_guard_commits_and_an_unchanged_one_sends_nothing() {
    let mut peer = Peer::start(json!({})).await;
    peer.settled().await;
    let before = peer.gate.history.len();

    // Mutable access that changes nothing publishes nothing.
    {
        let lock = peer.lock();
        let mut state = lock.lock().await;
        let state: &mut BridgeState = &mut state;
        state.catalog_revision += 0;
    }
    peer.settled().await;
    assert_eq!(
        peer.gate.history.len(),
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
    assert_eq!(peer.gate.history.len(), before + 1, "one commit");
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
// The gate's own counterexamples, on the registry and the commit step alone

fn ready_agent() -> crate::protocol::documents::AgentInfo {
    crate::protocol::documents::AgentInfo {
        phase: crate::protocol::documents::AgentPhase::Ready,
        error: None,
        launch: Default::default(),
        initialize: None,
        authenticated_method_id: None,
    }
}

/// One commit of `registry` to `feed`: its events and every published document.
fn publish(
    feed: &crate::protocol::feed::FeedHandle,
    commits: &mut mpsc::UnboundedReceiver<TapCommit>,
    registry: &SessionRegistry,
) -> (Vec<Event>, Documents) {
    feed.commit(registry, &ready_agent(), 0);
    match commits.try_recv() {
        Ok(commit) => (commit.events, commit.documents),
        Err(_) => (Vec::new(), feed.documents()),
    }
}

fn tapped_feed(registry: &SessionRegistry) -> (
    crate::protocol::feed::FeedHandle,
    mpsc::UnboundedReceiver<TapCommit>,
    TapCommit,
) {
    let feed = crate::protocol::feed::FeedHandle::new();
    feed.commit(registry, &ready_agent(), 0);
    let (tap, commits) = mpsc::unbounded_channel();
    let cut = feed.tap(tap);
    (feed, commits, cut)
}

#[test]
fn ever_sees_a_change_of_a_scope_aligned_by_the_cut() {
    let mut registry = SessionRegistry::new("epoch");
    let (feed, mut commits, cut) = tapped_feed(&registry);
    let mut gate = Gate::new(&cut);
    registry
        .upsert_elicitation("epoch", None, "brief", json!({"mode": "form"}))
        .unwrap();
    let (events, documents) = publish(&feed, &mut commits, &registry);
    gate.apply(events, &documents);
    registry
        .resolve_elicitation("epoch", None, "brief", None)
        .unwrap();
    let (events, documents) = publish(&feed, &mut commits, &registry);
    gate.apply(events, &documents);
    assert!(interaction_ids(&gate.replicas["global"].live()).is_empty());
    assert!(
        gate.ever("global", |live| interaction_ids(live) == ["brief"]),
        "a brief interaction on a scope from the cut is seen"
    );
}

#[test]
#[should_panic(expected = "session/s")]
fn the_gate_rejects_an_entry_change_without_its_event() {
    let mut registry = SessionRegistry::new("epoch");
    registry
        .open_new("epoch", "s", "/s", json!({"sessionId": "s"}))
        .unwrap();
    let (feed, mut commits, cut) = tapped_feed(&registry);
    let mut gate = Gate::new(&cut);
    registry
        .append_session_update(
            "s",
            json!({"sessionUpdate": "agent_message_chunk", "messageId": "m", "content": text("hi")}),
        )
        .unwrap();
    let (events, documents) = publish(&feed, &mut commits, &registry);
    assert!(
        events.iter().any(|event| !scope_changes(event, "session/s").is_empty()),
        "the change is session-only: {events:?}"
    );
    let dropped = events
        .into_iter()
        .filter(|event| scope_changes(event, "session/s").is_empty())
        .collect();
    gate.apply(dropped, &documents);
}

fn synthetic(rev: u64, node: Value) -> TapCommit {
    let event = serde_json::from_value(json!({
        "instance": "e", "rev": rev, "changes": [{
            "path": "/sessions", "snapshot": {
                "base": null, "at": rev, "hash": "fixture", "node": {"v": {"s": node}},
            },
        }],
    })).unwrap();
    TapCommit {
        events: vec![event],
        documents: BTreeMap::from([("session/s".into(), node)]),
    }
}

#[test]
#[should_panic(expected = "in one event")]
fn the_atomic_check_rejects_a_prompt_that_ends_before_its_operation() {
    let prompts = json!([{"entry": 0}]);
    let running = json!({"prompts": prompts, "op": {"id": "p", "status": "running"}});
    let ended = json!({"prompts": prompts, "op": {"id": "p", "status": "completed"}});
    let scope = "session/s";
    let mut gate = Gate::new(&synthetic(1, running));
    let completes = synthetic(2, ended.clone());
    gate.apply(completes.events, &completes.documents);
    let mut with_end = ended;
    with_end["prompts"][0]["end"] = json!({"response": {"stopReason": "end_turn"}});
    let end = synthetic(3, with_end);
    gate.apply(end.events, &end.documents);
    gate.assert_ended_atomically(scope, &json!("p"));
}

// ---------------------------------------------------------------------------
// 2. The session path

#[tokio::test]
async fn a_streamed_prompt_with_an_answered_permission_ends_in_one_event() {
    let mut peer = Peer::start(json!({})).await;
    peer.create(CREATED, json!({})).await;
    let scope = format!("session/{CREATED}");
    assert_eq!(peer.live(&scope)["op"]["kind"], "open");
    assert!(peer.entries(&scope).is_empty());

    let prompt = vec![text("hi")];
    let (op, request) = peer.prompt(CREATED, prompt).await;
    peer.update(CREATED, chunk("answer", text("hel"))).await;
    peer.until("the first chunk streams", |peer| {
        agent_text(&peer.entries(&scope)) == "hel"
    })
    .await;
    peer.update(CREATED, chunk("answer", text("lo"))).await;
    peer.until("the next chunk follows", |peer| {
        agent_text(&peer.entries(&scope)) == "hello"
    })
    .await;
    let streamed = peer.events_of(&scope).pop().unwrap();
    assert!(
        patch_paths(&streamed)
            .iter()
            .all(|(_, path)| path.starts_with("/entries/")),
        "a streamed chunk changes its entry alone: {streamed}"
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
    assert_eq!(live["interactions"][&interaction]["kind"], "permission");
    assert_eq!(
        peer.live("global")["sessions"][CREATED]["pending"]["interactions"],
        1
    );
    peer.answer(
        &scope,
        &interaction,
        json!({"outcome": {"outcome": "selected", "optionId": "allow"}}),
    )
    .await;
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
    peer.gate.assert_ended_atomically(&scope, &json!(op));
    let live = peer.live(&scope);
    assert_eq!(live["op"]["id"], op);
    assert_eq!(live["op"]["result"], json!({ "stopReason": "end_turn" }));
    let entries = peer.entries(&scope);
    // The prompt's message, the reply, and the tool call the permission request named.
    assert_eq!(entries.len(), 3, "{entries:?}");
    assert_eq!(entries[0]["role"], "user");
    assert_eq!(entries[0]["chunks"][0]["content"], text("hi"));
    assert_eq!(
        entries.iter().filter(|entry| entry["call"]["toolCallId"] == "tool").count(),
        1,
        "the permission's tool call is one entry: {entries:?}"
    );
    let prompts = &peer.gate.replicas[&scope].doc["prompts"];
    assert_eq!(prompts[0]["entry"], 0);
    assert_eq!(prompts[0]["end"]["response"]["stopReason"], "end_turn");
    peer.settled().await;
}

#[tokio::test]
async fn a_failed_prompt_keeps_what_streamed_and_ends_with_its_error() {
    let mut peer = Peer::start(json!({})).await;
    peer.create(CREATED, json!({})).await;
    let scope = format!("session/{CREATED}");
    let prompt = vec![text("hi")];
    let (op, request) = peer.prompt(CREATED, prompt).await;
    peer.update(CREATED, chunk("answer", text("partial"))).await;
    peer.until("the chunk streams", |peer| {
        agent_text(&peer.entries(&scope)) == "partial"
    })
    .await;
    peer.refuse(&request).await;
    peer.until("the prompt fails", |peer| {
        peer.live(&scope)["op"]["status"] == "failed"
    })
    .await;
    peer.gate.assert_ended_atomically(&scope, &json!(op));
    let live = peer.live(&scope);
    assert_eq!(live["op"]["error"]["code"], "agent_error");
    assert_eq!(live["op"]["input"], json!({ "prompt": [text("hi")] }));
    assert_eq!(
        agent_text(&peer.entries(&scope)),
        "partial",
        "a failed turn keeps what the Agent sent"
    );
    let prompts = &peer.gate.replicas[&scope].doc["prompts"];
    assert!(prompts[0]["end"]["error"].is_object(), "{prompts}");
    peer.settled().await;
}

#[tokio::test]
async fn a_cancelled_stop_reason_completes_the_prompt() {
    let mut peer = Peer::start(json!({})).await;
    peer.create(CREATED, json!({})).await;
    let scope = format!("session/{CREATED}");
    let prompt = vec![text("hi")];
    let (_, request) = peer.prompt(CREATED, prompt).await;
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
async fn attachments_are_served_while_the_entries_that_reference_them_are_published() {
    let mut peer = Peer::start(json!({"promptCapabilities": {"image": true}})).await;
    peer.create(CREATED, json!({})).await;
    let scope = format!("session/{CREATED}");
    let prompt = vec![text("look"), image("cHJvbXB0")];
    let (_, request) = peer.prompt(CREATED, prompt).await;
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
            "content": [{"type": "content", "content": image("dG9vbA==")}],
        }),
    )
    .await;
    let tool = |peer: &Peer| {
        peer.entries(&scope)
            .into_iter()
            .find(|entry| entry["kind"] == "tool")
    };
    peer.until("the tool call is published", |peer| tool(peer).is_some())
        .await;
    let tool = tool(&peer).unwrap();
    let reference = &tool["call"]["content"][0]["content"];
    assert_eq!(reference["type"], "attachment", "{tool}");
    let tool_attachment = reference["id"].as_str().unwrap().to_string();
    assert_eq!(
        peer.attachment(CREATED, &tool_attachment).await.unwrap()["data"],
        "dG9vbA=="
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
        "the user entry keeps the prompt's attachment"
    );
    assert!(
        peer.attachment(CREATED, &tool_attachment).await.is_some(),
        "the tool entry keeps its attachment: entries are never removed"
    );
    peer.settled().await;
}

#[tokio::test]
async fn a_set_mode_action_folds_its_acknowledgement_and_installs_no_operation() {
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
    let shown = peer.live(&scope)["op"].clone();
    let answer = peer
        .control(
            CREATED,
            crate::bridge::controls::Control::SetMode { mode_id: "plan".into() },
        )
        .await;
    let request = peer.request("session/set_mode").await;
    peer.reply(&request, json!({})).await;
    answer.await.unwrap().unwrap();
    peer.until("the mode is folded", |peer| {
        peer.live(&scope)["controls"]["modes"]["currentModeId"] == "plan"
    })
    .await;
    let live = peer.live(&scope);
    assert_eq!(live["op"], shown, "a control installs no operation");
    let event = peer.events_of(&scope).pop().unwrap();
    let paths = patch_paths(&event);
    assert!(
        paths.iter().all(|(_, path)| path.starts_with("/controls/")),
        "the acknowledgement changes controls and nothing else: {event}"
    );
    peer.settled().await;
}

// ---------------------------------------------------------------------------
// 3. Global and catalog operations

#[tokio::test]
async fn a_create_completes_with_its_session_and_advances_the_catalog() {
    let mut peer = Peer::start(json!({})).await;
    let revision = peer.live("catalog")["revision"].clone();
    let op = peer.create(CREATED, json!({})).await;
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
        .gate
        .history
        .iter()
        .position(|events| events.iter().any(|event| !scope_changes(event, &scope).is_empty()))
        .unwrap();
    assert!(
        peer.gate.history[appeared]
            .iter()
            .flat_map(|event| scope_changes(event, "global"))
            .any(|change| patch_paths(&change).iter().any(|(_, path)| path.starts_with("/op"))),
        "the target appears in the commit that completes the create"
    );
    assert_ne!(peer.live("catalog")["revision"], revision);
    peer.settled().await;
}

#[tokio::test]
async fn a_failed_create_fails_its_operation() {
    let mut peer = Peer::start(json!({})).await;
    let (_, created) = peer
        .start_op("global", "create", json!({"cwd": "/workspace"}))
        .await;
    let request = peer.request("session/new").await;
    peer.refuse(&request).await;
    peer.ended(created).await.unwrap_err();
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
    let (prompt_op, prompt) = peer.prompt("source", vec![text("source prompt")]).await;
    peer.update("source", chunk("source-reply", text("source reply")))
        .await;
    peer.update(
        "source",
        json!({"sessionUpdate": "session_info_update", "title": "Source title"}),
    )
    .await;
    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    peer.until("the source turn commits", |peer| {
        peer.live("session/source")["op"]["id"] == prompt_op
            && peer.live("session/source")["op"]["status"] == "completed"
    })
    .await;
    let source_entries = peer.entries("session/source");
    assert_eq!(source_entries.len(), 2);
    assert_eq!(source_entries[0]["chunks"][0]["content"], text("source prompt"));
    assert_eq!(agent_text(&source_entries), "source reply");
    let (op, forked) = peer.start_op("session/source", "fork", json!({})).await;
    let request = peer.request("session/fork").await;
    // The target's update follows its response in one physical ACP batch.
    // It must reach the new target after the response installs that session.
    peer.send(json!([
        {"jsonrpc": "2.0", "id": request["id"], "result": {"sessionId": "forked"}},
        {"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": "forked", "update": {
                "sessionUpdate": "session_info_update", "title": "Fork title",
            },
        }},
    ]))
    .await;
    peer.ended(forked).await.unwrap();
    peer.until("the fork completes", |peer| {
        peer.live("session/source")["op"]["id"] == op
            && peer.live("session/source")["op"]["status"] == "completed"
            && peer.live("session/forked")["controls"]["info"]["title"] == "Fork title"
    })
    .await;
    let source = peer.live("session/source");
    assert_eq!(source["op"]["id"], op);
    assert_eq!(source["op"]["result"], json!({"sessionId": "forked"}));
    assert_eq!(source["controls"]["info"]["title"], "Source title");
    assert_eq!(peer.entries("session/source"), source_entries);
    let target = peer.live("session/forked");
    assert_eq!(target["op"]["kind"], "open");
    assert_eq!(target["op"]["status"], "completed");
    assert!(
        peer.entries("session/forked").is_empty(),
        "the target never inherits the source's entries"
    );
    peer.settled().await;
}

/// Early process updates without a message are no history: the target
/// still keeps the source's completed messages (#8).
#[tokio::test]
async fn a_forks_early_process_updates_alone_keep_the_sources_log() {
    let mut peer = Peer::start(json!({"sessionCapabilities": {"fork": {}}})).await;
    peer.create("source", json!({})).await;
    let (prompt_op, prompt) = peer.prompt("source", vec![text("source prompt")]).await;
    peer.update("source", chunk("source-reply", text("source reply")))
        .await;
    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    peer.until("the source turn commits", |peer| {
        peer.live("session/source")["op"]["id"] == prompt_op
            && peer.live("session/source")["op"]["status"] == "completed"
    })
    .await;
    let source_entries = peer.entries("session/source");
    assert_eq!(source_entries.len(), 2);
    let (op, forked) = peer.start_op("session/source", "fork", json!({})).await;
    let request = peer.request("session/fork").await;
    peer.update(
        "forked",
        json!({"sessionUpdate": "agent_thought_chunk", "content": text("thinking")}),
    )
    .await;
    peer.update(
        "forked",
        json!({"sessionUpdate": "plan", "entries": [
            {"content": "Step", "priority": "medium", "status": "pending"},
        ]}),
    )
    .await;
    peer.reply(&request, json!({"sessionId": "forked"})).await;
    peer.ended(forked).await.unwrap();
    peer.until("the fork completes", |peer| {
        peer.live("session/source")["op"]["id"] == op
            && peer.live("session/source")["op"]["status"] == "completed"
            && peer.has("session/forked")
    })
    .await;
    assert_eq!(peer.entries("session/source"), source_entries);
    let kinds: Vec<_> = peer
        .entries("session/forked")
        .iter()
        .map(|entry| entry["kind"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        kinds,
        ["message", "plan"],
        "the target holds what the Agent sent for it, and the source's entries stay"
    );
    peer.settled().await;
}

/// The Agent's messages for the target before the fork response are its
/// history: without load support nothing replaces them (#8).
#[tokio::test]
async fn a_forks_early_messages_are_its_targets_log_without_load_support() {
    let mut peer = Peer::start(json!({"sessionCapabilities": {"fork": {}}})).await;
    peer.create("source", json!({})).await;
    let (op, forked) = peer.start_op("session/source", "fork", json!({})).await;
    let request = peer.request("session/fork").await;
    let early = [
        chunk("welcome", text("Forked ")),
        chunk("welcome", text("session.")),
    ];
    for update in &early {
        peer.update("forked", update.clone()).await;
    }
    peer.reply(&request, json!({"sessionId": "forked"})).await;
    peer.ended(forked).await.unwrap();
    peer.until("the fork completes", |peer| {
        peer.live("session/source")["op"]["id"] == op
            && peer.live("session/source")["op"]["status"] == "completed"
            && peer.has("session/forked")
    })
    .await;
    let entries = peer.entries("session/forked");
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0]["messageId"], "welcome");
    assert_eq!(
        entries[0]["chunks"],
        json!([{"content": text("Forked session.")}]),
        "the target's entries are its early messages, adjacent plain text merged"
    );
    peer.settled().await;
}

/// The Agent's messages for a new session before the `session/new` response are
/// its first history (#8): in bridge memory, with the notifications' own `_meta`.
#[tokio::test]
async fn a_new_sessions_early_messages_are_the_start_of_its_timeline() {
    let mut peer = Peer::start(json!({})).await;
    let (_, end) = peer
        .start_op("global", "create", json!({"cwd": "/workspace"}))
        .await;
    let new = peer.request("session/new").await;
    peer.update("early", chunk("welcome", text("Hello ")))
        .await;
    peer.update_with_meta(
        "early",
        chunk("welcome", text("there")),
        json!({"trace": "t1"}),
    )
    .await;
    peer.update(
        "early",
        json!({"sessionUpdate": "tool_call", "toolCallId": "t", "title": "Setup"}),
    )
    .await;
    peer.reply(&new, json!({"sessionId": "early"})).await;
    peer.ended(end).await.unwrap();
    peer.until("the created session is published", |peer| {
        peer.has("session/early")
    })
    .await;
    assert_eq!(
        peer.timeline("early").await,
        json!([
            {"kind": "message", "role": "agent", "messageId": "welcome", "chunks": [
                {"content": text("Hello ")},
                {"content": text("there"), "notificationMeta": {"trace": "t1"}},
            ]},
            {"kind": "tool", "call": {
                "toolCallId": "t", "title": "Setup", "kind": "other", "status": "pending",
                "content": [], "locations": [],
            }},
        ])
    );
    peer.settled().await;
}

#[tokio::test]
async fn a_new_session_without_early_messages_has_an_empty_known_timeline() {
    let mut peer = Peer::start(json!({})).await;
    peer.create("quiet", json!({})).await;
    assert_eq!(peer.timeline("quiet").await, json!([]));
    peer.settled().await;
}

/// A fork's target never inherits the source's entries (bridge memory model
/// §6.3): tool IDs are unique only within their own session, so an inherited
/// entry could be patched by the target's own tool, and a replay would add the
/// source's messages twice. Without load support the target holds only what the
/// Agent sent for it, as partial history.
#[tokio::test]
async fn a_forks_timeline_is_its_own_notifications_and_never_the_sources() {
    let mut peer = Peer::start(json!({"sessionCapabilities": {"fork": {}}})).await;
    peer.create("source", json!({})).await;
    peer.update(
        "source",
        json!({"sessionUpdate": "tool_call", "toolCallId": "1", "title": "source tool"}),
    )
    .await;
    peer.update("source", chunk("s", text("source says"))).await;
    let source = peer.timeline_len("source", 2).await;

    let (op, forked) = peer.start_op("session/source", "fork", json!({})).await;
    let request = peer.request("session/fork").await;
    peer.update(
        "forked",
        json!({"sessionUpdate": "tool_call", "toolCallId": "1", "title": "target tool"}),
    )
    .await;
    peer.update("forked", chunk("t", text("target says"))).await;
    peer.reply(&request, json!({"sessionId": "forked"})).await;
    peer.ended(forked).await.unwrap();
    peer.until("the fork completes", |peer| {
        peer.live("session/source")["op"]["id"] == op
            && peer.live("session/source")["op"]["status"] == "completed"
            && peer.has("session/forked")
    })
    .await;

    assert_eq!(
        peer.timeline("forked").await,
        json!([
            {"kind": "tool", "call": {
                "toolCallId": "1", "title": "target tool", "kind": "other",
                "status": "pending", "content": [], "locations": [],
            }},
            {"kind": "message", "role": "agent", "messageId": "t",
             "chunks": [{"content": text("target says")}]},
        ]),
        "the target's tool 1 is its own entry, not a patch of the source's"
    );
    assert_eq!(
        peer.history("forked").await,
        Some(crate::protocol::timeline::History::Partial)
    );
    assert_eq!(peer.timeline("source").await, source, "the source is unchanged");
    peer.settled().await;
}

#[tokio::test]
async fn a_fork_without_early_notifications_has_an_empty_partial_timeline() {
    let mut peer = Peer::start(json!({"sessionCapabilities": {"fork": {}}})).await;
    peer.create("source", json!({})).await;
    peer.update("source", chunk("s", text("source says"))).await;
    peer.timeline_len("source", 1).await;

    let (op, forked) = peer.start_op("session/source", "fork", json!({})).await;
    let request = peer.request("session/fork").await;
    peer.reply(&request, json!({"sessionId": "forked"})).await;
    peer.ended(forked).await.unwrap();
    peer.until("the fork completes", |peer| {
        peer.live("session/source")["op"]["id"] == op
            && peer.live("session/source")["op"]["status"] == "completed"
            && peer.has("session/forked")
    })
    .await;

    assert_eq!(peer.timeline("forked").await, json!([]));
    assert_eq!(
        peer.history("forked").await,
        Some(crate::protocol::timeline::History::Partial),
        "nothing says the target starts at the session's beginning"
    );
    peer.settled().await;
}

#[tokio::test]
async fn a_forks_load_replaces_its_timeline_with_the_replay_as_complete() {
    let mut peer =
        Peer::start(json!({"loadSession": true, "sessionCapabilities": {"fork": {}}})).await;
    peer.create("source", json!({})).await;
    let (op, forked) = peer.start_op("session/source", "fork", json!({})).await;
    let request = peer.request("session/fork").await;
    peer.update("forked", chunk("early", text("early"))).await;
    peer.reply(&request, json!({"sessionId": "forked"})).await;
    peer.ended(forked).await.unwrap();
    peer.until("the fork completes", |peer| {
        peer.live("session/source")["op"]["id"] == op
            && peer.live("session/source")["op"]["status"] == "completed"
            && peer.has("session/forked")
    })
    .await;

    let load = peer.request("session/load").await;
    peer.update(
        "forked",
        json!({"sessionUpdate": "user_message_chunk", "content": text("hi")}),
    )
    .await;
    peer.update("forked", chunk("answer", text("hello"))).await;
    peer.reply(&load, json!({})).await;
    // The target's load is a background history workflow, not a published
    // operation: its commit shows in bridge memory, where two replayed entries
    // replace the one staged entry.
    let timeline = peer.timeline_len("forked", 2).await;

    assert_eq!(
        timeline,
        json!([
            {"kind": "message", "role": "user", "chunks": [{"content": text("hi")}]},
            {"kind": "message", "role": "agent", "messageId": "answer",
             "chunks": [{"content": text("hello")}]},
        ]),
        "the replay replaces what was staged for the target"
    );
    assert_eq!(
        peer.history("forked").await,
        Some(crate::protocol::timeline::History::Complete)
    );
    peer.settled().await;
}

#[tokio::test]
async fn a_delete_takes_the_close_interaction_and_ends_the_scope_with_deleted() {
    let mut peer = Peer::start(json!({"sessionCapabilities": {"close": {}, "delete": {}}})).await;
    peer.create(CREATED, json!({})).await;
    let scope = format!("session/{CREATED}");
    let revision = peer.live("catalog")["revision"].clone();
    let mut stream = stream_harness::Stream::open(&peer, &[&scope], None);
    let (op, _) = peer
        .start_op("global", "delete", json!({"sessionId": CREATED}))
        .await;
    peer.until("the delete occupies the session", |peer| {
        peer.live(&scope)["op"]["id"] == op
    })
    .await;
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
    peer.answer(
        "global",
        &interaction,
        json!({"response": {"action": "accept", "content": {}}}),
    )
    .await;
    assert_eq!(
        peer.response("close-question").await["result"]["action"],
        "accept"
    );
    peer.reply(&close, json!({})).await;
    let delete = peer.request("session/delete").await;
    peer.reply(&delete, json!({})).await;
    peer.until("the delete completes", |peer| {
        peer.live("global")["op"]["status"] == "completed"
    })
    .await;
    stream.until(&mut peer, "the deleted path ends", |stream| stream.ended.contains_key(&scope)).await;
    assert_eq!(stream.ended[&scope]["reason"], "deleted");
    assert!(!peer.has(&scope));
    assert!(peer.live("global")["sessions"].get(CREATED).is_none());
    assert_eq!(peer.live("global")["op"]["id"], op);
    assert_eq!(peer.live("global")["op"]["result"], json!({}));
    assert_ne!(peer.live("catalog")["revision"], revision);
    peer.settled().await;
}

#[tokio::test]
async fn a_delayed_prompt_and_its_unchanged_retries_execute_once() {
    let mut peer = Peer::start(json!({})).await;
    peer.create(CREATED, json!({})).await;
    let scope = Scope::Session(CREATED.into());
    let scope_name = scope.to_string();
    let target = format!("sessions/{CREATED}");
    let (delayed, receipt) = peer.prepared_prompt(&target);
    // The original has not reached ingress. The resolution read overtakes it.
    assert_eq!(peer.outcome(&target, &receipt)["outcome"], "open");
    let retry = peer
        .write_request(scope.clone(), delayed.clone())
        .await
        .unwrap();
    assert_eq!(retry.op, receipt.id);
    let prompt = peer.request("session/prompt").await;
    assert_eq!(prompt["params"]["prompt"], delayed.args["prompt"]);
    // The original arrives while its retry is awaiting the Agent. It is admitted without
    // issuing another ACP request, and the same holds after the prompt completes.
    assert_eq!(
        peer.write_request(scope.clone(), delayed.clone())
            .await
            .unwrap()
            .op,
        receipt.id
    );
    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    peer.until("the one prompt completes", |peer| {
        peer.live(&scope_name)["op"]["status"] == "completed"
    })
    .await;
    assert_eq!(
        peer.write_request(scope, delayed).await.unwrap().op,
        receipt.id
    );
    assert_eq!(peer.outcome(&target, &receipt)["outcome"], "admitted");
    assert_eq!(peer.timeline(CREATED).await.as_array().unwrap().len(), 1);
    peer.settled().await;
    peer.finish_without_another_prompt().await;
}

#[tokio::test]
async fn a_failed_delete_rejects_a_delayed_prompt_and_allows_one_new_request() {
    let mut peer = Peer::start(json!({"sessionCapabilities": {"close": {}, "delete": {}}})).await;
    peer.create(CREATED, json!({})).await;
    let scope = format!("session/{CREATED}");
    let target = format!("sessions/{CREATED}");
    let mut stream = stream_harness::Stream::open(&peer, &[&scope], None);
    let (delayed, receipt) = peer.prepared_prompt(&target);
    let own = peer.live(&scope)["op"]["id"].clone();
    let (op, _) = peer
        .start_op("global", "delete", json!({"sessionId": CREATED}))
        .await;
    let close = peer.request("session/close").await;
    peer.refuse(&close).await;
    peer.until("the delete fails", |peer| {
        peer.live("global")["op"]["status"] == "failed"
    })
    .await;
    assert_eq!(peer.live("global")["op"]["id"], op);
    assert_eq!(peer.live("global")["op"]["error"]["code"], "agent_error");
    // The occupied session keeps the failed delete: its `op.id` never returns to `own`, so a
    // request that expected `own` stays superseded (design §9.2, §9.3).
    assert_ne!(peer.live(&scope)["op"]["id"], own);
    assert_eq!(peer.live(&scope)["op"]["id"], op);
    assert_eq!(peer.live(&scope)["op"]["status"], "failed");
    stream.until(&mut peer, "the failed delete is visible on the session stream", |stream| {
        stream.live(&scope)["op"]["status"] == "failed"
    }).await;
    assert!(stream.ended.is_empty(), "a failed delete does not end its session path");
    assert!(stream.events_of(&scope).iter().all(|change| change.get("end").is_none()));
    assert_eq!(peer.outcome(&target, &receipt)["outcome"], "superseded");
    // The delayed original reaches ingress before any subsequent write can hide a return to
    // the old ID. It must already be stale, and a refused prompt adds no user entry.
    let refused = peer
        .write_request(Scope::parse(&scope).unwrap(), delayed.clone())
        .await
        .err()
        .unwrap();
    assert_eq!(refused.code, crate::protocol::documents::ErrorCode::Stale);
    assert_eq!(peer.timeline(CREATED).await, json!([]));
    // Only now may the client give the input a new identity, against the failed delete.
    let (replacement, replacement_receipt) = peer.prepared_prompt(&target);
    assert_eq!(replacement.expect, op);
    let admitted = peer
        .write_request(Scope::parse(&scope).unwrap(), replacement.clone())
        .await
        .unwrap();
    assert_eq!(admitted.op, replacement_receipt.id);
    let prompt = peer.request("session/prompt").await;
    // Another delayed copy arrives after the replacement started. It remains stale; neither
    // it nor a retry of the new request may send another prompt.
    let refused = peer
        .write_request(Scope::parse(&scope).unwrap(), delayed)
        .await
        .err()
        .unwrap();
    assert_eq!(refused.code, crate::protocol::documents::ErrorCode::Stale);
    assert_eq!(
        peer.write_request(Scope::parse(&scope).unwrap(), replacement.clone())
            .await
            .unwrap()
            .op,
        admitted.op
    );
    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    peer.until("the replacement prompt completes", |peer| {
        peer.live(&scope)["op"]["status"] == "completed"
    })
    .await;
    assert_eq!(
        peer.write_request(Scope::parse(&scope).unwrap(), replacement)
            .await
            .unwrap()
            .op,
        admitted.op
    );
    assert_eq!(peer.timeline(CREATED).await.as_array().unwrap().len(), 1);
    peer.settled().await;
    peer.finish_without_another_prompt().await;
}

#[tokio::test]
async fn a_list_completes_on_the_catalog_with_its_page() {
    let mut peer = Peer::start(json!({"sessionCapabilities": {"list": {}}})).await;
    let (op, listed) = peer.start_op("catalog", "list", json!({})).await;
    let request = peer.request("session/list").await;
    peer.reply(
        &request,
        json!({"sessions": [{"sessionId": "saved", "cwd": "/workspace"}], "nextCursor": null}),
    )
    .await;
    peer.ended(listed).await.unwrap();
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
    peer.start_op("global", "authenticate", json!({"methodId": "agent-login"}))
        .await;
    let request = peer.request("authenticate").await;
    peer.reply(&request, json!({})).await;
    peer.until("the authentication completes", |peer| {
        peer.live("global")["op"]["status"] == "completed"
    })
    .await;
    assert_eq!(
        peer.live("global")["agent"]["authenticatedMethodId"],
        "agent-login"
    );

    peer.start_op("global", "logout", json!({})).await;
    let request = peer.request("logout").await;
    peer.reply(&request, json!({})).await;
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
    let (op, listed) = peer.start_op("catalog", "list", json!({})).await;
    let request = peer.request("session/list").await;
    // The parent's response clears its pending request before the
    // coordinator handles the interaction that names it.
    peer.send_held_batch(
        vec![
            agent_request("list-question", form(json!({"requestId": request["id"]}))),
            result(&request, json!({"sessions": [], "nextCursor": null})),
        ],
        &request,
    )
    .await;
    peer.ended(listed).await.unwrap();
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
    let (op, created) = peer
        .start_op("global", "create", json!({"cwd": "/workspace"}))
        .await;
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
    peer.answer(
        "global",
        &interaction,
        json!({"response": {"action": "accept"}}),
    )
    .await;
    assert_eq!(peer.response("sign-in").await["result"]["action"], "accept");
    peer.until("the accepted flow moves to urlFlows", |peer| {
        interaction_ids(&peer.live("global")).is_empty()
            && peer.live("global")["urlFlows"].get(&interaction).is_some()
    })
    .await;
    peer.reply(&request, json!({"sessionId": CREATED})).await;
    peer.ended(created).await.unwrap();
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
    peer.create(CREATED, json!({})).await;
    let scope = format!("session/{CREATED}");
    let prompt = vec![text("hi")];
    let (op, request) = peer.prompt(CREATED, prompt).await;

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
    peer.answer(
        &scope,
        &interaction,
        json!({"response": {"action": "accept"}}),
    )
    .await;
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
    peer.send_held_batch(
        vec![
            agent_request("question", form(json!({"requestId": request["id"]}))),
            result(&request, json!({"stopReason": "end_turn"})),
        ],
        &request,
    )
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
    let (a, _) = peer
        .start_op(
            "global",
            "terminal_login",
            json!({"methodId": "login-a", "cols": 80, "rows": 24}),
        )
        .await;
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
    let (b, _) = peer
        .start_op(
            "global",
            "terminal_login",
            json!({"methodId": "login-b", "cols": 80, "rows": 24}),
        )
        .await;
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
    let (reply, acted) = oneshot::channel();
    peer.commands
        .send(BridgeInput::Resource {
            scope: Scope::Global,
            resource: b.clone(),
            action: resources::ResourceAction::Input {
                data: "done\n".into(),
            },
            reply,
        })
        .unwrap();
    tokio::time::timeout(WAIT, acted)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
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

    // A login of a method the Agent did not advertise is refused before it
    // starts anything.
    let refused = peer
        .write(
            "global",
            "terminal_login",
            json!({"methodId": "missing", "cols": 80, "rows": 24}),
        )
        .await
        .err()
        .expect("an unknown login method is refused");
    assert_eq!(
        refused.code,
        crate::protocol::documents::ErrorCode::InvalidRequest
    );
    assert_eq!(peer.live("global")["op"]["id"], b);
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
    peer.create(CREATED, json!({})).await;
    let scope = format!("session/{CREATED}");
    let prompt = vec![text("hi")];
    let (_, request) = peer.prompt(CREATED, prompt).await;
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
    peer.answer(
        &scope,
        &interaction,
        json!({"response": {"action": "accept"}}),
    )
    .await;
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
    peer.start_op("global", "authenticate", json!({"methodId": "agent-login"}))
        .await;
    peer.request("authenticate").await;
    let (_, _listing) = peer.start_op("catalog", "list", json!({})).await;
    peer.request("session/list").await;
    peer.settled().await;

    peer.stop().await.unwrap();
    assert!(
        peer.state.upgrade().is_none(),
        "the stopped connection still owns its coordinator with requests pending"
    );
    let global = peer.live("global");
    assert_eq!(global["agent"]["phase"], "stopped");
    for scope in ["global", "catalog"] {
        let live = peer.live(scope);
        assert_eq!(live["op"]["status"], "failed", "{scope}: {live}");
        assert_eq!(live["op"]["error"]["code"], "unavailable", "{scope}");
        assert!(interaction_ids(&live).is_empty(), "{scope}");
        assert!(
            live["urlFlows"].as_object().unwrap().is_empty(),
            "{scope} keeps no URL flow"
        );
    }
    // No subscription follows the session: with its operation and interactions gone and its
    // URL flow ended, nothing needs it, and the connection's end releases it from the final
    // documents (S6).
    let published = peer.publication.lock();
    assert!(published.feed.with_tree(|tree| tree.get(&path_of(&scope)).is_none()));
    let summary = published.feed.with_tree(|tree| tree.get("/global").unwrap().to_value());
    assert!(summary["sessions"].get(CREATED).is_none(), "{summary}");
    assert!(global["sessions"].get(CREATED).is_none(), "{global}");
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
async fn the_same_history_gives_the_same_entries_after_a_restart() {
    async fn load(replay: &[Value]) -> Vec<Value> {
        let mut peer = Peer::start(json!({"loadSession": true})).await;
        let _stream = peer.subscribe(StreamQuery {
            follow: vec![Follow { path: "/sessions/saved".into(), have: None, window: None }],
            cwd: BTreeMap::from([("saved".into(), "/workspace".into())]),
            ..Default::default()
        });
        let request = peer.request("session/load").await;
        for update in replay {
            peer.update("saved", update.clone()).await;
        }
        peer.reply(&request, json!({})).await;
        peer.until("the load completes", |peer| {
            peer.live("session/saved")["op"]["status"] == "completed"
        })
        .await;
        peer.settled().await;
        let entries = peer.entries("session/saved");
        peer.stop().await.unwrap();
        entries
    }
    let replay = [
        json!({"sessionUpdate": "user_message_chunk", "content": text("hi")}),
        chunk("answer", text("hel")),
        chunk("answer", text("lo")),
    ];
    let first = load(&replay).await;
    assert_eq!(first.len(), 2);
    assert_eq!(first, load(&replay).await, "a restart keeps the same entries");
}

mod concurrent_controls;
mod holding;
mod loads;
mod reconnection;
mod review_lifecycle;
mod stream_harness;

// -- The revision 4 feed --------------------------------------------------------------

/// The path versions the feed publishes.
fn feed_versions(peer: &Peer) -> BTreeMap<String, u64> {
    peer.publication.lock().feed.with_tree(|tree| {
        ["global", "catalog", "session/created", "session/other"]
            .into_iter()
            .filter_map(|scope| tree.get(&path_of(scope)).map(|node| (scope.to_string(), node.srev())))
            .collect()
    })
}

#[tokio::test]
async fn every_commit_of_the_bridge_is_published_by_the_feed_too() {
    let mut peer = Peer::start(json!({})).await;
    let versions = feed_versions(&peer);
    assert!(versions.contains_key("global") && versions.contains_key("catalog"), "{versions:?}");

    peer.create("created", json!({})).await;
    let versions = feed_versions(&peer);
    let created = *versions.get("session/created").expect("the session is published");
    assert!(created > 0, "the tree assigns the creation commit's revision");

    peer.update("created", chunk("m", text("hello"))).await;
    peer.until("an update advances the session path", |peer| {
        agent_text(&peer.entries("session/created")) == "hello"
    }).await;
    assert!(feed_versions(&peer)["session/created"] > created, "an update that changed the timeline has a later path version");
    peer.settled().await;
}

#[tokio::test]
async fn a_subscription_to_the_feed_is_aligned_and_then_follows_the_bridge() {
    let mut peer = Peer::start(json!({})).await;
    peer.create("created", json!({})).await;
    let feed = peer.publication.lock().feed.clone();
    let subscription = feed.subscribe_tree(&StreamQuery {
        follow: vec![Follow { path: "/sessions/created".into(), have: None, window: None }],
        ..Default::default()
    });
    let mut client = Client::following(&["/sessions/created"]);
    let aligned = tokio::time::timeout(WAIT, subscription.next()).await.unwrap().unwrap();
    assert!(matches!(&aligned.changes.as_ref().unwrap()[0], Change::Snapshot { .. }), "the first event is a snapshot");
    client.apply(&aligned).unwrap();
    assert_eq!(client.value("/sessions/created").unwrap()["sessionId"], "created");

    peer.update("created", chunk("m", text("hello"))).await;
    let next = tokio::time::timeout(WAIT, subscription.next()).await.unwrap().unwrap();
    assert!(matches!(&next.changes.as_ref().unwrap()[0], Change::Ops { .. }), "the bridge's change reaches the subscriber: {next:?}");
    client.apply(&next).unwrap();
    assert_eq!(agent_text(client.value("/sessions/created").unwrap()["entries"].as_array().unwrap()), "hello");
    peer.settled().await;
}

// -- Source metadata of the controls (D12) ----------------------------------------------------

impl Peer {
    /// The controls of a session, read from bridge memory.
    async fn controls(&self, session_id: &str) -> Value {
        let lock = self.lock();
        let state = lock.lock().await;
        let live = state
            .sessions
            .state(session_id)
            .and_then(|session| session.live.as_ref())
            .expect("the session is held");
        serde_json::to_value(&live.controls).unwrap()
    }
}

#[tokio::test]
async fn a_control_update_keeps_its_own_metadata_and_that_of_its_notification() {
    let mut peer = Peer::start(json!({})).await;
    peer.create("created", json!({"modes": {"currentModeId": "ask", "availableModes": [
        {"id": "ask", "name": "Ask"}, {"id": "plan", "name": "Plan"},
    ]}})).await;
    peer.update_with_meta(
        "created",
        json!({"sessionUpdate": "current_mode_update", "currentModeId": "plan", "_meta": {"update": 1}}),
        json!({"envelope": "a"}),
    )
    .await;
    peer.update_with_meta(
        "created",
        json!({"sessionUpdate": "usage_update", "used": 2, "size": 8}),
        json!({"envelope": "u"}),
    )
    .await;
    let mut controls = peer.controls("created").await;
    for _ in 0..200 {
        if controls["meta"]["usage"].is_object() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        controls = peer.controls("created").await;
    }
    assert_eq!(controls["modes"]["currentModeId"], "plan");
    assert_eq!(
        controls["meta"]["modes"],
        json!({"update": {"update": 1}, "notification": {"envelope": "a"}})
    );
    assert_eq!(controls["meta"]["usage"], json!({"notification": {"envelope": "u"}}));
    peer.settled().await;
}

