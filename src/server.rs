use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
#[cfg(any(test, not(feature = "dev")))]
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
#[cfg(not(feature = "dev"))]
use axum::http::Method;
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
#[cfg(not(feature = "dev"))]
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tower_http::compression::CompressionLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::bridge;
use crate::dev_proxy::{DevProxy, reject_self_proxy};
use crate::event_queue::{self, EventQueueLimits, EventReceiver, EventSender};
use crate::options::{Options, normalize_origin};
use crate::runtime_cache::ActiveRuntimeProjection;
use crate::runtime_state::{RuntimeChange, RuntimeDelta, RuntimeSnapshot, fold_active_turn_update};

const BRIDGE_SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(5);
const HTTP_SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(6);
const SUBSCRIBER_QUEUE_CAPACITY: usize = 64;
const SUBSCRIBER_QUEUE_BYTE_CAPACITY: usize = 8 * 1024 * 1024;
const MAX_SUBSCRIBERS: usize = 64;
const BRIDGE_EVENT_QUEUE_CAPACITY: usize = 256;
const BRIDGE_EVENT_QUEUE_BYTE_CAPACITY: usize = 16 * 1024 * 1024;
const BRIDGE_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(not(feature = "dev"))]
#[derive(RustEmbed)]
#[folder = "dist/client/"]
struct ClientAssets;

#[derive(Clone)]
struct AppState {
    bridge: Arc<BridgeHub>,
}

#[derive(Clone)]
struct OriginPolicy {
    allowed_origins: Vec<String>,
}

impl OriginPolicy {
    fn allows(&self, request: &Request) -> bool {
        let mut hosts = request.headers().get_all(header::HOST).iter();
        let host = match hosts.next() {
            Some(value) => match value.to_str() {
                Ok(value) if hosts.next().is_none() => value,
                _ => return false,
            },
            None => match request.uri().authority() {
                Some(authority) => authority.as_str(),
                None => return false,
            },
        };
        let Ok(http_origin) = normalize_origin(&format!("http://{host}")) else {
            return false;
        };
        let parsed = url::Url::parse(&http_origin).expect("validated HTTP origin");
        let direct_host = matches!(parsed.host(), Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)))
            || parsed.host_str() == Some("localhost");
        let configured_host = self.allowed_origins.iter().any(|origin| {
            let scheme = origin.split_once("://").expect("validated origin").0;
            normalize_origin(&format!("{scheme}://{host}")).as_ref() == Ok(origin)
        });
        if !direct_host && !configured_host {
            return false;
        }

        let mut origins = request.headers().get_all(header::ORIGIN).iter();
        let Some(origin) = origins.next() else {
            // Command-line clients do not send Origin. Host is still checked
            // so an unconfigured DNS name cannot rebind to this server.
            return true;
        };
        if origins.next().is_some() {
            return false;
        }
        let Some(origin) = origin
            .to_str()
            .ok()
            .and_then(|value| normalize_origin(value).ok())
        else {
            return false;
        };
        (direct_host && origin == http_origin) || self.allowed_origins.contains(&origin)
    }
}

