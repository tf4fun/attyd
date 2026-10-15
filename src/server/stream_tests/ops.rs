//! `POST /api/v2/scopes/{scope}/ops`, the resource actions and the §8 reads
//! over real HTTP (migration step 6; protocol §6–§9).
//!
//! Every write goes through the server's routes: no test installs an
//! operation or answers through v1. Each event a stream delivers is applied
//! at the wire level and checked against the documents the bridge published
//! after each of its commits (the per-event gate, docs/state-sync.md §4.1).
//! The published documents are only compared with; a stream never repairs
//! its replicas from them.
use tokio::task::JoinHandle;

use super::*;
use crate::bridge::publication::{PublishedCommit, documents};
use crate::protocol::stream_check::Documents;

mod admission;
mod execution;
mod interactions;
mod login_terminals;
mod occupation;
mod url_flows;

const QUIET: Duration = Duration::from_millis(300);

// ---------------------------------------------------------------------------
// The published timeline

/// The documents the bridge published, after each commit of its first
/// generation, from a cut taken before that generation's first commit.
struct Timeline {
    documents: Vec<Documents>,
    commits: mpsc::UnboundedReceiver<PublishedCommit>,
}

impl Timeline {
    /// Taps the next generation of `hub`, before it starts.
    fn arm(hub: &BridgeHub) -> Arc<Mutex<Self>> {
        let (tap, commits) = mpsc::unbounded_channel();
        let mut published = hub.publication.lock();
        published.next_tap = Some(tap);
        let cut = documents(&published.publisher);
        Arc::new(Mutex::new(Self {
            documents: vec![cut],
            commits,
        }))
    }

    fn pull(&mut self) {
        while let Ok(commit) = self.commits.try_recv() {
            self.documents.push(commit.documents);
        }
    }

    fn latest(&mut self) -> usize {
        self.pull();
        self.documents.len() - 1
    }

