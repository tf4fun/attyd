use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use rust_embed::RustEmbed;
use serde_json::json;
use tokio::sync::{Mutex, Notify, mpsc};
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

use crate::bridge;
use crate::options::Options;

const BRIDGE_SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(5);

#[derive(RustEmbed)]
#[folder = "dist/client/"]
struct ClientAssets;

#[derive(Clone)]
struct AppState {
    bridge: Arc<BridgeHub>,
}

#[derive(Default)]
struct BridgeBootstrap {
    hello: Option<String>,
    initialized: Option<String>,
    error: Option<String>,
    phase: Option<String>,
    terminal_error: bool,
}

impl BridgeBootstrap {
    fn update(&mut self, event: &str) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(event) else {
            return;
        };
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("bridge/hello") => self.hello = Some(event.to_string()),
            Some("acp/initialized") => self.initialized = Some(event.to_string()),
            Some("bridge/error")
                if value.get("requestId").is_none() && value.get("operation").is_none() =>
            {
                self.error = Some(event.to_string());
            }
            Some("bridge/phase") => {
                self.phase = Some(event.to_string());
                let phase = value.get("phase").and_then(serde_json::Value::as_str);
                self.terminal_error = phase == Some("error");
                if phase == Some("ready") {
                    self.error = None;
                }
            }
            _ => {}
        }
    }

    fn events(&self) -> impl Iterator<Item = &String> {
        self.hello
            .iter()
            .chain(self.initialized.iter())
            .chain(self.error.iter().filter(|_| self.terminal_error))
            .chain(self.phase.iter())
    }
}

fn terminal_auth_succeeded(event: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(event)
        .ok()
        .is_some_and(|value| {
            value.get("type").and_then(serde_json::Value::as_str)
                == Some("bridge/auth_terminal_exited")
                && value.get("status").and_then(serde_json::Value::as_str) == Some("succeeded")
        })
}

#[derive(Default)]
struct BridgeHubState {
    generation: u64,
    input: Option<mpsc::Sender<bridge::BridgeInput>>,
    subscribers: HashMap<u64, mpsc::UnboundedSender<String>>,
    bootstrap: BridgeBootstrap,
    shutting_down: bool,
}

struct BridgeHub {
    options: Arc<Options>,
    state: Mutex<BridgeHubState>,
    next_subscriber_id: AtomicU64,
    stopped: Notify,
}

struct BridgeSubscription {
    id: u64,
    generation: u64,
    events: mpsc::UnboundedReceiver<String>,
}

struct BridgeRuntime {
    generation: u64,
    input: mpsc::Receiver<bridge::BridgeInput>,
    events: mpsc::UnboundedReceiver<String>,
    event_tx: mpsc::UnboundedSender<String>,
}

impl BridgeHub {
    fn new(options: Arc<Options>) -> Arc<Self> {
        Arc::new(Self {
            options,
            state: Mutex::new(BridgeHubState::default()),
            next_subscriber_id: AtomicU64::new(1),
            stopped: Notify::new(),
        })
    }

    async fn subscribe(self: &Arc<Self>) -> Option<BridgeSubscription> {
        let id = self.next_subscriber_id.fetch_add(1, Ordering::Relaxed);
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let mut runtime = None;
        let generation;
        {
            let mut state = self.state.lock().await;
            if state.shutting_down {
                return None;
            }
            if state.input.is_none() {
                state.generation = state.generation.wrapping_add(1).max(1);
                state.bootstrap = BridgeBootstrap::default();
                let (input_tx, input_rx) = mpsc::channel(256);
                let (bridge_event_tx, bridge_event_rx) = mpsc::unbounded_channel();
                state.input = Some(input_tx);
                runtime = Some(BridgeRuntime {
                    generation: state.generation,
                    input: input_rx,
                    events: bridge_event_rx,
                    event_tx: bridge_event_tx,
                });
            }
            generation = state.generation;
            for event in state.bootstrap.events() {
                let _ = event_tx.send(event.clone());
            }
            state.subscribers.insert(id, event_tx);
        }
        if let Some(runtime) = runtime {
            self.spawn_runtime(runtime);
        }
        Some(BridgeSubscription {
            id,
            generation,
            events: event_rx,
        })
    }