async fn enforce_origin(
    State(policy): State<OriginPolicy>,
    request: Request,
    next: Next,
) -> Response {
    if !policy.allows(&request) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({ "error": "Request Host or Origin is not allowed" })),
        )
            .into_response();
    }
    next.run(request).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StartTurnBody {
    prompt: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListSessionsQuery {
    cursor: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionViewQuery {
    cwd: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateSessionBody {
    cwd: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InteractionResponseBody {
    kind: String,
    outcome: Option<serde_json::Value>,
    response: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetModeBody {
    mode_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetConfigBody {
    value: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ContextSearchQuery {
    query: String,
    session_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ContextReadBody {
    path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AuthTerminalStartBody {
    method_id: String,
    cols: u16,
    rows: u16,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AuthTerminalInputBody {
    data: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AuthTerminalResizeBody {
    cols: u16,
    rows: u16,
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
            RuntimeChange::TerminalUpdated {
                session_id,
                incarnation,
                revision,
                terminal_id,
                terminal,
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
                if terminal
                    .get("released")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
                {
                    session.terminals.remove(&terminal_id);
                } else {
                    let terminal = crate::runtime_state::fold_terminal_snapshot(
                        session.terminals.get(&terminal_id),
                        &terminal,
                    );
                    session.terminals.insert(terminal_id, terminal);
                }
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
    session_subscribers: HashMap<u64, (String, SubscriberSender)>,
    bootstrap: BridgeBootstrap,
    runtime: ActiveRuntimeProjection,
    canonical: CanonicalProjection,
    canonical_resync_pending: bool,
    shutting_down: bool,
    unobserved: HashMap<String, (u64, crate::auto_close::AutoClosePermit)>,
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

struct SubscriptionGuard {
    bridge: Arc<BridgeHub>,
    id: u64,
    generation: u64,
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        let bridge = self.bridge.clone();
        let id = self.id;
        let generation = self.generation;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                bridge.unsubscribe(id, generation).await;
            });
        }
    }
}

#[derive(Clone)]
struct SubscriberSender {
    tx: mpsc::Sender<QueuedSubscriberEvent>,
    queued_bytes: Arc<AtomicUsize>,
}

struct QueuedSubscriberEvent {
    event: Arc<str>,
    queued_bytes: Arc<AtomicUsize>,
    bytes: usize,
}

impl QueuedSubscriberEvent {
    #[cfg(test)]
    fn into_arc(mut self) -> Arc<str> {
        std::mem::replace(&mut self.event, Arc::from(""))
    }

    fn into_string(mut self) -> String {
        std::mem::replace(&mut self.event, Arc::from("")).to_string()
    }
}

impl Drop for QueuedSubscriberEvent {
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

    fn try_send(&self, event: impl Into<Arc<str>>) -> Result<(), ()> {
        let event = event.into();
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

fn publish_shared_event(state: &mut BridgeHubState, event: Arc<str>) {
    let mut failed = Vec::new();
    for (&subscriber_id, subscriber) in &state.subscribers {
        if subscriber.try_send(event.clone()).is_err() {
            failed.push(subscriber_id);
        }
    }
    for subscriber_id in failed {
        state.subscribers.remove(&subscriber_id);
    }
    publish_to_session_subscribers(state, event);
}

fn publish_to_session_subscribers(state: &mut BridgeHubState, event: Arc<str>) {
    let Some((event_session_id, event)) = business_session_event(&event) else {
        return;
    };
    let mut failed = Vec::new();
    for (&subscriber_id, (session_id, subscriber)) in &state.session_subscribers {
        if session_id == &event_session_id && subscriber.try_send(event.clone()).is_err() {
            failed.push(subscriber_id);
        }
    }
    for subscriber_id in failed {
        state.session_subscribers.remove(&subscriber_id);
    }
}

fn business_session_event(event: &str) -> Option<(String, Arc<str>)> {
    let value = serde_json::from_str::<serde_json::Value>(event).ok()?;
    match value.get("type").and_then(serde_json::Value::as_str)? {
        "bridge/session_delta" => {
            let session_id = value.get("sessionId")?.as_str()?.to_string();
            Some((session_id, Arc::from(event)))
        }
        "bridge/session_view" => {
            let session_id = value.get("sessionId")?.as_str()?.to_string();
            let reset = session_reset_value(&session_id, value.get("view")?)?;
            Some((session_id, Arc::from(reset.to_string())))
        }
        "bridge/session_turn_complete" | "bridge/session_turn_failed" => {
            let session_id = value.get("sessionId")?.as_str()?.to_string();
            Some((session_id, Arc::from(event)))
        }
        _ => None,
    }
}

fn session_reset_value(session_id: &str, view: &serde_json::Value) -> Option<serde_json::Value> {
    Some(json!({
        "type": "bridge/session_reset",
        "bridgeEpoch": view.get("bridgeEpoch")?.as_str()?,
        "sessionId": session_id,
        "sessionIncarnation": view.pointer("/session/incarnation")?.as_u64()?,
        "viewRevision": view.pointer("/session/viewRevision")?.as_u64()?,
        "historyRevision": view.pointer("/session/historyRevision").cloned().unwrap_or(serde_json::Value::Null),
        "phase": view.pointer("/session/phase").cloned().unwrap_or(serde_json::Value::Null),
        "syncError": view.pointer("/session/syncError").cloned().unwrap_or(serde_json::Value::Null),
    }))
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

    fn update_auto_close(&self, state: &mut BridgeHubState) {
        use crate::runtime_state::SessionLifecycle;
        let enabled = self.options.session_unobserved_timeout >= 0
            && !state.shutting_down
            && state.input.is_some();
        let sessions = state
            .canonical
            .snapshot
            .as_ref()
            .map(|snapshot| &snapshot.sessions);
        state.unobserved.retain(|id, (incarnation, permit)| {
            let retain = enabled
                && !state
                    .session_subscribers
                    .values()
                    .any(|(observed, _)| observed == id)
                && sessions.is_none_or(|sessions| {
                    sessions.get(id).is_some_and(|session| {
                        session.incarnation == *incarnation
                            && !matches!(
                                session.lifecycle,
                                SessionLifecycle::Closed | SessionLifecycle::Deleting
                            )
                    })
                });
            if !retain {
                permit.cancel();
            }
            retain
        });
        if !enabled {
            return;
        }
        let Some(sessions) = sessions else {
            return;
        };
        for (id, session) in sessions {
            if session.lifecycle != SessionLifecycle::Active
                || state.unobserved.contains_key(id)
                || state
                    .session_subscribers
                    .values()
                    .any(|(observed, _)| observed == id)
            {
                continue;
            }
            let permit = crate::auto_close::AutoClosePermit::default();
            state
                .unobserved
                .insert(id.clone(), (session.incarnation, permit.clone()));
            let input = state.input.as_ref().unwrap().clone();
            let id = id.clone();
            let incarnation = session.incarnation;
            let mut remaining = self.options.session_unobserved_timeout as u64;
            tokio::spawn(async move {
                // Chunk very large positive values so Instant arithmetic cannot overflow.
                while remaining > 0 {
                    let seconds = remaining.min(86_400);
                    tokio::select! {
                        _ = permit.cancelled.cancelled() => return,
                        _ = tokio::time::sleep(Duration::from_secs(seconds)) => {}
                    }
                    remaining -= seconds;
                }
                while permit.pending() {
                    tokio::select! {
                        _ = permit.cancelled.cancelled() => return,
                        result = input.send(bridge::BridgeInput::RetireUnobservedSession {
                            session_id: id.clone(), incarnation, permit: permit.clone(),
                        }) => { if result.is_err() { return; } }
                    }
                    // Loading/control transactions may still own the session. Retry
                    // admission without resetting the completed absence interval.
                    tokio::select! {
                        _ = permit.cancelled.cancelled() => return,
                        _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                    }
                }
            });
        }
    }

    async fn ensure_runtime(self: &Arc<Self>) {
        let runtime = {
            let mut state = self.state.lock().await;
            if state.shutting_down || state.input.is_some() {
                None
            } else {
                for (_, permit) in state.unobserved.values() {
                    permit.cancel();
                }
                state.unobserved.clear();
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
            if state.subscribers.len() + state.session_subscribers.len() >= MAX_SUBSCRIBERS {
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

    async fn subscribe_session(self: &Arc<Self>, session_id: String) -> Option<BridgeSubscription> {
        self.ensure_runtime().await;
        let id = self.next_subscriber_id.fetch_add(1, Ordering::Relaxed);
        let (event_tx, event_rx) = SubscriberSender::channel(SUBSCRIBER_QUEUE_CAPACITY);
        let generation = {
            let mut state = self.state.lock().await;
            if state.shutting_down || state.input.is_none() {
                return None;
            }
            if state.subscribers.len() + state.session_subscribers.len() >= MAX_SUBSCRIBERS {
                return None;
            }
            let generation = state.generation;
            state.session_subscribers.insert(id, (session_id, event_tx));
            self.update_auto_close(&mut state);
            generation
        };
        Some(BridgeSubscription {
            id,
            generation,
            initial_events: Vec::new(),
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
                    let public_event: Arc<str> = public_event.into();
                    publish_shared_event(&mut state, public_event);
                }
                self.update_auto_close(&mut state);
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
        let restart_input = {
            let mut state = self.state.lock().await;
            if state.generation != generation || state.input.is_none() {
                return;
            }
            state.bootstrap.update(&event);
            let event = state.runtime.update_and_normalize(&event);
            let session_live_suffix = opened_runtime_session_id(&event)
                .map(|session_id| state.runtime.replay_session_live_suffix(&session_id))
                .unwrap_or_default()
                .into_iter()
                .map(Arc::<str>::from)
                .collect::<Vec<_>>();
            let event: Arc<str> = event.into();
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
            publish_to_session_subscribers(&mut state, event);
            for live_event in session_live_suffix {
                publish_to_session_subscribers(&mut state, live_event);
            }
            self.update_auto_close(&mut state);
            restart.then(|| state.cancellation.clone()).flatten()
        };
        if let Some(cancellation) = restart_input {
            cancellation.cancel();
        }
    }

    async fn finish_generation(&self, generation: u64) {
        let (subscribers, session_subscribers) = {
            let mut state = self.state.lock().await;
            if state.generation != generation {
                return;
            }
            state.input = None;
            state.cancellation = None;
            self.update_auto_close(&mut state);
            (
                std::mem::take(&mut state.subscribers),
                std::mem::take(&mut state.session_subscribers),
            )
        };
        drop(subscribers);
        drop(session_subscribers);
        self.stopped.notify_waiters();
    }

    #[cfg(test)]
    async fn session_view(
        &self,
        session_id: String,
    ) -> Result<serde_json::Value, bridge::SessionViewError> {
        self.session_view_with_cwd(session_id, None).await
    }

    async fn session_view_with_cwd(
        &self,
        session_id: String,
        cwd: Option<String>,
    ) -> Result<serde_json::Value, bridge::SessionViewError> {
        let input = {
            let state = self.state.lock().await;
            state
                .input
                .clone()
                .ok_or_else(|| bridge::SessionViewError::unavailable("bridge is not ready"))?
        };
        let (response, result) = oneshot::channel();
        input
            .send(bridge::BridgeInput::SessionViewRequest {
                session_id,
                cwd,
                response,
            })
            .await
            .map_err(|_| {
                bridge::SessionViewError::unavailable("bridge stopped before accepting the query")
            })?;
        tokio::time::timeout(BRIDGE_QUERY_TIMEOUT, result)
            .await
            .map_err(|_| bridge::SessionViewError::unavailable("bridge session query timed out"))?
            .map_err(|_| {
                bridge::SessionViewError::unavailable("bridge stopped before answering the query")
            })?
    }

    async fn start_turn(
        &self,
        session_id: String,
        history_revision: String,
        client_intent_id: String,
        prompt: Vec<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let input = {
            let state = self.state.lock().await;
            state
                .input
                .clone()
                .ok_or_else(|| "bridge is not ready".to_string())?
        };
        let (response, result) = oneshot::channel();
        input
            .send(bridge::BridgeInput::TurnRequest {
                session_id,
                history_revision,
                client_intent_id,
                prompt,
                response,
            })
            .await
            .map_err(|_| "bridge stopped before accepting the turn".to_string())?;
        tokio::time::timeout(BRIDGE_QUERY_TIMEOUT, result)
            .await
            .map_err(|_| "bridge turn admission timed out".to_string())?
            .map_err(|_| "bridge stopped before admitting the turn".to_string())?
    }

    async fn business_request(
        &self,
        command: serde_json::Value,
    ) -> Result<serde_json::Value, bridge::BridgeRequestError> {
        let input = {
            let state = self.state.lock().await;
            state
                .input
                .clone()
                .ok_or_else(|| bridge::BridgeRequestError::internal("bridge is not ready"))?
        };
        let (response, result) = oneshot::channel();
        input
            .send(bridge::BridgeInput::BusinessRequest { command, response })
            .await
            .map_err(|_| {
                bridge::BridgeRequestError::internal("bridge stopped before accepting the request")
            })?;
        tokio::time::timeout(BRIDGE_QUERY_TIMEOUT, result)
            .await
            .map_err(|_| bridge::BridgeRequestError::internal("bridge request timed out"))?
            .map_err(|_| {
                bridge::BridgeRequestError::internal("bridge stopped before answering the request")
            })?
    }

    async fn runtime_info(&self) -> serde_json::Value {
        let state = self.state.lock().await;
        let parse = |event: &Option<String>| {
            event
                .as_deref()
                .and_then(|event| serde_json::from_str::<serde_json::Value>(event).ok())
        };
        json!({
            "generation": state.generation,
            "connected": state.input.is_some(),
            "hello": parse(&state.bootstrap.hello),
            "initialized": parse(&state.bootstrap.initialized),
            "phase": parse(&state.bootstrap.phase),
            "error": parse(&state.bootstrap.error),
        })
    }

    async fn send_to_subscriber(&self, subscriber_id: u64, generation: u64, event: String) {
        let mut state = self.state.lock().await;
        if state.generation == generation
            && let Some(sender) = state.subscribers.get(&subscriber_id)
            && sender.try_send(Arc::<str>::from(event)).is_err()
        {
            state.subscribers.remove(&subscriber_id);
        }
    }

    async fn unsubscribe(&self, subscriber_id: u64, generation: u64) {
        let mut state = self.state.lock().await;
        if state.generation == generation {
            state.subscribers.remove(&subscriber_id);
            state.session_subscribers.remove(&subscriber_id);
            self.update_auto_close(&mut state);
        }
    }

    async fn shutdown(&self) {
        let cancellation = {
            let mut state = self.state.lock().await;
            state.shutting_down = true;
            self.update_auto_close(&mut state);
            // Event streams must finish before HTTP graceful draining can
            // complete, including when no Agent runtime is currently active.
            state.subscribers.clear();
            state.session_subscribers.clear();
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
    #[cfg(feature = "dev")]
    if options.dev_server.is_none() {
        anyhow::bail!(
            "this development build requires --dev-server <http://host:port>; use a normal build for embedded frontend assets"
        );
    }
    if options.transport == crate::options::Transport::Stdio {
        std::env::set_current_dir(&options.cwd).with_context(|| {
            format!("failed to enter Agent workspace {}", options.cwd.display())
        })?;
    }
    let address = SocketAddr::new(options.host, options.port);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to bind http://{address}"))?;
    let actual = listener.local_addr()?;
    if let Some(origin) = &options.dev_server {
        reject_self_proxy(origin, actual).await?;
    }
    let bridge = BridgeHub::new(options.clone());
    bridge.ensure_runtime().await;
    let app = app_router(&options, bridge.clone());

    println!("attyd listening on http://{actual}");
    if let Some(origin) = &options.dev_server {
        println!("frontend development server: {origin}");
    }
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

    let http_shutdown = CancellationToken::new();
    let server = async {
        axum::serve(listener, app)
            .with_graceful_shutdown(http_shutdown.clone().cancelled_owned())
            .await
    };
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => {
            bridge.shutdown().await;
            result.context("HTTP server failed")
        }
        _ = shutdown_signal() => {
            http_shutdown.cancel();
            let ((), result) = tokio::join!(
                bridge.shutdown(),
                tokio::time::timeout(HTTP_SHUTDOWN_GRACE_PERIOD, &mut server),
            );
            match result {
                Ok(result) => result.context("HTTP server failed"),
                Err(_) => {
                    tracing::warn!("HTTP drain deadline reached; closing remaining connections");
                    Ok(())
                }
            }
        }
    }
}

fn app_router(options: &Options, bridge: Arc<BridgeHub>) -> Router {
    let dev_proxy = options.dev_server.as_deref().map(DevProxy::new);
    Router::new()
        .route("/api/health", get(health))
        .route("/api/v1/runtime", get(get_runtime))
        .route("/api/v1/events", get(global_events))
        .route("/api/v1/auth/{method_id}", post(authenticate))
        .route("/api/v1/auth/logout", post(logout))
        .route("/api/v1/auth/terminal", post(start_auth_terminal))
        .route(
            "/api/v1/auth/terminal/{request_id}/input",
            post(write_auth_terminal),
        )
        .route(
            "/api/v1/auth/terminal/{request_id}/resize",
            post(resize_auth_terminal),
        )
        .route(
            "/api/v1/auth/terminal/{request_id}/cancel",
            post(cancel_auth_terminal),
        )
        .route("/api/v1/context/search", get(search_context))
        .route(
            "/api/v1/sessions/{session_id}/context/read",
            post(read_context),
        )
        .route("/api/v1/sessions", get(list_sessions).post(create_session))
        .route(
            "/api/v1/sessions/{session_id}",
            get(get_session_view).delete(delete_session),
        )
        .route(
            "/api/v1/sessions/{session_id}/turns",
            post(start_session_turn),
        )
        .route(
            "/api/v1/sessions/{session_id}/turns/{operation_id}/cancel",
            post(cancel_session_turn),
        )
        .route("/api/v1/sessions/{session_id}/fork", post(fork_session))
        .route("/api/v1/sessions/{session_id}/close", post(close_session))
        .route("/api/v1/sessions/{session_id}/mode", post(set_session_mode))
        .route(
            "/api/v1/sessions/{session_id}/configuration/{config_id}",
            post(set_session_config),
        )
        .route(
            "/api/v1/sessions/{session_id}/interactions/{interaction_id}/response",
            post(respond_to_interaction),
        )
        .route("/api/v1/sessions/{session_id}/events", get(session_events))
        .fallback(move |request: Request| frontend(dev_proxy.clone(), request))
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(bridge::MAX_BRIDGE_MESSAGE_BYTES))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .layer(middleware::from_fn_with_state(
            OriginPolicy {
                allowed_origins: options.allowed_origins.clone(),
            },
            enforce_origin,
        ))
        .with_state(AppState { bridge })
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

async fn get_runtime(State(state): State<AppState>) -> Response {
    let mut response = axum::Json(state.bridge.runtime_info().await).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

fn global_business_event(event: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(event).ok()?;
    match value.get("type").and_then(serde_json::Value::as_str)? {
        "bridge/phase" => Some(
            json!({
                "type": "bridge/connection",
                "phase": value.get("phase"),
            })
            .to_string(),
        ),
        "acp/authenticated"
        | "acp/logged_out"
        | "bridge/auth_terminal_started"
        | "bridge/auth_terminal_output"
        | "bridge/auth_terminal_exited" => Some(event.to_string()),
        "bridge/error" if value.get("requestId").is_none() && value.get("operation").is_none() => {
            Some(
                json!({
                    "type": "bridge/connection_error",
                    "message": value.get("message"),
                    "code": value.get("code"),
                    "data": value.get("data"),
                })
                .to_string(),
            )
        }
        _ => None,
    }
}

async fn global_events(State(state): State<AppState>) -> Response {
    let Some(subscription) = state.bridge.subscribe().await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let guard = SubscriptionGuard {
        bridge: state.bridge,
        id: subscription.id,
        generation: subscription.generation,
    };
    let stream = futures::stream::unfold(
        (
            subscription.initial_events.into_iter(),
            subscription.events,
            guard,
        ),
        |(mut bootstrap, mut events, guard)| async move {
            loop {
                let event = if let Some(event) = bootstrap.next() {
                    event
                } else {
                    events.recv().await?.into_string()
                };
                let Some(event) = global_business_event(&event) else {
                    continue;
                };
                return Some((
                    Ok::<_, Infallible>(SseEvent::default().data(event)),
                    (bootstrap, events, guard),
                ));
            }
        },
    );
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn authenticate(Path(method_id): Path<String>, State(state): State<AppState>) -> Response {
    if !valid_api_identifier(&method_id) {
        return api_bad_request("invalid authentication method ID");
    }
    let request_id = format!("api-auth-{}", Uuid::new_v4());
    match state
        .bridge
        .business_request(json!({
            "type": "auth/authenticate",
            "requestId": request_id,
            "methodId": method_id,
        }))
        .await
    {
        Ok(response) => axum::Json(json!({
            "requestId": request_id,
            "response": response,
        }))
        .into_response(),
        Err(message) => business_error(message),
    }
}

async fn logout(State(state): State<AppState>) -> Response {
    let request_id = format!("api-logout-{}", Uuid::new_v4());
    match state
        .bridge
        .business_request(json!({
            "type": "auth/logout",
            "requestId": request_id,
        }))
        .await
    {
        Ok(response) => axum::Json(json!({
            "requestId": request_id,
            "response": response,
        }))
        .into_response(),
        Err(message) => business_error(message),
    }
}

async fn start_auth_terminal(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<AuthTerminalStartBody>,
) -> Response {
    if !valid_api_identifier(&body.method_id) || body.cols == 0 || body.rows == 0 {
        return api_bad_request("invalid terminal authentication parameters");
    }
    let request_id = format!("api-terminal-{}", Uuid::new_v4());
    match state
        .bridge
        .business_request(json!({
            "type": "auth/terminal_start",
            "requestId": request_id,
            "methodId": body.method_id,
            "cols": body.cols,
            "rows": body.rows,
        }))
        .await
    {
        Ok(_) => (
            StatusCode::ACCEPTED,
            axum::Json(json!({ "requestId": request_id })),
        )
            .into_response(),
        Err(message) => business_error(message),
    }
}

async fn write_auth_terminal(
    Path(request_id): Path<String>,
    State(state): State<AppState>,
    axum::Json(body): axum::Json<AuthTerminalInputBody>,
) -> Response {
    if !valid_api_identifier(&request_id) {
        return api_bad_request("invalid terminal request ID");
    }
    business_response(
        state
            .bridge
            .business_request(json!({
                "type": "auth/terminal_input",
                "requestId": request_id,
                "data": body.data,
            }))
            .await,
    )
}

async fn resize_auth_terminal(
    Path(request_id): Path<String>,
    State(state): State<AppState>,
    axum::Json(body): axum::Json<AuthTerminalResizeBody>,
) -> Response {
    if !valid_api_identifier(&request_id) || body.cols == 0 || body.rows == 0 {
        return api_bad_request("invalid terminal resize parameters");
    }
    business_response(
        state
            .bridge
            .business_request(json!({
                "type": "auth/terminal_resize",
                "requestId": request_id,
                "cols": body.cols,
                "rows": body.rows,
            }))
            .await,
    )
}

async fn cancel_auth_terminal(
    Path(request_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    if !valid_api_identifier(&request_id) {
        return api_bad_request("invalid terminal request ID");
    }
    business_response(
        state
            .bridge
            .business_request(json!({
                "type": "auth/terminal_cancel",
                "requestId": request_id,
            }))
            .await,
    )
}

async fn search_context(
    State(state): State<AppState>,
    Query(query): Query<ContextSearchQuery>,
) -> Response {
    if query.query.encode_utf16().count() > 256 || !valid_api_identifier(&query.session_id) {
        return api_bad_request("invalid context session or query");
    }
    business_response(
        state
            .bridge
            .business_request(json!({
                "type": "context/search",
                "requestId": format!("api-context-search-{}", Uuid::new_v4()),
                "sessionId": query.session_id,
                "query": query.query,
            }))
            .await,
    )
}

async fn read_context(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    axum::Json(body): axum::Json<ContextReadBody>,
) -> Response {
    if !valid_api_identifier(&session_id) || body.path.is_empty() || body.path.len() > 16_384 {
        return api_bad_request("invalid session ID or context path");
    }
    business_response(
        state
            .bridge
            .business_request(json!({
                "type": "context/read",
                "requestId": format!("api-context-read-{}", Uuid::new_v4()),
                "sessionId": session_id,
                "path": body.path,
            }))
            .await,
    )
}

async fn list_sessions(
    State(state): State<AppState>,
    Query(query): Query<ListSessionsQuery>,
) -> Response {
    let mut command = json!({
        "type": "session/list",
        "requestId": format!("api-list-{}", Uuid::new_v4()),
    });
    if let Some(cursor) = query.cursor {
        command["cursor"] = json!(cursor);
    }
    business_response(state.bridge.business_request(command).await)
}

async fn create_session(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<CreateSessionBody>,
) -> Response {
    let mut command = json!({
        "type": "session/new",
        "requestId": format!("api-new-{}", Uuid::new_v4()),
    });
    if let Some(cwd) = body.cwd {
        command["cwd"] = json!(cwd);
    }
    match state.bridge.business_request(command).await {
        Ok(value) => match normalize_embedded_session_view(value) {
            Some(value) => (StatusCode::CREATED, axum::Json(value)).into_response(),
            None => (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": "bridge returned an invalid created session" })),
            )
                .into_response(),
        },
        Err(message) => business_error(message),
    }
}

async fn delete_session(Path(session_id): Path<String>, State(state): State<AppState>) -> Response {
    if !valid_api_identifier(&session_id) {
        return api_bad_request("invalid session ID");
    }
    let command = json!({
        "type": "session/delete",
        "requestId": format!("api-delete-{}", Uuid::new_v4()),
        "sessionId": session_id,
    });
    match state.bridge.business_request(command).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(message) => business_error(message),
    }
}

async fn cancel_session_turn(
    Path((session_id, operation_id)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    if !valid_api_identifier(&session_id) || !valid_api_identifier(&operation_id) {
        return api_bad_request("invalid session or operation ID");
    }
    business_response(
        state
            .bridge
            .business_request(json!({
                "type": "session/cancel",
                "sessionId": session_id,
                "expectedOperationId": operation_id,
            }))
            .await,
    )
}

async fn fork_session(Path(session_id): Path<String>, State(state): State<AppState>) -> Response {
    if !valid_api_identifier(&session_id) {
        return api_bad_request("invalid session ID");
    }
    match state
        .bridge
        .business_request(json!({
            "type": "session/fork",
            "requestId": format!("api-fork-{}", Uuid::new_v4()),
            "sessionId": session_id,
        }))
        .await
    {
        Ok(value) => match normalize_embedded_session_view(value) {
            Some(value) => (StatusCode::CREATED, axum::Json(value)).into_response(),
            None => (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": "bridge returned an invalid forked session" })),
            )
                .into_response(),
        },
        Err(message) => business_error(message),
    }
}

async fn close_session(Path(session_id): Path<String>, State(state): State<AppState>) -> Response {
    if !valid_api_identifier(&session_id) {
        return api_bad_request("invalid session ID");
    }
    business_response(
        state
            .bridge
            .business_request(json!({
                "type": "session/close",
                "requestId": format!("api-close-{}", Uuid::new_v4()),
                "sessionId": session_id,
            }))
            .await,
    )
}

async fn set_session_mode(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    axum::Json(body): axum::Json<SetModeBody>,
) -> Response {
    if !valid_api_identifier(&session_id) || !valid_api_identifier(&body.mode_id) {
        return api_bad_request("invalid session or mode ID");
    }
    business_response(
        state
            .bridge
            .business_request(json!({
                "type": "session/set_mode",
                "requestId": format!("api-mode-{}", Uuid::new_v4()),
                "sessionId": session_id,
                "modeId": body.mode_id,
            }))
            .await,
    )
}

async fn set_session_config(
    Path((session_id, config_id)): Path<(String, String)>,
    State(state): State<AppState>,
    axum::Json(body): axum::Json<SetConfigBody>,
) -> Response {
    if !valid_api_identifier(&session_id) || !valid_api_identifier(&config_id) {
        return api_bad_request("invalid session or configuration ID");
    }
    if !matches!(
        body.value,
        serde_json::Value::String(_) | serde_json::Value::Bool(_)
    ) {
        return api_bad_request("configuration value must be a string or boolean");
    }
    business_response(
        state
            .bridge
            .business_request(json!({
                "type": "session/set_config_option",
                "requestId": format!("api-config-{}", Uuid::new_v4()),
                "sessionId": session_id,
                "configId": config_id,
                "value": body.value,
            }))
            .await,
    )
}

async fn respond_to_interaction(
    Path((session_id, interaction_id)): Path<(String, String)>,
    State(state): State<AppState>,
    axum::Json(body): axum::Json<InteractionResponseBody>,
) -> Response {
    if !valid_api_identifier(&session_id) || !valid_api_identifier(&interaction_id) {
        return api_bad_request("invalid session or interaction ID");
    }
    let request_id = format!("api-interaction-{}", Uuid::new_v4());
    let command = match body.kind.as_str() {
        "permission" => {
            let Some(outcome) = body.outcome else {
                return api_bad_request("permission response requires outcome");
            };
            json!({
                "type": "permission/respond",
                "requestId": request_id,
                "sessionId": session_id,
                "permissionId": interaction_id,
                "outcome": outcome,
            })
        }
        "elicitation" => {
            let Some(response) = body.response else {
                return api_bad_request("elicitation response requires response");
            };
            json!({
                "type": "elicitation/respond",
                "requestId": request_id,
                "sessionId": session_id,
                "elicitationId": interaction_id,
                "response": response,
            })
        }
        _ => return api_bad_request("interaction kind must be permission or elicitation"),
    };
    business_response(state.bridge.business_request(command).await)
}

fn valid_api_identifier(value: &str) -> bool {
    !value.is_empty() && value.encode_utf16().count() <= 1_024
}

fn api_bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({ "error": message })),
    )
        .into_response()
}

fn business_error(error: bridge::BridgeRequestError) -> Response {
    let display = error
        .data
        .as_ref()
        .map_or_else(|| error.message.clone(), serde_json::Value::to_string);
    (
        StatusCode::CONFLICT,
        axum::Json(json!({
            "error": display,
            "message": error.message,
            "code": error.code,
            "data": error.data,
        })),
    )
        .into_response()
}

fn business_response(result: Result<serde_json::Value, bridge::BridgeRequestError>) -> Response {
    match result {
        Ok(value) => axum::Json(value).into_response(),
        Err(message) => business_error(message),
    }
}

fn business_session_view(view: &serde_json::Value) -> Option<serde_json::Value> {
    let session = view.get("session")?;
    let live = view.get("live").unwrap_or(&serde_json::Value::Null);
    let mut terminals = view
        .get("terminals")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(live_terminals) = live.get("terminals").and_then(serde_json::Value::as_object) {
        terminals.extend(live_terminals.clone());
    }
    Some(json!({
        "bridgeEpoch": view.get("bridgeEpoch")?,
        "sessionId": session.get("sessionId")?,
        "sessionIncarnation": session.get("incarnation")?,
        "viewRevision": session.get("viewRevision")?,
        "historyRevision": session.get("historyRevision").cloned().unwrap_or(serde_json::Value::Null),
        "phase": session.get("phase")?,
        "syncError": session.get("syncError").cloned().unwrap_or(serde_json::Value::Null),
        "historyNotice": session.get("historyNotice").cloned().unwrap_or(serde_json::Value::Null),
        "timeline": view.pointer("/baseline/updates").cloned().unwrap_or_else(|| json!([])),
        "turnOutcomes": session.get("turnOutcomes").cloned().unwrap_or_else(|| json!([])),
        "activeTurn": session.get("activeTurn").cloned().unwrap_or(serde_json::Value::Null),
        "workspace": {
            "cwd": live.get("cwd").cloned().unwrap_or(serde_json::Value::Null),
            "session": live.get("session").cloned().unwrap_or(serde_json::Value::Null),
        },
        "controls": live.get("controlState").cloned().unwrap_or_else(|| json!({})),
        "interactions": {
            "permissions": live.get("permissions").cloned().unwrap_or_else(|| json!({})),
            "elicitations": live.get("elicitations").cloned().unwrap_or_else(|| json!({})),
            "urlFlows": live.get("urlFlows").cloned().unwrap_or_else(|| json!({})),
        },
        "operation": live.get("operation").cloned().unwrap_or(serde_json::Value::Null),
        "terminals": terminals,
    }))
}

fn normalize_embedded_session_view(mut value: serde_json::Value) -> Option<serde_json::Value> {
    let view = business_session_view(value.get("view")?)?;
    value["view"] = view;
    Some(value)
}

async fn get_session_view(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    Query(query): Query<SessionViewQuery>,
) -> Response {
    if session_id.is_empty() || session_id.encode_utf16().count() > 1_024 {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": "invalid session ID" })),
        )
            .into_response();
    }
    if query.cwd.as_ref().is_some_and(|cwd| {
        cwd.is_empty() || cwd.encode_utf16().count() > 16_384 || cwd.contains('\0')
    }) {
        return api_bad_request("invalid session cwd");
    }
    match state
        .bridge
        .session_view_with_cwd(session_id, query.cwd)
        .await
    {
        Ok(view) => {
            let revision = view
                .pointer("/session/historyRevision")
                .and_then(serde_json::Value::as_str)
                .map(|revision| format!("\"{revision}\""));
            let Some(view) = business_session_view(&view) else {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(json!({ "error": "bridge returned an invalid session view" })),
                )
                    .into_response();
            };
            let mut response = axum::Json(view).into_response();
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-store"),
            );
            if let Some(revision) = revision
                && let Ok(revision) = axum::http::HeaderValue::from_str(&revision)
            {
                response.headers_mut().insert(header::ETAG, revision);
            }
            response
        }
        Err(error) => session_view_error(error),
    }
}

fn session_view_error(error: bridge::SessionViewError) -> Response {
    match error {
        bridge::SessionViewError::NotFound => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({
                "error": "session was not found in the current agent workspace",
                "code": "session_not_found",
            })),
        )
            .into_response(),
        bridge::SessionViewError::Unavailable(message)
            if message == "session is not materialized" =>
        {
            (
                StatusCode::CONFLICT,
                axum::Json(json!({ "error": message })),
            )
                .into_response()
        }
        bridge::SessionViewError::Unavailable(message) => (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({ "error": message })),
        )
            .into_response(),
    }
}

async fn start_session_turn(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<StartTurnBody>,
) -> Response {
    if session_id.is_empty() || session_id.encode_utf16().count() > 1_024 {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": "invalid session ID" })),
        )
            .into_response();
    }
    let Some(history_revision) = headers
        .get(header::IF_MATCH)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_strong_etag)
    else {
        return (
            StatusCode::PRECONDITION_REQUIRED,
            axum::Json(
                json!({ "error": "If-Match with the current history revision is required" }),
            ),
        )
            .into_response();
    };
    let Some(client_intent_id) = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.encode_utf16().count() <= 1_024)
        .map(str::to_string)
    else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": "a bounded Idempotency-Key is required" })),
        )
            .into_response();
    };
    if body.prompt.is_empty() || body.prompt.len() > 64 {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": "prompt must contain between 1 and 64 content blocks" })),
        )
            .into_response();
    }
    match state
        .bridge
        .start_turn(session_id, history_revision, client_intent_id, body.prompt)
        .await
    {
        Ok(ack) => (StatusCode::ACCEPTED, axum::Json(ack)).into_response(),
        Err(message) => (
            StatusCode::CONFLICT,
            axum::Json(json!({ "error": message })),
        )
            .into_response(),
    }
}

fn parse_strong_etag(value: &str) -> Option<String> {
    if value.starts_with("W/") || value.len() < 2 {
        return None;
    }
    value
        .strip_prefix('"')?
        .strip_suffix('"')
        .filter(|value| !value.is_empty() && value.len() <= 4_096)
        .map(str::to_string)
}

async fn session_events(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    Query(query): Query<SessionViewQuery>,
) -> Response {
    if session_id.is_empty() || session_id.encode_utf16().count() > 1_024 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if query.cwd.as_ref().is_some_and(|cwd| {
        cwd.is_empty() || cwd.encode_utf16().count() > 16_384 || cwd.contains('\0')
    }) {
        return api_bad_request("invalid session cwd");
    }
    let Some(subscription) = state.bridge.subscribe_session(session_id.clone()).await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let initial = match state
        .bridge
        .session_view_with_cwd(session_id.clone(), query.cwd)
        .await
    {
        Ok(view) => match session_reset_value(&session_id, &view) {
            Some(reset) => reset.to_string(),
            None => {
                state
                    .bridge
                    .unsubscribe(subscription.id, subscription.generation)
                    .await;
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        },
        Err(error) => {
            state
                .bridge
                .unsubscribe(subscription.id, subscription.generation)
                .await;
            return session_view_error(error);
        }
    };
    let guard = SubscriptionGuard {
        bridge: state.bridge,
        id: subscription.id,
        generation: subscription.generation,
    };
    let initial_position = session_event_position(&initial);
    let stream = futures::stream::unfold(
        (
            Some(initial),
            subscription.initial_events.into_iter(),
            subscription.events,
            session_id,
            initial_position,
            guard,
        ),
        |(mut initial, mut bootstrap, mut events, session_id, mut position, guard)| async move {
            loop {
                let is_initial = initial.is_some();
                let event = if let Some(initial_event) = initial.take() {
                    initial_event
                } else if let Some(event) = bootstrap.next() {
                    event
                } else {
                    let event = events.recv().await?;
                    event.into_string()
                };
                if !session_event_matches(&event, &session_id) {
                    continue;
                }
                let next_position = session_event_position(&event);
                if !is_initial
                    && let (Some(current), Some(next)) = (&position, &next_position)
                    && current.0 == next.0
                    && current.1 == next.1
                    && next.2 <= current.2
                {
                    continue;
                }
                if next_position.is_some() {
                    position = next_position;
                }
                let id = session_event_cursor(&event);
                let mut outgoing = SseEvent::default().data(event);
                if let Some(id) = id {
                    outgoing = outgoing.id(id);
                }
                return Some((
                    Ok::<_, Infallible>(outgoing),
                    (initial, bootstrap, events, session_id, position, guard),
                ));
            }
        },
    );
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn session_event_matches(event: &str, session_id: &str) -> bool {
    session_event_id(event).as_deref() == Some(session_id)
}

fn session_event_id(event: &str) -> Option<String> {
    let Ok(event) = serde_json::from_str::<serde_json::Value>(event) else {
        return None;
    };
    if !matches!(
        event.get("type").and_then(serde_json::Value::as_str),
        Some(
            "bridge/session_reset"
                | "bridge/session_delta"
                | "bridge/session_turn_complete"
                | "bridge/session_turn_failed"
        )
    ) {
        return None;
    }
    event
        .get("sessionId")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn session_event_cursor(event: &str) -> Option<String> {
    let (epoch, incarnation, revision) = session_event_position(event)?;
    Some(format!("{epoch}:{incarnation}:{revision}"))
}

fn session_event_position(event: &str) -> Option<(String, u64, u64)> {
    let event = serde_json::from_str::<serde_json::Value>(event).ok()?;
    if !matches!(
        event.get("type").and_then(serde_json::Value::as_str),
        Some("bridge/session_reset" | "bridge/session_delta")
    ) {
        return None;
    }
    Some((
        event.get("bridgeEpoch")?.as_str()?.to_string(),
        event.get("sessionIncarnation")?.as_u64()?,
        event.get("viewRevision")?.as_u64()?,
    ))
}

async fn frontend(proxy: Option<DevProxy>, request: Request) -> Response {
    if request.uri().path() == "/api" || request.uri().path().starts_with("/api/") {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "API route not found" })),
        )
            .into_response();
    }
    if let Some(proxy) = proxy {
        return proxy.forward(request).await;
    }
    #[cfg(feature = "dev")]
    {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "Development builds require --dev-server",
        )
            .into_response()
    }
    #[cfg(not(feature = "dev"))]
    {
        if !matches!(*request.method(), Method::GET | Method::HEAD) {
            return (
                StatusCode::METHOD_NOT_ALLOWED,
                [(header::ALLOW, "GET, HEAD")],
            )
                .into_response();
        }
        // Axum strips HEAD bodies after deriving the corresponding GET length.
        static_asset(request.uri().clone()).await
    }
}

#[cfg(not(feature = "dev"))]
async fn static_asset(uri: axum::http::Uri) -> Response {
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
        .header(header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'")
        .header(header::X_FRAME_OPTIONS, "DENY")
        .body(Body::from(asset.data))
        .expect("valid embedded asset response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_state::{RuntimeLimits, RuntimeState};
    use clap::Parser;
    use tower::ServiceExt;

    #[cfg(not(feature = "dev"))]
    #[tokio::test]
    async fn embedded_frontend_head_keeps_the_get_content_length() {
        let options = Options::try_parse_from(["attyd", "--", "fake-agent"]).unwrap();
        let app = app_router(&options, BridgeHub::new(Arc::new(options.clone())));
        let request = |method| {
            Request::builder()
                .method(method)
                .uri("/")
                .header(header::HOST, "localhost:7331")
                .body(Body::empty())
                .unwrap()
        };
        let get = app.clone().oneshot(request("GET")).await.unwrap();
        let head = app.oneshot(request("HEAD")).await.unwrap();
        assert_eq!(get.status(), StatusCode::OK);
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(
            get.headers()[header::CONTENT_LENGTH],
            head.headers()[header::CONTENT_LENGTH]
        );
        assert_ne!(head.headers()[header::CONTENT_LENGTH], "0");
        assert!(
            axum::body::to_bytes(head.into_body(), 100)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(feature = "dev")]
    #[tokio::test]
    async fn development_build_requires_an_explicit_frontend_server() {
        let options = Options::try_parse_from(["attyd", "--", "fake-agent"]).unwrap();
        assert!(
            serve(options)
                .await
                .unwrap_err()
                .to_string()
                .contains("requires --dev-server")
        );
    }

    #[tokio::test]
    async fn development_proxy_keeps_api_routes_and_origin_checks_in_rust() {
        let options = Options::try_parse_from([
            "attyd",
            "--dev-server",
            "http://127.0.0.1:5173",
            "--",
            "fake-agent",
        ])
        .unwrap();
        let app = app_router(&options, BridgeHub::new(Arc::new(options.clone())));
        for (path, method, origin, expected) in [
            ("/api/health", "GET", None, StatusCode::OK),
            ("/api/health", "POST", None, StatusCode::METHOD_NOT_ALLOWED),
            ("/api", "GET", None, StatusCode::NOT_FOUND),
            ("/api/unknown", "GET", None, StatusCode::NOT_FOUND),
            ("/api/unknown", "POST", None, StatusCode::NOT_FOUND),
            (
                "/api/health",
                "GET",
                Some("http://localhost:5173"),
                StatusCode::FORBIDDEN,
            ),
            (
                "/src/app.tsx",
                "GET",
                Some("http://localhost:5173"),
                StatusCode::FORBIDDEN,
            ),
        ] {
            let mut request = Request::builder()
                .uri(path)
                .method(method)
                .header(header::HOST, "localhost:7331");
            if let Some(origin) = origin {
                request = request.header(header::ORIGIN, origin);
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{method} {path}");
            if expected == StatusCode::NOT_FOUND {
                assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
            }
        }
    }

    impl CanonicalProjection {
        fn update(&mut self, event: &str) -> bool {
            self.update_with_public_event(event).0
        }
    }

    fn test_hub() -> Arc<BridgeHub> {
        let options = Options::try_parse_from(["attyd", "--", "fake-agent"]).unwrap();
        BridgeHub::new(Arc::new(options))
    }

    #[test]
    fn origin_boundary_allows_direct_local_and_explicit_proxy_origins_only() {
        let policy = OriginPolicy {
            allowed_origins: vec!["https://agent.example".to_string()],
        };
        for (host, origin, allowed) in [
            ("127.0.0.1:7331", None, true),
            ("localhost:7331", Some("http://localhost:7331"), true),
            ("[::1]:7331", Some("http://[::1]:7331"), true),
            ("192.168.1.20:7331", Some("http://192.168.1.20:7331"), true),
            ("127.0.0.1:7331", Some("http://127.0.0.1:8000"), false),
            ("127.0.0.1:7331", Some("https://evil.example"), false),
            ("127.0.0.1:7331", Some("null"), false),
            ("evil.example:7331", None, false),
            ("evil.example:7331", Some("http://evil.example:7331"), false),
            ("localhost.evil.example:7331", None, false),
            ("user@localhost:7331", None, false),
            ("localhost:7331/path", None, false),
            ("agent.example", Some("https://agent.example"), true),
            ("agent.example:443", Some("https://agent.example"), true),
            ("agent.example:8443", Some("https://agent.example"), false),
            ("agent.example", Some("http://agent.example"), false),
            ("agent.example", None, true),
            ("127.0.0.1:7331", Some("https://agent.example"), true),
            ("evil.example", Some("https://agent.example"), false),
        ] {
            let mut request = Request::builder()
                .uri("/api/v1/auth/logout")
                .header(header::HOST, host);
            if let Some(origin) = origin {
                request = request.header(header::ORIGIN, origin);
            }
            assert_eq!(
                policy.allows(&request.body(Body::empty()).unwrap()),
                allowed,
                "Host={host}, Origin={origin:?}"
            );
        }
    }

    #[test]
    fn origin_boundary_rejects_ambiguous_headers_and_ignores_forwarded_headers() {
        let policy = OriginPolicy {
            allowed_origins: Vec::new(),
        };
        for headers in [
            vec![("host", "localhost:7331"), ("host", "evil.example")],
            vec![
                ("host", "localhost:7331"),
                ("origin", "http://localhost:7331"),
                ("origin", "http://evil.example"),
            ],
            vec![
                ("host", "evil.example"),
                ("x-forwarded-host", "localhost:7331"),
            ],
            vec![
                ("host", "localhost:7331"),
                ("origin", "https://localhost:7331"),
                ("x-forwarded-proto", "https"),
            ],
        ] {
            let mut request = Request::builder().uri("/api/v1/runtime");
            for (name, value) in headers {
                request = request.header(name, value);
            }
            assert!(!policy.allows(&request.body(Body::empty()).unwrap()));
        }
        assert!(!policy.allows(&Request::new(Body::empty())));
        assert!(
            policy.allows(
                &Request::builder()
                    .uri("http://localhost:7331/api/v1/runtime")
                    .body(Body::empty())
                    .unwrap()
            )
        );
    }

    #[tokio::test]
    async fn shutdown_closes_event_streams_without_an_active_agent() {
        let hub = test_hub();
        let (global_tx, mut global_rx) = SubscriberSender::channel(4);
        let (session_tx, mut session_rx) = SubscriberSender::channel(4);
        {
            let mut state = hub.state.lock().await;
            state.subscribers.insert(1, global_tx);
            state
                .session_subscribers
                .insert(2, ("session".to_string(), session_tx));
        }
        hub.shutdown().await;
        tokio::time::timeout(Duration::from_millis(100), async {
            assert!(global_rx.recv().await.is_none());
            assert!(session_rx.recv().await.is_none());
        })
        .await
        .expect("shutdown must close every SSE source before HTTP draining");
        assert!(hub.subscribe().await.is_none());
        assert!(hub.subscribe_session("session".to_string()).await.is_none());
    }

    #[test]
    fn turn_append_requires_a_strong_bounded_history_etag() {
        assert_eq!(
            parse_strong_etag("\"epoch:1:7\""),
            Some("epoch:1:7".to_string())
        );
        assert_eq!(parse_strong_etag("W/\"epoch:1:7\""), None);
        assert_eq!(parse_strong_etag("epoch:1:7"), None);
        assert_eq!(parse_strong_etag("\"\""), None);
    }

    #[test]
    fn session_sse_accepts_only_business_events_and_uses_reset_cursor() {
        let reset = json!({
            "type": "bridge/session_reset",
            "bridgeEpoch": "epoch",
            "sessionId": "wanted",
            "sessionIncarnation": 3,
            "viewRevision": 9,
        })
        .to_string();
        let other = json!({
            "type": "acp/session_update",
            "notification": { "sessionId": "other", "update": {} },
        })
        .to_string();
        let runtime = json!({
            "type": "bridge/runtime_delta",
            "delta": {
                "change": {
                    "kind": "session_upsert",
                    "session": { "sessionId": "wanted" },
                },
            },
        })
        .to_string();
        let turn_complete = json!({
            "type": "bridge/session_turn_complete",
            "sessionId": "wanted",
            "operationId": "turn-1",
            "response": { "stopReason": "end_turn" },
        })
        .to_string();

        assert!(session_event_matches(&reset, "wanted"));
        let (_, relayed_turn) =
            business_session_event(&turn_complete).expect("turn outcome is session business data");
        assert!(session_event_matches(&relayed_turn, "wanted"));
        assert_eq!(session_event_cursor(&relayed_turn), None);
        assert!(!session_event_matches(&other, "wanted"));
        assert!(!session_event_matches(&runtime, "wanted"));
        assert_eq!(session_event_cursor(&reset).as_deref(), Some("epoch:3:9"));
    }

    #[test]
    fn full_session_views_are_reduced_to_reset_tokens_for_sse() {
        let full = json!({
            "type": "bridge/session_view",
            "sessionId": "wanted",
            "view": {
                "bridgeEpoch": "epoch",
                "session": {
                    "incarnation": 3,
                    "viewRevision": 9,
                    "historyRevision": "history-9",
                    "phase": "ready",
                    "syncError": null,
                },
                "baseline": { "updates": [{ "large": "payload" }] },
            },
        })
        .to_string();
        let (_, event) = business_session_event(&full).expect("session view becomes reset");
        let event: serde_json::Value = serde_json::from_str(&event).unwrap();

        assert_eq!(event["type"], "bridge/session_reset");
        assert_eq!(event["historyRevision"], "history-9");
        assert!(event.get("baseline").is_none());
    }

    #[test]
    fn rest_session_view_is_a_business_projection_without_legacy_runtime_envelopes() {
        let internal = json!({
            "bridgeEpoch": "epoch",
            "session": {
                "sessionId": "session",
                "incarnation": 2,
                "viewRevision": 7,
                "historyRevision": "history-7",
                "phase": "running",
                "syncError": null,
                "activeTurn": { "operationId": "turn", "updates": [] },
                "turnOutcomes": [{
                    "operationId": "prior-turn",
                    "afterUpdate": 1,
                    "response": { "stopReason": "end_turn" },
                }],
            },
            "baseline": {
                "updates": [{ "sessionUpdate": "agent_message_chunk" }],
                "bytes": 42,
                "digest": "debug-only",
            },
            "terminals": {
                "released": { "output": "retained", "released": true },
                "overlap": { "output": "old", "released": true },
            },
            "live": {
                "cwd": "/workspace",
                "session": { "title": "Agent session" },
                "controlState": { "current_mode_update": { "currentModeId": "plan" } },
                "permissions": { "permission": { "request": {} } },
                "elicitations": {},
                "urlFlows": {},
                "terminals": {
                    "live": { "output": "running", "released": false },
                    "overlap": { "output": "current", "released": false },
                },
            },
        });
        let view = business_session_view(&internal).expect("valid internal projection");

        assert_eq!(view["sessionId"], "session");
        assert_eq!(view["timeline"].as_array().unwrap().len(), 1);
        assert_eq!(view["turnOutcomes"][0]["operationId"], "prior-turn");
        assert_eq!(view["activeTurn"]["operationId"], "turn");
        assert_eq!(view["workspace"]["cwd"], "/workspace");
        assert_eq!(view["terminals"]["released"]["output"], "retained");
        assert_eq!(view["terminals"]["live"]["output"], "running");
        assert_eq!(view["terminals"]["overlap"]["output"], "current");
        assert!(view.get("baseline").is_none());
        assert!(view.get("live").is_none());
    }

    #[test]
    fn canonical_terminal_deltas_reconstruct_live_output_without_session_copies() {
        let mut runtime = RuntimeState::new("epoch", RuntimeLimits::default());
        let incarnation = runtime
            .open_new(
                "epoch",
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        let snapshot = runtime.snapshot();
        let mut projection = CanonicalProjection {
            snapshot: Some(snapshot.clone()),
        };
        for output in ["first", "second"] {
            runtime
                .upsert_terminal(
                    "epoch",
                    "session",
                    incarnation,
                    "terminal",
                    json!({
                        "sessionId": "session", "terminalId": "terminal",
                        "output": output, "outputAppend": true, "released": false,
                    }),
                )
                .unwrap();
        }
        for delta in runtime.deltas_after(snapshot.through_seq).unwrap() {
            assert!(projection.apply_delta(delta));
        }
        assert_eq!(
            projection.snapshot.as_ref().unwrap().sessions["session"].terminals["terminal"]["output"],
            "firstsecond"
        );
        assert_eq!(
            projection.snapshot.as_ref().unwrap().sessions,
            runtime.snapshot().sessions
        );
    }

    #[tokio::test]
    async fn session_subscription_does_not_queue_unrelated_session_traffic() {
        let hub = test_hub();
        let (input, _commands) = mpsc::channel(1);
        {
            let mut state = hub.state.lock().await;
            state.generation = 1;
            state.input = Some(input);
        }
        let mut subscription = hub
            .subscribe_session("wanted".to_string())
            .await
            .expect("session subscription");

        for index in 0..SUBSCRIBER_QUEUE_CAPACITY * 2 {
            hub.publish(
                1,
                json!({
                    "type": "bridge/session_delta",
                    "bridgeEpoch": "epoch",
                    "sessionId": "other",
                    "sessionIncarnation": 1,
                    "fromRevision": index,
                    "viewRevision": index + 1,
                    "change": { "kind": "noise" },
                })
                .to_string(),
            )
            .await;
        }
        hub.publish(
            1,
            json!({
                "type": "bridge/session_delta",
                "bridgeEpoch": "epoch",
                "sessionId": "wanted",
                "sessionIncarnation": 1,
                "fromRevision": 0,
                "viewRevision": 1,
                "change": { "kind": "visible" },
            })
            .to_string(),
        )
        .await;

        let event = tokio::time::timeout(Duration::from_millis(100), subscription.events.recv())
            .await
            .expect("wanted event was not routed")
            .expect("session subscription was evicted by unrelated traffic");
        let event: serde_json::Value = serde_json::from_str(&event.into_string()).unwrap();
        assert_eq!(event["sessionId"], "wanted");
        assert_eq!(event["change"]["kind"], "visible");
        assert!(subscription.events.try_recv().is_err());

        hub.unsubscribe(subscription.id, subscription.generation)
            .await;
        assert!(hub.state.lock().await.session_subscribers.is_empty());
    }

    #[tokio::test]
    async fn unobserved_close_cancels_on_return_and_rearms_after_disconnect() {
        let mut options = (*test_hub().options).clone();
        options.session_unobserved_timeout = 0;
        let hub = BridgeHub::new(Arc::new(options));
        let (input, mut commands) = mpsc::channel(4);
        let mut runtime = crate::runtime_state::RuntimeState::new("test", Default::default());
        runtime
            .open_new_with_replay(
                "test",
                "session",
                "/tmp",
                json!({"sessionId":"session"}),
                Vec::new(),
            )
            .unwrap();
        {
            let mut state = hub.state.lock().await;
            state.generation = 1;
            state.input = Some(input);
            state.canonical.snapshot = Some(runtime.snapshot());
            hub.update_auto_close(&mut state);
        }
        let command = commands.recv().await.unwrap();
        let bridge::BridgeInput::RetireUnobservedSession { permit: old, .. } = command else {
            panic!("expected close");
        };
        let first = hub.subscribe_session("session".into()).await.unwrap();
        let second = hub.subscribe_session("session".into()).await.unwrap();
        assert!(!old.claim(), "return cancels even a queued close");
        hub.unsubscribe(first.id, first.generation).await;
        assert!(hub.state.lock().await.unobserved.is_empty());
        hub.unsubscribe(second.id, second.generation).await;
        let command = commands.recv().await.unwrap();
        let bridge::BridgeInput::RetireUnobservedSession { permit, .. } = command else {
            panic!("expected close");
        };
        assert!(
            permit.claim(),
            "last observer leaving starts a fresh interval"
        );
        hub.shutdown().await;
    }

    async fn timer_hub(timeout: i64) -> (Arc<BridgeHub>, mpsc::Receiver<bridge::BridgeInput>) {
        let mut options = (*test_hub().options).clone();
        options.session_unobserved_timeout = timeout;
        let hub = BridgeHub::new(Arc::new(options));
        let (input, commands) = mpsc::channel(32);
        let mut runtime = crate::runtime_state::RuntimeState::new("test", Default::default());
        runtime
            .open_new_with_replay(
                "test",
                "session",
                "/tmp",
                json!({"sessionId":"session"}),
                vec![],
            )
            .unwrap();
        let mut state = hub.state.lock().await;
        state.generation = 1;
        state.input = Some(input);
        state.canonical.snapshot = Some(runtime.snapshot());
        hub.update_auto_close(&mut state);
        drop(state);
        tokio::task::yield_now().await;
        (hub, commands)
    }

    #[tokio::test(start_paused = true)]
    async fn unobserved_timeout_measures_absence_not_output_and_global_observers_do_not_count() {
        let (hub, mut commands) = timer_hub(30).await;
        let global = hub.subscribe().await.unwrap();
        tokio::time::advance(Duration::from_secs(20)).await;
        hub.publish(1, json!({"type":"acp/session_update","sessionId":"session", "update": {"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"output"}}}).to_string()).await;
        tokio::time::advance(Duration::from_secs(9)).await;
        tokio::task::yield_now().await;
        assert!(commands.try_recv().is_err());
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        let bridge::BridgeInput::RetireUnobservedSession { permit, .. } =
            commands.try_recv().unwrap()
        else {
            panic!("expected timer close");
        };
        assert!(permit.claim());
        hub.unsubscribe(global.id, global.generation).await;
        hub.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn observer_return_restarts_the_full_timeout_and_shutdown_cancels_it() {
        let (hub, mut commands) = timer_hub(30).await;
        tokio::time::advance(Duration::from_secs(20)).await;
        let observer = hub.subscribe_session("session".into()).await.unwrap();
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(commands.try_recv().is_err());
        hub.unsubscribe(observer.id, observer.generation).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(29)).await;
        tokio::task::yield_now().await;
        assert!(commands.try_recv().is_err());
        hub.shutdown().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert!(commands.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn negative_timeouts_disable_recycling_and_huge_values_do_not_overflow() {
        for timeout in [i64::MIN, -2, -1, i64::MAX] {
            let (hub, mut commands) = timer_hub(timeout).await;
            tokio::time::advance(Duration::from_secs(1_000_000)).await;
            tokio::task::yield_now().await;
            assert!(commands.try_recv().is_err());
            hub.shutdown().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn changed_incarnation_invalidates_a_queued_timer() {
        let (hub, mut commands) = timer_hub(0).await;
        let bridge::BridgeInput::RetireUnobservedSession {
            permit: old,
            incarnation,
            ..
        } = commands.try_recv().unwrap()
        else {
            panic!("expected close");
        };
        {
            let mut state = hub.state.lock().await;
            state
                .canonical
                .snapshot
                .as_mut()
                .unwrap()
                .sessions
                .get_mut("session")
                .unwrap()
                .incarnation += 1;
            hub.update_auto_close(&mut state);
        }
        assert!(!old.claim());
        tokio::task::yield_now().await;
        let bridge::BridgeInput::RetireUnobservedSession {
            permit,
            incarnation: next,
            ..
        } = commands.try_recv().unwrap()
        else {
            panic!("expected new timer");
        };
        assert_eq!(next, incarnation + 1);
        assert!(permit.claim());
        hub.shutdown().await;
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

    async fn capability_fixture(flags: &[&str]) -> (Arc<BridgeHub>, BridgeSubscription) {
        capability_fixture_with_timeout(flags, 1800).await
    }

    async fn capability_fixture_with_timeout(
        flags: &[&str],
        timeout: i64,
    ) -> (Arc<BridgeHub>, BridgeSubscription) {
        let cwd = env!("CARGO_MANIFEST_DIR");
        let fixture = format!("{cwd}/tests/fixtures/session-capabilities-agent.ts");
        let mut args = vec![
            "attyd", "--cwd", cwd, "--", "node", "--import", "tsx", &fixture,
        ];
        args.extend_from_slice(flags);
        let mut options = Options::try_parse_from(args).unwrap().normalized().unwrap();
        options.session_unobserved_timeout = timeout;
        let hub = BridgeHub::new(Arc::new(options));
        let mut observer = hub.subscribe().await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = observer.events.recv().await {
                let event: serde_json::Value = serde_json::from_str(&event.into_string()).unwrap();
                assert_ne!(event["type"], "bridge/error", "{event}");
                if event["type"] == "bridge/phase" && event["phase"] == "ready" {
                    return;
                }
            }
            panic!("fixture disconnected before ready");
        })
        .await
        .unwrap();
        (hub, observer)
    }

    #[tokio::test]
    async fn independent_session_capabilities_restore_without_listing() {
        let (hub, observer) = capability_fixture(&["--no-list"]).await;
        let response = get_session_view(
            Path("saved".to_string()),
            State(AppState {
                bridge: hub.clone(),
            }),
            Query(SessionViewQuery {
                cwd: Some(env!("CARGO_MANIFEST_DIR").to_string()),
            }),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "known ID and cwd should not require list capability"
        );
        let view = hub.session_view("saved".to_string()).await.unwrap();
        assert_eq!(view["session"]["phase"], "ready");
        assert!(view["baseline"]["updates"].to_string().contains("Loaded"));
        hub.unsubscribe(observer.id, observer.generation).await;
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn independent_session_known_workspace_survives_another_observer_clearing_discovery() {
        let (hub, observer) = capability_fixture(&["--split-list"]).await;
        let first = hub
            .business_request(json!({ "type": "session/list", "requestId": "first" }))
            .await
            .unwrap();
        let discovered = hub
            .business_request(json!({
                "type": "session/list", "requestId": "next", "cursor": first["nextCursor"],
            }))
            .await
            .unwrap();
        assert_eq!(discovered["sessions"][0]["sessionId"], "saved");
        hub.business_request(json!({ "type": "session/list", "requestId": "other-observer" }))
            .await
            .unwrap();
        // The browser retains the row's workspace even after the shared metadata
        // cache is replaced. Its SSE handshake must materialize the known ID.
        let stream = session_events(
            Path("saved".to_string()),
            State(AppState {
                bridge: hub.clone(),
            }),
            Query(SessionViewQuery {
                cwd: Some(env!("CARGO_MANIFEST_DIR").to_string()),
            }),
        )
        .await;
        assert_eq!(stream.status(), StatusCode::OK);
        let view = get_session_view(
            Path("saved".to_string()),
            State(AppState {
                bridge: hub.clone(),
            }),
            Query(SessionViewQuery {
                cwd: Some("/ignored-route-workspace".to_string()),
            }),
        )
        .await;
        assert_eq!(view.status(), StatusCode::OK);
        let body = axum::body::to_bytes(view.into_body(), 64 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["workspace"]["cwd"], env!("CARGO_MANIFEST_DIR"));
        drop(stream);
        hub.unsubscribe(observer.id, observer.generation).await;
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn independent_session_missing_cold_link_stays_not_found() {
        let (hub, observer) = capability_fixture(&["--no-list"]).await;
        for _ in 0..2 {
            let response = get_session_view(
                Path("missing".to_string()),
                State(AppState {
                    bridge: hub.clone(),
                }),
                Query(SessionViewQuery {
                    cwd: Some(env!("CARGO_MANIFEST_DIR").to_string()),
                }),
            )
            .await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        hub.unsubscribe(observer.id, observer.generation).await;
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn independent_session_list_cursors_survive_other_observers_refreshing() {
        let (hub, observer) = capability_fixture(&[]).await;
        let (a, b) = tokio::join!(
            hub.business_request(json!({ "type": "session/list", "requestId": "list-a" })),
            hub.business_request(json!({ "type": "session/list", "requestId": "list-b" })),
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_ne!(a["nextCursor"], b["nextCursor"]);
        for (index, first) in [a, b].into_iter().enumerate() {
            let next = hub.business_request(json!({
                "type": "session/list", "requestId": format!("next-{index}"), "cursor": first["nextCursor"],
            })).await.expect("another observer must not revoke this Agent-provided cursor");
            assert_eq!(next["_meta"]["requestedCursor"], first["nextCursor"]);
        }
        hub.unsubscribe(observer.id, observer.generation).await;
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn independent_session_list_pages_budget_retained_metadata() {
        let (hub, observer) = capability_fixture(&["--large-list-meta"]).await;
        let first = hub
            .business_request(json!({ "type": "session/list", "requestId": "first" }))
            .await
            .unwrap();
        for index in 0..9 {
            hub.business_request(json!({
                "type": "session/list", "requestId": format!("observer-{index}"), "cursor": first["nextCursor"],
            })).await.expect("repeated observers must not spend the retained metadata budget again");
        }
        hub.unsubscribe(observer.id, observer.generation).await;
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn resume_and_fork_work_without_history_loading() {
        let (hub, observer) = capability_fixture(&["--no-load"]).await;
        hub.business_request(json!({ "type": "session/new", "requestId": "new" }))
            .await
            .unwrap();
        let fork = hub
            .business_request(json!({
                "type": "session/fork", "requestId": "fork", "sessionId": "created",
            }))
            .await
            .expect("fork capability is independent of load");
        assert_eq!(fork["sessionId"], "forked");
        assert_eq!(fork["view"]["session"]["phase"], "ready");
        assert!(
            fork["view"]["session"]["historyNotice"]
                .as_str()
                .unwrap()
                .contains("unavailable")
        );
        hub.business_request(json!({ "type": "session/list", "requestId": "list" }))
            .await
            .unwrap();
        let resumed = hub
            .session_view("saved".into())
            .await
            .expect("cold observation uses resume when load is absent");
        assert_eq!(resumed["session"]["phase"], "ready");
        assert!(resumed["session"]["historyNotice"].is_string());
        let listed = hub
            .business_request(json!({ "type": "session/list", "requestId": "counts" }))
            .await
            .unwrap();
        assert_eq!(listed["_meta"]["forks"], 1);
        assert_eq!(listed["_meta"]["resumes"], 1);
        assert_eq!(listed["_meta"]["loads"], 0);
        let revision = resumed["session"]["historyRevision"]
            .as_str()
            .unwrap()
            .to_string();
        hub.start_turn(
            "saved".into(),
            revision,
            "continue".into(),
            vec![json!({"type":"text","text":"Continue"})],
        )
        .await
        .expect("missing earlier history must not block prompting");
        hub.unsubscribe(observer.id, observer.generation).await;
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn fork_history_failure_uses_source_cache_and_empty_success_is_authoritative() {
        for empty in [false, true] {
            let flags = if empty {
                vec!["--empty-fork-history"]
            } else {
                vec![]
            };
            let (hub, observer) = capability_fixture(&flags).await;
            hub.business_request(json!({ "type": "session/list", "requestId": "list" }))
                .await
                .unwrap();
            let source = hub.session_view("saved".into()).await.unwrap();
            let fork = hub
                .business_request(json!({
                    "type": "session/fork", "requestId": "fork", "sessionId": "saved",
                }))
                .await
                .expect("a successful fork survives a failed history load");
            assert_eq!(fork["sessionId"], "forked");
            assert_eq!(fork["view"]["session"]["phase"], "ready");
            if empty {
                assert_eq!(fork["view"]["baseline"]["updates"], json!([]));
                assert!(fork["view"]["session"]["historyNotice"].is_null());
            } else {
                assert_eq!(
                    fork["view"]["baseline"]["updates"],
                    source["baseline"]["updates"]
                );
                assert!(
                    fork["view"]["session"]["historyNotice"]
                        .as_str()
                        .unwrap()
                        .contains("source session")
                );
            }
            for _ in 0..2 {
                hub.session_view("forked".into()).await.unwrap();
            }
            let counts = hub
                .business_request(json!({ "type": "session/list", "requestId": "counts" }))
                .await
                .unwrap();
            assert_eq!(
                counts["_meta"]["forks"], 1,
                "view refresh must not fork again"
            );
            assert_eq!(
                counts["_meta"]["loads"], 2,
                "view refresh reuses materialized memory"
            );
            hub.unsubscribe(observer.id, observer.generation).await;
            hub.shutdown().await;
        }
    }

    #[tokio::test]
    async fn active_prompt_controls_and_close_preserve_a_reopened_incarnation() {
        let (hub, observer) = capability_fixture(&["--no-load", "--close"]).await;
        let created = hub
            .business_request(json!({"type":"session/new", "requestId":"new"}))
            .await
            .unwrap();
        let initial = created["view"]["session"]["incarnation"].as_u64().unwrap();
        hub.start_turn(
            "created".into(),
            created["view"]["session"]["historyRevision"]
                .as_str()
                .unwrap()
                .into(),
            "old-turn".into(),
            vec![json!({"type":"text","text":"wait-close"})],
        )
        .await
        .unwrap();
        // Both controls are sent without cancelling or redispatching the prompt.
        hub.business_request(json!({"type":"session/set_mode", "requestId":"mode", "sessionId":"created", "modeId":"plan"})).await.unwrap();
        hub.business_request(json!({"type":"session/set_config_option", "requestId":"config", "sessionId":"created", "configId":"verbose", "value":true})).await.unwrap();
        assert_eq!(
            hub.session_view("created".into()).await.unwrap()["session"]["phase"],
            "running"
        );
        hub.business_request(
            json!({"type":"session/close", "requestId":"close", "sessionId":"created"}),
        )
        .await
        .unwrap();
        hub.business_request(json!({"type":"session/resume", "requestId":"resume", "sessionId":"created", "cwd":env!("CARGO_MANIFEST_DIR")})).await.unwrap();
        let reopened = hub.session_view("created".into()).await.unwrap();
        assert!(reopened["session"]["incarnation"].as_u64().unwrap() > initial);
        hub.start_turn(
            "created".into(),
            reopened["session"]["historyRevision"]
                .as_str()
                .unwrap()
                .into(),
            "new-turn".into(),
            vec![json!({"type":"text","text":"wait-close"})],
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(350)).await;
        let current = hub.session_view("created".into()).await.unwrap();
        assert_eq!(
            current["session"]["phase"], "running",
            "late old response must not finish the new turn"
        );
        assert_eq!(
            current["session"]["activeTurn"]["clientIntentId"],
            "new-turn"
        );
        let counts = hub
            .business_request(json!({"type":"session/list", "requestId":"counts"}))
            .await
            .unwrap();
        assert_eq!(counts["_meta"]["prompts"], 2);
        assert_eq!(counts["_meta"]["controls"], 2);
        assert_eq!(counts["_meta"]["closes"], 1);
        hub.business_request(
            json!({"type":"session/close", "requestId":"close-new", "sessionId":"created"}),
        )
        .await
        .unwrap();
        hub.unsubscribe(observer.id, observer.generation).await;
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn close_refusal_keeps_running_turn_and_cached_messages() {
        let (hub, observer) = capability_fixture(&["--close", "--fail-close"]).await;
        let created = hub
            .business_request(json!({"type":"session/new", "requestId":"new"}))
            .await
            .unwrap();
        hub.start_turn(
            "created".into(),
            created["view"]["session"]["historyRevision"]
                .as_str()
                .unwrap()
                .into(),
            "turn".into(),
            vec![json!({"type":"text","text":"wait-close"})],
        )
        .await
        .unwrap();
        let error = hub
            .business_request(
                json!({"type":"session/close", "requestId":"close", "sessionId":"created"}),
            )
            .await
            .unwrap_err();
        assert!(error.message.contains("Close refused"));
        let current = hub.session_view("created".into()).await.unwrap();
        assert_eq!(current["session"]["phase"], "running");
        assert_eq!(current["session"]["activeTurn"]["clientIntentId"], "turn");
        assert!(current["live"]["operation"].is_null());
        hub.unsubscribe(observer.id, observer.generation).await;
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn expired_unobserved_timer_closes_running_work_and_does_not_retry_refusal() {
        for refused in [false, true] {
            let flags = if refused {
                vec!["--close", "--fail-close"]
            } else {
                vec!["--close"]
            };
            let (hub, observer) = capability_fixture_with_timeout(&flags, 0).await;
            // Observe before creation so zero means exactly the subsequent disconnect.
            let session_stream = hub.subscribe_session("created".into()).await.unwrap();
            let created = hub
                .business_request(json!({"type":"session/new", "requestId":"new"}))
                .await
                .unwrap();
            hub.start_turn(
                "created".into(),
                created["view"]["session"]["historyRevision"]
                    .as_str()
                    .unwrap()
                    .into(),
                "turn".into(),
                vec![json!({"type":"text","text":"wait-close"})],
            )
            .await
            .unwrap();
            hub.unsubscribe(session_stream.id, session_stream.generation)
                .await;
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let counts = hub
                        .business_request(json!({"type":"session/list", "requestId":"count"}))
                        .await
                        .unwrap();
                    if counts["_meta"]["closes"] == 1 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("expiry must not wait for the running prompt to finish");
            tokio::time::sleep(Duration::from_millis(250)).await;
            let counts = hub
                .business_request(json!({"type":"session/list", "requestId":"later-count"}))
                .await
                .unwrap();
            assert_eq!(
                counts["_meta"]["closes"], 1,
                "a dispatched close is not repeated automatically"
            );
            assert_eq!(counts["_meta"]["prompts"], 1);
            let lifecycle = hub
                .state
                .lock()
                .await
                .canonical
                .snapshot
                .as_ref()
                .unwrap()
                .sessions
                .get("created")
                .map(|session| session.lifecycle.clone());
            assert_eq!(
                lifecycle,
                if refused {
                    Some(crate::runtime_state::SessionLifecycle::Active)
                } else {
                    None
                }
            );
            hub.unsubscribe(observer.id, observer.generation).await;
            hub.shutdown().await;
        }
    }

    #[tokio::test]
    async fn workspace_context_follows_session_workspace() {
        let project = tempfile::tempdir().unwrap();
        let path = project.path().join("session-context.txt");
        std::fs::write(&path, "selected project context").unwrap();
        let (hub, observer) = capability_fixture(&[]).await;
        hub.business_request(json!({
            "type": "session/new", "requestId": "context-new", "cwd": project.path(),
        }))
        .await
        .unwrap();
        let matches = hub
            .business_request(json!({
                "type": "context/search", "requestId": "context-search",
                "sessionId": "created", "query": "session-context",
            }))
            .await
            .unwrap();
        assert_eq!(matches["matches"].as_array().unwrap().len(), 1);
        let attachment = hub
            .business_request(json!({
                "type": "context/read", "requestId": "context-read",
                "sessionId": "created", "path": path,
            }))
            .await
            .unwrap();
        assert_eq!(
            attachment["attachment"]["block"]["resource"]["text"],
            "selected project context"
        );
        let outside = hub.business_request(json!({
            "type": "context/read", "requestId": "context-outside",
            "sessionId": "created", "path": format!("{}/Cargo.toml", env!("CARGO_MANIFEST_DIR")),
        })).await;
        assert!(
            outside.is_err(),
            "the startup workspace is not an implicit additional root"
        );
        let missing = hub.business_request(json!({
            "type": "context/search", "requestId": "context-missing", "query": "session-context",
        })).await;
        assert!(
            missing.is_err(),
            "search must not silently fall back to the startup workspace"
        );
        hub.unsubscribe(observer.id, observer.generation).await;
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn stdio_discovery_lists_other_workspaces_and_loads_their_own_cwd() {
        let cwd = env!("CARGO_MANIFEST_DIR");
        let fixture = std::path::Path::new(cwd).join("tests/fixtures/fake-agent.ts");
        let fixture = fixture.to_string_lossy().into_owned();
        let options = Options::try_parse_from([
            "attyd",
            "--cwd",
            cwd,
            "--",
            "node",
            "--import",
            "tsx",
            &fixture,
            "--cross-workspace-sessions",
        ])
        .unwrap()
        .normalized()
        .unwrap();
        let hub = BridgeHub::new(Arc::new(options));
        let mut browser = hub.subscribe().await.expect("browser subscription");
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = browser.events.recv().await {
                let event: serde_json::Value = serde_json::from_str(&event.into_string()).unwrap();
                if event["type"] == "bridge/phase" && event["phase"] == "ready" {
                    return;
                }
            }
            panic!("bridge subscriber closed before initialization");
        })
        .await
        .expect("bridge initialization");

        let first_page = hub
            .business_request(json!({
                "type": "session/list",
                "requestId": "list-workspaces",
            }))
            .await
            .expect("first workspace page");
        assert_eq!(first_page["sessions"][0]["sessionId"], "saved-session");
        assert_eq!(first_page["sessions"][0]["cwd"], cwd);
        assert_eq!(first_page["nextCursor"], "workspace-page-2");

        let second_page = hub
            .business_request(json!({
                "type": "session/list",
                "requestId": "list-other-workspace",
                "cursor": first_page["nextCursor"],
            }))
            .await
            .expect("other workspace page");
        assert_eq!(second_page["sessions"][0]["sessionId"], "earlier-session");
        assert_eq!(second_page["sessions"][0]["cwd"], "/other-workspace");
        assert!(second_page["nextCursor"].is_null());

        // Cold loading must use the Agent's listed cwd, even outside --cwd.
        // The fixture rejects requests that substitute the startup directory.
        let view = hub
            .session_view("earlier-session".to_string())
            .await
            .expect("other workspace session can be loaded");
        assert_eq!(view["live"]["cwd"], "/other-workspace");
        assert_eq!(view["session"]["phase"], "ready");
        assert!(
            view["baseline"]["updates"]
                .as_array()
                .unwrap()
                .iter()
                .any(|update| { update["content"]["text"] == "Loaded history." })
        );

        let created = hub
            .business_request(json!({
                "type": "session/new",
                "requestId": "new-default-workspace",
            }))
            .await
            .expect("startup cwd remains the new session default");
        assert_eq!(created["cwd"], cwd);

        hub.unsubscribe(browser.id, browser.generation).await;
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn missing_session_views_return_not_found_but_live_unlisted_sessions_remain_readable() {
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
        let mut browser = hub.subscribe().await.expect("browser subscription");
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = browser.events.recv().await {
                let event: serde_json::Value = serde_json::from_str(&event.into_string()).unwrap();
                if event["type"] == "bridge/phase" && event["phase"] == "ready" {
                    return;
                }
            }
            panic!("bridge subscriber closed before initialization");
        })
        .await
        .expect("bridge initialization");
        let state = AppState {
            bridge: hub.clone(),
        };
        let listed = hub
            .business_request(json!({
                "type": "session/list",
                "requestId": "list-before-restore",
            }))
            .await
            .unwrap();
        assert!(
            listed["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .all(|session| {
                    session["sessionId"] != "stale-session"
                        && session["sessionId"] != "test-session"
                })
        );

        let missing = get_session_view(
            Path("stale-session".to_string()),
            State(state.clone()),
            Query(SessionViewQuery::default()),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(missing.into_body(), 4_096)
            .await
            .unwrap();
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["code"], "session_not_found");
        let missing_events = session_events(
            Path("stale-session".to_string()),
            State(state.clone()),
            Query(SessionViewQuery::default()),
        )
        .await;
        assert_eq!(missing_events.status(), StatusCode::NOT_FOUND);

        hub.business_request(json!({
            "type": "session/new",
            "requestId": "new-unlisted",
            "cwd": cwd,
        }))
        .await
        .expect("new session is materialized without listing it");
        let live = get_session_view(
            Path("test-session".to_string()),
            State(state),
            Query(SessionViewQuery::default()),
        )
        .await;
        assert_eq!(live.status(), StatusCode::OK);
        let body = axum::body::to_bytes(live.into_body(), 64 * 1_024)
            .await
            .unwrap();
        let view: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(view["sessionId"], "test-session");
        assert_eq!(view["phase"], "ready");

        hub.unsubscribe(browser.id, browser.generation).await;
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn same_session_reload_commits_one_public_view_without_replaying_staging_to_observers() {
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

        hub.business_request(json!({
            "type": "session/new",
            "requestId": "new",
            "cwd": cwd,
        }))
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
        let queried = hub
            .session_view("test-session".to_string())
            .await
            .expect("materialized session view query");
        assert_eq!(queried["session"]["phase"], "ready");
        assert!(queried["session"]["historyRevision"].is_string());
        assert_eq!(queried["baseline"]["updates"], json!([]));

        // No session/list precedes this request. The bridge must use the cwd
        // already associated with the tracked session and execute a reload,
        // not allocate a second business session.
        hub.business_request(json!({
            "type": "session/load",
            "requestId": "reload",
            "sessionId": "test-session",
        }))
        .await
        .unwrap();

        for subscription in [&mut requester, &mut observer] {
            loop {
                let event = next_event(subscription).await;
                assert_ne!(
                    event["type"], "acp/session_update",
                    "private load staging leaked to an observer"
                );
                assert_ne!(
                    event["type"], "acp/session_attached",
                    "internal attachment response leaked to an observer"
                );
                if event["type"] == "bridge/session_view"
                    && event["view"]["baseline"]["updates"]
                        .as_array()
                        .is_some_and(|updates| {
                            updates
                                .iter()
                                .any(|update| update["content"]["text"] == "Loaded history.")
                        })
                {
                    break;
                }
            }
        }

        let refreshed = hub
            .session_view("test-session".to_string())
            .await
            .expect("refreshed session view");
        let revision = refreshed["session"]["historyRevision"]
            .as_str()
            .unwrap()
            .to_string();
        let session_stream = hub
            .subscribe_session("test-session".to_string())
            .await
            .expect("session subscription");
        let prompt = vec![json!({ "type": "text", "text": "message-actions-flow" })];
        let accepted = hub
            .start_turn(
                "test-session".to_string(),
                revision.clone(),
                "rest-turn".to_string(),
                prompt.clone(),
            )
            .await
            .expect("REST turn admission");
        assert_eq!(accepted["disposition"], "accepted");
        let duplicate = hub
            .start_turn(
                "test-session".to_string(),
                revision,
                "rest-turn".to_string(),
                prompt,
            )
            .await
            .expect("idempotent REST retry");
        assert_eq!(duplicate["disposition"], "duplicate");
        assert_eq!(duplicate["operationId"], accepted["operationId"]);
        loop {
            let event = next_event(&mut requester).await;
            if event["type"] == "acp/prompt_complete" && event["requestId"] == "rest-turn" {
                break;
            }
        }

        hub.unsubscribe(session_stream.id, session_stream.generation)
            .await;
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn session_subscriber_disconnect_keeps_running_turn_then_reloads_after_idle_close() {
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
            "attyd",
            "--session-unobserved-timeout",
            "2",
            "--cwd",
            cwd,
            "--",
            "node",
            "--import",
            "tsx",
            &fixture,
        ])
        .unwrap()
        .normalized()
        .unwrap();
        let hub = BridgeHub::new(Arc::new(options));
        let mut browser = hub.subscribe().await.expect("test observer subscription");

        loop {
            let event = next_event(&mut browser).await;
            if event["type"] == "bridge/phase" && event["phase"] == "ready" {
                break;
            }
        }
        hub.business_request(json!({
            "type": "session/list",
            "requestId": "list-before-disconnect",
        }))
        .await
        .expect("business session list");

        let initial = hub
            .session_view("saved-session".to_string())
            .await
            .expect("saved session view");
        let initial_incarnation = initial["session"]["incarnation"].as_u64().unwrap();
        let session_stream = hub
            .subscribe_session("saved-session".to_string())
            .await
            .expect("session subscription");
        let revision = initial["session"]["historyRevision"]
            .as_str()
            .expect("new session revision")
            .to_string();
        let accepted = hub
            .start_turn(
                "saved-session".to_string(),
                revision.clone(),
                "turn-before-disconnect".to_string(),
                vec![json!({ "type": "text", "text": "stream-follow-flow" })],
            )
            .await
            .expect("turn admission");
        assert_eq!(accepted["disposition"], "accepted");

        hub.unsubscribe(session_stream.id, session_stream.generation)
            .await;
        drop(session_stream);

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = next_event(&mut browser).await;
                if event["type"] == "acp/session_closed" && event["sessionId"] == "saved-session" {
                    break;
                }
            }
        })
        .await
        .expect("the completed unobserved session was not closed");

        let reconnected = hub
            .subscribe_session("saved-session".to_string())
            .await
            .expect("reconnected session subscription");
        let reloaded = hub
            .session_view("saved-session".to_string())
            .await
            .expect("reconnected browser reloads Agent history");
        assert!(reloaded["session"]["activeTurn"].is_null());
        assert!(
            reloaded["session"]["incarnation"].as_u64().unwrap() > initial_incarnation,
            "a new observation after idle close must create a fresh materialization"
        );
        assert!(
            reloaded["baseline"]["updates"]
                .as_array()
                .is_some_and(|updates| updates.iter().any(|update| {
                    update["sessionUpdate"] == "agent_message_chunk"
                        && update["content"]["text"]
                            .as_str()
                            .is_some_and(|text| text.contains("Stream follow complete."))
                }))
        );
        hub.unsubscribe(reconnected.id, reconnected.generation)
            .await;
        hub.unsubscribe(browser.id, browser.generation).await;
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn agent_error_after_dispatch_reconciles_persisted_output_without_redispatch() {
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
        let mut browser = hub.subscribe().await.expect("browser subscription");
        loop {
            let event = next_event(&mut browser).await;
            if event["type"] == "bridge/phase" && event["phase"] == "ready" {
                break;
            }
        }
        let created = hub
            .business_request(json!({
                "type": "session/new",
                "requestId": "new-before-error",
                "cwd": cwd,
            }))
            .await
            .expect("business session creation");
        let revision = created["view"]["session"]["historyRevision"]
            .as_str()
            .unwrap()
            .to_string();
        // Global events do not pin a session after turn completion. Model the
        // session page's SSE subscription so the duplicate targets the same incarnation.
        let mut session_stream = hub
            .subscribe_session("test-session".to_string())
            .await
            .expect("session subscription");
        let prompt = vec![json!({ "type": "text", "text": "error-after-output-flow" })];
        let accepted = hub
            .start_turn(
                "test-session".to_string(),
                revision.clone(),
                "error-after-output-intent".to_string(),
                prompt.clone(),
            )
            .await
            .expect("turn is accepted before the Agent completes it");
        assert_eq!(accepted["disposition"], "accepted");

        let failed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = next_event(&mut session_stream).await;
                if event["type"] == "bridge/session_turn_failed"
                    && event["operationId"] == accepted["operationId"]
                {
                    break event;
                }
            }
        })
        .await
        .expect("the errored turn did not publish its terminal outcome");
        assert_eq!(failed["phase"], "ready");
        assert_ne!(failed["historyRevision"], revision);
        let completed = hub
            .session_view("test-session".to_string())
            .await
            .expect("session remains observable after the Agent error");
        assert_eq!(completed["session"]["phase"], "ready");
        assert_eq!(
            completed["session"]["historyRevision"],
            failed["historyRevision"]
        );
        assert_eq!(
            completed["session"]["incarnation"], created["view"]["session"]["incarnation"],
            "the observed session must not close and rematerialize after the error"
        );
        assert!(
            completed["baseline"]["updates"]
                .as_array()
                .is_some_and(|updates| updates.iter().any(|update| {
                    update["content"]["text"] == "Output persisted before the Agent error."
                }))
        );

        let duplicate = hub
            .start_turn(
                "test-session".to_string(),
                completed["session"]["historyRevision"]
                    .as_str()
                    .unwrap()
                    .to_string(),
                "error-after-output-intent".to_string(),
                prompt,
            )
            .await
            .expect("repeating the same accepted intent must not redispatch");
        assert_eq!(duplicate["disposition"], "duplicate");
        assert_eq!(duplicate["operationId"], accepted["operationId"]);

        hub.unsubscribe(session_stream.id, session_stream.generation)
            .await;
        hub.unsubscribe(browser.id, browser.generation).await;
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn concurrent_cold_observers_join_one_retrying_materialization() {
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
            "attyd",
            "--cwd",
            cwd,
            "--",
            "node",
            "--import",
            "tsx",
            &fixture,
            "--slow-load",
            "--fail-load-once",
        ])
        .unwrap()
        .normalized()
        .unwrap();
        let hub = BridgeHub::new(Arc::new(options));
        let mut browser = hub.subscribe().await.expect("browser subscription");
        loop {
            let event = next_event(&mut browser).await;
            if event["type"] == "bridge/phase" && event["phase"] == "ready" {
                break;
            }
        }
        let listed = hub
            .business_request(json!({
                "type": "session/list",
                "requestId": "list-before-observe",
            }))
            .await
            .expect("business session listing");
        assert!(listed["sessions"].as_array().is_some_and(|sessions| {
            sessions
                .iter()
                .any(|session| session["sessionId"] == "saved-session")
        }));

        let first = hub.session_view("saved-session".to_string());
        let second = hub.session_view("saved-session".to_string());
        let (first, second) = tokio::join!(first, second);
        let first = first.expect("first observer joins materialization");
        let second = second.expect("second observer joins materialization");

        assert_eq!(first["session"]["phase"], "ready");
        assert_eq!(first["session"], second["session"]);
        assert_eq!(first["baseline"], second["baseline"]);
        assert!(
            first["baseline"]["updates"]
                .as_array()
                .is_some_and(|updates| updates
                    .iter()
                    .any(|update| { update["content"]["text"] == "Loaded history." }))
        );
        hub.unsubscribe(browser.id, browser.generation).await;
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn deterministic_cold_materialization_failure_is_sticky_for_observers() {
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
            "attyd",
            "--cwd",
            cwd,
            "--",
            "node",
            "--import",
            "tsx",
            &fixture,
            "--fail-load-always",
        ])
        .unwrap()
        .normalized()
        .unwrap();
        let hub = BridgeHub::new(Arc::new(options));
        let mut browser = hub.subscribe().await.expect("browser subscription");
        loop {
            let event = next_event(&mut browser).await;
            if event["type"] == "bridge/phase" && event["phase"] == "ready" {
                break;
            }
        }
        hub.business_request(json!({
            "type": "session/list",
            "requestId": "list-before-failed-observe",
        }))
        .await
        .expect("business session listing");

        let first = hub.session_view("saved-session".to_string()).await;
        assert!(
            matches!(first, Err(bridge::SessionViewError::Unavailable(ref error))
                if error.contains("deterministic load failure")),
            "a non-not-found load failure stays visible instead of becoming a missing session: {first:?}"
        );
        for _ in 0..3 {
            let blocked = hub
                .session_view("saved-session".to_string())
                .await
                .expect("later observers read the sticky blocked projection");
            assert_eq!(blocked["session"]["phase"], "blocked");
            assert!(
                blocked["session"]["syncError"]
                    .as_str()
                    .is_some_and(|error| error.contains("deterministic load failure"))
            );
            assert!(blocked["session"]["historyRevision"].is_null());
        }

        hub.unsubscribe(browser.id, browser.generation).await;
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn different_sessions_run_and_reconcile_independently() {
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
            "attyd",
            "--cwd",
            cwd,
            "--",
            "node",
            "--import",
            "tsx",
            &fixture,
            "--race-new",
        ])
        .unwrap()
        .normalized()
        .unwrap();
        let hub = BridgeHub::new(Arc::new(options));
        let mut observer = hub.subscribe().await.expect("observer subscription");
        loop {
            let event = next_event(&mut observer).await;
            if event["type"] == "bridge/phase" && event["phase"] == "ready" {
                break;
            }
        }

        let first = hub
            .business_request(json!({
                "type": "session/new",
                "requestId": "new-one",
                "cwd": cwd,
            }))
            .await
            .expect("first session");
        let second = hub
            .business_request(json!({
                "type": "session/new",
                "requestId": "new-two",
                "cwd": cwd,
            }))
            .await
            .expect("second session");
        let first_id = first["sessionId"].as_str().unwrap().to_string();
        let second_id = second["sessionId"].as_str().unwrap().to_string();
        assert_ne!(first_id, second_id);
        let first_stream = hub
            .subscribe_session(first_id.clone())
            .await
            .expect("first session subscription");
        let second_stream = hub
            .subscribe_session(second_id.clone())
            .await
            .expect("second session subscription");

        hub.start_turn(
            first_id.clone(),
            first["view"]["session"]["historyRevision"]
                .as_str()
                .unwrap()
                .to_string(),
            "long-turn".to_string(),
            vec![json!({ "type": "text", "text": "stream-follow-flow" })],
        )
        .await
        .expect("long turn admission");
        hub.start_turn(
            second_id.clone(),
            second["view"]["session"]["historyRevision"]
                .as_str()
                .unwrap()
                .to_string(),
            "short-turn".to_string(),
            vec![json!({ "type": "text", "text": "message-actions-flow" })],
        )
        .await
        .expect("short turn admission");

        let short_completed = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let view = hub.session_view(second_id.clone()).await.unwrap();
                if view["session"]["phase"] == "ready" {
                    break view;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("short session was serialized behind an unrelated long session");
        assert!(short_completed["session"]["activeTurn"].is_null());
        let long_during_short_completion = hub.session_view(first_id.clone()).await.unwrap();
        assert!(matches!(
            long_during_short_completion["session"]["phase"].as_str(),
            Some("running" | "reconciling")
        ));

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if hub.session_view(first_id.clone()).await.unwrap()["session"]["phase"] == "ready"
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("long session did not finish");
        hub.unsubscribe(first_stream.id, first_stream.generation)
            .await;
        hub.unsubscribe(second_stream.id, second_stream.generation)
            .await;
        hub.unsubscribe(observer.id, observer.generation).await;
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn post_turn_does_not_reload_an_observed_session() {
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
            "attyd",
            "--cwd",
            cwd,
            "--",
            "node",
            "--import",
            "tsx",
            &fixture,
            "--invalid-load-mode-once",
        ])
        .unwrap()
        .normalized()
        .unwrap();
        let hub = BridgeHub::new(Arc::new(options));
        let mut observer = hub.subscribe().await.expect("observer subscription");
        loop {
            let event = next_event(&mut observer).await;
            if event["type"] == "bridge/phase" && event["phase"] == "ready" {
                break;
            }
        }
        let created = hub
            .business_request(json!({
                "type": "session/new",
                "requestId": "new-before-invalid-reconcile",
                "cwd": cwd,
            }))
            .await
            .unwrap();
        let session_stream = hub
            .subscribe_session("test-session".to_string())
            .await
            .expect("session subscription");
        let initial_revision = created["view"]["session"]["historyRevision"]
            .as_str()
            .unwrap()
            .to_string();
        hub.start_turn(
            "test-session".to_string(),
            initial_revision.clone(),
            "invalid-reconcile-turn".to_string(),
            vec![json!({ "type": "text", "text": "message-actions-flow" })],
        )
        .await
        .unwrap();

        let completed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let view = hub.session_view("test-session".to_string()).await.unwrap();
                if view["session"]["phase"] == "ready"
                    && view["session"]["historyRevision"] != initial_revision
                {
                    break view;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("observed turn did not commit its in-memory history");
        assert!(completed["session"]["activeTurn"].is_null());
        assert!(completed["session"]["syncError"].is_null());

        hub.unsubscribe(session_stream.id, session_stream.generation)
            .await;
        hub.unsubscribe(observer.id, observer.generation).await;
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
    async fn old_generation_direct_events_are_rejected() {
        let hub = test_hub();
        let (input, _commands) = mpsc::channel(1);
        let (subscriber, mut events) = SubscriberSender::channel(1);
        {
            let mut state = hub.state.lock().await;
            state.generation = 2;
            state.input = Some(input);
            state.subscribers.insert(42, subscriber);
        }

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

        let event = receiver.recv().await.unwrap().into_arc();
        assert_eq!(event.as_ref(), "first");
        assert_eq!(
            sender.queued_bytes.load(Ordering::Acquire),
            0,
            "dequeueing an SSE event must release its queue reservation"
        );
        drop(receiver);
        assert!(sender.try_send("closed".to_string()).is_err());
        assert_eq!(
            sender.queued_bytes.load(Ordering::Acquire),
            0,
            "closed-channel rejection must roll back its byte reservation"
        );
    }

    #[tokio::test]
    async fn broadcast_payload_is_shared_across_subscriber_backlogs() {
        let (first, mut first_rx) = SubscriberSender::channel(1);
        let (second, mut second_rx) = SubscriberSender::channel(1);
        let payload: Arc<str> = Arc::from("authoritative baseline");

        first.try_send(payload.clone()).unwrap();
        second.try_send(payload.clone()).unwrap();
        let first_payload = first_rx.recv().await.unwrap().into_arc();
        let second_payload = second_rx.recv().await.unwrap().into_arc();

        assert!(Arc::ptr_eq(&payload, &first_payload));
        assert!(Arc::ptr_eq(&first_payload, &second_payload));
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
            .send(bridge::BridgeInput::RuntimeSnapshotRequest)
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
                .try_send(bridge::BridgeInput::RuntimeSnapshotRequest)
                .is_err()
        );
        hub.finish_generation(1).await;
        shutdown.await.unwrap();
    }
}