    /// The first published documents from `from` on that satisfy `matches`,
    /// waiting for later commits.
    async fn find(
        &mut self,
        from: usize,
        what: &str,
        matches: impl Fn(&Documents) -> bool,
    ) -> usize {
        let deadline = tokio::time::Instant::now() + WAIT;
        let mut index = from;
        loop {
            self.pull();
            while index < self.documents.len() {
                if matches(&self.documents[index]) {
                    return index;
                }
                index += 1;
            }
            match tokio::time::timeout_at(deadline, self.commits.recv()).await {
                Ok(Some(commit)) => self.documents.push(commit.documents),
                _ => panic!("{what}: no published documents from #{from} on match"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The server

/// A server whose first generation's commits are tapped, with the HTTP
/// client the tests write through.
struct Bench {
    address: SocketAddr,
    hub: Arc<BridgeHub>,
    timeline: Arc<Mutex<Timeline>>,
    agents: Option<Agents>,
}

fn ws_options(agents: &Agents) -> Options {
    Options::try_parse_from(["attyd", "--transport", "ws", "--", &agents.url])
        .unwrap()
        .normalized()
        .unwrap()
}

impl Bench {
    async fn serve(options: Options, agents: Option<Agents>) -> Self {
        let options = Arc::new(options);
        let hub = BridgeHub::new(options.clone());
        let timeline = Timeline::arm(&hub);
        hub.ensure_runtime().await;
        let app = app_router(&options, hub.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            address,
            hub,
            timeline,
            agents,
        }
    }

    /// A server whose scripted Agent answers `initialize` with `capabilities`.
    async fn ws(capabilities: Value) -> (Self, Link) {
        Self::ws_initialized(json!({
            "protocolVersion": 1, "agentCapabilities": capabilities, "authMethods": [],
        }))
        .await
    }

    /// A server whose scripted Agent answers `initialize` with `result`,
    /// once the bridge published `global.agent.phase == ready`.
    async fn ws_initialized(result: Value) -> (Self, Link) {
        let agents = Agents::listen().await;
        let options = ws_options(&agents);
        let mut bench = Self::serve(options, Some(agents)).await;
        let mut link = bench.agents.as_mut().unwrap().connect().await;
        let initialize = link.request("initialize").await;
        link.reply(&initialize, result).await;
        bench
            .timeline
            .lock()
            .await
            .find(0, "the Agent is ready", |documents| {
                documents
                    .get("global")
                    .is_some_and(|global| global.live["agent"]["phase"] == "ready")
            })
            .await;
        (bench, link)
    }

    /// A server whose Agent is the stdio fake Agent fixture, run through
    /// `sh -c script agent`; the script decides what a login runs.
    async fn stdio(script: &str, cwd: &str) -> Self {
        let options =
            Options::try_parse_from(["attyd", "--cwd", cwd, "--", "sh", "-c", script, "agent"])
                .unwrap()
                .normalized()
                .unwrap();
        Self::serve(options, None).await
    }

    fn accepted(&self) -> usize {
        self.agents.as_ref().unwrap().accepted()
    }

    // -- HTTP -----------------------------------------------------------------

    /// A request with an optional JSON body: its status and its JSON body
    /// (`null` when empty, a string when it is not JSON).
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let response = send(self.address, method, path, body).await;
        body_of(response).await
    }

    /// A raw request: `content_type` and `body` exactly as given.
    async fn raw(&self, path: &str, content_type: Option<&str>, body: &str) -> (StatusCode, Value) {
        let stream = TcpStream::connect(self.address).await.unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(connection);
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::HOST, self.address.to_string())
            .header(header::ORIGIN, format!("http://{}", self.address));
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        let request = request.body(Body::from(body.to_string())).unwrap();
        let response = tokio::time::timeout(WAIT, sender.send_request(request))
            .await
            .expect("the server did not answer")
            .unwrap();
        body_of(response).await
    }

    /// `POST /api/v2/scopes/{scope}/ops`.
    async fn op(
        &self,
        scope: &str,
        expect: &Value,
        kind: &str,
        args: Value,
    ) -> (StatusCode, Value) {
        self.call(
            Method::POST,
            &ops_path(scope),
            Some(json!({ "expect": expect, "kind": kind, "args": args })),
        )
        .await
    }

    /// The same write, sent from a task: for `create`, `fork` and `list`,
    /// whose response waits for the operation's end.
    fn spawn_op(
        &self,
        scope: &str,
        expect: &Value,
        kind: &str,
        args: Value,
    ) -> JoinHandle<(StatusCode, Value)> {
        let address = self.address;
        let path = ops_path(scope);
        let body = json!({ "expect": expect, "kind": kind, "args": args });
        tokio::spawn(async move {
            let response = send(address, Method::POST, &path, Some(body)).await;
            body_of(response).await
        })
    }

    /// The same write on a connection of its own, which the test closes.
    async fn open_op(&self, scope: &str, expect: &Value, kind: &str, args: Value) -> Open {
        let stream = TcpStream::connect(self.address).await.unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        let connection = tokio::spawn(async move {
            let _ = connection.await;
        });
        let request = Request::builder()
            .method(Method::POST)
            .uri(ops_path(scope))
            .header(header::HOST, self.address.to_string())
            .header(header::ORIGIN, format!("http://{}", self.address))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "expect": expect, "kind": kind, "args": args }).to_string(),
            ))
            .unwrap();
        let request = tokio::spawn(async move {
            let _ = sender.send_request(request).await;
        });
        Open {
            request,
            connection,
        }
    }

    /// Holds the next `create`, `fork` or `list` response once it is admitted
    /// (the server's test barrier).
    fn hold(&self) -> Held {
        let (admitted, admitted_rx) = oneshot::channel();
        let (resume, resume_rx) = oneshot::channel();
        *self.hub.before_op_response.lock().unwrap() = Some(ResponseHold {
            admitted,
            resume: resume_rx,
        });
        Held {
            admitted: Some(admitted_rx),
            resume: Some(resume),
        }
    }

    /// An `answer` of `op`, in `scope`.
    async fn answer(&self, scope: &str, op: &Value, answers: Value) -> (StatusCode, Value) {
        self.op(scope, op, "answer", json!({ "answers": answers }))
            .await
    }

    /// A `cancel` of `op`, in `scope`.
    async fn cancel(&self, scope: &str, op: &Value) -> (StatusCode, Value) {
        self.op(scope, op, "cancel", json!({})).await
    }

    /// `POST /api/v2/scopes/{scope}/resources/{resourceId}/{action}`.
    async fn act(
        &self,
        scope: &str,
        resource: &str,
        action: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        self.call(
            Method::POST,
            &format!(
                "/api/v2/scopes/{}/resources/{}/{action}",
                encoded(scope),
                encoded(resource)
            ),
            Some(body),
        )
        .await
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.call(Method::GET, path, None).await
    }

    /// `GET /api/v2/scopes/{scope}`.
    async fn read(&self, scope: &str) -> (StatusCode, Value) {
        self.get(&format!("/api/v2/scopes/{}", encoded(scope)))
            .await
    }

    /// A held scope's document, read once.
    async fn document(&self, scope: &str) -> Value {
        let (status, document) = self.read(scope).await;
        assert_eq!(status, StatusCode::OK, "{scope}: {document}");
        document
    }

    /// The `op` a held scope shows now.
    async fn op_of(&self, scope: &str) -> Value {
        self.document(scope).await["live"]["op"].clone()
    }

    async fn expect_of(&self, scope: &str) -> Value {
        self.op_of(scope).await["id"].clone()
    }

    /// `GET /api/v2/stream?…`, checked event by event.
    async fn watch(&self, query: &str) -> Watch {
        // The subscription's cut is taken while the request is handled, so
        // its first alignment is at or after the documents published before
        // the request is sent.
        let start = self.timeline.lock().await.latest();
        let response = send(
            self.address,
            Method::GET,
            &format!("/api/v2/stream?{query}"),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{query}");
        Watch {
            sse: Sse {
                body: Body::new(response.into_body()).into_data_stream(),
                buffer: String::new(),
                replicas: BTreeMap::new(),
                aligned: BTreeSet::new(),
                ended: BTreeSet::new(),
            },
            timeline: self.timeline.clone(),
            start,
            cursor: BTreeMap::new(),
            events: Vec::new(),
        }
    }

    /// Creates `session_id` in `cwd` through a v2 `create`; the Agent's
    /// `session/new` response is `response` with the session ID. Returns the
    /// create's operation ID.
    async fn create_in(
        &self,
        link: &mut Link,
        session_id: &str,
        cwd: &str,
        response: Value,
    ) -> Value {
        let expect = self.expect_of("global").await;
        let created = self.spawn_op("global", &expect, "create", json!({ "cwd": cwd }));
        let new = link.request("session/new").await;
        assert_eq!(new["params"]["cwd"], cwd);
        let mut response = response;
        response["sessionId"] = json!(session_id);
        link.reply(&new, response).await;
        let (status, body) = created.await.unwrap();
        assert_eq!(status, StatusCode::OK, "create: {body}");
        assert_eq!(body["result"], json!({ "sessionId": session_id }), "{body}");
        body["op"].clone()
    }

    async fn create(&self, link: &mut Link, session_id: &str, response: Value) -> Value {
        self.create_in(link, session_id, CWD, response).await
    }
}

