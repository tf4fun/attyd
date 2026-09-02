use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

use crate::bridge;
use crate::event_queue::{self, EventQueueLimits, EventReceiver, EventSender};
use crate::options::Options;
use crate::runtime_cache::ActiveRuntimeProjection;
use crate::runtime_state::{RuntimeChange, RuntimeDelta, RuntimeSnapshot, fold_active_turn_update};

const BRIDGE_SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(5);
const SUBSCRIBER_QUEUE_CAPACITY: usize = 64;
const SUBSCRIBER_QUEUE_BYTE_CAPACITY: usize = 8 * 1024 * 1024;
const MAX_SUBSCRIBERS: usize = 64;
const BRIDGE_EVENT_QUEUE_CAPACITY: usize = 256;
const BRIDGE_EVENT_QUEUE_BYTE_CAPACITY: usize = 16 * 1024 * 1024;

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

    fn hello_event(&self) -> Option<&String> {
        self.hello.as_ref()
    }

    fn events_after_runtime(&self) -> impl Iterator<Item = &String> {
        self.initialized
            .iter()
            .chain(self.error.iter().filter(|_| self.terminal_error))
            .chain(self.phase.iter())
    }

    #[cfg(test)]
    fn events(&self) -> impl Iterator<Item = &String> {
        self.hello.iter().chain(self.events_after_runtime())
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

fn opened_runtime_session_id(event: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(event).ok()?;
    match value.get("type").and_then(serde_json::Value::as_str)? {
        "acp/session_created" | "acp/session_forked" => value
            .get("response")?
            .get("sessionId")?
            .as_str()
            .map(str::to_string),
        "acp/session_attached" => value.get("sessionId")?.as_str().map(str::to_string),
        _ => None,
    }
}

fn is_internal_direct_event(event: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(event)
        .ok()
        .and_then(|value| value.get("type").cloned())
        .and_then(|value| value.as_str().map(str::to_string))
        .as_deref()
        == Some("bridge/internal_direct")
}

fn is_internal_runtime_event(event: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(event)
        .ok()
        .and_then(|value| value.get("type").cloned())
        .and_then(|value| value.as_str().map(str::to_string))
        .is_some_and(|kind| {
            matches!(
                kind.as_str(),
                "bridge/internal_runtime_snapshot" | "bridge/internal_runtime_delta"
            )
        })
}

fn directed_bridge_event(event: &str) -> Option<(u64, String)> {
    let value = serde_json::from_str::<serde_json::Value>(event).ok()?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("bridge/internal_direct") {
        return None;
    }
    let subscriber_id = value.get("subscriberId")?.as_u64()?;
    let event = value.get("event")?;
    Some((subscriber_id, event.to_string()))
}

#[derive(Default)]
struct CanonicalProjection {
    snapshot: Option<RuntimeSnapshot>,
}

impl CanonicalProjection {
    fn update_with_public_event(&mut self, event: &str) -> (bool, Option<String>) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(event) else {
            return (false, None);
        };
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("bridge/internal_runtime_snapshot") => {
                let mut snapshot = value
                    .get("value")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<RuntimeSnapshot>(value).ok());
                if let Some(snapshot) = snapshot.as_mut() {
                    retire_released_terminals(snapshot);
                }
                self.snapshot = snapshot.clone();
                (
                    true,
                    snapshot.map(|snapshot| {
                        json!({
                            "type": "bridge/runtime_snapshot",
                            "snapshot": snapshot,
                        })
                        .to_string()
                    }),
                )
            }
            Some("bridge/internal_runtime_delta") => {
                let delta = value
                    .get("value")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<RuntimeDelta>(value).ok());
                let Some(delta) = delta else {
                    self.snapshot = None;
                    return (true, None);
                };
                if !self.apply_delta(delta.clone()) {
                    self.snapshot = None;
                    return (true, None);
                }
                let public_event = Some(
                    json!({
                        "type": "bridge/runtime_delta",
                        "delta": delta,
                    })
                    .to_string(),
                );
                if let Some(snapshot) = self.snapshot.as_mut() {
                    retire_released_terminals(snapshot);
                }
                (true, public_event)
            }
            _ => (false, None),
        }
    }

    fn apply_delta(&mut self, delta: RuntimeDelta) -> bool {
        let Some(snapshot) = self.snapshot.as_mut() else {
            return false;
        };
        if delta.epoch != snapshot.epoch || delta.seq != snapshot.through_seq.saturating_add(1) {
            return false;
        }
        match delta.change {
            RuntimeChange::ConnectionUpsert {
                request_elicitations,
                request_url_flows,
            } => {
                let Some(revision) = delta.scope_revision else {
                    return false;
                };
                if revision != snapshot.connection_revision.saturating_add(1) {
                    return false;
                }
                snapshot.connection_revision = revision;
                snapshot.request_elicitations = request_elicitations;
                snapshot.request_url_flows = request_url_flows;
            }
            RuntimeChange::SessionUpsert { session } => {
                if delta.scope_revision != Some(session.revision) {
                    return false;
                }
                match snapshot.sessions.get(&session.session_id) {
                    Some(current) if current.incarnation == session.incarnation => {
                        if session.revision != current.revision.saturating_add(1) {
                            return false;
                        }
                    }
                    Some(current)
                        if current.lifecycle == crate::runtime_state::SessionLifecycle::Closed
                            && session.incarnation != current.incarnation
                            && session.revision == 1 => {}
                    Some(_) => return false,
                    None if session.revision == 1 => {}
                    None => return false,
                }
                snapshot
                    .sessions
                    .insert(session.session_id.clone(), *session);
            }
            RuntimeChange::TurnUpdateAppended {
                session_id,
                incarnation,
                revision,
                operation_id,
                update,
            } => {
                if delta.scope_revision != Some(revision) {
                    return false;
                }
                let Some(session) = snapshot.sessions.get_mut(&session_id) else {
                    return false;
                };
                if session.incarnation != incarnation
                    || revision != session.revision.saturating_add(1)
                {
                    return false;
                }
                let Some(turn) = session.active_turn.as_mut() else {
                    return false;
                };
                if turn.operation_id != operation_id {
                    return false;
                }
                let Ok(folded) = fold_active_turn_update(&turn.updates, &update) else {
                    return false;
                };
                turn.updates = folded;
                session.revision = revision;
            }
            RuntimeChange::SessionRemoved {
                session_id,
                incarnation,
            } => {
                if snapshot
                    .sessions
                    .get(&session_id)
                    .is_some_and(|session| session.incarnation != incarnation)
                {
                    return false;
                }
                snapshot.sessions.remove(&session_id);
            }
        }
        snapshot.through_seq = delta.seq;
        true
    }
}

fn retire_released_terminals(snapshot: &mut RuntimeSnapshot) {
    for session in snapshot.sessions.values_mut() {
        session.terminals.retain(|_, terminal| {
            !terminal
                .get("released")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        });
    }
}

#[derive(Default)]
struct BridgeHubState {
    generation: u64,
    input: Option<mpsc::Sender<bridge::BridgeInput>>,
    cancellation: Option<CancellationToken>,
    subscribers: HashMap<u64, SubscriberSender>,
    bootstrap: BridgeBootstrap,
    runtime: ActiveRuntimeProjection,
    canonical: CanonicalProjection,
    canonical_resync_pending: bool,
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
    initial_events: Vec<String>,
    events: mpsc::Receiver<QueuedSubscriberEvent>,
}