    fn spawn_runtime(self: &Arc<Self>, runtime: BridgeRuntime) {
        let BridgeRuntime {
            generation,
            input,
            mut events,
            event_tx,
        } = runtime;
        let options = self.options.clone();
        tokio::spawn(async move {
            bridge::run(options, input, event_tx).await;
        });

        let hub = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let Some(hub) = hub.upgrade() else {
                    return;
                };
                hub.publish(generation, event).await;
            }
            if let Some(hub) = hub.upgrade() {
                hub.finish_generation(generation).await;
            }
        });
    }

    async fn publish(&self, generation: u64, event: String) {
        let restart = terminal_auth_succeeded(&event);
        let input = {
            let mut state = self.state.lock().await;
            if state.generation != generation || state.input.is_none() {
                return;
            }
            state.bootstrap.update(&event);
            for subscriber in state.subscribers.values() {
                let _ = subscriber.send(event.clone());
            }
            restart.then(|| state.input.clone()).flatten()
        };
        if let Some(input) = input {
            let _ = input.send(bridge::BridgeInput::Restart).await;
        }
    }

    async fn finish_generation(&self, generation: u64) {
        let subscribers = {
            let mut state = self.state.lock().await;
            if state.generation != generation {
                return;
            }
            state.input = None;
            std::mem::take(&mut state.subscribers)
        };
        drop(subscribers);
        self.stopped.notify_waiters();
    }

    async fn send_command(
        &self,
        subscriber_id: u64,
        generation: u64,
        command: String,
    ) -> Result<(), ()> {
        let input = {
            let state = self.state.lock().await;
            if state.generation != generation || !state.subscribers.contains_key(&subscriber_id) {
                return Err(());
            }
            state.input.clone().ok_or(())?
        };
        input
            .send(bridge::BridgeInput::Command(command))
            .await
            .map_err(|_| ())
    }

    async fn send_to_subscriber(&self, subscriber_id: u64, generation: u64, event: String) {
        let state = self.state.lock().await;
        if state.generation == generation
            && let Some(sender) = state.subscribers.get(&subscriber_id)
        {
            let _ = sender.send(event);
        }
    }

    async fn unsubscribe(&self, subscriber_id: u64, generation: u64) {
        let input = {
            let mut state = self.state.lock().await;
            if state.generation != generation
                || state.subscribers.remove(&subscriber_id).is_none()
                || !state.subscribers.is_empty()
            {
                return;
            }
            state.input.clone()
        };
        if let Some(input) = input {
            let _ = input.send(bridge::BridgeInput::SubscribersGone).await;
        }
    }

    async fn shutdown(&self) {
        let input = {
            let mut state = self.state.lock().await;
            state.shutting_down = true;
            state.input.clone()
        };
        let Some(input) = input else {
            return;
        };
        let _ = input.send(bridge::BridgeInput::Shutdown).await;
        let wait = async {
            loop {
                let stopped = self.stopped.notified();
                if self.state.lock().await.input.is_none() {
                    return;
                }
                stopped.await;
            }
        };
        let _ = tokio::time::timeout(BRIDGE_SHUTDOWN_GRACE_PERIOD, wait).await;
    }
}