/// A held response: admitted, waiting for the test before it takes its
/// operation's result.
struct Held {
    admitted: Option<oneshot::Receiver<String>>,
    resume: Option<oneshot::Sender<()>>,
}

impl Held {
    /// The admitted operation's ID.
    async fn admitted(&mut self) -> Value {
        let admitted = self.admitted.take().expect("admitted once");
        let id = tokio::time::timeout(WAIT, admitted)
            .await
            .expect("the held write was not admitted")
            .expect("the held write was not admitted");
        json!(id)
    }

    /// Lets the response take its operation's result.
    fn resume(&mut self) {
        let _ = self.resume.take().expect("resumed once").send(());
    }
}

/// A write on a connection of its own.
struct Open {
    request: JoinHandle<()>,
    connection: JoinHandle<()>,
}

impl Open {
    /// Closes the connection, the request with it.
    async fn close(self) {
        self.request.abort();
        self.connection.abort();
        let _ = self.request.await;
        let _ = self.connection.await;
    }
}

async fn body_of(response: Response<hyper::body::Incoming>) -> (StatusCode, Value) {
    let status = response.status();
    let json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    let bytes = axum::body::to_bytes(Body::new(response.into_body()), usize::MAX)
        .await
        .unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else if json {
        serde_json::from_slice(&bytes).unwrap()
    } else {
        Value::String(String::from_utf8_lossy(&bytes).into_owned())
    };
    (status, body)
}