#[derive(Clone)]
struct SubscriberSender {
    tx: mpsc::Sender<QueuedSubscriberEvent>,
    queued_bytes: Arc<AtomicUsize>,
}

struct QueuedSubscriberEvent {
    event: String,
    queued_bytes: Arc<AtomicUsize>,
    bytes: usize,
}

struct SubscriberByteLease {
    queued_bytes: Arc<AtomicUsize>,
    bytes: usize,
}

impl QueuedSubscriberEvent {
    fn into_inflight(mut self) -> (String, SubscriberByteLease) {
        let event = std::mem::take(&mut self.event);
        let bytes = std::mem::take(&mut self.bytes);
        (
            event,
            SubscriberByteLease {
                queued_bytes: self.queued_bytes.clone(),
                bytes,
            },
        )
    }
}

impl Drop for QueuedSubscriberEvent {
    fn drop(&mut self) {
        self.queued_bytes.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

impl Drop for SubscriberByteLease {
    fn drop(&mut self) {
        self.queued_bytes.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

impl SubscriberSender {
    fn channel(capacity: usize) -> (Self, mpsc::Receiver<QueuedSubscriberEvent>) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            Self {
                tx,
                queued_bytes: Arc::new(AtomicUsize::new(0)),
            },
            rx,
        )
    }

    fn try_send(&self, event: String) -> Result<(), ()> {
        let bytes = event.len();
        self.queued_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current
                    .checked_add(bytes)
                    .filter(|next| *next <= SUBSCRIBER_QUEUE_BYTE_CAPACITY)
            })
            .map_err(|_| ())?;
        self.tx
            .try_send(QueuedSubscriberEvent {
                event,
                queued_bytes: self.queued_bytes.clone(),
                bytes,
            })
            .map_err(|_| ())
    }
}

struct BridgeRuntime {
    generation: u64,
    input: mpsc::Receiver<bridge::BridgeInput>,
    events: EventReceiver,
    event_tx: EventSender,
    cancellation: CancellationToken,
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

    async fn ensure_runtime(self: &Arc<Self>) {
        let runtime = {
            let mut state = self.state.lock().await;
            if state.shutting_down || state.input.is_some() {
                None
            } else {
                state.generation = state.generation.wrapping_add(1).max(1);
                state.bootstrap = BridgeBootstrap::default();
                state.runtime = ActiveRuntimeProjection::default();
                state.canonical = CanonicalProjection::default();
                state.canonical_resync_pending = false;
                let (input_tx, input) = mpsc::channel(256);
                let cancellation = CancellationToken::new();
                let (event_tx, events) = event_queue::channel(
                    EventQueueLimits {
                        max_items: BRIDGE_EVENT_QUEUE_CAPACITY,
                        max_bytes: BRIDGE_EVENT_QUEUE_BYTE_CAPACITY,
                    },
                    cancellation.clone(),
                );
                state.input = Some(input_tx);
                state.cancellation = Some(cancellation.clone());
                Some(BridgeRuntime {
                    generation: state.generation,
                    input,
                    events,
                    event_tx,
                    cancellation,
                })
            }
        };
        if let Some(runtime) = runtime {
            self.spawn_runtime(runtime);
        }
    }

    async fn subscribe(self: &Arc<Self>) -> Option<BridgeSubscription> {
        self.ensure_runtime().await;
        let id = self.next_subscriber_id.fetch_add(1, Ordering::Relaxed);
        let (event_tx, event_rx) = SubscriberSender::channel(SUBSCRIBER_QUEUE_CAPACITY);
        let (generation, initial_events) = {
            let mut state = self.state.lock().await;
            if state.shutting_down || state.input.is_none() {
                return None;
            }
            if state.subscribers.len() >= MAX_SUBSCRIBERS {
                return None;
            }
            let mut initial_events = Vec::new();
            if let Some(event) = state.bootstrap.hello_event() {
                initial_events.push(event.clone());
            }
            if let Some(snapshot) = &state.canonical.snapshot {
                initial_events.push(
                    json!({
                        "type": "bridge/runtime_snapshot",
                        "snapshot": snapshot,
                    })
                    .to_string(),
                );
            }
            initial_events.extend(state.runtime.replay_events());
            initial_events.extend(state.bootstrap.events_after_runtime().cloned());
            state.subscribers.insert(id, event_tx);
            (state.generation, initial_events)
        };
        Some(BridgeSubscription {
            id,
            generation,
            initial_events,
            events: event_rx,
        })
    }

    fn spawn_runtime(self: &Arc<Self>, runtime: BridgeRuntime) {
        let BridgeRuntime {
            generation,
            input,
            events,
            event_tx,
            cancellation,
        } = runtime;
        let options = self.options.clone();
        let (runtime_done_tx, runtime_done_rx) = oneshot::channel();
        tokio::spawn(async move {
            bridge::run_with_cancellation(options, input, event_tx, cancellation).await;
            let _ = runtime_done_tx.send(());
        });

        let hub = Arc::downgrade(self);
        tokio::spawn(forward_generation_events(
            hub,
            generation,
            events,
            runtime_done_rx,
        ));
    }

    async fn publish(&self, generation: u64, event: String) {
        if is_internal_runtime_event(&event) {
            let snapshot_request = {
                let mut state = self.state.lock().await;
                if state.generation != generation || state.input.is_none() {
                    return;
                }
                let (recognized, public_event) = state.canonical.update_with_public_event(&event);
                if state.canonical.snapshot.is_some() {
                    state.canonical_resync_pending = false;
                }
                if let Some(public_event) = public_event {
                    let mut failed_subscribers = Vec::new();
                    for (&subscriber_id, subscriber) in &state.subscribers {
                        if subscriber.try_send(public_event.clone()).is_err() {
                            failed_subscribers.push(subscriber_id);
                        }
                    }
                    for subscriber_id in failed_subscribers {
                        state.subscribers.remove(&subscriber_id);
                    }
                }
                if recognized
                    && state.canonical.snapshot.is_none()
                    && !state.canonical_resync_pending
                {
                    state.canonical_resync_pending = true;
                    state.input.clone()
                } else {
                    None
                }
            };
            if let Some(input) = snapshot_request {
                let _ = input
                    .send(bridge::BridgeInput::RuntimeSnapshotRequest)
                    .await;
            }
            return;
        }
        if is_internal_direct_event(&event) {
            if let Some((subscriber_id, event)) = directed_bridge_event(&event) {
                {
                    let mut state = self.state.lock().await;
                    if state.generation != generation || state.input.is_none() {
                        return;
                    }
                    state.runtime.update(&event);
                }
                self.send_to_subscriber(subscriber_id, generation, event)
                    .await;
            }
            return;
        }
        let restart = terminal_auth_succeeded(&event);
        let input = {
            let mut state = self.state.lock().await;
            if state.generation != generation || state.input.is_none() {
                return;
            }
            state.bootstrap.update(&event);
            let event = state.runtime.update_and_normalize(&event);
            let session_live_suffix = opened_runtime_session_id(&event)
                .map(|session_id| state.runtime.replay_session_live_suffix(&session_id))
                .unwrap_or_default();
            let mut failed_subscribers = Vec::new();
            for (&subscriber_id, subscriber) in &state.subscribers {
                let mut delivered = subscriber.try_send(event.clone()).is_ok();
                for live_event in &session_live_suffix {
                    delivered &= subscriber.try_send(live_event.clone()).is_ok();
                }
                if !delivered {
                    failed_subscribers.push(subscriber_id);
                }
            }
            for subscriber_id in failed_subscribers {
                state.subscribers.remove(&subscriber_id);
            }
            restart.then(|| state.cancellation.clone()).flatten()
        };
        if let Some(cancellation) = input {
            cancellation.cancel();
        }
    }

