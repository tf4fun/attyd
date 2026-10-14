//! `GET /api/v2/stream` and `GET /api/v2/scopes/{scope}` over real HTTP
//! (migration step 5; protocol §5, §8).
//!
//! The server listens on a TCP port and its bridge connects to a scripted
//! Agent over the WebSocket transport, so each Agent connection is a real ACP
//! connection the test answers. Requests go through a hyper client.
use std::collections::VecDeque;

use clap::Parser;
use futures::{SinkExt, StreamExt};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use super::*;

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

    /// The next connection, initialized with `capabilities`.
    async fn accept(&mut self, capabilities: Value) -> Link {
        let mut link = tokio::time::timeout(WAIT, self.links.recv())
            .await
            .expect("the bridge did not connect to the Agent")
            .unwrap();
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
        let mut agents = Agents::listen().await;
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
        let link = agents.accept(capabilities).await;
        (
            Self {
                address,
                hub,
                agents,
            },
            link,
        )
    }

    async fn send(&self, path: &str) -> Response<hyper::body::Incoming> {
        let stream = TcpStream::connect(self.address).await.unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(connection);
        let request = Request::builder()
            .uri(path)
            .header(header::HOST, self.address.to_string())
            .body(Body::empty())
            .unwrap();
        tokio::time::timeout(WAIT, sender.send_request(request))
            .await
            .expect("the server did not answer")
            .unwrap()
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
        }
    }

    async fn create(&self, link: &mut Link, session_id: &str) {
        let hub = self.hub.clone();
        let created = tokio::spawn(async move {
            hub.business_request(json!({
                "type": "session/new", "requestId": "new", "cwd": CWD,
            }))
            .await
        });
        let new = link.request("session/new").await;
        link.reply(&new, json!({"sessionId": session_id})).await;
        created.await.unwrap().unwrap();
    }
}

/// An SSE response body, read message by message.
struct Sse {
    body: axum::body::BodyDataStream,
    buffer: String,
}

impl Sse {
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
            return serde_json::from_str(&data.join("\n")).unwrap();
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
    scope.replace('/', "%2F").replace(':', "%3A")
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

    let (status, _) = server.read("session/unheld").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "an unheld session");
    link.quiet(Duration::from_millis(300)).await;
    let (status, _) = server.read("session/unheld").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a read holds and loads nothing"
    );

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
    for (have, from, appended) in [
        (format!("1:{h1}"), 1, 1),
        (format!("2:{h2}"), 2, 0),
        ("1:wrong".to_string(), 0, 2),
        (format!("3:{h2}"), 0, 2),
    ] {
        let mut stream = server
            .stream(&format!(
                "scope=session%2Fsaved&have={}",
                query_scope(&format!("session/saved:{have}"))
            ))
            .await;
        let aligned = stream.event().await;
        assert_eq!(aligned["log"]["from"], from, "have {have}: {aligned}");
        assert_eq!(
            aligned["log"]["entries"].as_array().unwrap().len(),
            appended,
            "have {have}"
        );
        assert!(aligned["live"].get("replace").is_some());
    }
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
    let (status, _) = server.read("session/unheld").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(server.agents.accepted(), 1);
}

#[tokio::test]
async fn a_session_stream_waits_through_an_agent_eof_and_converges_on_the_next_connection() {
    let replay = turns(&[("one", "1"), ("two", "2")]);
    let (mut server, link, mut stream) = loaded(&replay).await;
    let (_, before) = server.read("session/saved").await;
    link.close().await;
    stream
        .until("session/saved", |event| mentions(event, "running"))
        .await;
    let (_, waiting) = server.read("session/saved").await;
    assert_eq!(waiting["live"]["op"]["kind"], "load");
    assert_eq!(waiting["live"]["op"]["status"], "running");
    assert_eq!(waiting["log"], before["log"], "the known log stays");

    // The pending session subscription starts the next connection.
    let mut next = server.agents.accept(json!({"loadSession": true})).await;
    let load = next.serve_load("saved", &replay).await;
    assert_eq!(load["params"]["cwd"], CWD);
    stream
        .until("session/saved", |event| mentions(event, "completed"))
        .await;
    let (_, after) = server.read("session/saved").await;
    assert_eq!(
        after["log"], before["log"],
        "the same history keeps the log"
    );
    assert_eq!(after["live"]["op"]["status"], "completed");
    assert_eq!(server.agents.accepted(), 2);
}
