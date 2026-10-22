use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
#[cfg(any(test, not(feature = "dev")))]
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, RawQuery, Request, State};
#[cfg(not(feature = "dev"))]
use axum::http::Method;
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
#[cfg(not(feature = "dev"))]
use rust_embed::RustEmbed;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

use crate::bridge;
use crate::bridge_event::{BridgeEvent, EventReceiver, EventSender};
use crate::dev_proxy::{DevProxy, reject_self_proxy};
use crate::options::{Options, normalize_origin};
use crate::protocol::admission::{self, OpWrite};
use crate::protocol::documents::{ErrorCode, ProtocolError, Scope};
use crate::protocol::publication::Publication;
use crate::protocol::subscription::HEARTBEAT;
use crate::protocol::wire;

const BRIDGE_SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(5);
const HTTP_SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(6);
/// The recovery's delay after a connection that never became ready; it
/// doubles per such attempt up to the maximum (protocol §5.4).
const RECOVERY_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const RECOVERY_MAX_BACKOFF: Duration = Duration::from_secs(8);

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
        let allow_any = self.allowed_origins.iter().any(|origin| origin == "*");
        let configured_host = allow_any
            || self.allowed_origins.iter().any(|origin| {
                let scheme = origin.split_once("://").expect("validated origin").0;
                normalize_origin(&format!("{scheme}://{host}")).as_ref() == Ok(origin)
            });
        if !direct_host && !configured_host {
            return false;
        }

        let mut origins = request.headers().get_all(header::ORIGIN).iter();
        let Some(origin) = origins.next() else {
            // Command-line clients do not send Origin. Unless '*' is configured,
            // Host must still be allowlisted to prevent DNS rebinding.
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
        allow_any
            || (direct_host && origin == http_origin)
            || self.allowed_origins.contains(&origin)
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
            axum::Json(wire::HostError {
                error: "Request Host or Origin is not allowed".to_string(),
            }),
        )
            .into_response();
    }
    next.run(request).await
}

#[derive(Default)]
struct BridgeHubState {
    generation: u64,
    input: Option<mpsc::UnboundedSender<bridge::BridgeInput>>,
    cancellation: Option<CancellationToken>,
    /// The generation's Agent connection became ready.
    initialized: bool,
    shutting_down: bool,
    /// The one recovery task, while it waits to start the next connection.
    recovery: Option<CancellationToken>,
    /// A successful terminal login authorized one next connection (Q2).
    login_restart: bool,
    /// The recovery's delay before its next attempt.
    recovery_backoff: Duration,
}

impl BridgeHubState {
    /// Starts the next generation.
    fn begin_generation(&mut self) -> BridgeRuntime {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.initialized = false;
        self.login_restart = false;
        let (input_tx, input) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let (event_tx, events) = mpsc::unbounded_channel();
        self.input = Some(input_tx);
        self.cancellation = Some(cancellation.clone());
        BridgeRuntime {
            generation: self.generation,
            input,
            events,
            event_tx,
            cancellation,
        }
    }
}

struct BridgeHub {
    options: Arc<Options>,
    /// The published documents and the v2 streams, shared by the
    /// generations (protocol §5.4).
    publication: Publication,
    state: Mutex<BridgeHubState>,
    stopped: Notify,
}

struct BridgeRuntime {
    generation: u64,
    input: mpsc::UnboundedReceiver<bridge::BridgeInput>,
    events: EventReceiver,
    event_tx: EventSender,
    cancellation: CancellationToken,
}

impl BridgeHub {
    fn new(options: Arc<Options>) -> Arc<Self> {
        Arc::new(Self {
            options,
            publication: Publication::default(),
            state: Mutex::new(BridgeHubState::default()),
            stopped: Notify::new(),
        })
    }

    async fn ensure_runtime(self: &Arc<Self>) {
        let runtime = {
            let mut state = self.state.lock().await;
            if state.shutting_down || state.input.is_some() {
                None
            } else {
                Some(state.begin_generation())
            }
        };
        if let Some(runtime) = runtime {
            self.spawn_runtime(runtime);
        }
    }

