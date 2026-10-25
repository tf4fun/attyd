//! `GET /api/v2/stream`, the reads of nodes and the writes to targets over real HTTP (protocol
//! revision 4 §6–§9).
//!
//! The server listens on a TCP port and its bridge connects to a scripted
//! Agent over the WebSocket transport, so each Agent connection is a real ACP
//! connection the test answers. Requests go through a hyper client.
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::Method;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use super::*;

mod actions;
mod bench;
mod tree;

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

// -- The scripted Agent, continued -----------------------------------------------------------

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
        Self::serve(options, agents).await
    }

    /// A server run with `options`, whose Agent connections `agents` accepts.
    async fn serve(options: Options, agents: Agents) -> Self {
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

    /// The document of `scope` as scopes were once read: the status and the JSON body
    /// `{ scope, snapshot: { epoch, rev, digest, doc, entries? } }`, made from a read of the
    /// scope's node. A scope that is not there, as an unheld session, is `404`.
    async fn read(&self, scope: &str) -> (StatusCode, Value) {
        read_scope_snapshot(self.address, scope).await
    }

    /// Creates `session_id` through a `create` on `global`: the write is accepted before the
    /// Agent answers, and the session is there once it has.
    async fn create(&self, link: &mut Link, session_id: &str) {
        let (_, global) = self.read("global").await;
        let body = json!({
            "id": crate::protocol::op::operation_id(),
            "expect": global["snapshot"]["doc"]["live"]["op"]["id"], "kind": "create",
            "args": {"cwd": CWD},
        });
        let created = send(
            self.address,
            Method::POST,
            "/api/v2/targets/global/ops",
            Some(body),
        )
        .await;
        assert_eq!(created.status(), StatusCode::ACCEPTED);
        let new = link.request("session/new").await;
        link.reply(&new, json!({"sessionId": session_id})).await;
        let scope = format!("session/{session_id}");
        let deadline = tokio::time::Instant::now() + WAIT;
        while self.read(&scope).await.0 != StatusCode::OK {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{scope} was not created"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
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


/// `GET /api/v2/stream?…`: the status and the SSE body.
async fn open(server: &Server, query: &str) -> (StatusCode, Sse) {
    let response = server.send(&format!("/api/v2/stream?{query}")).await;
    let status = response.status();
    if status == StatusCode::OK {
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/event-stream");
    }
    let sse = Sse {
        body: Body::new(response.into_body()).into_data_stream(),
        buffer: String::new(),
    };
    (status, sse)
}

/// The next event of the stream, skipping comments (the heartbeat).
async fn event(sse: &mut Sse, period: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + period;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let message = sse.message(left).await?;
        assert!(
            message.lines().all(|line| !line.starts_with("id:")),
            "events carry no SSE id: {message:?}"
        );
        let data: Vec<&str> = message
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect();
        if !data.is_empty() {
            return Some(serde_json::from_str(&data.join("\n")).unwrap());
        }
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
}

/// A read of the node of `scope`, as the document of the scope it holds (design §7.1).
async fn read_scope_snapshot(address: SocketAddr, scope: &str) -> (StatusCode, Value) {
    use crate::protocol::tree_stream::documents::{path_of, snapshot_body};
    let encoded: String = path_of(scope)
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect();
    let path = format!("/api/v2/nodes{encoded}");
    let response = send(address, Method::GET, &path, None).await;
    let status = response.status();
    let body = json_of(response).await;
    let value = &body["change"]["snapshot"]["node"]["v"];
    let at = body["change"]["snapshot"]["at"].as_u64().unwrap_or(0);
    match snapshot_body(scope, body["instance"].as_str().unwrap_or(""), at, value) {
        Some(snapshot) if status == StatusCode::OK => (StatusCode::OK, snapshot),
        _ => (
            StatusCode::NOT_FOUND,
            json!({"error": {"code": "not_found", "message": format!("{scope} is not there")}}),
        ),
    }
}

fn query_scope(scope: &str) -> String {
    url::form_urlencoded::byte_serialize(scope.as_bytes()).collect()
}

async fn json_of(response: Response<hyper::body::Incoming>) -> Value {
    serde_json::from_slice(
        &axum::body::to_bytes(Body::new(response.into_body()), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap()
}