    async fn finish_generation(&self, generation: u64) {
        let subscribers = {
            let mut state = self.state.lock().await;
            if state.generation != generation {
                return;
            }
            state.input = None;
            state.cancellation = None;
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
            .send(bridge::BridgeInput::Command {
                subscriber_id,
                raw: command,
            })
            .await
            .map_err(|_| ())
    }

    async fn send_to_subscriber(&self, subscriber_id: u64, generation: u64, event: String) {
        let mut state = self.state.lock().await;
        if state.generation == generation
            && let Some(sender) = state.subscribers.get(&subscriber_id)
            && sender.try_send(event).is_err()
        {
            state.subscribers.remove(&subscriber_id);
        }
    }

    async fn unsubscribe(&self, subscriber_id: u64, generation: u64) {
        let mut state = self.state.lock().await;
        if state.generation == generation {
            state.subscribers.remove(&subscriber_id);
        }
    }

    async fn shutdown(&self) {
        let cancellation = {
            let mut state = self.state.lock().await;
            state.shutting_down = true;
            state.cancellation.clone()
        };
        let Some(cancellation) = cancellation else {
            return;
        };
        cancellation.cancel();
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

async fn forward_generation_events(
    hub: std::sync::Weak<BridgeHub>,
    generation: u64,
    mut events: EventReceiver,
    mut runtime_done: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            biased;
            event = events.recv() => {
                let Some(event) = event else {
                    break;
                };
                let (event, _lease) = event.into_parts();
                let Some(hub) = hub.upgrade() else {
                    return;
                };
                hub.publish(generation, event).await;
            }
            _ = &mut runtime_done => {
                // run_with_cancellation emits its terminal phase before it
                // returns. Drain everything already queued, then ignore any
                // leaked/late sender from this completed generation.
                while let Ok(event) = events.try_recv() {
                    let (event, _lease) = event.into_parts();
                    let Some(hub) = hub.upgrade() else {
                        return;
                    };
                    hub.publish(generation, event).await;
                }
                break;
            }
        }
    }
    if let Some(hub) = hub.upgrade() {
        hub.finish_generation(generation).await;
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
    bridge.ensure_runtime().await;
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
        initial_events,
        mut events,
    } = subscription;
    let (mut writer, mut reader) = socket.split();
    for event in initial_events {
        if writer.send(Message::Text(event.into())).await.is_err() {
            bridge.unsubscribe(id, generation).await;
            return;
        }
    }
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
                let (event, lease) = event.into_inflight();
                let result = writer.send(Message::Text(event.into())).await;
                drop(lease);
                if result.is_err() {
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
    use crate::runtime_state::{RuntimeLimits, RuntimeState};
    use clap::Parser;

    impl CanonicalProjection {
        fn update(&mut self, event: &str) -> bool {
            self.update_with_public_event(event).0
        }
    }

    impl QueuedSubscriberEvent {
        fn into_string(mut self) -> String {
            std::mem::take(&mut self.event)
        }
    }

    fn test_hub() -> Arc<BridgeHub> {
        let options = Options::try_parse_from(["attyd", "--", "fake-agent"]).unwrap();
        BridgeHub::new(Arc::new(options))
    }

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

    #[tokio::test]
    async fn dropping_legacy_and_debug_state_does_not_change_canonical_projection() {
        let hub = test_hub();
        let (input, _commands) = mpsc::channel(1);
        let mut runtime = RuntimeState::new("epoch", RuntimeLimits::default());
        runtime
            .open_new(
                "epoch",
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        let canonical = runtime.snapshot();
        {
            let mut state = hub.state.lock().await;
            state.generation = 1;
            state.input = Some(input);
            state.canonical.snapshot = Some(canonical.clone());
        }
        hub.publish(
            1,
            json!({
                "type": "acp/session_created",
                "cwd": "/legacy",
                "response": { "sessionId": "legacy-session" },
            })
            .to_string(),
        )
        .await;

        let mut state = hub.state.lock().await;
        assert!(!state.runtime.replay_events().is_empty());
        assert_eq!(state.canonical.snapshot, Some(canonical.clone()));
        state.runtime = ActiveRuntimeProjection::default();
        state.bootstrap = BridgeBootstrap::default();
        assert_eq!(
            state.canonical.snapshot,
            Some(canonical),
            "raw/legacy retention is not a materialized business-state dependency"
        );
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

    #[test]
    fn identifies_session_lifecycle_events_that_may_have_a_live_suffix() {
        assert_eq!(
            opened_runtime_session_id(
                r#"{"type":"acp/session_created","response":{"sessionId":"created"}}"#
            ),
            Some("created".to_string())
        );
        assert_eq!(
            opened_runtime_session_id(
                r#"{"type":"acp/session_attached","sessionId":"attached","response":{}}"#
            ),
            Some("attached".to_string())
        );
        assert_eq!(
            opened_runtime_session_id(r#"{"type":"acp/prompt_complete","sessionId":"done"}"#),
            None
        );
    }

    #[tokio::test]
    async fn live_session_open_is_not_followed_by_a_redundant_runtime_snapshot() {
        let hub = test_hub();
        let (input, _commands) = mpsc::channel(1);
        let (subscriber_tx, mut subscriber_rx) = SubscriberSender::channel(4);
        {
            let mut state = hub.state.lock().await;
            state.generation = 1;
            state.input = Some(input);
            state.subscribers.insert(1, subscriber_tx);
        }

        hub.publish(
            1,
            json!({
                "type": "acp/session_forked",
                "requestId": "fork",
                "sourceSessionId": "source",
                "cwd": "/workspace",
                "response": { "sessionId": "fork" },
                "earlyUpdates": [{
                    "sessionId": "fork",
                    "update": {
                        "sessionUpdate": "available_commands_update",
                        "availableCommands": [{ "name": "inspect", "description": "Inspect" }],
                    },
                }],
            })
            .to_string(),
        )
        .await;

        let lifecycle: serde_json::Value =
            serde_json::from_str(&subscriber_rx.try_recv().unwrap().into_string()).unwrap();
        assert_eq!(lifecycle["type"], "acp/session_forked");
        assert!(
            std::iter::from_fn(|| subscriber_rx.try_recv().ok())
                .map(QueuedSubscriberEvent::into_string)
                .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event).ok())
                .all(|event| event["type"] != "bridge/runtime_session"),
            "the lifecycle response already carries the session and its atomic early updates"
        );
    }

    #[test]
    fn internal_direct_event_is_unwrapped_without_becoming_public_state() {
        let event = r#"{"type":"bridge/internal_direct","subscriberId":42,"event":{"type":"bridge/error","requestId":"request","message":"collision"}}"#;

        let (subscriber_id, direct) = directed_bridge_event(event).unwrap();
        assert_eq!(subscriber_id, 42);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&direct).unwrap(),
            json!({
                "type": "bridge/error",
                "requestId": "request",
                "message": "collision",
            }),
        );
        assert!(directed_bridge_event(r#"{"type":"bridge/phase","phase":"ready"}"#).is_none());
    }

    #[tokio::test]
    async fn directed_event_reaches_only_the_requesting_subscriber() {
        let hub = test_hub();
        let (input, _commands) = mpsc::channel(1);
        let (target_tx, mut target_rx) = SubscriberSender::channel(4);
        let (other_tx, mut other_rx) = SubscriberSender::channel(4);
        {
            let mut state = hub.state.lock().await;
            state.generation = 1;
            state.input = Some(input);
            state.subscribers.insert(42, target_tx);
            state.subscribers.insert(43, other_tx);
        }

        hub.publish(
            1,
            r#"{"type":"bridge/internal_direct","subscriberId":42,"event":{"type":"bridge/error","message":"collision"}}"#.to_string(),
        )
        .await;

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                &target_rx.try_recv().unwrap().into_string(),
            )
            .unwrap(),
            json!({ "type": "bridge/error", "message": "collision" }),
        );
        assert!(other_rx.try_recv().is_err());
        assert!(
            hub.state
                .lock()
                .await
                .runtime
                .replay_events()
                .iter()
                .all(|event| !event.contains("collision"))
        );
    }

    #[tokio::test]
    async fn load_replacement_is_private_and_never_becomes_completed_replay() {
        let hub = test_hub();
        let (input, _commands) = mpsc::channel(1);
        let (target_tx, mut target_rx) = SubscriberSender::channel(8);
        let (other_tx, mut other_rx) = SubscriberSender::channel(8);
        {
            let mut state = hub.state.lock().await;
            state.generation = 1;
            state.input = Some(input);
            state.subscribers.insert(42, target_tx);
            state.subscribers.insert(43, other_tx);
        }

        for event in [
            json!({
                "type": "acp/session_update",
                "notification": {
                    "sessionId": "saved",
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "messageId": "loaded",
                        "content": { "type": "text", "text": "Agent history" },
                    },
                },
            }),
            json!({
                "type": "acp/session_attached",
                "requestId": "load",
                "method": "load",
                "sessionId": "saved",
                "cwd": "/workspace",
                "response": { "sessionId": "saved" },
            }),
        ] {
            hub.publish(
                1,
                json!({
                    "type": "bridge/internal_direct",
                    "subscriberId": 42,
                    "event": event,
                })
                .to_string(),
            )
            .await;
        }

        let delivered = std::iter::from_fn(|| target_rx.try_recv().ok())
            .map(QueuedSubscriberEvent::into_string)
            .collect::<Vec<_>>();
        assert_eq!(delivered.len(), 2);
        assert!(delivered[0].contains("Agent history"));
        assert!(delivered[1].contains("acp/session_attached"));
        assert!(
            other_rx.try_recv().is_err(),
            "load staging must be private to the browser that requested replacement"
        );

        let replay = hub.state.lock().await.runtime.replay_events().join("\n");
        assert!(!replay.contains("Agent history"));
        assert!(!replay.contains("acp/session_attached"));
        assert!(!replay.contains("bridge/runtime_session"));
    }