    /// Whether an open v2 stream needs a connection: it follows a session
    /// scope that has not ended (S6, Q2). Global and catalog alone do not.
    fn streams_need_connection(&self) -> bool {
        let published = self.publication.lock();
        !published.feed.followed_sessions().is_empty()
    }

    /// Schedules the one recovery task when no generation runs and a stream
    /// or a login needs the next connection. GET never calls it.
    async fn recover(self: &Arc<Self>) {
        let (token, delay) = {
            let mut state = self.state.lock().await;
            if state.shutting_down
                || state.input.is_some()
                || state.recovery.is_some()
                || !(state.login_restart || self.streams_need_connection())
            {
                return;
            }
            let token = CancellationToken::new();
            state.recovery = Some(token.clone());
            (token, state.recovery_backoff)
        };
        let hub = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::select! {
                _ = token.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
            }
            let Some(hub) = hub.upgrade() else {
                return;
            };
            // The need is decided again when the attempt starts: a stream
            // that left or ended during the delay starts nothing.
            let runtime = {
                let mut state = hub.state.lock().await;
                if token.is_cancelled() {
                    return;
                }
                state.recovery = None;
                if state.shutting_down
                    || state.input.is_some()
                    || !(state.login_restart || hub.streams_need_connection())
                {
                    return;
                }
                state.begin_generation()
            };
            hub.spawn_runtime(runtime);
        });
    }

    /// Asks the current generation to serve the streams: a subscription of the
    /// revision 3 feed may follow a session the bridge has to load. With no
    /// generation, the next one serves them when it is ready.
    async fn serve_streams(&self) {
        if let Some(input) = &self.state.lock().await.input {
            let _ = input.send(bridge::BridgeInput::ServeStreams);
        }
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
        let publication = self.publication.clone();
        tokio::spawn(async move {
            bridge::run_publishing(options, input, event_tx, cancellation, publication).await;
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

    async fn publish(&self, generation: u64, event: BridgeEvent) {
        let restart_input = {
            let mut state = self.state.lock().await;
            if state.generation != generation || state.input.is_none() {
                return;
            }
            match event {
                BridgeEvent::Initialized => {
                    state.initialized = true;
                    None
                }
                BridgeEvent::TerminalLoginSucceeded => {
                    let cancellation = state.cancellation.clone();
                    if cancellation.is_some() {
                        state.login_restart = true;
                    }
                    cancellation
                }
            }
        };
        if let Some(cancellation) = restart_input {
            cancellation.cancel();
        }
    }

    async fn finish_generation(self: &Arc<Self>, generation: u64) {
        {
            let mut state = self.state.lock().await;
            if state.generation != generation {
                return;
            }
            state.input = None;
            state.cancellation = None;
            // A connection that became ready recovers at once; attempts that
            // never did back off.
            state.recovery_backoff = if state.initialized {
                Duration::ZERO
            } else {
                state
                    .recovery_backoff
                    .saturating_mul(2)
                    .clamp(RECOVERY_INITIAL_BACKOFF, RECOVERY_MAX_BACKOFF)
            };
        }
        self.stopped.notify_waiters();
        // The generation's final commit is published: the next one may start.
        self.recover().await;
    }

    /// An operation write (protocol §6): the bridge admits it under one
    /// guard. Without an Agent connection the same checks run on the published
    /// documents, and an admissible write is `unavailable`; nothing starts a
    /// connection.
    async fn operate(
        &self,
        scope: Scope,
        write: OpWrite,
    ) -> Result<bridge::operations::OperationAdmission, ProtocolError> {
        let input = self.state.lock().await.input.clone();
        if let Some(input) = input {
            let (reply, admitted) = oneshot::channel();
            let sent = input.send(bridge::BridgeInput::Operation {
                scope: scope.clone(),
                write: write.clone(),
                reply,
            });
            if sent.is_ok()
                && let Ok(admitted) = admitted.await
            {
                return admitted;
            }
        }
        let published = self.publication.lock();
        admission::admit(
            &|scope| published.feed.live(scope),
            &|session, id| published.feed.attachment(session, id),
            &scope,
            &write,
        )?;
        Err(ProtocolError::new(
            ErrorCode::Unavailable,
            "there is no Agent connection",
        ))
    }

    /// A control action (protocol §7): refused before anything is sent, or
    /// answered when the Agent answers. Without an Agent connection there is
    /// nothing to set.
    async fn control(
        &self,
        session_id: String,
        control: bridge::controls::Control,
    ) -> Result<Result<(), ProtocolError>, ProtocolError> {
        let unavailable =
            || ProtocolError::new(ErrorCode::Unavailable, "there is no Agent connection");
        let input = self.state.lock().await.input.clone().ok_or_else(unavailable)?;
        let (reply, admitted) = oneshot::channel();
        input
            .send(bridge::BridgeInput::Control {
                session_id,
                control,
                reply,
            })
            .map_err(|_| unavailable())?;
        let answer = admitted.await.map_err(|_| unavailable())??;
        Ok(answer.await.unwrap_or_else(|_| {
            Err(ProtocolError::new(
                ErrorCode::Unavailable,
                "the Agent connection ended before it answered",
            ))
        }))
    }

    /// A resource action (protocol §7), applied under one bridge guard.
    /// Without an Agent connection its resources are gone with it.
    async fn act(
        &self,
        scope: Scope,
        resource: String,
        action: bridge::resources::ResourceAction,
    ) -> Result<(), ProtocolError> {
        let input = self.state.lock().await.input.clone();
        if let Some(input) = input {
            let (reply, acted) = oneshot::channel();
            let sent = input.send(bridge::BridgeInput::Resource {
                scope: scope.clone(),
                resource: resource.clone(),
                action,
                reply,
            });
            if sent.is_ok()
                && let Ok(acted) = acted.await
            {
                return acted;
            }
        }
        Err(ProtocolError::new(
            ErrorCode::NotFound,
            format!("{scope} has no resource {resource}"),
        ))
    }

    /// The workspace of a held session's context reads (protocol §8).
    async fn workspace(
        &self,
        session_id: &str,
    ) -> Result<crate::filesystem::WorkspaceFileSystem, ProtocolError> {
        let input = self.state.lock().await.input.clone();
        if let Some(input) = input {
            let (reply, workspace) = oneshot::channel();
            let sent = input.send(bridge::BridgeInput::Workspace {
                session_id: session_id.to_string(),
                reply,
            });
            if sent.is_ok()
                && let Ok(workspace) = workspace.await
            {
                return workspace;
            }
        }
        Err(ProtocolError::new(
            ErrorCode::Unavailable,
            "there is no Agent connection",
        ))
    }

    async fn shutdown(&self) {
        let cancellation = {
            let mut state = self.state.lock().await;
            state.shutting_down = true;
            if let Some(recovery) = state.recovery.take() {
                recovery.cancel();
            }
            state.cancellation.clone()
        };
        if let Some(cancellation) = cancellation {
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
        // The streams deliver the final commit, then end.
        self.publication.lock().feed.close();
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
                let Some(hub) = hub.upgrade() else {
                    return;
                };
                hub.publish(generation, event).await;
            }
            _ = &mut runtime_done => {
                // Drain everything run_publishing queued before it returned,
                // then ignore any leaked/late sender of this generation.
                while let Ok(event) = events.try_recv() {
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
        .route("/api/v3/stream", get(feed_stream))
        .route("/api/v3/scopes/{*scope}", get(read_scope_v3).post(write_scope_v3))
        .route("/api/v2/sessions/{*read}", get(read_session))
        .fallback(move |request: Request| frontend(dev_proxy.clone(), request))
        .layer(DefaultBodyLimit::disable())
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
    axum::Json(wire::Health {
        ok: true,
        protocol: "acp/v1".to_string(),
        backend: "rust".to_string(),
    })
}

/// A protocol error body `{ error: { code, message } }` (protocol §9).
fn protocol_error(status: StatusCode, error: ProtocolError) -> Response {
    (status, axum::Json(wire::ErrorBody { error })).into_response()
}

/// `GET /api/v3/scopes/{scope}` (protocol §8): the document as one snapshot, the one a stream
/// aligns with. It starts, loads and holds nothing; a session the bridge does not hold is
/// `not_found`.
async fn read_scope_v3(Path(scope): Path<String>, State(state): State<AppState>) -> Response {
    if Scope::parse(&scope).is_none() {
        return protocol_error(
            StatusCode::BAD_REQUEST,
            ProtocolError::new(ErrorCode::InvalidRequest, format!("no scope {scope}")),
        );
    }
    let snapshot = state.bridge.publication.lock().feed.snapshot(&scope);
    match snapshot {
        Some(snapshot) => axum::Json(snapshot).into_response(),
        None => protocol_error(
            StatusCode::NOT_FOUND,
            ProtocolError::new(ErrorCode::NotFound, format!("{scope} is not held")),
        ),
    }
}

/// The reads of a held session (protocol §8): an attachment its published
/// `log` or `live` references, and its workspace context. A read holds,
/// loads and renews nothing; an unheld session is `not_found`. The raw path
/// is split before decoding, so an encoded session ID keeps its `/`.
async fn read_session(
    State(state): State<AppState>,
    uri: axum::http::Uri,
    RawQuery(query): RawQuery,
) -> Response {
    let segments: Option<Vec<String>> = uri
        .path()
        .strip_prefix("/api/v2/sessions/")
        .unwrap_or_default()
        .split('/')
        .map(path_segment)
        .collect();
    let Some(segments) = segments else {
        return invalid_request("the path is not valid percent-encoded UTF-8");
    };
    let segments = segments.iter().map(String::as_str).collect::<Vec<_>>();
    let (session_id, read) = match segments[..] {
        [session_id, ref read @ ..] if !session_id.is_empty() => (session_id, read),
        _ => return invalid_request("no session"),
    };
    // An attachment ID is the lowercase hex SHA-256 of its block (§3.1); any
    // other is malformed.
    if let ["attachments", attachment_id] = read
        && (attachment_id.len() != 64
            || !attachment_id
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')))
    {
        return invalid_request(format!("{attachment_id} is not an attachment ID"));
    }
    let session = Scope::Session(session_id.to_string());
    let held = state
        .bridge
        .publication
        .lock()
        .feed
        .live(&session)
        .is_some();
    let scope = session.to_string();
    let unheld = || {
        refused(ProtocolError::new(
            ErrorCode::NotFound,
            format!("{scope} is not held"),
        ))
    };
    if !held {
        return unheld();
    }
    let parameter = |name: &str| {
        url::form_urlencoded::parse(query.as_deref().unwrap_or("").as_bytes())
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
            .ok_or_else(|| invalid_request(format!("{name} is required")))
    };
    match read {
        ["attachments", attachment_id] => {
            let block = state
                .bridge
                .publication
                .lock()
                .feed
                .attachment(session_id, attachment_id);
            match block {
                Some(block) => axum::Json(block).into_response(),
                None => refused(ProtocolError::new(
                    ErrorCode::NotFound,
                    format!("{scope} references no attachment {attachment_id}"),
                )),
            }
        }
        ["files"] => {
            let query = match parameter("query") {
                Ok(query) => query,
                Err(response) => return response,
            };
            let workspace = match state.bridge.workspace(session_id).await {
                Ok(workspace) => workspace,
                Err(error) => return refused(error),
            };
            match workspace.search_context(&query).await {
                Ok(matches) => axum::Json(wire::ContextMatches { matches }).into_response(),
                Err(error) => refused(bridge::resources::refusal(error)),
            }
        }
        ["files", "content"] => {
            let path = match parameter("path") {
                Ok(path) if !path.is_empty() => path,
                Ok(_) => return invalid_request("path is required"),
                Err(response) => return response,
            };
            let workspace = match state.bridge.workspace(session_id).await {
                Ok(workspace) => workspace,
                Err(error) => return refused(error),
            };
            match workspace.read_context(std::path::Path::new(&path)).await {
                Ok(attachment) => axum::Json(wire::ContextRead { attachment }).into_response(),
                Err(error) => refused(bridge::resources::refusal(error)),
            }
        }
        _ => refused(ProtocolError::new(
            ErrorCode::NotFound,
            format!("no such read of {scope}"),
        )),
    }
}

/// The status of a refusal (protocol §9).
fn error_status(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::InvalidRequest => StatusCode::BAD_REQUEST,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::Stale | ErrorCode::NotAllowed | ErrorCode::CatalogChanged => {
            StatusCode::CONFLICT
        }
        ErrorCode::Unsupported => StatusCode::NOT_IMPLEMENTED,
        ErrorCode::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::AgentError => StatusCode::BAD_GATEWAY,
    }
}

fn refused(error: ProtocolError) -> Response {
    protocol_error(error_status(error.code), error)
}

fn invalid_request(message: impl Into<String>) -> Response {
    refused(ProtocolError::new(ErrorCode::InvalidRequest, message))
}

/// Percent-decodes one raw path segment, so an encoded `/` stays inside it.
fn path_segment(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// A write's JSON body. Its media type is exactly `application/json`, in
/// any case, with optional parameters.
fn json_body(headers: &HeaderMap, body: &[u8]) -> Result<serde_json::Value, Response> {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| {
            media
                .trim_matches([' ', '\t'])
                .eq_ignore_ascii_case("application/json")
        });
    if !json {
        return Err(invalid_request("the body must be application/json"));
    }
    serde_json::from_slice(body)
        .map_err(|error| invalid_request(format!("the body is not JSON: {error}")))
}

/// A resource action's body (protocol §7).
fn resource_action(
    action: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<bridge::resources::ResourceAction, Response> {
    use bridge::resources::ResourceAction;
    use wire::{TerminalInput, TerminalResize};
    let body = json_body(headers, body)?;
    let invalid = |error: serde_json::Error| invalid_request(format!("{action}: {error}"));
    match action {
        "input" => {
            let TerminalInput { data } = serde_json::from_value(body).map_err(invalid)?;
            Ok(ResourceAction::Input { data })
        }
        "resize" => {
            let TerminalResize { cols, rows } = serde_json::from_value(body).map_err(invalid)?;
            crate::auth_terminal::validate_size(cols, rows)
                .map_err(|error| refused(bridge::resources::refusal(error)))?;
            Ok(ResourceAction::Resize { cols, rows })
        }
        "answer" if body.is_object() => Ok(ResourceAction::Answer { body }),
        _ if body.is_object() => Ok(ResourceAction::Dismiss),
        _ => Err(invalid_request(format!(
            "{action}: the body must be an object"
        ))),
    }
}

/// The control action in a request body (protocol §7).
fn control_action(
    action: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<bridge::controls::Control, Response> {
    use bridge::controls::Control;
    let body = json_body(headers, body)?;
    let text = |name: &str| {
        body[name]
            .as_str()
            .map(String::from)
            .ok_or_else(|| invalid_request(format!("{name} must be a string")))
    };
    match action {
        "close" => Ok(Control::Close),
        "set_mode" => Ok(Control::SetMode {
            mode_id: text("modeId")?,
        }),
        _ => Ok(Control::SetConfig {
            config_id: text("configId")?,
            value: body
                .get("value")
                .cloned()
                .ok_or_else(|| invalid_request("value is required"))?,
        }),
    }
}

/// The operation write in a request body (protocol §6.1).
fn op_write(headers: &HeaderMap, body: &[u8]) -> Result<OpWrite, Response> {
    let body = json_body(headers, body)?;
    let field = |name: &str| {
        body[name]
            .as_str()
            .map(String::from)
            .ok_or_else(|| invalid_request(format!("{name} must be a string")))
    };
    let args = match body.get("args") {
        None => json!({}),
        Some(args) if args.is_object() => args.clone(),
        Some(_) => return Err(invalid_request("args must be an object")),
    };
    let id = match body.get("id") {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) => Some(id.clone()),
        Some(_) => return Err(invalid_request("id must be a string")),
    };
    Ok(OpWrite {
        id,
        expect: field("expect")?,
        kind: field("kind")?,
        args,
    })
}

/// `POST /api/v3/scopes/{scope}/ops` (protocol §6.3): every operation is
/// answered `202` when it is admitted, none holds its response open; the result
/// arrives in the document.
async fn write_scope_v3(
    State(state): State<AppState>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let segments: Option<Vec<String>> = uri
        .path()
        .strip_prefix("/api/v3/scopes/")
        .unwrap_or_default()
        .split('/')
        .map(path_segment)
        .collect();
    let Some(segments) = segments else {
        return invalid_request("the path is not valid percent-encoded UTF-8");
    };
    let Some(scope) = segments.first().and_then(|scope| Scope::parse(scope)) else {
        return invalid_request(format!(
            "no scope {}",
            segments.first().map_or("", |scope| scope)
        ));
    };
    match segments[1..].iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["ops"] => {}
        ["actions", action @ ("set_mode" | "set_config" | "close")] => {
            let Scope::Session(session_id) = &scope else {
                return refused(ProtocolError::new(
                    ErrorCode::NotFound,
                    format!("no such action in {scope}"),
                ));
            };
            let control = match control_action(action, &headers, &body) {
                Ok(control) => control,
                Err(response) => return response,
            };
            return match state.bridge.control(session_id.clone(), control).await {
                Ok(Ok(())) => axum::Json(json!({})).into_response(),
                Ok(Err(error)) => axum::Json(wire::ErrorBody { error }).into_response(),
                Err(error) => refused(error),
            };
        }
        [
            "resources",
            resource,
            action @ ("input" | "resize" | "dismiss" | "answer"),
        ] => {
            let action = match resource_action(action, &headers, &body) {
                Ok(action) => action,
                Err(response) => return response,
            };
            return match state.bridge.act(scope, resource.to_string(), action).await {
                Ok(()) => StatusCode::NO_CONTENT.into_response(),
                Err(error) => refused(error),
            };
        }
        _ => {
            return refused(ProtocolError::new(
                ErrorCode::NotFound,
                format!("no such write in {scope}"),
            ));
        }
    }
    let write = match op_write(&headers, &body) {
        Ok(write) => write,
        Err(response) => return response,
    };
    let admission = match state.bridge.operate(scope, write).await {
        Ok(admission) => admission,
        Err(error) => return refused(error),
    };
    let op = admission.op;
    (StatusCode::ACCEPTED, axum::Json(wire::OpAccepted { op })).into_response()
}

/// `GET /api/v3/stream` (protocol §5): the events of the scopes it follows from
/// the revision 3 feed, without SSE ids, with a heartbeat while idle. Dropping
/// the body closes the subscription.
async fn feed_stream(RawQuery(query): RawQuery, State(state): State<AppState>) -> Response {
    let follow = match crate::protocol::subscription::parse_stream_query(query.as_deref().unwrap_or("")) {
        Ok(follow) => follow,
        Err(error) => return protocol_error(StatusCode::BAD_REQUEST, error),
    };
    if state.bridge.state.lock().await.shutting_down {
        return protocol_error(
            StatusCode::SERVICE_UNAVAILABLE,
            ProtocolError::new(ErrorCode::Unavailable, "the server is shutting down"),
        );
    }
    let subscription = state.bridge.publication.lock().feed.subscribe(&follow);
    state.bridge.serve_streams().await;
    let stream = futures::stream::unfold(subscription, |subscription| async move {
        let event = subscription.next().await?;
        let data = serde_json::to_string(&event).expect("a protocol event serializes");
        Some((
            Ok::<_, Infallible>(SseEvent::default().data(data)),
            subscription,
        ))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(HEARTBEAT))
        .into_response()
}

async fn frontend(proxy: Option<DevProxy>, request: Request) -> Response {
    if request.uri().path() == "/api" || request.uri().path().starts_with("/api/") {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(wire::HostError {
                error: "API route not found".to_string(),
            }),
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
mod stream_tests;

#[cfg(test)]
mod tests {
    use super::*;
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
    fn wildcard_origin_boundary_allows_any_http_origin_and_host() {
        for allowed_origins in [
            vec!["*".to_string()],
            vec!["https://agent.example".to_string(), "*".to_string()],
        ] {
            let policy = OriginPolicy { allowed_origins };
            for (host, origin, allowed) in [
                ("proxy.example:8443", None, true),
                (
                    "proxy.example:8443",
                    Some("https://proxy.example:8443"),
                    true,
                ),
                ("proxy.example:8443", Some("https://other.example"), true),
                ("127.0.0.1:7331", Some("https://other.example"), true),
                ("localhost:7331", Some("http://localhost:8000"), true),
                ("[::1]:7331", Some("http://[::1]:8000"), true),
                ("proxy.example", Some("null"), false),
                ("proxy.example", Some("*"), false),
                ("proxy.example", Some("https://*.example"), false),
                ("proxy.example", Some("file://other.example"), false),
                ("proxy.example", Some("https://other.example/path"), false),
                (
                    "proxy.example",
                    Some("https://one.example https://two.example"),
                    false,
                ),
                ("user@proxy.example", None, false),
                ("proxy.example/path", None, false),
                ("*", None, false),
            ] {
                let mut request = Request::builder()
                    .uri("/api/v1/runtime")
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
            for headers in [
                vec![],
                vec![("host", "one.example"), ("host", "two.example")],
                vec![
                    ("host", "proxy.example"),
                    ("origin", "https://one.example"),
                    ("origin", "https://two.example"),
                ],
            ] {
                let mut request = Request::builder().uri("/api/v1/runtime");
                for (name, value) in headers {
                    request = request.header(name, value);
                }
                assert!(!policy.allows(&request.body(Body::empty()).unwrap()));
            }
            assert!(
                policy.allows(
                    &Request::builder()
                        .uri("http://proxy.example:7331/api/v1/runtime")
                        .body(Body::empty())
                        .unwrap()
                )
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
        let follow =
            crate::protocol::subscription::parse_stream_query("scope=global").unwrap();
        let subscription = hub.publication.lock().feed.subscribe(&follow);
        hub.shutdown().await;
        tokio::time::timeout(Duration::from_millis(100), async {
            while subscription.next().await.is_some() {}
        })
        .await
        .expect("shutdown must close every SSE source before HTTP draining");
        // A stream opened after the shutdown has nothing to follow.
        let late = hub.publication.lock().feed.subscribe(&follow);
        assert!(late.next().await.is_none());
    }

    #[tokio::test]
    async fn bridge_completion_finishes_generation_with_a_leaked_event_sender() {
        let hub = test_hub();
        let (input, _commands) = mpsc::unbounded_channel();
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
            event_rx,
            done_rx,
        ));
        event_tx.send(BridgeEvent::Initialized).unwrap();
        done_tx.send(()).unwrap();

        tokio::time::timeout(Duration::from_millis(100), forwarder)
            .await
            .expect("generation completion still depended on event channel EOF")
            .unwrap();
        let state = hub.state.lock().await;
        assert!(state.input.is_none());
        assert_eq!(
            state.recovery_backoff,
            Duration::ZERO,
            "terminal events queued before runtime completion must be drained first"
        );
        drop(state);
        assert!(
            leaked_sender.send(BridgeEvent::Initialized).is_err(),
            "late senders from a completed generation must be disconnected"
        );
    }

    #[tokio::test]
    async fn shutdown_signal_bypasses_a_command_backlog() {
        let hub = test_hub();
        let (input, _commands) = mpsc::unbounded_channel();
        input.send(bridge::BridgeInput::ServeStreams).unwrap();
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
        assert!(input_probe.send(bridge::BridgeInput::ServeStreams).is_ok());
        hub.finish_generation(1).await;
        shutdown.await.unwrap();
    }
}
