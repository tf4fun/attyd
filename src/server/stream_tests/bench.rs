//! The bench of the operation tests: writes through `POST /api/v2/targets/{target}/ops` and the
//! actions, reads through `GET /api/v2/nodes/{path}`, and watches of `GET /api/v2/stream`
//! whose replicas are built from the events alone (protocol revision 4 §6–§9). The tests keep
//! speaking of scopes and their documents: a scope is the node at its path, and a document is
//! read back from it as it was once published.
//!
//! A write is answered `202` when it is admitted; the result of `create`, `fork` and `list` is in
//! the document, so `spawn_op` waits for it there. A watched document is only compared with a
//! read: a stream is never repaired from one.
use tokio::task::JoinHandle;

use super::*;
use crate::protocol::tree_stream::documents::Document;

mod admission;
mod close_interactions;
mod closing;
mod execution;
mod interactions;
mod login_terminals;
mod occupation;
mod reads;
mod recovery;
mod request_identity;
mod restart;
mod retired_v1;
#[cfg(unix)]
mod review_terminal_io;
mod url_flows;

const QUIET: Duration = Duration::from_millis(300);

/// The server and the HTTP client the tests write through.
type Bench = Server;

fn ws_options(agents: &Agents) -> Options {
    Options::try_parse_from(["attyd", "--transport", "ws", "--", &agents.url])
        .unwrap()
        .normalized()
        .unwrap()
}

/// The target of a scope: `global`, `catalog` or `sessions/<id>`.
fn target_of(scope: &str) -> String {
    match scope.strip_prefix("session/") {
        Some(id) => format!("sessions/{}", encoded(id)),
        None => scope.to_string(),
    }
}

/// The path of a scope's ops.
fn ops_path(scope: &str) -> String {
    format!("/api/v2/targets/{}/ops", target_of(scope))
}

/// A test client chooses its identity before sending a new operation.
fn operation_request(expect: &Value, kind: &str, args: Value) -> Value {
    let mut body = json!({ "expect": expect, "kind": kind, "args": args });
    if kind != "cancel" {
        body["id"] = json!(crate::protocol::op::operation_id());
    }
    body
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

/// The document of `scope` at `address`, read from its node.
async fn read_scope(address: SocketAddr, scope: &str) -> (StatusCode, Value) {
    use crate::protocol::tree_stream::{client::Client, documents::{document, path_of}, wire::Event};
    let path = path_of(scope);
    let url = format!("/api/v2/nodes{}", path.split('/').map(encoded).collect::<Vec<_>>().join("/"));
    let response = send(address, Method::GET, &url, None).await;
    let status = response.status();
    let body = json_of(response).await;
    if status != StatusCode::OK { return (status, body); }
    let event: Event = serde_json::from_value(json!({
        "instance": body["instance"], "rev": body["rev"], "changes": [body["change"]],
    })).unwrap();
    let mut client = Client::following(&[&path]);
    client.apply(&event).unwrap();
    match client.value(&path).and_then(|value| document(scope, value)) {
        Some(doc) => (status, doc),
        None => (
            StatusCode::NOT_FOUND,
            json!({ "error": { "code": "not_found", "message": format!("{scope} is not there") } }),
        ),
    }
}

/// The query of a stream in revision 4's terms for one in the scopes the tests speak: `scope=<s>`
/// follows the node at its path, `cwd=<scope>:<dir>` gives the directory to load that session with.
fn follow_query(query: &str) -> (String, Vec<String>) {
    use crate::protocol::tree_stream::documents::path_of;
    let mut paths = Vec::new();
    let mut out = Vec::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "scope" => {
                let path = path_of(&value);
                out.push(format!("follow={}", query_scope(&path)));
                paths.push(path);
            }
            "cwd" => {
                let (scope, dir) = value.split_once(':').expect("cwd is <scope>:<dir>");
                let id = scope.strip_prefix("session/").expect("a session scope");
                out.push(format!("cwd={}", query_scope(&format!("{}:{dir}", query_scope(id)))));
            }
            _ => out.push(format!("{}={}", query_scope(&key), query_scope(&value))),
        }
    }
    (out.join("&"), paths)
}