    #[tokio::test]
    async fn real_same_session_load_uses_tracked_cwd_and_isolates_other_subscribers() {
        async fn next_event(subscription: &mut BridgeSubscription) -> serde_json::Value {
            let queued = tokio::time::timeout(Duration::from_secs(10), subscription.events.recv())
                .await
                .expect("timed out waiting for bridge event")
                .expect("bridge subscriber closed unexpectedly");
            serde_json::from_str(&queued.into_string()).expect("bridge event must be JSON")
        }

        let cwd = env!("CARGO_MANIFEST_DIR");
        let fixture = std::path::Path::new(cwd).join("tests/fixtures/fake-agent.ts");
        let fixture = fixture.to_string_lossy().into_owned();
        let options = Options::try_parse_from([
            "attyd", "--cwd", cwd, "--", "node", "--import", "tsx", &fixture,
        ])
        .unwrap()
        .normalized()
        .unwrap();
        let hub = BridgeHub::new(Arc::new(options));
        let mut requester = hub.subscribe().await.expect("requester subscription");

        loop {
            let event = next_event(&mut requester).await;
            if event["type"] == "bridge/phase" && event["phase"] == "ready" {
                break;
            }
        }
        let mut observer = hub.subscribe().await.expect("observer subscription");
        observer.initial_events.clear();

        hub.send_command(
            requester.id,
            requester.generation,
            json!({
                "type": "session/new",
                "requestId": "new",
                "cwd": cwd,
            })
            .to_string(),
        )
        .await
        .unwrap();
        loop {
            let event = next_event(&mut requester).await;
            if event["type"] == "acp/session_created" {
                assert_eq!(event["response"]["sessionId"], "test-session");
                break;
            }
        }
        loop {
            if next_event(&mut observer).await["type"] == "acp/session_created" {
                break;
            }
        }

        // No session/list precedes this request. The bridge must use the cwd
        // already associated with the tracked session and execute a reload,
        // not allocate a second business session.
        hub.send_command(
            requester.id,
            requester.generation,
            json!({
                "type": "session/load",
                "requestId": "reload",
                "sessionId": "test-session",
            })
            .to_string(),
        )
        .await
        .unwrap();

        let mut requester_events = Vec::new();
        loop {
            let event = next_event(&mut requester).await;
            let complete =
                event["type"] == "acp/session_attached" && event["requestId"] == "reload";
            requester_events.push(event);
            if complete {
                break;
            }
        }
        assert!(requester_events.iter().any(|event| {
            event["type"] == "acp/session_update"
                && event["notification"]["update"]["content"]["text"] == "Loaded history."
        }));
        let attached = requester_events.last().unwrap();
        assert_eq!(attached["cwd"], cwd);
        assert_eq!(attached["sessionId"], "test-session");
        assert_eq!(
            attached["response"]["_meta"]["observedSessionCloses"],
            json!([])
        );

        let observer_events = std::iter::from_fn(|| observer.events.try_recv().ok())
            .map(QueuedSubscriberEvent::into_string)
            .collect::<Vec<_>>();
        assert!(
            observer_events.iter().all(|event| {
                !event.contains("Loaded history.")
                    && !event.contains("acp/session_attached")
                    && !event.contains("acp/session_update")
            }),
            "load replacement leaked to observer: {observer_events:?}"
        );

        hub.send_command(
            requester.id,
            requester.generation,
            json!({
                "type": "session/prompt",
                "requestId": "after-reload",
                "sessionId": "test-session",
                "prompt": [{ "type": "text", "text": "message-actions-flow" }],
            })
            .to_string(),
        )
        .await
        .unwrap();
        loop {
            let event = next_event(&mut requester).await;
            if event["type"] == "acp/prompt_complete" && event["requestId"] == "after-reload" {
                break;
            }
        }

        hub.shutdown().await;
    }