/// Percent-encodes one path segment: a session scope's `/` included.
fn encoded(segment: &str) -> String {
    segment
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn ops_path(scope: &str) -> String {
    format!("/api/v2/scopes/{}/ops", encoded(scope))
}

/// A refused write: its status and a protocol error body with `code`.
#[track_caller]
fn assert_refused(status: StatusCode, body: &Value, expected: StatusCode, code: &str, what: &str) {
    assert_eq!(status, expected, "{what}: {body}");
    assert_eq!(body["error"]["code"], code, "{what}: {body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "{what}: a protocol error has a message: {body}"
    );
}

#[track_caller]
fn assert_accepted(status: StatusCode, body: &Value, what: &str) -> Value {
    assert_eq!(status, StatusCode::ACCEPTED, "{what}: {body}");
    assert!(
        body["op"].is_string(),
        "{what}: 202 names the operation: {body}"
    );
    body["op"].clone()
}

fn stale(status: StatusCode, body: &Value, what: &str) {
    assert_refused(status, body, StatusCode::CONFLICT, "stale", what);
}

fn not_allowed(status: StatusCode, body: &Value, what: &str) {
    assert_refused(status, body, StatusCode::CONFLICT, "not_allowed", what);
}

fn invalid(status: StatusCode, body: &Value, what: &str) {
    assert_refused(
        status,
        body,
        StatusCode::BAD_REQUEST,
        "invalid_request",
        what,
    );
}

fn not_found(status: StatusCode, body: &Value, what: &str) {
    assert_refused(status, body, StatusCode::NOT_FOUND, "not_found", what);
}

// ---------------------------------------------------------------------------
// A checked stream

/// An HTTP SSE stream whose every event is checked against the published
/// timeline.
struct Watch {
    sse: Sse,
    timeline: Arc<Mutex<Timeline>>,
    /// The published documents when the stream was opened: its alignments
    /// are at or after them.
    start: usize,
    /// Per scope, the published documents it last matched.
    cursor: BTreeMap<String, usize>,
    /// Every event taken, in order.
    events: Vec<Value>,
}

impl Watch {
    async fn event(&mut self) -> Value {
        let event = self.sse.event().await;
        let scope = event["scope"].as_str().unwrap().to_string();
        let from = self.cursor.get(&scope).copied().unwrap_or(self.start);
        let mut timeline = self.timeline.lock().await;
        let at = if event.get("end").is_some() {
            timeline
                .find(from, &format!("the end of {scope}: {event}"), |documents| {
                    !documents.contains_key(&scope)
                })
                .await
        } else {
            let replica = self.sse.replica(&scope).clone();
            let unlogged = event.get("log").is_none();
            timeline
                .find(
                    from,
                    &format!("{scope} after {event}; replica {replica:#?}"),
                    |documents| {
                        documents.get(&scope).is_some_and(|document| {
                            document.live == replica.live
                                && match &document.log {
                                    Some(log) => {
                                        crate::protocol::stream_check::log_values(log)
                                            == replica.log
                                    }
                                    None => unlogged,
                                }
                        })
                    },
                )
                .await
        };
        self.cursor.insert(scope, at);
        self.events.push(event.clone());
        event
    }

    fn has(&self, scope: &str) -> bool {
        self.sse.replicas.contains_key(scope)
    }

    fn live(&self, scope: &str) -> Value {
        self.sse
            .replicas
            .get(scope)
            .map_or(Value::Null, |replica| replica.live.clone())
    }

    fn log(&self, scope: &str) -> Vec<Value> {
        self.sse
            .replicas
            .get(scope)
            .map_or_else(Vec::new, |replica| replica.log.clone())
    }

    fn ended(&self, scope: &str) -> Option<Value> {
        self.events
            .iter()
            .find(|event| event["scope"] == scope && event.get("end").is_some())
            .map(|event| event["end"].clone())
    }

    /// Takes events until `condition` holds of this client's replicas.
    async fn until(&mut self, what: &str, condition: impl Fn(&Self) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !condition(self) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}; replicas: {:#?}",
                self.sse.replicas
            );
            self.event().await;
        }
    }

    /// Takes events until `scope`'s live satisfies `condition`.
    async fn until_live(&mut self, scope: &str, what: &str, condition: impl Fn(&Value) -> bool) {
        let scope = scope.to_string();
        self.until(what, move |watch| {
            watch.has(&scope) && condition(&watch.live(&scope))
        })
        .await;
    }

    /// Takes events until `scope` shows the operation `op` with `status`.
    async fn until_op(&mut self, scope: &str, op: &Value, status: &str) {
        let (expected, wanted) = (op.clone(), status.to_string());
        self.until_live(scope, &format!("{scope}: op {op} {status}"), move |live| {
            live["op"]["id"] == expected && live["op"]["status"] == wanted
        })
        .await;
    }

    /// No event arrives for `period`; heartbeats are not events.
    async fn quiet(&mut self, period: Duration) {
        let deadline = tokio::time::Instant::now() + period;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return;
            }
            let Some(message) = self.sse.message(remaining).await else {
                return;
            };
            assert!(
                message.lines().all(|line| line.starts_with(':')),
                "unexpected event {message:?}"
            );
        }
    }

    /// The replicas equal the documents published now.
    async fn converged(&mut self, bench: &Bench) {
        for scope in self.sse.replicas.keys().cloned().collect::<Vec<_>>() {
            let document = bench.document(&scope).await;
            self.sse.assert_document(&scope, &document);
        }
    }
}

