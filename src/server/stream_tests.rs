//! `GET /api/v2/stream` and `GET /api/v2/scopes/{scope}` over real HTTP
//! (migration step 5; protocol §5, §8).
//!
//! The server listens on a TCP port and its bridge connects to a scripted
//! Agent over the WebSocket transport, so each Agent connection is a real ACP
//! connection the test answers. Requests go through a hyper client.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::Method;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use crate::protocol::canon::TurnLog;
use crate::protocol::stream_check::{Replica, log_values};

use super::*;

mod ops;

const WAIT: Duration = Duration::from_secs(10);
const CWD: &str = "/workspace";

fn text(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

/// The replay of turns `(prompt, reply)`.
fn turns(turns: &[(&str, &str)]) -> Vec<Value> {
    turns
        .iter()
        .enumerate()
        .flat_map(|(index, (prompt, reply))| {
            [
                json!({"sessionUpdate": "user_message_chunk", "content": text(prompt)}),
                json!({
                    "sessionUpdate": "agent_message_chunk", "messageId": format!("reply-{index}"),
                    "content": text(reply),
                }),
            ]
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The scripted Agent

/// Accepts the bridge's Agent connections, one per generation.
struct Agents {
    url: String,
    links: mpsc::UnboundedReceiver<Link>,
    accepted: Arc<AtomicUsize>,
}

impl Agents {
    async fn listen() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/acp", listener.local_addr().unwrap());
        let (sender, links) = mpsc::unbounded_channel();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = accepted.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let Ok(socket) = tokio_tungstenite::accept_async(stream).await else {
                    continue;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                if sender
                    .send(Link {
                        socket,
                        buffered: VecDeque::new(),
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        Self {
            url,
            links,
            accepted,
        }
    }

    /// The next connection, before its `initialize` is answered.
    async fn connect(&mut self) -> Link {
        tokio::time::timeout(WAIT, self.links.recv())
            .await
            .expect("the bridge did not connect to the Agent")
            .unwrap()
    }

    /// The next connection, initialized with `capabilities`.
    async fn accept(&mut self, capabilities: Value) -> Link {
        let mut link = self.connect().await;
        let initialize = link.request("initialize").await;
        link.reply(
            &initialize,
            json!({"protocolVersion": 1, "agentCapabilities": capabilities, "authMethods": []}),
        )
        .await;
        link
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

/// One Agent connection.
struct Link {
    socket: WebSocketStream<TcpStream>,
    buffered: VecDeque<Value>,
}

impl Link {
    async fn next(&mut self, within: Duration) -> Option<Value> {
        tokio::time::timeout(within, async {
            loop {
                if let Some(message) = self.buffered.pop_front() {
                    return Some(message);
                }
                match self.socket.next().await? {
                    Ok(Message::Text(raw)) => match serde_json::from_str(raw.as_str()).unwrap() {
                        Value::Array(batch) => self.buffered.extend(batch),
                        message => self.buffered.push_back(message),
                    },
                    Ok(Message::Close(_)) | Err(_) => return None,
                    Ok(_) => {}
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    /// The next request with `method`, skipping notifications.
    async fn request(&mut self, method: &str) -> Value {
        loop {
            let message = self
                .next(WAIT)
                .await
                .unwrap_or_else(|| panic!("no {method} reached the Agent"));
            if message["method"] == method {
                return message;
            }
            assert!(
                message.get("id").is_none() || message.get("method").is_none(),
                "unexpected Agent request {message} while waiting for {method}"
            );
        }
    }

    /// Nothing reaches the Agent for `period`.
    async fn quiet(&mut self, period: Duration) {
        if let Some(message) = self.next(period).await {
            panic!("unexpected Agent message {message}");
        }
    }

    async fn send(&mut self, message: Value) {
        self.socket
            .send(Message::Text(message.to_string().into()))
            .await
            .unwrap();
    }

    async fn reply(&mut self, request: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": request["id"], "result": result}))
            .await;
    }

    async fn update(&mut self, session_id: &str, update: Value) {
        self.send(json!({
            "jsonrpc": "2.0", "method": "session/update",
            "params": {"sessionId": session_id, "update": update},
        }))
        .await;
    }

    async fn serve_load(&mut self, session_id: &str, replay: &[Value]) -> Value {
        let load = self.request("session/load").await;
        assert_eq!(load["params"]["sessionId"], session_id);
        for update in replay {
            self.update(session_id, update.clone()).await;
        }
        self.reply(&load, json!({})).await;
        load
    }

    /// The Agent ends the connection: the bridge reads an EOF.
    async fn close(mut self) {
        let _ = self.socket.close(None).await;
    }
}

// ---------------------------------------------------------------------------
// The server and its HTTP client

struct Server {
    address: SocketAddr,
    hub: Arc<BridgeHub>,
    agents: Agents,
}

impl Server {
    async fn start(capabilities: Value) -> (Self, Link) {
        let mut server = Self::launch().await;
        let link = server.agents.accept(capabilities).await;
        (server, link)
    }

    /// A server whose first Agent connection is not accepted yet.
    async fn launch() -> Self {
        let agents = Agents::listen().await;
        let options = Options::try_parse_from(["attyd", "--transport", "ws", "--", &agents.url])
            .unwrap()
            .normalized()
            .unwrap();
        let options = Arc::new(options);
        let hub = BridgeHub::new(options.clone());
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
            agents,
        }
    }

    async fn send(&self, path: &str) -> Response<hyper::body::Incoming> {
        send(self.address, Method::GET, path, None).await
    }

    /// `GET /api/v2/scopes/{scope}`: the status and the JSON body.
    async fn read(&self, scope: &str) -> (StatusCode, Value) {
        let path = format!("/api/v2/scopes/{}", scope.replace('/', "%2F"));
        let response = self.send(&path).await;
        let status = response.status();
        let bytes = axum::body::to_bytes(Body::new(response.into_body()), usize::MAX)
            .await
            .unwrap();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, body)
    }

    /// `GET /api/v2/stream?…`.
    async fn stream(&self, query: &str) -> Sse {
        let response = self.send(&format!("/api/v2/stream?{query}")).await;
        assert_eq!(response.status(), StatusCode::OK, "{query}");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream",
            "{query}"
        );
        Sse {
            body: Body::new(response.into_body()).into_data_stream(),
            buffer: String::new(),
            replicas: BTreeMap::new(),
            aligned: BTreeSet::new(),
            ended: BTreeSet::new(),
        }
    }

    async fn create(&self, link: &mut Link, session_id: &str) {
        let (_, global) = self.read("global").await;
        let body = json!({
            "expect": global["live"]["op"]["id"], "kind": "create", "args": {"cwd": CWD},
        });
        let address = self.address;
        let created = tokio::spawn(async move {
            send(
                address,
                Method::POST,
                "/api/v2/scopes/global/ops",
                Some(body),
            )
            .await
        });
        let new = link.request("session/new").await;
        link.reply(&new, json!({"sessionId": session_id})).await;
        let created = created.await.unwrap();
        assert_eq!(created.status(), StatusCode::OK);
        let created = json_of(created).await;
        assert_eq!(
            created["result"],
            json!({"sessionId": session_id}),
            "{created}"
        );
    }
}

async fn send(
    address: SocketAddr,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Response<hyper::body::Incoming> {
    let stream = TcpStream::connect(address).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, address.to_string());
    let request = match body {
        Some(body) => request
            .header(header::ORIGIN, format!("http://{address}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .unwrap();
    tokio::time::timeout(WAIT, sender.send_request(request))
        .await
        .expect("the server did not answer")
        .unwrap()
}

/// An SSE response body, read message by message.
struct Sse {
    body: axum::body::BodyDataStream,
    buffer: String,
    replicas: BTreeMap<String, Replica>,
    aligned: BTreeSet<String>,
    ended: BTreeSet<String>,
}

impl Sse {
    /// A client's pre-existing cache, installed before its first event. GET
    /// responses are only compared with; they never repair these replicas.
    fn cache_log(&mut self, scope: &str, log: Vec<Value>) {
        assert!(!self.aligned.contains(scope));
        self.replicas.entry(scope.to_string()).or_default().log = log;
    }

    fn replica(&self, scope: &str) -> &Replica {
        self.replicas.get(scope).expect("the scope was delivered")
    }

    fn assert_document(&self, scope: &str, document: &Value) {
        let replica = self.replica(scope);
        assert_eq!(
            replica.live, document["live"],
            "{scope}: live from HTTP SSE"
        );
        if let Some(log) = document.get("log") {
            assert_eq!(&json!(replica.log), log, "{scope}: log from HTTP SSE");
        }
    }

    /// The next SSE message, as its raw lines, within `period`.
    async fn message(&mut self, period: Duration) -> Option<String> {
        tokio::time::timeout(period, async {
            loop {
                if let Some(end) = self.buffer.find("\n\n") {
                    let message = self.buffer[..end].to_string();
                    self.buffer.drain(..end + 2);
                    return Some(message);
                }
                let chunk = self.body.next().await?.unwrap();
                self.buffer.push_str(std::str::from_utf8(&chunk).unwrap());
            }
        })
        .await
        .ok()
        .flatten()
    }

    /// The next event, skipping comments. Events carry no SSE `id`.
    async fn event(&mut self) -> Value {
        loop {
            let message = self.message(WAIT).await.expect("the stream sent no event");
            assert!(
                message.lines().all(|line| !line.starts_with("id:")),
                "events carry no SSE id: {message:?}"
            );
            let data: Vec<&str> = message
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect();
            if data.is_empty() {
                continue;
            }
            let event: Value = serde_json::from_str(&data.join("\n")).unwrap();
            let scope = event["scope"].as_str().expect("an event names its scope");
            assert!(!self.ended.contains(scope), "an event after end: {event}");
            if event.get("end").is_some() {
                self.replicas.remove(scope);
                self.ended.insert(scope.to_string());
            } else {
                if self.aligned.insert(scope.to_string()) {
                    assert!(
                        event["live"].get("replace").is_some(),
                        "the first event is an alignment: {event}"
                    );
                }
                self.replicas
                    .entry(scope.to_string())
                    .or_default()
                    .apply(&event)
                    .unwrap_or_else(|error| panic!("invalid HTTP SSE event: {error}"));
            }
            return event;
        }
    }

    /// Events until one of `scope` satisfies `condition`.
    async fn until(&mut self, scope: &str, condition: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut events = Vec::new();
        loop {
            let event = self.event().await;
            let done = event["scope"] == scope && condition(&event);
            events.push(event);
            if done {
                return events;
            }
        }
    }
}

/// Whether an event's `live` change mentions `value`, such as an operation's
/// new status.
fn mentions(event: &Value, value: &str) -> bool {
    event["live"].to_string().contains(&format!("\"{value}\""))
}

fn query_scope(scope: &str) -> String {
    url::form_urlencoded::byte_serialize(scope.as_bytes()).collect()
}

fn assert_error_body(body: &Value, code: &str) {
    assert_eq!(body["error"]["code"], code, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "a structured protocol error has a message: {body}"
    );
}

/// A server whose Agent holds `saved`, loaded with `replay` through a stream
/// that stays open.
async fn loaded(replay: &[Value]) -> (Server, Link, Sse) {
    let (server, mut link) = Server::start(json!({"loadSession": true})).await;
    let mut stream = server
        .stream(&format!(
            "scope=global&scope=session%2Fsaved&cwd={}",
            query_scope(&format!("session/saved:{CWD}"))
        ))
        .await;
    link.serve_load("saved", replay).await;
    stream
        .until("session/saved", |event| mentions(event, "completed"))
        .await;
    (server, link, stream)
}

// ---------------------------------------------------------------------------
// Reads (protocol §8)

#[tokio::test]
async fn reads_return_one_cut_hold_nothing_and_tell_an_unknown_log_from_an_empty_one() {
    let (server, mut link) = Server::start(json!({"loadSession": true})).await;
    server.create(&mut link, "created").await;

    let (status, global) = server.read("global").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(global["log"], json!([]));
    assert_eq!(global["live"]["agent"]["phase"], "ready");
    let (status, catalog) = server.read("catalog").await;
    assert_eq!(status, StatusCode::OK);
    assert!(catalog["live"].is_object());
    let (status, created) = server.read("session/created").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["log"], json!([]), "a known empty log is []");

    let (status, body) = server.read("session/unheld").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "an unheld session");
    assert_error_body(&body, "not_found");
    link.quiet(Duration::from_millis(300)).await;
    let (status, body) = server.read("session/unheld").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a read holds and loads nothing"
    );
    assert_error_body(&body, "not_found");

    let mut stream = server
        .stream(&format!(
            "scope=session%2Fsaved&cwd={}",
            query_scope(&format!("session/saved:{CWD}"))
        ))
        .await;
    let load = link.request("session/load").await;
    let aligned = stream.event().await;
    assert!(aligned.get("log").is_none(), "{aligned}");
    let (status, loading) = server.read("session/saved").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !loading.as_object().unwrap().contains_key("log"),
        "an unknown log is omitted, not null: {loading}"
    );
    assert_eq!(loading["live"], aligned["live"]["replace"]);
    for update in turns(&[("one", "1")]) {
        link.update("saved", update).await;
    }
    link.reply(&load, json!({})).await;
    let end = stream
        .until("session/saved", |event| mentions(event, "completed"))
        .await
        .pop()
        .unwrap();
    assert_eq!(end["log"]["entries"].as_array().unwrap().len(), 1, "{end}");
    let (_, loaded) = server.read("session/saved").await;
    assert_eq!(loaded["log"], end["log"]["entries"]);
    stream.assert_document("session/saved", &loaded);
}

// ---------------------------------------------------------------------------
// The stream (protocol §5)

#[tokio::test]
async fn a_stream_aligns_several_scopes_without_ids_and_heartbeats_while_idle() {
    let (server, mut link) = Server::start(json!({})).await;
    server.create(&mut link, "created").await;
    let mut stream = server
        .stream("scope=global&scope=catalog&scope=session%2Fcreated")
        .await;
    let mut scopes = Vec::new();
    for _ in 0..3 {
        let event = stream.event().await;
        assert!(event["live"].get("replace").is_some(), "{event}");
        assert_eq!(event["log"]["from"], 0);
        scopes.push(event["scope"].as_str().unwrap().to_string());
    }
    scopes.sort();
    assert_eq!(scopes, ["catalog", "global", "session/created"]);

    let comment = stream
        .message(crate::protocol::hub::HEARTBEAT + Duration::from_secs(5))
        .await
        .expect("an idle stream gets a heartbeat");
    assert!(
        comment.lines().all(|line| line.starts_with(':')),
        "a heartbeat is an SSE comment: {comment:?}"
    );
}

#[tokio::test]
async fn have_selects_the_suffix_and_a_mismatched_or_longer_head_gets_the_whole_log() {
    let (server, _link, _stream) = loaded(&turns(&[("one", "1"), ("two", "2")])).await;
    let (_, document) = server.read("session/saved").await;
    let log = document["log"].as_array().unwrap().clone();
    assert_eq!(log.len(), 2);
    let h1 = log[0]["hash"].as_str().unwrap();
    let h2 = log[1]["hash"].as_str().unwrap();
    let longer = log_values(
        TurnLog::from_replay(&turns(&[("one", "1"), ("two", "2"), ("three", "3")])).entries(),
    );
    let mut wrong = log[..1].to_vec();
    wrong[0]["hash"] = json!("wrong");
    for (have, from, appended, cached) in [
        (format!("1:{h1}"), 1, 1, log[..1].to_vec()),
        (format!("2:{h2}"), 2, 0, log.clone()),
        ("1:wrong".to_string(), 0, 2, wrong),
        (
            format!("3:{}", longer[2]["hash"].as_str().unwrap()),
            0,
            2,
            longer,
        ),
    ] {
        let mut stream = server
            .stream(&format!(
                "scope=session%2Fsaved&have={}",
                query_scope(&format!("session/saved:{have}"))
            ))
            .await;
        stream.cache_log("session/saved", cached);
        let aligned = stream.event().await;
        assert_eq!(aligned["log"]["from"], from, "have {have}: {aligned}");
        assert_eq!(
            aligned["log"]["entries"].as_array().unwrap().len(),
            appended,
            "have {have}"
        );
        assert!(aligned["live"].get("replace").is_some());
        stream.assert_document("session/saved", &document);
    }
}

#[tokio::test]
async fn each_session_uses_its_own_have_and_cwd_on_a_multi_scope_stream() {
    let (server, mut link) = Server::start(json!({"loadSession": true})).await;
    let alpha_replay = turns(&[("alpha one", "a1"), ("alpha two", "a2")]);
    let beta_replay = turns(&[("beta one", "b1"), ("beta two", "b2"), ("beta three", "b3")]);
    let alpha_cache = log_values(TurnLog::from_replay(&alpha_replay[..2]).entries());
    let beta_cache = log_values(TurnLog::from_replay(&beta_replay[..4]).entries());
    let alpha_cwd = "/workspace/alpha:one + space";
    let beta_cwd = "/workspace/beta:two & more";
    // Scope, have and cwd orders intentionally differ; association is by scope.
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("scope", "session/alpha")
        .append_pair("scope", "session/beta")
        .append_pair(
            "have",
            &format!("session/beta:2:{}", beta_cache[1]["hash"].as_str().unwrap()),
        )
        .append_pair("cwd", &format!("session/alpha:{alpha_cwd}"))
        .append_pair(
            "have",
            &format!(
                "session/alpha:1:{}",
                alpha_cache[0]["hash"].as_str().unwrap()
            ),
        )
        .append_pair("cwd", &format!("session/beta:{beta_cwd}"))
        .finish();
    let mut stream = server.stream(&query).await;
    stream.cache_log("session/alpha", alpha_cache);
    stream.cache_log("session/beta", beta_cache);

    // Do not assume an order or concurrent admission of the two loads.
    let mut loaded = BTreeSet::new();
    for _ in 0..2 {
        let load = link.request("session/load").await;
        let id = load["params"]["sessionId"].as_str().unwrap();
        assert!(loaded.insert(id.to_string()), "a duplicate load: {load}");
        let (cwd, replay) = match id {
            "alpha" => (alpha_cwd, &alpha_replay),
            "beta" => (beta_cwd, &beta_replay),
            _ => panic!("an unexpected load: {load}"),
        };
        assert_eq!(load["params"]["cwd"], cwd, "the cwd belongs to {id}");
        for update in replay {
            link.update(id, update.clone()).await;
        }
        link.reply(&load, json!({})).await;
    }

    let mut log_from = BTreeMap::new();
    while !["session/alpha", "session/beta"].iter().all(|scope| {
        stream
            .replicas
            .get(*scope)
            .is_some_and(|replica| replica.live["op"]["status"] == "completed")
    }) {
        let event = stream.event().await;
        if let Some(log) = event.get("log") {
            log_from.insert(
                event["scope"].as_str().unwrap().to_string(),
                log["from"].clone(),
            );
            assert_eq!(log["entries"].as_array().unwrap().len(), 1, "{event}");
        }
    }
    assert_eq!(log_from["session/alpha"], 1, "alpha resumes its own head");
    assert_eq!(log_from["session/beta"], 2, "beta resumes its own head");
    for scope in ["session/alpha", "session/beta"] {
        let (status, document) = server.read(scope).await;
        assert_eq!(status, StatusCode::OK);
        stream.assert_document(scope, &document);
    }
    link.quiet(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn an_invalid_stream_query_is_refused() {
    let (server, _link) = Server::start(json!({})).await;
    for query in [
        "",
        "scope=elsewhere",
        "scope=session%2Fs&have=session%2Fs%3Ax%3Ah",
    ] {
        let response = server.send(&format!("/api/v2/stream?{query}")).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query:?}");
        let bytes = axum::body::to_bytes(Body::new(response.into_body()), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).expect("a JSON protocol error");
        assert_error_body(&body, "invalid_request");
    }
}

// ---------------------------------------------------------------------------
// The Agent connection (protocol §5.4)

#[tokio::test]
async fn after_an_agent_eof_reads_serve_the_last_documents_and_a_global_stream_restarts_nothing() {
    let (server, link) = Server::start(json!({})).await;
    let mut stream = server.stream("scope=global&scope=catalog").await;
    stream.event().await;
    stream.event().await;
    link.close().await;
    stream
        .until("global", |event| {
            mentions(event, "stopped") || mentions(event, "error")
        })
        .await;
    let (status, global) = server.read("global").await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(global["live"]["agent"]["phase"], "ready");
    let (status, _) = server.read("catalog").await;
    assert_eq!(status, StatusCode::OK);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        server.agents.accepted(),
        1,
        "neither a read nor a global stream starts a connection"
    );
    let (status, body) = server.read("session/unheld").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_error_body(&body, "not_found");
    assert_eq!(server.agents.accepted(), 1);
}

#[tokio::test]
async fn a_session_stream_waits_through_an_agent_eof_and_converges_on_the_next_connection() {
    let replay = turns(&[("one", "1"), ("two", "2")]);
    resumes_on_the_same_stream(replay.clone(), replay, false).await;
}

#[tokio::test]
async fn a_session_stream_applies_a_diverging_replay_after_an_agent_eof() {
    resumes_on_the_same_stream(
        turns(&[("one", "1"), ("two", "2")]),
        turns(&[("one", "1"), ("other", "3")]),
        true,
    )
    .await;
}

async fn resumes_on_the_same_stream(replay: Vec<Value>, next_replay: Vec<Value>, diverged: bool) {
    let (mut server, link, mut stream) = loaded(&replay).await;
    let (status, before) = server.read("session/saved").await;
    assert_eq!(status, StatusCode::OK);
    stream.assert_document("session/saved", &before);
    link.close().await;
    stream
        .until("session/saved", |event| mentions(event, "running"))
        .await;
    assert_eq!(stream.replica("session/saved").live["op"]["kind"], "load");
    assert_eq!(
        stream.replica("session/saved").live["op"]["status"],
        "running"
    );
    assert_eq!(json!(stream.replica("session/saved").log), before["log"]);

    // No GET or replacement subscription may trigger this connection: the
    // original HTTP stream alone must cause the bridge to reconnect.
    let mut next = server.agents.accept(json!({"loadSession": true})).await;
    let load = next.request("session/load").await;
    assert_eq!(load["params"]["sessionId"], "saved");
    assert_eq!(load["params"]["cwd"], CWD);

    // The actual request on the new Agent connection is outstanding. A
    // uniquely named elicitation from that connection is an ordered barrier:
    // an old queued "running" event cannot satisfy it. Check every event of
    // the original HTTP stream before the replay and again after its updates,
    // while withholding the load response throughout.
    for (stage, updates) in [
        ("before-replay", &[] as &[Value]),
        ("after-replay", next_replay.as_slice()),
    ] {
        for update in updates {
            next.update("saved", update.clone()).await;
        }
        let barrier = format!("recovery-{stage}-{}", load["id"]);
        next.send(json!({
            "jsonrpc": "2.0", "id": barrier, "method": "elicitation/create",
            "params": {
                "sessionId": "saved", "mode": "form", "message": barrier,
                "requestedSchema": {"type": "object", "properties": {}},
            },
        }))
        .await;
        let interaction = loop {
            let event = stream.event().await;
            let replica = stream.replica("session/saved");
            assert_eq!(
                json!(replica.log),
                before["log"],
                "the known log must stay until load completion: {event}"
            );
            assert_eq!(replica.live["op"]["kind"], "load", "{event}");
            assert_eq!(replica.live["op"]["status"], "running", "{event}");
            if let Some((id, _)) = replica.live["op"]["interactions"]
                .as_object()
                .expect("the load has an interaction map")
                .iter()
                .find(|(_, interaction)| interaction["request"]["message"] == barrier)
            {
                break id.clone();
            }
        };
        // This read only compares after the ordered SSE barrier. It also
        // rejects a bridge that silently turns Known into Unknown, which
        // would leave the client's cached log looking unchanged.
        let (status, waiting) = server.read("session/saved").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(waiting["live"]["op"]["kind"], "load");
        assert_eq!(waiting["live"]["op"]["status"], "running");
        assert_eq!(
            waiting["log"], before["log"],
            "the bridge keeps its known log"
        );
        let answer = send(
            server.address,
            Method::POST,
            "/api/v2/scopes/session%2Fsaved/ops",
            Some(json!({
                "expect": waiting["live"]["op"]["id"], "kind": "answer",
                "args": {"answers": {
                    &interaction: {"response": {"action": "accept", "content": {}}},
                }},
            })),
        )
        .await;
        assert_eq!(answer.status(), StatusCode::ACCEPTED);
        let answered = next.next(WAIT).await.expect("the barrier was answered");
        assert_eq!(answered["id"], barrier, "{answered}");
        assert_eq!(answered["result"]["action"], "accept", "{answered}");
    }

    next.reply(&load, json!({})).await;
    let expected_log = json!(log_values(TurnLog::from_replay(&next_replay).entries()));
    let mut events = Vec::new();
    loop {
        let event = stream.event().await;
        let replica = stream.replica("session/saved");
        let completed = event["scope"] == "session/saved"
            && replica.live["op"]["kind"] == "load"
            && replica.live["op"]["status"] == "completed";
        assert_eq!(
            &json!(replica.log),
            if completed {
                &expected_log
            } else {
                &before["log"]
            },
            "only the event that completes the load may replace its known log: {event}"
        );
        events.push(event);
        if completed {
            break;
        }
    }
    let (status, after) = server.read("session/saved").await;
    assert_eq!(status, StatusCode::OK);
    stream.assert_document("session/saved", &after);
    if diverged {
        let rebuilt = events
            .iter()
            .rev()
            .find(|event| event["scope"] == "session/saved" && event.get("log").is_some())
            .expect("the original stream receives the rebuilt log");
        assert_eq!(rebuilt["log"]["from"], 1, "the common prefix is preserved");
        assert_eq!(after["log"][0], before["log"][0]);
        assert_ne!(after["log"][1], before["log"][1]);
    } else {
        assert_eq!(
            after["log"], before["log"],
            "the same history keeps the log"
        );
    }
    assert_eq!(after["live"]["op"]["status"], "completed");
    assert_eq!(server.agents.accepted(), 2);
}

/// The JSON body of a response.
async fn json_of(response: Response<hyper::body::Incoming>) -> Value {
    serde_json::from_slice(
        &axum::body::to_bytes(Body::new(response.into_body()), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap()
}

/// Runs a real terminal login to success through a v2 `terminal_login`. Its
/// prompt and its exit are read from a global stream of its own, opened
/// while the connection runs and dropped once the login completed. The
/// caller's stream alone then follows the next connection.
async fn sign_in(address: SocketAddr) {
    let response = send(address, Method::GET, "/api/v2/stream?scope=global", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut events = Sse {
        body: Body::new(response.into_body()).into_data_stream(),
        buffer: String::new(),
        replicas: BTreeMap::new(),
        aligned: BTreeSet::new(),
        ended: BTreeSet::new(),
    };
    events.event().await;
    let expect = events.replica("global").live["op"]["id"].clone();
    let started = send(
        address,
        Method::POST,
        "/api/v2/scopes/global/ops",
        Some(json!({
            "expect": expect,
            "kind": "terminal_login",
            "args": {"methodId": "terminal-login", "cols": 80, "rows": 24},
        })),
    )
    .await;
    assert_eq!(started.status(), StatusCode::ACCEPTED);
    let login = json_of(started).await["op"].as_str().unwrap().to_string();
    // Input waits for the prompt the login prints.
    let output = |events: &Sse| {
        events.replica("global").live["terminals"][&login]["output"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    while !output(&events).contains("Enter access code") {
        events.event().await;
    }
    let input = send(
        address,
        Method::POST,
        &format!("/api/v2/scopes/global/resources/{login}/input"),
        Some(json!({"data": "open-sesame\r"})),
    )
    .await;
    assert!(input.status().is_success(), "{}", input.status());
    loop {
        let op = events.replica("global").live["op"].clone();
        if op["id"] == login.as_str() && op["status"] != "running" {
            assert_eq!(op["status"], "completed", "{op}");
            assert_eq!(op["result"], json!({"exitCode": 0, "signal": null}), "{op}");
            break;
        }
        events.event().await;
    }
}

/// Q2: a successful terminal login refreshes the connection. With only a
/// global stream open, and no read or new subscription, the bridge starts the
/// next connection once the old one ends; the original stream follows it.
#[tokio::test]
async fn a_successful_terminal_login_starts_the_next_connection_for_a_global_only_stream() {
    let root = tempfile::tempdir().unwrap();
    let auth = root.path().join("authenticated");
    let launches = root.path().join("launches");
    let manifest = env!("CARGO_MANIFEST_DIR");
    let fixture = std::path::Path::new(manifest).join("tests/fixtures/fake-agent.ts");
    // Each launch, the Agent's and the login's (which appends its method's
    // arguments), records its arguments before it becomes the fixture.
    let script = format!(
        "export ATTYD_FAKE_AUTH_FILE='{}'; echo \"$*\" >> '{}'; exec node --import tsx '{}' \"$@\"",
        auth.display(),
        launches.display(),
        fixture.display(),
    );
    let options = Options::try_parse_from([
        "attyd",
        "--cwd",
        manifest,
        "--",
        "sh",
        "-c",
        &script,
        "agent",
        "--terminal-auth-required",
    ])
    .unwrap()
    .normalized()
    .unwrap();
    let options = Arc::new(options);
    let hub = BridgeHub::new(options.clone());
    hub.ensure_runtime().await;
    let app = app_router(&options, hub.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let launched = || {
        let lines = std::fs::read_to_string(&launches).unwrap_or_default();
        let (logins, agents): (Vec<&str>, Vec<&str>) = lines
            .lines()
            .partition(|line| line.contains("--terminal-login"));
        (agents.len(), logins.len())
    };

    let response = send(address, Method::GET, "/api/v2/stream?scope=global", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = Sse {
        body: Body::new(response.into_body()).into_data_stream(),
        buffer: String::new(),
        replicas: BTreeMap::new(),
        aligned: BTreeSet::new(),
        ended: BTreeSet::new(),
    };
    let phase = |stream: &Sse| stream.replica("global").live["agent"]["phase"].clone();
    // The alignment comes first; only then is there a replica to inspect.
    assert_eq!(stream.event().await["scope"], "global");
    while phase(&stream) != "ready" {
        stream.event().await;
    }
    assert_eq!(launched(), (1, 0));

    // The login runs to success; then the old connection ends, and the next
    // one becomes ready on the same stream.
    sign_in(address).await;
    assert!(auth.exists(), "the login signed in");
    let mut ended = false;
    loop {
        stream.event().await;
        let phase = phase(&stream);
        ended |= phase != "ready";
        if ended && phase == "ready" {
            break;
        }
    }
    assert_eq!(
        launched(),
        (2, 1),
        "one login and exactly one next connection"
    );

    let response = send(address, Method::GET, "/api/v2/scopes/global", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let read: Value = serde_json::from_slice(
        &axum::body::to_bytes(Body::new(response.into_body()), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    stream.assert_document("global", &read);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(launched(), (2, 1), "the next connection is started once");
    hub.shutdown().await;
}

// ---------------------------------------------------------------------------
// Subscribing while the Agent initializes, and the recovery's boundaries
// (protocol §5.1, §5.4; Q2)

/// A dropped body has actually left the Hub, not merely had time to do so.
async fn wait_subscribers(hub: &BridgeHub, session_id: &str, expected: usize) {
    tokio::time::timeout(WAIT, async {
        loop {
            if hub.publication.lock().hub.subscribers(session_id) == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the HTTP subscriber count did not settle");
}

/// Start the negative observation only after the old runtime really stopped,
/// then cross its actual next-attempt backoff, with a scheduling margin.
async fn cross_recovery_window(hub: &BridgeHub) {
    let backoff = tokio::time::timeout(WAIT, async {
        loop {
            let state = hub.state.lock().await;
            if state.input.is_none() {
                return state.recovery_backoff;
            }
            drop(state);
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the old Agent runtime did not finish");
    tokio::time::sleep(backoff + Duration::from_secs(1)).await;
}

fn initialized(capabilities: Value) -> Value {
    json!({"protocolVersion": 1, "agentCapabilities": capabilities, "authMethods": []})
}

/// Events until `global` shows the Agent `phase`.
async fn until_phase(stream: &mut Sse, phases: &[&str]) {
    loop {
        let event = stream.event().await;
        if event["scope"] == "global"
            && stream.replica("global").live["agent"]["phase"]
                .as_str()
                .is_some_and(|phase| phases.contains(&phase))
        {
            return;
        }
    }
}

#[tokio::test]
async fn a_new_global_stream_aligns_while_initialize_is_unanswered() {
    let mut server = Server::launch().await;
    let mut link = server.agents.connect().await;
    let initialize = link.request("initialize").await;

    let mut stream = server.stream("scope=global").await;
    let aligned = stream.event().await;
    assert_eq!(aligned["scope"], "global", "{aligned}");
    let agent = &aligned["live"]["replace"]["agent"];
    assert_eq!(agent["phase"], "initializing", "{aligned}");
    assert!(agent["initialize"].is_null(), "{aligned}");

    link.reply(&initialize, initialized(json!({}))).await;
    while stream.replica("global").live["agent"]["phase"] != "ready" {
        stream.event().await;
    }
    let (status, global) = server.read("global").await;
    assert_eq!(status, StatusCode::OK);
    stream.assert_document("global", &global);
}

#[tokio::test]
async fn a_known_session_aligns_on_a_new_stream_while_the_next_initialize_is_unanswered() {
    let replay = turns(&[("one", "1"), ("two", "2")]);
    let (mut server, link, mut first) = loaded(&replay).await;
    let (_, before) = server.read("session/saved").await;
    link.close().await;
    first
        .until("session/saved", |event| mentions(event, "running"))
        .await;

    let mut next = server.agents.connect().await;
    let initialize = next.request("initialize").await;
    // A held session needs no cwd; its known log aligns at once.
    let mut second = server.stream("scope=session%2Fsaved").await;
    let aligned = second.event().await;
    assert_eq!(aligned["scope"], "session/saved", "{aligned}");
    assert_eq!(
        json!(second.replica("session/saved").log),
        before["log"],
        "{aligned}"
    );
    assert_eq!(aligned["live"]["replace"]["op"]["kind"], "load");
    assert_eq!(aligned["live"]["replace"]["op"]["status"], "running");

    next.reply(&initialize, initialized(json!({"loadSession": true})))
        .await;
    next.serve_load("saved", &replay).await;
    second
        .until("session/saved", |event| mentions(event, "completed"))
        .await;
    let (_, after) = server.read("session/saved").await;
    second.assert_document("session/saved", &after);
    first
        .until("session/saved", |event| mentions(event, "completed"))
        .await;
    first.assert_document("session/saved", &after);
    assert_eq!(after["log"], before["log"]);
    assert_eq!(server.agents.accepted(), 2);
}

#[tokio::test]
async fn an_initialization_failure_keeps_recovering_while_a_session_stream_needs_it() {
    let replay = turns(&[("one", "1")]);
    let (mut server, link, mut stream) = loaded(&replay).await;
    let (_, before) = server.read("session/saved").await;
    link.close().await;
    stream
        .until("session/saved", |event| mentions(event, "running"))
        .await;

    // The next connection ends before it answers initialize.
    let mut failed = server.agents.connect().await;
    failed.request("initialize").await;
    failed.close().await;

    let mut next = server.agents.accept(json!({"loadSession": true})).await;
    next.serve_load("saved", &replay).await;
    loop {
        let event = stream.event().await;
        assert!(
            event.get("end").is_none(),
            "the stream keeps the session: {event}"
        );
        let replica = stream.replica("session/saved");
        assert_eq!(json!(replica.log), before["log"], "{event}");
        if replica.live["op"]["kind"] == "load" && replica.live["op"]["status"] == "completed" {
            break;
        }
    }
    let (_, after) = server.read("session/saved").await;
    stream.assert_document("session/saved", &after);
    assert_eq!(server.agents.accepted(), 3);
}

#[tokio::test]
async fn recovery_continues_for_a_remaining_subscriber_and_stops_after_the_last_leaves() {
    let replay = turns(&[("one", "1")]);
    let (mut server, link, first) = loaded(&replay).await;
    let mut second = server.stream("scope=session%2Fsaved").await;
    second.event().await;
    // A global stream only watches the attempts; it needs no connection.
    let mut watcher = server.stream("scope=global").await;
    watcher.event().await;

    link.close().await;
    until_phase(&mut watcher, &["stopped", "error"]).await;
    drop(first);
    wait_subscribers(&server.hub, "saved", 1).await;

    // One subscriber left: a failed attempt is followed by another.
    let mut failed = server.agents.connect().await;
    failed.request("initialize").await;
    until_phase(&mut watcher, &["initializing"]).await;
    failed.close().await;
    until_phase(&mut watcher, &["stopped", "error"]).await;
    let mut pending = server.agents.connect().await;
    pending.request("initialize").await;
    until_phase(&mut watcher, &["initializing"]).await;

    // The last subscriber leaves while that attempt runs; once it ends, no
    // further attempt starts.
    drop(second);
    wait_subscribers(&server.hub, "saved", 0).await;
    pending.close().await;
    until_phase(&mut watcher, &["stopped", "error"]).await;
    cross_recovery_window(&server.hub).await;
    assert_eq!(
        server.agents.accepted(),
        3,
        "no attempt after the last subscriber"
    );
}

#[tokio::test]
async fn a_cold_load_cut_by_an_eof_keeps_the_cache_and_converges_on_the_next_connection() {
    let replay = turns(&[("one", "1"), ("two", "2"), ("three", "3")]);
    let cached = log_values(TurnLog::from_replay(&replay[..4]).entries());
    let (mut server, mut link) = Server::start(json!({"loadSession": true})).await;
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("scope", "session/saved")
        .append_pair(
            "have",
            &format!("session/saved:2:{}", cached[1]["hash"].as_str().unwrap()),
        )
        .append_pair("cwd", &format!("session/saved:{CWD}"))
        .finish();
    let mut stream = server.stream(&query).await;
    stream.cache_log("session/saved", cached.clone());

    // The client has two turns, but the interrupted load only replays one.
    // Publishing that incomplete history would visibly truncate its cache.
    let load = link.request("session/load").await;
    assert_eq!(load["params"]["sessionId"], "saved");
    let aligned = stream.event().await;
    assert_eq!(aligned["scope"], "session/saved", "{aligned}");
    assert!(aligned.get("log").is_none(), "{aligned}");
    let replica = stream.replica("session/saved");
    assert_eq!(json!(replica.log), json!(cached), "{aligned}");
    assert_eq!(replica.live["op"]["kind"], "load", "{aligned}");
    assert_eq!(replica.live["op"]["status"], "running", "{aligned}");
    for update in &replay[..2] {
        link.update("saved", update.clone()).await;
    }
    // This real Agent request follows the partial replay in ordered ingress.
    // Seeing it in the same SSE proves the coordinator processed that replay;
    // neither a delay nor a GET establishes that ordering.
    let barrier = "cold-load-partial-replay-barrier";
    link.send(json!({
        "jsonrpc": "2.0", "id": barrier, "method": "elicitation/create",
        "params": {
            "sessionId": "saved", "mode": "form", "message": barrier,
            "requestedSchema": {"type": "object", "properties": {}},
        },
    }))
    .await;
    loop {
        let event = stream.event().await;
        assert!(event.get("end").is_none(), "{event}");
        let replica = stream.replica("session/saved");
        assert_eq!(
            json!(replica.log),
            json!(cached),
            "an unfinished replay must not replace the cache: {event}"
        );
        assert_eq!(replica.live["op"]["kind"], "load", "{event}");
        assert_eq!(replica.live["op"]["status"], "running", "{event}");
        if replica.live["op"]["interactions"]
            .as_object()
            .expect("the running load has an interaction map")
            .values()
            .any(|interaction| interaction["request"]["message"] == barrier)
        {
            break;
        }
    }
    // Comparison only, after the wire barrier: an unchanged client cache must
    // not hide the bridge prematurely changing Unknown into Known.
    let (status, loading) = server.read("session/saved").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !loading.as_object().unwrap().contains_key("log"),
        "the unfinished first load still has an unknown log: {loading}"
    );
    stream.assert_document("session/saved", &loading);
    // Leave the elicitation unanswered. EOF cancels it with the first load.
    link.close().await;

    let mut next = server.agents.accept(json!({"loadSession": true})).await;
    let load = next.request("session/load").await;
    assert_eq!(load["params"]["sessionId"], "saved");
    assert_eq!(load["params"]["cwd"], CWD);
    for update in &replay {
        next.update("saved", update.clone()).await;
    }
    next.reply(&load, json!({})).await;

    let expected = json!(log_values(TurnLog::from_replay(&replay).entries()));
    loop {
        let event = stream.event().await;
        assert!(
            event.get("end").is_none(),
            "the stream keeps the session: {event}"
        );
        let replica = stream.replica("session/saved");
        let completed =
            replica.live["op"]["kind"] == "load" && replica.live["op"]["status"] == "completed";
        if completed {
            assert_eq!(json!(replica.log), expected, "{event}");
            break;
        }
        assert_eq!(
            json!(replica.log),
            json!(cached),
            "no partial replay replaces the client's cache: {event}"
        );
    }
    let (_, after) = server.read("session/saved").await;
    stream.assert_document("session/saved", &after);
    assert_eq!(server.agents.accepted(), 2);
}

#[tokio::test]
async fn a_session_ended_with_load_failed_is_no_recovery_need_behind_a_global_stream() {
    let (server, mut link) = Server::start(json!({"loadSession": true})).await;
    let mut stream = server
        .stream(&format!(
            "scope=global&scope=session%2Fsaved&cwd={}",
            query_scope(&format!("session/saved:{CWD}"))
        ))
        .await;
    let load = link.request("session/load").await;
    link.send(json!({
        "jsonrpc": "2.0", "id": load["id"],
        "error": {"code": -32002, "message": "no such session"},
    }))
    .await;
    let end = stream
        .until("session/saved", |event| event.get("end").is_some())
        .await
        .pop()
        .unwrap();
    assert_eq!(end["end"]["reason"], "load_failed", "{end}");
    wait_subscribers(&server.hub, "saved", 0).await;

    link.close().await;
    until_phase(&mut stream, &["stopped", "error"]).await;
    cross_recovery_window(&server.hub).await;
    assert_eq!(
        server.agents.accepted(),
        1,
        "an ended session scope needs no connection"
    );
}

/// Q2: the login authorizes one next connection. When that connection ends
/// with an ordinary EOF, a global-only stream starts no third.
#[tokio::test]
async fn a_login_restart_is_consumed_and_the_next_ordinary_eof_starts_nothing() {
    let root = tempfile::tempdir().unwrap();
    let auth = root.path().join("authenticated");
    let launches = root.path().join("launches");
    let manifest = env!("CARGO_MANIFEST_DIR");
    let fixture = std::path::Path::new(manifest).join("tests/fixtures/fake-agent.ts");
    // Each launch records its process ID (kept by exec) and arguments.
    let script = format!(
        "export ATTYD_FAKE_AUTH_FILE='{}'; echo \"$$ $*\" >> '{}'; exec node --import tsx '{}' \"$@\"",
        auth.display(),
        launches.display(),
        fixture.display(),
    );
    let options = Options::try_parse_from([
        "attyd",
        "--cwd",
        manifest,
        "--",
        "sh",
        "-c",
        &script,
        "agent",
        "--terminal-auth-required",
    ])
    .unwrap()
    .normalized()
    .unwrap();
    let options = Arc::new(options);
    let hub = BridgeHub::new(options.clone());
    hub.ensure_runtime().await;
    let app = app_router(&options, hub.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let agents = || {
        std::fs::read_to_string(&launches)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.contains("--terminal-login"))
            .map(|line| line.split(' ').next().unwrap().to_string())
            .collect::<Vec<_>>()
    };

    let response = send(address, Method::GET, "/api/v2/stream?scope=global", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = Sse {
        body: Body::new(response.into_body()).into_data_stream(),
        buffer: String::new(),
        replicas: BTreeMap::new(),
        aligned: BTreeSet::new(),
        ended: BTreeSet::new(),
    };
    let phase = |stream: &Sse| stream.replica("global").live["agent"]["phase"].clone();
    assert_eq!(stream.event().await["scope"], "global");
    while phase(&stream) != "ready" {
        stream.event().await;
    }

    sign_in(address).await;
    let mut ended = false;
    loop {
        stream.event().await;
        let phase = phase(&stream);
        ended |= phase != "ready";
        if ended && phase == "ready" {
            break;
        }
    }
    let launched = agents();
    assert_eq!(launched.len(), 2, "the login's one next connection");

    // The next connection's Agent exits: an ordinary EOF.
    let killed = std::process::Command::new("kill")
        .arg(&launched[1])
        .status()
        .unwrap();
    assert!(killed.success());
    while !matches!(phase(&stream).as_str(), Some("stopped" | "error")) {
        stream.event().await;
    }
    cross_recovery_window(&hub).await;
    assert_eq!(
        agents().len(),
        2,
        "the login authorized one connection only"
    );
    hub.shutdown().await;
}

/// A stream request can retain the old generation's sender while that
/// generation ends and a new one becomes ready. Its fallback must still
/// schedule the new cold scope on the current actor.
#[tokio::test]
async fn review_a_subscription_crossing_generations_loads_on_the_current_agent() {
    let replay = turns(&[("one", "1")]);
    let (mut server, old, mut keeper) = loaded(&replay).await;
    let old_generation = server.hub.state.lock().await.generation;
    let (reached, captured) = oneshot::channel();
    let (resume, paused) = oneshot::channel();
    *server.hub.after_subscribe_input.lock().unwrap() = Some((reached, paused));
    let address = server.address;
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("scope", "session/new-cold")
        .append_pair("cwd", &format!("session/new-cold:{CWD}"))
        .finish();
    let opening = tokio::spawn(async move {
        send(
            address,
            Method::GET,
            &format!("/api/v2/stream?{query}"),
            None,
        )
        .await
    });
    tokio::time::timeout(WAIT, captured)
        .await
        .expect("the HTTP request captures the old input")
        .unwrap();

    // The paused request has registered no scope yet. The existing real
    // HTTP stream alone drives recovery and the next generation's reload.
    assert_eq!(server.hub.publication.lock().hub.subscribers("new-cold"), 0);
    old.close().await;
    let mut next = server.agents.accept(json!({"loadSession": true})).await;
    next.serve_load("saved", &replay).await;
    keeper
        .until("session/saved", |event| mentions(event, "completed"))
        .await;
    assert!(server.hub.state.lock().await.generation > old_generation);
    assert!(server.hub.state.lock().await.input.is_some());
    assert_eq!(
        keeper.replica("session/saved").live["op"]["status"],
        "completed"
    );

    // Readiness's ServeStreams has already dispatched and completed the
    // sole reload. No unrelated stream or command wakes the actor afterward.
    resume.send(()).unwrap();
    let response = tokio::time::timeout(WAIT, opening)
        .await
        .expect("the request opens a stream on the current generation")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut newcomer = Sse {
        body: Body::new(response.into_body()).into_data_stream(),
        buffer: String::new(),
        replicas: BTreeMap::new(),
        aligned: BTreeSet::new(),
        ended: BTreeSet::new(),
    };
    assert_eq!(server.hub.publication.lock().hub.subscribers("new-cold"), 1);
    let load = next.request("session/load").await;
    assert_eq!(load["params"]["sessionId"], "new-cold");
    assert_eq!(load["params"]["cwd"], CWD);
    next.reply(&load, json!({})).await;
    newcomer
        .until("session/new-cold", |event| mentions(event, "completed"))
        .await;
    let (status, document) = server.read("session/new-cold").await;
    assert_eq!(status, StatusCode::OK);
    newcomer.assert_document("session/new-cold", &document);
    assert_eq!(server.agents.accepted(), 2, "no extra connection is needed");
    server.hub.shutdown().await;
}
/// A peer that never answers initialize must not leave the published Agent
/// running after shutdown has drained and closed the protocol streams.
#[tokio::test]
async fn review_shutdown_during_initialize_publishes_a_final_agent_before_closing_streams() {
    let mut server = Server::launch().await;
    let mut link = server.agents.connect().await;
    link.request("initialize").await;
    let mut stream = server.stream("scope=global").await;
    let aligned = stream.event().await;
    assert_eq!(aligned["live"]["replace"]["agent"]["phase"], "initializing");

    tokio::time::timeout(WAIT, server.hub.shutdown())
        .await
        .expect("shutdown must finish even when initialize is unanswered");
    // Observe the actual HTTP body ending, not a quiet timeout. Keep every
    // queued event: publishing a final cut after closing the stream cannot
    // substitute for delivering that cut before EOF.
    let mut delivered = std::mem::take(&mut stream.buffer).into_bytes();
    tokio::time::timeout(WAIT, async {
        while let Some(chunk) = stream.body.next().await {
            delivered.extend_from_slice(&chunk.unwrap());
        }
    })
    .await
    .expect("shutdown must drain and close the SSE body");
    let (status, document) = server.read("global").await;
    let runtime_remains = server.hub.state.lock().await.input.is_some();
    // Release the scripted peer before asserting, including on the old code.
    link.close().await;
    let delivered = String::from_utf8(delivered).unwrap();
    for message in delivered.split("\n\n") {
        let data: Vec<_> = message
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect();
        if data.is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(&data.join("\n")).unwrap();
        assert_eq!(event["scope"], "global", "{event}");
        assert!(event.get("end").is_none(), "{event}");
        stream
            .replicas
            .get_mut("global")
            .unwrap()
            .apply(&event)
            .unwrap();
    }
    assert_eq!(status, StatusCode::OK);
    assert!(
        matches!(
            document["live"]["agent"]["phase"].as_str(),
            Some("stopped" | "error")
        ),
        "the final published cut must precede SSE EOF: {document}"
    );
    stream.assert_document("global", &document);
    assert!(
        !runtime_remains,
        "shutdown left initialize running after closing SSE"
    );
}

/// Options' URI parser accepts this authority, but the HTTP transport's URL
/// parser rejects its port before it has a connection. That startup failure
/// still belongs in the always-existing global/catalog documents.
#[tokio::test]
async fn review_an_http_transport_construction_error_is_published_to_v2() {
    let options = Options::try_parse_from([
        "attyd",
        "--transport",
        "http",
        "--",
        "http://127.0.0.1:99999/acp",
    ])
    .unwrap()
    .normalized()
    .expect("the configured endpoint passes the current CLI validation");
    let options = Arc::new(options);
    let hub = BridgeHub::new(options.clone());
    hub.ensure_runtime().await;
    tokio::time::timeout(WAIT, async {
        loop {
            let stopped = hub.stopped.notified();
            if hub.state.lock().await.input.is_none() {
                break;
            }
            stopped.await;
        }
    })
    .await
    .expect("the rejected transport construction ends the generation");

    let app = app_router(&options, hub.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut reads = Vec::new();
    for scope in ["global", "catalog"] {
        let response = send(
            address,
            Method::GET,
            &format!("/api/v2/scopes/{scope}"),
            None,
        )
        .await;
        let status = response.status();
        let bytes = axum::body::to_bytes(Body::new(response.into_body()), usize::MAX)
            .await
            .unwrap();
        reads.push((
            scope,
            status,
            serde_json::from_slice::<Value>(&bytes).unwrap(),
        ));
    }
    hub.shutdown().await;
    server.abort();
    for (scope, status, document) in &reads {
        assert_eq!(
            *status,
            StatusCode::OK,
            "{scope} must exist after startup failed: {document}"
        );
        assert_eq!(document["log"], json!([]));
    }
    let global = &reads[0].2;
    assert_eq!(global["live"]["agent"]["phase"], "error", "{global}");
    assert!(
        !global["live"]["agent"]["error"]["message"]
            .as_str()
            .unwrap_or("")
            .is_empty()
    );
}

/// Readiness must serve cold scopes registered during initialize as well as
/// the held sessions inherited from the previous Agent connection.
#[tokio::test]
async fn review_a_cold_subscription_registered_during_initialize_is_served_after_reloads() {
    let replay = turns(&[("one", "1")]);
    let (mut server, old, mut keeper) = loaded(&replay).await;
    old.close().await;
    keeper
        .until("session/saved", |event| mentions(event, "running"))
        .await;

    let mut next = server.agents.connect().await;
    let initialize = next.request("initialize").await;
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("scope", "session/new-cold")
        .append_pair("cwd", &format!("session/new-cold:{CWD}"))
        .finish();
    let mut cold = server.stream(&query).await;
    // The HTTP handshake only completes after the actor registers this
    // subscription. Observe that registration before answering initialize;
    // no sleep or a request captured from the previous generation is used.
    wait_subscribers(&server.hub, "new-cold", 1).await;
    assert_eq!(
        server.read("session/new-cold").await.0,
        StatusCode::NOT_FOUND
    );

    next.reply(&initialize, initialized(json!({"loadSession": true})))
        .await;
    next.serve_load("saved", &replay).await;
    keeper
        .until("session/saved", |event| mentions(event, "completed"))
        .await;
    let (status, saved) = server.read("session/saved").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["live"]["op"]["status"], "completed");
    keeper.assert_document("session/saved", &saved);

    // Even after the inherited reload has succeeded, the new cold scope
    // must start without another HTTP request, subscription or Agent event.
    let load = next.request("session/load").await;
    assert_eq!(load["params"]["sessionId"], "new-cold", "{load}");
    assert_eq!(load["params"]["cwd"], CWD, "{load}");
    next.reply(&load, json!({})).await;
    cold.until("session/new-cold", |event| mentions(event, "completed"))
        .await;
    let (status, document) = server.read("session/new-cold").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["log"], json!([]));
    cold.assert_document("session/new-cold", &document);
    assert_eq!(server.agents.accepted(), 2);
    server.hub.shutdown().await;
}