pub async fn serve(options: Options) -> Result<()> {
    let options = Arc::new(options.normalized().map_err(anyhow::Error::msg)?);
    if options.transport == crate::options::Transport::Stdio {
        std::env::set_current_dir(&options.cwd).with_context(|| {
            format!("failed to enter Agent workspace {}", options.cwd.display())
        })?;
    }
    let address = SocketAddr::new(options.host, options.port);
    let bridge = BridgeHub::new(options.clone());
    let app = Router::new()
        .route("/api/health", get(health))
        .route("/ws", get(websocket))
        .fallback(get(static_asset))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(AppState {
            bridge: bridge.clone(),
        });
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to bind http://{address}"))?;
    let actual = listener.local_addr()?;

    println!("attyd listening on http://{actual}");
    println!(
        "agent ({}): {}",
        options.transport.as_str(),
        options.command.join(" ")
    );
    if options.transport == crate::options::Transport::Stdio {
        println!("default Agent workspace: {}", options.cwd.display());
    } else {
        println!("Agent workspace: selected per new thread in the web UI");
    }

    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("HTTP server failed");
    bridge.shutdown().await;
    result
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let _ = &state.bridge;
    axum::Json(json!({ "ok": true, "protocol": "acp/v1", "backend": "rust" }))
}

async fn websocket(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.max_message_size(bridge::MAX_BRIDGE_MESSAGE_BYTES)
        .on_upgrade(move |socket| serve_websocket(socket, state.bridge))
}

async fn serve_websocket(socket: WebSocket, bridge: Arc<BridgeHub>) {
    let Some(subscription) = bridge.subscribe().await else {
        return;
    };
    let BridgeSubscription {
        id,
        generation,
        mut events,
    } = subscription;
    let (mut writer, mut reader) = socket.split();
    tokio::select! {
        _ = async {
            while let Some(message) = reader.next().await {
                match message {
                    Ok(Message::Text(text)) => {
                        if bridge
                            .send_command(id, generation, text.to_string())
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(Message::Binary(_)) => {
                        bridge.send_to_subscriber(
                            id,
                            generation,
                            json!({
                                "type": "bridge/error",
                                "message": "WebSocket commands must use text frames",
                            }).to_string(),
                        ).await;
                    }
                    Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                }
            }
        } => {},
        _ = async {
            while let Some(event) = events.recv().await {
                if writer.send(Message::Text(event.into())).await.is_err() {
                    break;
                }
            }
        } => {},
    }
    bridge.unsubscribe(id, generation).await;
}

async fn static_asset(uri: Uri) -> Response {
    let requested = uri.path().trim_start_matches('/');
    let path = if requested.is_empty() {
        "index.html"
    } else {
        requested
    };
    let (asset, served_path) = match ClientAssets::get(path) {
        Some(asset) => (Some(asset), path),
        None => (ClientAssets::get("index.html"), "index.html"),
    };
    let Some(asset) = asset else {
        return (StatusCode::NOT_FOUND, "Not found").into_response();
    };
    let content_type = mime_guess::from_path(served_path).first_or_octet_stream();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type.as_ref())
        .body(Body::from(asset.data))
        .expect("valid embedded asset response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_replays_connection_state_without_transient_command_errors() {
        let mut bootstrap = BridgeBootstrap::default();
        bootstrap.update(r#"{"type":"bridge/hello"}"#);
        bootstrap.update(r#"{"type":"acp/initialized","response":{}}"#);
        bootstrap.update(r#"{"type":"bridge/phase","phase":"ready"}"#);
        bootstrap.update(r#"{"type":"bridge/error","message":"bad command"}"#);
        let events = bootstrap.events().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|event| !event.contains("bad command")));

        bootstrap.update(r#"{"type":"bridge/error","message":"Agent exited"}"#);
        bootstrap.update(r#"{"type":"bridge/phase","phase":"error"}"#);
        let events = bootstrap.events().map(String::as_str).collect::<Vec<_>>();
        assert!(events.iter().any(|event| event.contains("Agent exited")));
    }

    #[test]
    fn restarts_only_after_successful_terminal_authentication() {
        assert!(terminal_auth_succeeded(
            r#"{"type":"bridge/auth_terminal_exited","status":"succeeded"}"#
        ));
        assert!(!terminal_auth_succeeded(
            r#"{"type":"bridge/auth_terminal_exited","status":"cancelled"}"#
        ));
        assert!(!terminal_auth_succeeded(
            r#"{"type":"acp/authenticated","status":"succeeded"}"#
        ));
    }
}