// ---------------------------------------------------------------------------
// The scripted Agent's side

impl Link {
    async fn refuse(&mut self, request: &Value) {
        self.send(json!({
            "jsonrpc": "2.0", "id": request["id"],
            "error": {"code": -32600, "message": "Refused by the Agent"},
        }))
        .await;
    }

    async fn agent_request(&mut self, id: &str, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    /// The bridge's response to the Agent request `id`.
    async fn response(&mut self, id: &str) -> Value {
        self.responses(&[id]).await.remove(id).unwrap()
    }

    /// The bridge's responses to the Agent requests `ids`, in whatever order
    /// they arrive. Responses to other requests stay queued, in order, for a
    /// later call.
    async fn responses(&mut self, ids: &[&str]) -> BTreeMap<String, Value> {
        let mut found = BTreeMap::new();
        let mut kept = Vec::new();
        while found.len() < ids.len() {
            let message = self.next(WAIT).await.unwrap_or_else(|| {
                panic!("the responses to {ids:?} did not all reach the Agent: {found:?}")
            });
            if message.get("method").is_none() {
                match message["id"].as_str().filter(|id| ids.contains(id)) {
                    Some(id) => {
                        assert!(
                            found.insert(id.to_string(), message.clone()).is_none(),
                            "two responses to {id}"
                        );
                    }
                    None => kept.push(message),
                }
                continue;
            }
            assert!(
                message.get("id").is_none(),
                "unexpected Agent request {message} while waiting for the responses to {ids:?}"
            );
        }
        for message in kept.into_iter().rev() {
            self.buffered.push_front(message);
        }
        found
    }
}

fn permission(session_id: &str, tool: &str) -> Value {
    json!({
        "sessionId": session_id,
        "toolCall": {"toolCallId": tool, "title": "Confirm", "status": "pending"},
        "options": [
            {"optionId": "allow", "name": "Allow once", "kind": "allow_once"},
            {"optionId": "deny", "name": "Deny", "kind": "reject_once"},
        ],
    })
}

/// A form elicitation in `scope` (`{sessionId}` or `{requestId}`) that asks
/// `message`.
fn form(scope: Value, message: &str) -> Value {
    let mut params = json!({
        "mode": "form", "message": message,
        "requestedSchema": {"type": "object", "properties": {}},
    });
    params
        .as_object_mut()
        .unwrap()
        .extend(scope.as_object().unwrap().clone());
    params
}

/// A URL elicitation in `scope`.
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

/// `answers` keyed by interaction ID.
fn answers(entries: &[(&str, Value)]) -> Value {
    Value::Object(
        entries
            .iter()
            .map(|(id, answer)| (id.to_string(), answer.clone()))
            .collect(),
    )
}

fn allow() -> Value {
    json!({"outcome": {"outcome": "selected", "optionId": "allow"}})
}

fn accept() -> Value {
    json!({"response": {"action": "accept", "content": {}}})
}

fn accept_url() -> Value {
    json!({"response": {"action": "accept"}})
}

fn interaction_ids(live: &Value) -> Vec<String> {
    live["op"]["interactions"]
        .as_object()
        .map(|interactions| interactions.keys().cloned().collect())
        .unwrap_or_default()
}

fn flow_ids(live: &Value) -> Vec<String> {
    live["urlFlows"]
        .as_object()
        .map(|flows| flows.keys().cloned().collect())
        .unwrap_or_default()
}

fn image(data: &str) -> Value {
    json!({"type": "image", "mimeType": "image/png", "data": data})
}

/// Session controls offered by `session/new`: two modes and one boolean
/// config option.
fn controls() -> Value {
    json!({
        "modes": {"currentModeId": "ask", "availableModes": [
            {"id": "ask", "name": "Ask"}, {"id": "plan", "name": "Plan"},
        ]},
        "configOptions": [
            {"type": "boolean", "id": "verbose", "name": "Verbose", "currentValue": false},
        ],
    })
}

fn verbose(value: bool) -> Value {
    json!([{"type": "boolean", "id": "verbose", "name": "Verbose", "currentValue": value}])
}

/// The patch paths of an event's live change.
fn patch_paths(event: &Value) -> Vec<String> {
    event["live"]["patch"]
        .as_array()
        .map(|ops| {
            ops.iter()
                .map(|op| op["path"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// A stream query for the unheld session `session_id` in `CWD`: its
/// subscription starts a `load`.
fn cold(session_id: &str) -> String {
    format!(
        "scope={}&cwd={}",
        query_scope(&format!("session/{session_id}")),
        query_scope(&format!("session/{session_id}:{CWD}"))
    )
}

/// The pending interaction whose Agent request asks about `tool`, or whose
/// elicitation says `message`.
fn interaction_for(live: &Value, about: &str) -> String {
    let interactions = live["op"]["interactions"].as_object();
    interactions
        .and_then(|interactions| {
            interactions.iter().find(|(_, interaction)| {
                interaction["request"]["toolCall"]["toolCallId"] == about
                    || interaction["request"]["message"] == about
            })
        })
        .map(|(id, _)| id.clone())
        .unwrap_or_else(|| panic!("no interaction about {about}: {live}"))
}

/// A scope query for one stream.
fn scopes(scopes: &[&str]) -> String {
    scopes
        .iter()
        .map(|scope| format!("scope={}", query_scope(scope)))
        .collect::<Vec<_>>()
        .join("&")
}