/// `GET /api/v2/stream?…` at `address`, applied to replicas.
async fn watch_at(address: SocketAddr, query: &str) -> Watch {
    let (query, paths) = follow_query(query);
    let response = send(address, Method::GET, &format!("/api/v2/stream?{query}"), None).await;
    assert_eq!(response.status(), StatusCode::OK, "{query}");
    let followed: Vec<&str> = paths.iter().map(String::as_str).collect();
    Watch {
        sse: Sse {
            body: Body::new(response.into_body()).into_data_stream(),
            buffer: String::new(),
        },
        client: crate::protocol::tree_stream::client::Client::following(&followed),
        replicas: BTreeMap::new(),
        ended: BTreeMap::new(),
        events: Vec::new(),
    }
}

impl Bench {
    /// A server whose scripted Agent answers `initialize` with `capabilities`.
    async fn ws(capabilities: Value) -> (Self, Link) {
        Self::ws_initialized(json!({
            "protocolVersion": 1, "agentCapabilities": capabilities, "authMethods": [],
        }))
        .await
    }

    /// A server whose scripted Agent answers `initialize` with `result`, once the bridge
    /// published `global.agent.phase == ready`.
    async fn ws_initialized(result: Value) -> (Self, Link) {
        let agents = Agents::listen().await;
        let options = ws_options(&agents);
        let mut bench = Self::serve(options, agents).await;
        let mut link = bench.agents.connect().await;
        let initialize = link.request("initialize").await;
        link.reply(&initialize, result).await;
        bench.until_ready().await;
        (bench, link)
    }