    #[tokio::test]
    async fn bridge_completion_finishes_generation_with_a_leaked_event_sender() {
        let hub = test_hub();
        let (input, _commands) = mpsc::channel(1);
        {
            let mut state = hub.state.lock().await;
            state.generation = 1;
            state.input = Some(input);
        }
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let leaked_sender = event_tx.clone();
        let (done_tx, done_rx) = oneshot::channel();
        let forwarder = tokio::spawn(forward_generation_events(
            Arc::downgrade(&hub),
            1,
            event_rx.into(),
            done_rx,
        ));
        event_tx
            .send(r#"{"type":"bridge/phase","phase":"stopped"}"#.to_string())
            .unwrap();
        done_tx.send(()).unwrap();

        tokio::time::timeout(Duration::from_millis(100), forwarder)
            .await
            .expect("generation completion still depended on event channel EOF")
            .unwrap();
        let state = hub.state.lock().await;
        assert!(state.input.is_none());
        assert_eq!(
            state
                .bootstrap
                .phase
                .as_deref()
                .and_then(|event| serde_json::from_str::<serde_json::Value>(event).ok())
                .and_then(|event| event["phase"].as_str().map(str::to_string))
                .as_deref(),
            Some("stopped"),
            "terminal events queued before runtime completion must be drained first"
        );
        drop(state);
        assert!(
            leaked_sender
                .send(r#"{"type":"bridge/phase","phase":"ready"}"#.to_string())
                .is_err(),
            "late senders from a completed generation must be disconnected"
        );
    }

    #[tokio::test]
    async fn old_generation_commands_and_direct_events_are_rejected() {
        let hub = test_hub();
        let (input, mut commands) = mpsc::channel(1);
        let (subscriber, mut events) = SubscriberSender::channel(1);
        {
            let mut state = hub.state.lock().await;
            state.generation = 2;
            state.input = Some(input);
            state.subscribers.insert(42, subscriber);
        }

        assert!(
            hub.send_command(42, 1, r#"{"type":"bridge/ping"}"#.to_string())
                .await
                .is_err()
        );
        assert!(commands.try_recv().is_err());
        hub.send_to_subscriber(42, 1, "stale".to_string()).await;
        assert!(events.try_recv().is_err());
        assert!(hub.state.lock().await.subscribers.contains_key(&42));
    }

    #[test]
    fn saturated_bridge_event_queue_cancels_the_generation() {
        let cancellation = CancellationToken::new();
        let (events, _receiver) = event_queue::channel(
            EventQueueLimits {
                max_items: 1,
                max_bytes: 64,
            },
            cancellation.clone(),
        );
        events.send("first".to_string()).unwrap();
        assert_eq!(
            events.send("second".to_string()),
            Err(crate::event_queue::EventSendError::Full)
        );
        assert!(cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn subscriber_count_has_a_hard_global_limit() {
        let hub = test_hub();
        let (input, _commands) = mpsc::channel(1);
        {
            let mut state = hub.state.lock().await;
            state.generation = 1;
            state.input = Some(input);
        }

        let mut subscriptions = Vec::with_capacity(MAX_SUBSCRIBERS);
        for _ in 0..MAX_SUBSCRIBERS {
            subscriptions.push(hub.subscribe().await.expect("subscriber below hard limit"));
        }
        assert!(hub.subscribe().await.is_none());

        let released = subscriptions.pop().unwrap();
        hub.unsubscribe(released.id, released.generation).await;
        assert!(hub.subscribe().await.is_some());
    }

    #[tokio::test]
    async fn slow_subscriber_is_evicted_without_blocking_a_healthy_subscriber() {
        let hub = test_hub();
        let (input, _commands) = mpsc::channel(1);
        let (slow_tx, _slow_rx) = SubscriberSender::channel(1);
        let (healthy_tx, mut healthy_rx) = SubscriberSender::channel(4);
        {
            let mut state = hub.state.lock().await;
            state.generation = 1;
            state.input = Some(input);
            state.subscribers.insert(1, slow_tx);
            state.subscribers.insert(2, healthy_tx);
        }

        hub.publish(
            1,
            r#"{"type":"bridge/phase","phase":"starting"}"#.to_string(),
        )
        .await;
        hub.publish(1, r#"{"type":"bridge/phase","phase":"ready"}"#.to_string())
            .await;

        let state = hub.state.lock().await;
        assert!(!state.subscribers.contains_key(&1));
        assert!(state.subscribers.contains_key(&2));
        drop(state);
        assert_eq!(
            healthy_rx.try_recv().unwrap().into_string(),
            r#"{"type":"bridge/phase","phase":"starting"}"#
        );
        assert_eq!(
            healthy_rx.try_recv().unwrap().into_string(),
            r#"{"type":"bridge/phase","phase":"ready"}"#
        );
    }

    #[tokio::test]
    async fn released_terminal_is_not_replayed_to_a_new_subscriber() {
        let hub = test_hub();
        let (input, _commands) = mpsc::channel(1);
        {
            let mut state = hub.state.lock().await;
            state.generation = 1;
            state.input = Some(input);
        }
        hub.publish(
            1,
            json!({
                "type": "acp/session_created",
                "cwd": "/workspace",
                "response": { "sessionId": "session" },
            })
            .to_string(),
        )
        .await;
        hub.publish(
            1,
            json!({
                "type": "acp/terminal_state",
                "terminal": {
                    "sessionId": "session",
                    "terminalId": "released-terminal",
                    "output": "release-only-output",
                    "truncated": false,
                    "released": false,
                },
            })
            .to_string(),
        )
        .await;
        hub.publish(
            1,
            json!({
                "type": "acp/terminal_state",
                "terminal": {
                    "sessionId": "session",
                    "terminalId": "released-terminal",
                    "output": "release-only-output",
                    "truncated": false,
                    "released": true,
                },
            })
            .to_string(),
        )
        .await;

        let subscription = hub.subscribe().await.unwrap();
        assert!(
            subscription.initial_events.iter().all(|event| {
                serde_json::from_str::<serde_json::Value>(event)
                    .ok()
                    .is_none_or(|event| {
                        event.get("type").and_then(serde_json::Value::as_str)
                            != Some("acp/terminal_state")
                            || event["terminal"]["terminalId"] != "released-terminal"
                    })
            }),
            "release must remove terminal runtime output instead of retaining a replay tombstone"
        );
    }

    #[tokio::test]
    async fn new_subscriber_receives_no_completed_history() {
        const COMPLETED_MARKER: &str = "completed-history-must-not-bootstrap";

        let hub = test_hub();
        let (input, _commands) = mpsc::channel(1);
        {
            let mut state = hub.state.lock().await;
            state.generation = 1;
            state.input = Some(input);
        }
        for event in [
            json!({
                "type": "acp/session_created",
                "cwd": "/workspace",
                "response": { "sessionId": "session" },
            }),
            json!({
                "type": "acp/prompt_started",
                "sessionId": "session",
                "requestId": "prompt",
            }),
            json!({
                "type": "acp/session_update",
                "notification": {
                    "sessionId": "session",
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "text": COMPLETED_MARKER,
                    },
                },
            }),
            json!({
                "type": "acp/prompt_complete",
                "sessionId": "session",
                "requestId": "prompt",
                "response": { "stopReason": "end_turn" },
            }),
        ] {
            hub.publish(1, event.to_string()).await;
        }

        let subscription = hub.subscribe().await.unwrap();
        assert!(
            subscription
                .initial_events
                .iter()
                .all(|event| !event.contains(COMPLETED_MARKER)),
            "ACP is the completed-history authority; Hub bootstrap must contain active state only"
        );
        let initial = subscription
            .initial_events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(event).ok())
            .collect::<Vec<_>>();
        assert_eq!(
            initial
                .iter()
                .find(|event| event["type"] == "bridge/runtime_replay_started")
                .unwrap()["sessionCount"],
            0
        );
        assert_eq!(
            initial
                .iter()
                .find(|event| event["type"] == "bridge/runtime_replay_complete")
                .unwrap()["sessionIds"],
            json!([])
        );
        assert!(
            initial
                .iter()
                .all(|event| event["type"] != "bridge/runtime_session"),
            "an idle completed session shell would suppress the required session/load flow"
        );
    }

    #[tokio::test]
    async fn slow_subscriber_does_not_pin_a_completed_turn_in_hub_state() {
        const COMPLETED_MARKER: &str = "slow-subscriber-completed-turn";

        let hub = test_hub();
        let (input, _commands) = mpsc::channel(1);
        let mut runtime = RuntimeState::new("epoch", RuntimeLimits::default());
        let incarnation = runtime
            .open_new(
                "epoch",
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        runtime
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        runtime
            .append_turn_update(
                "epoch",
                "session",
                incarnation,
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "answer",
                    "content": { "type": "text", "text": COMPLETED_MARKER },
                }),
            )
            .unwrap();
        let active = runtime.snapshot();
        let (slow_tx, _slow_rx) = SubscriberSender::channel(SUBSCRIBER_QUEUE_CAPACITY);
        let slow_queued_bytes = slow_tx.queued_bytes.clone();
        {
            let mut state = hub.state.lock().await;
            state.generation = 1;
            state.input = Some(input);
            state.canonical.snapshot = Some(active.clone());
            state.subscribers.insert(1, slow_tx);
        }

        runtime
            .complete_prompt(
                "epoch",
                "session",
                incarnation,
                "prompt",
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        for delta in runtime.deltas_after(active.through_seq).unwrap() {
            hub.publish(
                1,
                json!({
                    "type": "bridge/internal_runtime_delta",
                    "value": delta,
                })
                .to_string(),
            )
            .await;
        }

        assert!(
            slow_queued_bytes.load(Ordering::Acquire) > 0,
            "the subscriber must still be holding the final publication for this test"
        );
        let state = hub.state.lock().await;
        let retained = serde_json::to_string(&state.canonical.snapshot).unwrap();
        assert!(
            !retained.contains(COMPLETED_MARKER),
            "final publication must retire Hub-owned turn state without waiting for a slow subscriber"
        );
    }

    #[tokio::test]
    async fn subscriber_backlog_is_bounded_by_bytes_not_only_event_count() {
        let hub = test_hub();
        let (input, _commands) = mpsc::channel(1);
        let (subscriber_tx, _subscriber_rx) = SubscriberSender::channel(SUBSCRIBER_QUEUE_CAPACITY);
        {
            let mut state = hub.state.lock().await;
            state.generation = 1;
            state.input = Some(input);
            state.subscribers.insert(1, subscriber_tx);
        }
        let large_event = json!({
            "type": "bridge/stderr",
            "chunk": "x".repeat(SUBSCRIBER_QUEUE_BYTE_CAPACITY / 2 + 1),
        })
        .to_string();

        hub.publish(1, large_event.clone()).await;
        assert!(hub.state.lock().await.subscribers.contains_key(&1));
        hub.publish(1, large_event).await;
        assert!(
            !hub.state.lock().await.subscribers.contains_key(&1),
            "a few large events must not bypass subscriber memory bounds"
        );
    }

    #[tokio::test]
    async fn subscriber_byte_accounting_releases_on_receive_and_failed_send() {
        let (sender, mut receiver) = SubscriberSender::channel(1);
        sender.try_send("first".to_string()).unwrap();
        assert_eq!(sender.queued_bytes.load(Ordering::Acquire), 5);
        assert!(sender.try_send("overflow".to_string()).is_err());
        assert_eq!(
            sender.queued_bytes.load(Ordering::Acquire),
            5,
            "event-count rejection must roll back its byte reservation"
        );

        let (event, lease) = receiver.recv().await.unwrap().into_inflight();
        assert_eq!(event, "first");
        assert_eq!(
            sender.queued_bytes.load(Ordering::Acquire),
            5,
            "the byte lease must remain charged while WebSocket send is in flight"
        );
        drop(lease);
        assert_eq!(sender.queued_bytes.load(Ordering::Acquire), 0);
        drop(receiver);
        assert!(sender.try_send("closed".to_string()).is_err());
        assert_eq!(
            sender.queued_bytes.load(Ordering::Acquire),
            0,
            "closed-channel rejection must roll back its byte reservation"
        );
    }

    #[tokio::test]
    async fn subscriber_byte_ledgers_are_independent_and_release_on_receiver_drop() {
        let (first, first_rx) = SubscriberSender::channel(2);
        let (second, mut second_rx) = SubscriberSender::channel(2);
        first.try_send("first".to_string()).unwrap();
        second.try_send("second".to_string()).unwrap();
        assert_eq!(first.queued_bytes.load(Ordering::Acquire), 5);
        assert_eq!(second.queued_bytes.load(Ordering::Acquire), 6);

        drop(first_rx);
        assert_eq!(
            first.queued_bytes.load(Ordering::Acquire),
            0,
            "dropping a subscriber receiver must release every queued lease"
        );
        assert_eq!(
            second.queued_bytes.load(Ordering::Acquire),
            6,
            "one subscriber's teardown must not alter another subscriber's ledger"
        );
        assert_eq!(second_rx.recv().await.unwrap().into_string(), "second");
        assert_eq!(second.queued_bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn canonical_projection_folds_typed_snapshot_and_contiguous_deltas() {
        let mut runtime = RuntimeState::new("epoch", RuntimeLimits::default());
        let mut projection = CanonicalProjection::default();
        assert!(
            projection.update(
                &json!({
                    "type": "bridge/internal_runtime_snapshot",
                    "value": runtime.snapshot(),
                })
                .to_string(),
            )
        );
        let incarnation = runtime
            .open_new(
                "epoch",
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        runtime
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        runtime
            .append_turn_update(
                "epoch",
                "session",
                incarnation,
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "answer",
                    "content": { "type": "text", "text": "incre" },
                }),
            )
            .unwrap();
        runtime
            .append_turn_update(
                "epoch",
                "session",
                incarnation,
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "answer",
                    "content": { "type": "text", "text": "ment" },
                }),
            )
            .unwrap();
        let active_through = runtime.snapshot().through_seq;
        for delta in runtime.deltas_after(0).unwrap() {
            assert!(
                projection.update(
                    &json!({
                        "type": "bridge/internal_runtime_delta",
                        "value": delta,
                    })
                    .to_string(),
                )
            );
        }
        let projected_updates = &projection.snapshot.as_ref().unwrap().sessions["session"]
            .active_turn
            .as_ref()
            .unwrap()
            .updates;
        assert_eq!(projected_updates.len(), 1);
        assert_eq!(projected_updates[0]["content"]["text"], "increment");
        assert_eq!(
            projection.snapshot.as_ref().unwrap().sessions["session"],
            runtime.snapshot().sessions["session"],
            "raw delta application must retain the same folded active turn as RuntimeState"
        );
        runtime
            .complete_prompt(
                "epoch",
                "session",
                incarnation,
                "prompt",
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        for delta in runtime.deltas_after(active_through).unwrap() {
            assert!(
                projection.update(
                    &json!({
                        "type": "bridge/internal_runtime_delta",
                        "value": delta,
                    })
                    .to_string(),
                )
            );
        }

        assert_eq!(
            serde_json::to_value(projection.snapshot.unwrap()).unwrap(),
            serde_json::to_value(runtime.snapshot()).unwrap()
        );
    }

    #[test]
    fn replaying_the_same_canonical_snapshot_is_idempotent() {
        let mut runtime = RuntimeState::new("epoch", RuntimeLimits::default());
        let incarnation = runtime
            .open_new(
                "epoch",
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        runtime
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        let event = json!({
            "type": "bridge/internal_runtime_snapshot",
            "value": runtime.snapshot(),
        })
        .to_string();
        let mut projection = CanonicalProjection::default();

        assert!(projection.update(&event));
        let once = projection.snapshot.clone();
        assert!(projection.update(&event));
        assert_eq!(projection.snapshot, once);
        assert_eq!(
            projection.snapshot.as_ref().unwrap().sessions["session"]
                .active_turn
                .iter()
                .count(),
            1,
            "snapshot replay must replace state rather than duplicate live entities"
        );
    }

    #[test]
    fn stale_turn_update_operation_forces_resnapshot_without_partial_mutation() {
        let mut runtime = RuntimeState::new("epoch", RuntimeLimits::default());
        let incarnation = runtime
            .open_new(
                "epoch",
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        runtime
            .start_prompt("epoch", "session", incarnation, "current", Vec::new())
            .unwrap();
        let snapshot = runtime.snapshot();
        let revision = snapshot.sessions["session"].revision + 1;
        let stale = RuntimeDelta {
            epoch: "epoch".to_string(),
            seq: snapshot.through_seq + 1,
            scope_revision: Some(revision),
            change: RuntimeChange::TurnUpdateAppended {
                session_id: "session".to_string(),
                incarnation,
                revision,
                operation_id: "stale".to_string(),
                update: json!({ "sessionUpdate": "agent_message_chunk", "text": "late" }),
            },
        };

        let mut direct = CanonicalProjection {
            snapshot: Some(snapshot.clone()),
        };
        assert!(!direct.apply_delta(stale.clone()));
        assert_eq!(
            direct.snapshot,
            Some(snapshot.clone()),
            "a rejected delta must not partially mutate the materialized projection"
        );

        let mut public = CanonicalProjection {
            snapshot: Some(snapshot),
        };
        assert!(
            public.update(
                &json!({
                    "type": "bridge/internal_runtime_delta",
                    "value": stale,
                })
                .to_string(),
            )
        );
        assert!(
            public.snapshot.is_none(),
            "the Hub must request a replacement snapshot after an invalid turn delta"
        );
    }

    #[test]
    fn canonical_projection_discards_a_gapped_delta_stream() {
        let runtime = RuntimeState::new("epoch", RuntimeLimits::default());
        let mut projection = CanonicalProjection::default();
        projection.update(
            &json!({
                "type": "bridge/internal_runtime_snapshot",
                "value": runtime.snapshot(),
            })
            .to_string(),
        );
        let gap = RuntimeDelta {
            epoch: "epoch".to_string(),
            seq: 2,
            scope_revision: None,
            change: RuntimeChange::SessionRemoved {
                session_id: "missing".to_string(),
                incarnation: 1,
            },
        };

        assert!(
            projection.update(
                &json!({
                    "type": "bridge/internal_runtime_delta",
                    "value": gap,
                })
                .to_string(),
            )
        );
        assert!(projection.snapshot.is_none());
    }

    #[test]
    fn canonical_projection_rejects_a_session_revision_jump() {
        let mut runtime = RuntimeState::new("epoch", RuntimeLimits::default());
        runtime
            .open_new(
                "epoch",
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        let mut projection = CanonicalProjection::default();
        projection.update(
            &json!({
                "type": "bridge/internal_runtime_snapshot",
                "value": runtime.snapshot(),
            })
            .to_string(),
        );
        let mut jumped = runtime.session("session").unwrap().clone();
        jumped.revision += 2;
        let delta = RuntimeDelta {
            epoch: "epoch".to_string(),
            seq: runtime.seq() + 1,
            scope_revision: Some(jumped.revision),
            change: RuntimeChange::SessionUpsert {
                session: Box::new(jumped),
            },
        };

        assert!(
            projection.update(
                &json!({
                    "type": "bridge/internal_runtime_delta",
                    "value": delta,
                })
                .to_string(),
            )
        );
        assert!(projection.snapshot.is_none());
    }

    #[test]
    fn canonical_projection_tracks_sequential_terminal_retirement() {
        let mut runtime = RuntimeState::new("epoch", RuntimeLimits::default());
        let initial = runtime.snapshot();
        let mut projection = CanonicalProjection::default();
        projection.update(
            &json!({
                "type": "bridge/internal_runtime_snapshot",
                "value": initial,
            })
            .to_string(),
        );
        let mut through_seq = initial.through_seq;
        for index in 0..3 {
            let session_id = format!("session-{index}");
            let operation_id = format!("prompt-{index}");
            let incarnation = runtime
                .open_new(
                    "epoch",
                    session_id.clone(),
                    "/workspace",
                    json!({ "sessionId": session_id }),
                )
                .unwrap();
            runtime
                .start_prompt("epoch", &session_id, incarnation, &operation_id, Vec::new())
                .unwrap();
            let active_through = runtime.snapshot().through_seq;
            for delta in runtime.deltas_after(through_seq).unwrap() {
                assert!(
                    projection.update(
                        &json!({
                            "type": "bridge/internal_runtime_delta",
                            "value": delta,
                        })
                        .to_string(),
                    )
                );
            }
            runtime
                .complete_prompt(
                    "epoch",
                    &session_id,
                    incarnation,
                    &operation_id,
                    json!({ "stopReason": "end_turn" }),
                )
                .unwrap();
            for delta in runtime.deltas_after(active_through).unwrap() {
                assert!(
                    projection.update(
                        &json!({
                            "type": "bridge/internal_runtime_delta",
                            "value": delta,
                        })
                        .to_string(),
                    )
                );
            }
            through_seq = runtime.snapshot().through_seq;
        }

        assert_eq!(
            serde_json::to_value(projection.snapshot.unwrap()).unwrap(),
            serde_json::to_value(runtime.snapshot()).unwrap()
        );
    }

    #[tokio::test]
    async fn subscriber_bootstrap_contains_atomic_canonical_snapshot_and_suffix() {
        let hub = test_hub();
        let (input, _commands) = mpsc::channel(1);
        let mut runtime = RuntimeState::new("epoch", RuntimeLimits::default());
        let initial = runtime.snapshot();
        {
            let mut state = hub.state.lock().await;
            state.generation = 1;
            state.input = Some(input);
            state.canonical.snapshot = Some(initial.clone());
        }

        let mut subscription = hub.subscribe().await.unwrap();
        let snapshot_event = subscription
            .initial_events
            .iter()
            .find_map(|event| {
                let value = serde_json::from_str::<serde_json::Value>(event).ok()?;
                (value.get("type")?.as_str()? == "bridge/runtime_snapshot").then_some(value)
            })
            .expect("canonical snapshot must be part of the atomic bootstrap");
        assert_eq!(snapshot_event["snapshot"]["epoch"], initial.epoch);
        assert_eq!(
            snapshot_event["snapshot"]["throughSeq"],
            initial.through_seq,
        );

        runtime
            .open_new(
                "epoch",
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        let delta = runtime.deltas_after(initial.through_seq).unwrap()[0].clone();
        hub.publish(
            1,
            json!({
                "type": "bridge/internal_runtime_delta",
                "value": delta,
            })
            .to_string(),
        )
        .await;

        let delta_event =
            tokio::time::timeout(Duration::from_millis(100), subscription.events.recv())
                .await
                .expect("canonical suffix was not delivered")
                .expect("subscriber was disconnected before its suffix");
        let delta_event: serde_json::Value =
            serde_json::from_str(&delta_event.into_string()).unwrap();
        assert_eq!(delta_event["type"], "bridge/runtime_delta");
        assert_eq!(delta_event["delta"]["seq"], initial.through_seq + 1);
    }

    #[tokio::test]
    async fn canonical_gap_requests_a_fresh_bridge_snapshot() {
        let hub = test_hub();
        let (input, mut commands) = mpsc::channel(1);
        let runtime = RuntimeState::new("epoch", RuntimeLimits::default());
        {
            let mut state = hub.state.lock().await;
            state.generation = 1;
            state.input = Some(input);
            state.canonical.snapshot = Some(runtime.snapshot());
        }
        let gap = RuntimeDelta {
            epoch: "epoch".to_string(),
            seq: 2,
            scope_revision: None,
            change: RuntimeChange::SessionRemoved {
                session_id: "missing".to_string(),
                incarnation: 1,
            },
        };

        hub.publish(
            1,
            json!({
                "type": "bridge/internal_runtime_delta",
                "value": gap,
            })
            .to_string(),
        )
        .await;

        let command = tokio::time::timeout(Duration::from_millis(100), commands.recv())
            .await
            .expect("hub did not request a replacement snapshot")
            .expect("bridge input closed before resync");
        assert!(matches!(
            command,
            bridge::BridgeInput::RuntimeSnapshotRequest
        ));
    }

    #[tokio::test]
    async fn gap_resnapshot_is_single_flight_and_reestablishes_contiguous_suffix() {
        let hub = test_hub();
        let (input, mut commands) = mpsc::channel(8);
        let mut runtime = RuntimeState::new("epoch", RuntimeLimits::default());
        {
            let mut state = hub.state.lock().await;
            state.generation = 1;
            state.input = Some(input);
            state.canonical.snapshot = Some(runtime.snapshot());
        }
        let mut subscription = hub.subscribe().await.unwrap();

        for seq in [2, 3] {
            hub.publish(
                1,
                json!({
                    "type": "bridge/internal_runtime_delta",
                    "value": RuntimeDelta {
                        epoch: "epoch".to_string(),
                        seq,
                        scope_revision: None,
                        change: RuntimeChange::SessionRemoved {
                            session_id: "missing".to_string(),
                            incarnation: 1,
                        },
                    },
                })
                .to_string(),
            )
            .await;
        }
        assert!(matches!(
            commands.try_recv().unwrap(),
            bridge::BridgeInput::RuntimeSnapshotRequest
        ));
        assert!(
            commands.try_recv().is_err(),
            "only one replacement snapshot may be in flight"
        );

        let incarnation = runtime
            .open_new(
                "epoch",
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        hub.publish(
            1,
            json!({
                "type": "bridge/internal_runtime_snapshot",
                "value": runtime.snapshot(),
            })
            .to_string(),
        )
        .await;
        let replacement = subscription.events.recv().await.unwrap();
        let replacement: serde_json::Value =
            serde_json::from_str(&replacement.into_string()).unwrap();
        assert_eq!(replacement["type"], "bridge/runtime_snapshot");
        assert_eq!(replacement["snapshot"]["throughSeq"], 1);

        runtime
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        let suffix = runtime.deltas_after(1).unwrap().remove(0);
        hub.publish(
            1,
            json!({
                "type": "bridge/internal_runtime_delta",
                "value": suffix,
            })
            .to_string(),
        )
        .await;
        let suffix = subscription.events.recv().await.unwrap();
        let suffix: serde_json::Value = serde_json::from_str(&suffix.into_string()).unwrap();
        assert_eq!(suffix["type"], "bridge/runtime_delta");
        assert_eq!(suffix["delta"]["seq"], 2);
    }

    #[tokio::test]
    async fn two_subscribers_receive_the_same_canonical_revision_and_state() {
        let hub = test_hub();
        let (input, _commands) = mpsc::channel(1);
        let mut runtime = RuntimeState::new("epoch", RuntimeLimits::default());
        {
            let mut state = hub.state.lock().await;
            state.generation = 1;
            state.input = Some(input);
            state.canonical.snapshot = Some(runtime.snapshot());
        }
        let mut first = hub.subscribe().await.unwrap();
        let mut second = hub.subscribe().await.unwrap();
        let incarnation = runtime
            .open_new(
                "epoch",
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        runtime
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();

        for delta in runtime.deltas_after(0).unwrap() {
            hub.publish(
                1,
                json!({
                    "type": "bridge/internal_runtime_delta",
                    "value": delta,
                })
                .to_string(),
            )
            .await;
        }

        for expected_seq in 1..=runtime.seq() {
            let first_event = first.events.recv().await.unwrap().into_string();
            let second_event = second.events.recv().await.unwrap().into_string();
            assert_eq!(first_event, second_event);
            let event: serde_json::Value = serde_json::from_str(&first_event).unwrap();
            assert_eq!(event["delta"]["seq"], expected_seq);
        }
        let canonical = hub.state.lock().await.canonical.snapshot.clone().unwrap();
        assert_eq!(
            serde_json::to_value(canonical).unwrap(),
            serde_json::to_value(runtime.snapshot()).unwrap(),
        );
    }

    #[tokio::test]
    async fn shutdown_signal_bypasses_a_saturated_command_queue() {
        let hub = test_hub();
        let (input, _commands) = mpsc::channel(1);
        input
            .send(bridge::BridgeInput::Command {
                subscriber_id: 1,
                raw: "queued".to_string(),
            })
            .await
            .unwrap();
        let input_probe = input.clone();
        let cancellation = CancellationToken::new();
        {
            let mut state = hub.state.lock().await;
            state.generation = 1;
            state.input = Some(input);
            state.cancellation = Some(cancellation.clone());
        }
        let shutdown = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.shutdown().await })
        };

        tokio::time::timeout(Duration::from_millis(100), cancellation.cancelled())
            .await
            .expect("shutdown was blocked behind the command queue");
        assert!(
            input_probe
                .try_send(bridge::BridgeInput::Command {
                    subscriber_id: 1,
                    raw: "still-full".to_string(),
                })
                .is_err()
        );
        hub.finish_generation(1).await;
        shutdown.await.unwrap();
    }
}
