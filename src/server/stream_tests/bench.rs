//! The bench of the operation tests: writes through `POST /api/v2/scopes/{scope}/ops` and the
//! actions, reads through `GET /api/v2/scopes/{scope}`, and watches of `GET /api/v2/stream`
//! whose replicas are built from the events alone (protocol §5, §6, §8).
//!
//! A write is answered `202` when it is admitted; the result of `create`, `fork` and `list` is in
//! the document, so `spawn_op` waits for it there. A watched document is only compared with a
//! read: a stream is never repaired from one.
use tokio::task::JoinHandle;

use super::*;
use crate::protocol::replica::Replica;

mod admission;
mod execution;
mod interactions;
mod login_terminals;
mod occupation;
mod reads;
mod recovery;
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

/// The path of a scope's ops.
fn ops_path(scope: &str) -> String {
    format!("/api/v2/scopes/{}/ops", encoded(scope))
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

/// `GET /api/v2/scopes/{scope}` at `address`.
async fn read_scope(address: SocketAddr, scope: &str) -> (StatusCode, Value) {
    let path = format!("/api/v2/scopes/{}", encoded(scope));
    let (status, body) = body_of(send(address, Method::GET, &path, None).await).await;
    if status == StatusCode::OK {
        (status, Replica::from_snapshot(&body).doc)
    } else {
        (status, body)
    }
}

/// `GET /api/v2/stream?…` at `address`, applied to replicas.
async fn watch_at(address: SocketAddr, query: &str) -> Watch {
    let response = send(address, Method::GET, &format!("/api/v2/stream?{query}"), None).await;
    assert_eq!(response.status(), StatusCode::OK, "{query}");
    Watch {
        sse: Sse {
            body: Body::new(response.into_body()).into_data_stream(),
            buffer: String::new(),
        },
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

    /// `POST /api/v2/scopes/{scope}/ops`.
    async fn op(&self, scope: &str, expect: &Value, kind: &str, args: Value) -> (StatusCode, Value) {
        self.call(
            Method::POST,
            &ops_path(scope),
            Some(json!({ "expect": expect, "kind": kind, "args": args })),
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
    fn spawn_op(
        &self,
        scope: &str,
        expect: &Value,
        kind: &str,
        args: Value,
    ) -> JoinHandle<(StatusCode, Value)> {
        let address = self.address;
        let body = json!({ "expect": expect, "kind": kind, "args": args });
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

    /// `POST /api/v2/scopes/{scope}/resources/{resourceId}/{action}`.
    async fn act(&self, scope: &str, resource: &str, action: &str, body: Value) -> (StatusCode, Value) {
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

    /// `GET /api/v2/scopes/{scope}`: the document `{ live, prompts, entries?, … }` the replica
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

/// An HTTP SSE stream whose events are applied to replicas.
struct Watch {
    sse: Sse,
    replicas: BTreeMap<String, Replica>,
    /// How each scope ended, if it did.
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
        let scope = event["scope"].as_str().expect("an event names its scope").to_string();
        assert!(!self.ended.contains_key(&scope), "an event after end: {event}");
        if let Some(end) = event.get("end") {
            self.replicas.remove(&scope);
            self.ended.insert(scope, end.clone());
        } else {
            self.replicas
                .entry(scope)
                .or_default()
                .apply(event)
                .unwrap_or_else(|error| panic!("invalid HTTP SSE event: {error}"));
        }
        self.events.push(event.clone());
    }

    /// The next event, skipping comments (the heartbeat), applied.
    async fn event(&mut self) -> Value {
        loop {
            let message = self.sse.message(WAIT).await.expect("the stream sent no event");
            if let Some(event) = Self::parse(&message) {
                self.take(&event);
                return event;
            }
        }
    }

    fn has(&self, scope: &str) -> bool {
        self.replicas.contains_key(scope)
    }

    fn live(&self, scope: &str) -> Value {
        self.replicas.get(scope).map_or(Value::Null, Replica::live)
    }

    fn entries(&self, scope: &str) -> Vec<Value> {
        self.replicas.get(scope).map_or_else(Vec::new, Replica::entries)
    }

    fn ended(&self, scope: &str) -> Option<Value> {
        self.ended.get(scope).cloned()
    }

    fn events_of(&self, scope: &str) -> Vec<Value> {
        self.events.iter().filter(|event| event["scope"] == scope).cloned().collect()
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
                    event.get("digest").is_some() && event.get("ops").is_none(),
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

/// The paths of the ops of an event.
fn patch_paths(event: &Value) -> Vec<String> {
    event["ops"]
        .as_array()
        .map(|ops| ops.iter().map(|op| op["path"].as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
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