    /// Waits until the bridge published `global.agent.phase == ready`.
    async fn until_ready(&self) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while self.document("global").await["live"]["agent"]["phase"] != "ready" {
            assert!(tokio::time::Instant::now() < deadline, "the Agent never got ready");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    // -- HTTP -----------------------------------------------------------------

    /// A request with an optional JSON body: its status and its JSON body (`null` when empty, a
    /// string when it is not JSON).
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        body_of(send(self.address, method, path, body).await).await
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

    /// `POST /api/v2/targets/{target}/ops`.
    async fn op(&self, scope: &str, expect: &Value, kind: &str, args: Value) -> (StatusCode, Value) {
        self.call(
            Method::POST,
            &ops_path(scope),
            Some(operation_request(expect, kind, args)),
        )
        .await
    }

    /// An operation write whose ID the client chose (protocol §6.5).
    async fn op_with_id(
        &self,
        scope: &str,
        expect: &Value,
        id: &str,
        kind: &str,
        args: Value,
    ) -> (StatusCode, Value) {
        self.call(
            Method::POST,
            &ops_path(scope),
            Some(json!({ "id": id, "expect": expect, "kind": kind, "args": args })),
        )
        .await
    }

    /// The same write, sent from a task, which then waits for the operation's end in the
    /// document: for `create`, `fork` and `list`, whose result is there. It answers `200` and
    /// `{ op, result }` or `{ op, error }`; a refused write answers its refusal.
    /// A session action (protocol §7: `set_mode`, `set_config`, `close`), started now and answered
    /// when the Agent answers: `200 {}`, `200 { error }` or its refusal.
    fn spawn_action(
        &self,
        session_id: &str,
        action: &str,
        body: Value,
    ) -> JoinHandle<(StatusCode, Value)> {
        let address = self.address;
        let path = format!("/api/v2/targets/sessions/{}/actions/{action}", encoded(session_id));
        tokio::spawn(async move { body_of(send(address, Method::POST, &path, Some(body)).await).await })
    }

    fn spawn_op(
        &self,
        scope: &str,
        expect: &Value,
        kind: &str,
        args: Value,
    ) -> JoinHandle<(StatusCode, Value)> {
        let address = self.address;
        let body = operation_request(expect, kind, args);
        let path = ops_path(scope);
        let scope = scope.to_string();
        tokio::spawn(async move {
            let (status, body) = body_of(send(address, Method::POST, &path, Some(body)).await).await;
            if status != StatusCode::ACCEPTED {
                return (status, body);
            }
            let op = body["op"].clone();
            let deadline = tokio::time::Instant::now() + WAIT;
            loop {
                let (status, read) = read_scope(address, &scope).await;
                assert_eq!(status, StatusCode::OK, "{scope}: {read}");
                let live = read["live"].clone();
                if live["op"]["id"] == op && live["op"]["status"] != "running" {
                    let outcome = match live["op"].get("error") {
                        Some(error) => json!({"op": op, "error": error}),
                        None => json!({"op": op, "result": live["op"]["result"]}),
                    };
                    return (StatusCode::OK, outcome);
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "{scope}: the operation {op} did not end: {live}"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    }

    /// The answers to interactions of `scope` (protocol §7), one at a time at their resources, in
    /// the order of their IDs. The first refusal is the result; otherwise `202` naming `op`, which
    /// the answers leave as it is.
    async fn answer(&self, scope: &str, op: &Value, answers: Value) -> (StatusCode, Value) {
        for (interaction, answer) in answers.as_object().into_iter().flatten() {
            let (status, body) = self.act(scope, interaction, "answer", answer.clone()).await;
            if status != StatusCode::NO_CONTENT {
                return (status, body);
            }
        }
        (StatusCode::ACCEPTED, json!({ "op": op }))
    }

    /// A `cancel` of `op`, in `scope`.
    async fn cancel(&self, scope: &str, op: &Value) -> (StatusCode, Value) {
        self.op(scope, op, "cancel", json!({})).await
    }

    /// `POST /api/v2/targets/{target}/resources/{resourceId}/{action}`.
    async fn act(&self, scope: &str, resource: &str, action: &str, body: Value) -> (StatusCode, Value) {
        self.call(
            Method::POST,
            &format!(
                "/api/v2/targets/{}/resources/{}/{action}",
                target_of(scope),
                encoded(resource)
            ),
            Some(body),
        )
        .await
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.call(Method::GET, path, None).await
    }

    /// The document `{ live, prompts, entries?, … }` the replica
    /// of a client would hold, or the refusal.
    async fn read_document(&self, scope: &str) -> (StatusCode, Value) {
        read_scope(self.address, scope).await
    }

    /// A held scope's document, read once.
    async fn document(&self, scope: &str) -> Value {
        let (status, document) = self.read_document(scope).await;
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

    /// `GET /api/v2/stream?…`, applied to replicas.
    async fn watch(&self, query: &str) -> Watch {
        watch_at(self.address, query).await
    }

    /// Creates `session_id` in `cwd` through a `create`; the Agent's `session/new` response is
    /// `response` with the session ID. Returns the create's operation ID.
    async fn create_in(&self, link: &mut Link, session_id: &str, cwd: &str, response: Value) -> Value {
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

    async fn create_with(&self, link: &mut Link, session_id: &str, response: Value) -> Value {
        self.create_in(link, session_id, CWD, response).await
    }

    fn accepted(&self) -> usize {
        self.agents.accepted()
    }

    /// A server whose Agent is the stdio fake Agent fixture, run through `sh -c script agent`;
    /// the script decides what a login runs.
    async fn stdio(script: &str, cwd: &str) -> Self {
        let options =
            Options::try_parse_from(["attyd", "--cwd", cwd, "--", "sh", "-c", script, "agent"])
                .unwrap()
                .normalized()
                .unwrap();
        Self::serve(options, Agents::listen().await).await
    }
}

fn ops_status(status: StatusCode, body: &Value, expected: StatusCode, code: &str, what: &str) {
    assert_eq!(status, expected, "{what}: {body}");
    assert_eq!(body["error"]["code"], code, "{what}: {body}");
    assert!(
        body["error"]["message"].as_str().is_some_and(|message| !message.is_empty()),
        "{what}: a protocol error has a message: {body}"
    );
}

/// A refused write: its status and a protocol error body with `code`.
#[track_caller]
fn assert_refused(status: StatusCode, body: &Value, expected: StatusCode, code: &str, what: &str) {
    ops_status(status, body, expected, code, what);
}

#[track_caller]
fn assert_accepted(status: StatusCode, body: &Value, what: &str) -> Value {
    assert_eq!(status, StatusCode::ACCEPTED, "{what}: {body}");
    assert!(body["op"].is_string(), "{what}: 202 names the operation: {body}");
    body["op"].clone()
}

#[track_caller]
fn stale(status: StatusCode, body: &Value, what: &str) {
    assert_refused(status, body, StatusCode::CONFLICT, "stale", what);
}

#[track_caller]
fn not_allowed(status: StatusCode, body: &Value, what: &str) {
    assert_refused(status, body, StatusCode::CONFLICT, "not_allowed", what);
}

#[track_caller]
fn invalid(status: StatusCode, body: &Value, what: &str) {
    assert_refused(status, body, StatusCode::BAD_REQUEST, "invalid_request", what);
}

#[track_caller]
fn not_found(status: StatusCode, body: &Value, what: &str) {
    assert_refused(status, body, StatusCode::NOT_FOUND, "not_found", what);
}

// ---------------------------------------------------------------------------
// A watch

/// An HTTP SSE stream whose events are applied to replicas: the paths of the tree it follows,
/// and the documents of the scopes they are the roots of.
struct Watch {
    sse: Sse,
    client: crate::protocol::tree_stream::client::Client,
    replicas: BTreeMap<String, Document>,
    /// How each scope that ended, with a reason, ended.
    ended: BTreeMap<String, Value>,
    /// Every event taken, in order.
    events: Vec<Value>,
}

impl Watch {
    /// The event an SSE message carries, if it carries one (a heartbeat does not).
    fn parse(message: &str) -> Option<Value> {
        assert!(
            message.lines().all(|line| !line.starts_with("id:")),
            "events carry no SSE id: {message:?}"
        );
        let data: Vec<&str> = message
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect();
        (!data.is_empty()).then(|| serde_json::from_str(&data.join("\n")).unwrap())
    }

    /// Applies `event` to the replicas.
    fn take(&mut self, event: &Value) {
        use crate::protocol::tree_stream::documents::{document, scope_of};
        let wire: crate::protocol::tree_stream::wire::Event = serde_json::from_value(event.clone())
            .unwrap_or_else(|error| panic!("not an event of the stream: {error}: {event}"));
        self.client
            .apply(&wire)
            .unwrap_or_else(|error| panic!("invalid HTTP SSE event: {error}: {event}"));
        for change in event["changes"].as_array().into_iter().flatten() {
            let Some(scope) = change["path"].as_str().and_then(scope_of) else { continue };
            let path = change["path"].as_str().unwrap();
            match self.client.value(path).and_then(|value| document(&scope, value)) {
                Some(doc) => {
                    self.ended.remove(&scope);
                    self.replicas.insert(
                        scope,
                        Document { rev: self.client.version(path), doc },
                    );
                }
                None => {
                    self.replicas.remove(&scope);
                    // A path not there yet is not an end.
                    if let Some(end) = change.get("end").filter(|end| end["reason"] != "absent") {
                        let mut ended = json!({ "reason": end["reason"] });
                        if let Some(error) = end.get("error") {
                            ended["error"] = json!({ "code": error["code"] });
                            if let Some(agent) = error.get("agentCode") {
                                ended["error"]["data"] = json!({ "code": agent });
                            }
                        }
                        self.ended.insert(scope, ended);
                    }
                }
            }
        }
        self.events.push(event.clone());
    }

    /// The next event, skipping comments (the heartbeat) and events that only say a path is not
    /// there yet, applied.
    async fn event(&mut self) -> Value {
        loop {
            let message = self.sse.message(WAIT).await.expect("the stream sent no event");
            if let Some(event) = Self::parse(&message) {
                self.take(&event);
                let nothing_yet = event["changes"].as_array().is_some_and(|changes| {
                    changes.iter().all(|change| change["end"]["reason"] == "absent")
                });
                if !nothing_yet {
                    return event;
                }
            }
        }
    }

    fn has(&self, scope: &str) -> bool {
        self.replicas.contains_key(scope)
    }

    fn live(&self, scope: &str) -> Value {
        self.replicas.get(scope).map_or(Value::Null, Document::live)
    }

    fn entries(&self, scope: &str) -> Vec<Value> {
        self.replicas.get(scope).map_or_else(Vec::new, Document::entries)
    }

    fn ended(&self, scope: &str) -> Option<Value> {
        self.ended.get(scope).cloned()
    }

    /// Takes events until `condition` holds of this client's replicas.
    async fn until(&mut self, what: &str, condition: impl Fn(&Self) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !condition(self) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}; replicas: {:#?}",
                self.replicas
            );
            self.event().await;
        }
    }

    /// Takes events until `scope`'s live satisfies `condition`.
    async fn until_live(&mut self, scope: &str, what: &str, condition: impl Fn(&Value) -> bool) {
        let scope = scope.to_string();
        self.until(what, move |watch| watch.has(&scope) && condition(&watch.live(&scope)))
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

    /// Nothing more is published for `period`: what the bridge published already is taken
    /// first, as a client reads it all; then only heartbeats and checkpoints (which change
    /// nothing, protocol §5.3) may arrive.
    async fn quiet(&mut self, period: Duration) {
        while let Some(message) = self.sse.message(Duration::from_millis(100)).await {
            if let Some(event) = Self::parse(&message) {
                self.take(&event);
            }
        }
        let deadline = tokio::time::Instant::now() + period;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return;
            }
            let Some(message) = self.sse.message(remaining).await else {
                return;
            };
            if let Some(event) = Self::parse(&message) {
                assert!(
                    event.get("check").is_some() && event.get("changes").is_none(),
                    "unexpected event {message:?}"
                );
                self.take(&event);
            }
        }
    }

    /// The replicas equal the documents published now.
    async fn converged(&mut self, bench: &Bench) {
        for scope in self.replicas.keys().cloned().collect::<Vec<_>>() {
            let document = bench.document(&scope).await;
            assert_eq!(self.replicas[&scope].doc, document, "{scope}: HTTP SSE");
        }
    }
}

// ---------------------------------------------------------------------------
// Fixtures of the bench

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
    live["interactions"]
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

/// A form elicitation in `scope` (`{sessionId}` or `{requestId}`) that asks `message`.
fn form(scope: Value, message: &str) -> Value {
    let mut params = json!({
        "mode": "form", "message": message,
        "requestedSchema": {"type": "object", "properties": {}},
    });
    params.as_object_mut().unwrap().extend(scope.as_object().unwrap().clone());
    params
}

/// A URL elicitation in `scope`.
fn url_elicitation(scope: Value, elicitation_id: &str) -> Value {
    let mut params = json!({
        "mode": "url", "message": "Sign in", "elicitationId": elicitation_id,
        "url": "https://example.test/sign-in",
    });
    params.as_object_mut().unwrap().extend(scope.as_object().unwrap().clone());
    params
}

/// The paths of the ops of an event, relative to the path each belongs to.
fn patch_paths(event: &Value) -> Vec<String> {
    event["changes"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|change| change["ops"].as_array().into_iter().flatten())
        .map(|op| op["path"].as_str().unwrap().to_string())
        .collect()
}

/// A stream query for the unheld session `session_id` in `CWD`: its subscription starts a
/// `load`.
fn cold(session_id: &str) -> String {
    format!(
        "scope={}&cwd={}",
        query_scope(&format!("session/{session_id}")),
        query_scope(&format!("session/{session_id}:{CWD}"))
    )
}

/// The pending interaction whose Agent request asks about `tool`, or whose elicitation says
/// `message`.
fn interaction_for(live: &Value, about: &str) -> String {
    let interactions = live["interactions"].as_object();
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
