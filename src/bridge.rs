use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::{ProtocolVersion, v1::*};
use agent_client_protocol::{AcpAgentConfig, Agent, Client, ConnectTo, ConnectionTo};
use agent_client_protocol_http::HttpClient;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use uuid::Uuid;

use crate::agent_process::BoundedAcpAgent;
use crate::auth_terminal::{AuthTerminalManager, validate_method as validate_terminal_auth_method};
use crate::elicitation_validation::{
    validate_elicitation_request, validate_elicitation_response_value,
};
use crate::filesystem::WorkspaceFileSystem;
use crate::mcp::McpManager;
use crate::mcp_config::{server_name, server_type};
use crate::options::{Options, Transport};
use crate::semantic::{
    SessionUpdateSemanticState, terminal_references, validate_and_track_session_update,
    validate_content_block, validate_permission_request, validate_prompt_response,
    validate_session_config_options, validate_session_config_reference, validate_session_controls,
    validate_session_metadata, validate_session_mode_reference,
};
use crate::terminal::TerminalManager;

pub const MAX_BRIDGE_MESSAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_BRIDGE_TYPE_LENGTH: usize = 128;
const MAX_BRIDGE_IDENTIFIER_LENGTH: usize = 1_024;
const MAX_BRIDGE_PATH_LENGTH: usize = 16_384;
const MAX_AGENT_RELAY_BYTES: usize = 4_000_000;
const MAX_PENDING_INTERACTIONS: usize = 128;
const MAX_URL_ELICITATION_IDS: usize = 10_000;
const MAX_AUTH_METHODS: usize = 100;
const MAX_AUTH_METHOD_NAME_LENGTH: usize = 4_096;
const MAX_AUTH_METHOD_DESCRIPTION_LENGTH: usize = 16_384;
const MAX_EARLY_UPDATES: usize = 10_000;
const MAX_EARLY_UPDATE_BYTES: usize = 1_000_000;
const MAX_TRACKED_SESSIONS: usize = 32;
const MAX_BRIDGE_ERROR_DATA_BYTES: usize = 256 * 1024;
const MAX_BRIDGE_ERROR_MESSAGE_CHARS: usize = 16_384;
const MAX_SESSION_LIST_TOTAL_BYTES: usize = 16_000_000;
const MAX_LISTED_SESSIONS: usize = 10_000;
const DISCONNECT_CANCEL_GRACE_PERIOD: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct EventSink {
    tx: mpsc::UnboundedSender<String>,
}

impl EventSink {
    fn send(&self, event: Value) {
        let serialized = event.to_string();
        if serialized.len() <= MAX_BRIDGE_MESSAGE_BYTES {
            let _ = self.tx.send(serialized);
        } else {
            let _ = self.tx.send(
                json!({
                    "type": "bridge/error",
                    "message": format!(
                        "browser event exceeds {MAX_BRIDGE_MESSAGE_BYTES} bytes"
                    ),
                })
                .to_string(),
            );
        }
    }

    fn typed(&self, kind: &str, value: impl Serialize) {
        match serde_json::to_value(value) {
            Ok(value) => self.send(json!({ "type": kind, "response": value })),
            Err(error) => self.error(
                format!("failed to serialize ACP value: {error}"),
                None,
                None,
            ),
        }
    }

    fn error(&self, message: impl Into<String>, request_id: Option<&str>, operation: Option<&str>) {
        let mut event = json!({ "type": "bridge/error", "message": message.into() });
        if let Some(request_id) = request_id {
            event["requestId"] = json!(request_id);
        }
        if let Some(operation) = operation {
            event["operation"] = json!(operation);
        }
        self.send(event);
    }

    fn acp_error(&self, error: Error, request_id: Option<&str>, operation: Option<&str>) {
        let message = truncate_chars(error.message, MAX_BRIDGE_ERROR_MESSAGE_CHARS);
        let mut event = json!({
            "type": "bridge/error",
            "message": message,
            "code": i32::from(error.code),
        });
        if let Some(data) = error.data {
            match serde_json::to_vec(&data) {
                Ok(serialized) if serialized.len() <= MAX_BRIDGE_ERROR_DATA_BYTES => {
                    event["data"] = data;
                    event["dataBytes"] = json!(serialized.len());
                }
                Ok(serialized) => {
                    event["dataTruncated"] = json!(true);
                    event["dataBytes"] = json!(serialized.len());
                }
                Err(_) => event["dataTruncated"] = json!(true),
            }
        }
        if let Some(request_id) = request_id {
            event["requestId"] = json!(request_id);
        }
        if let Some(operation) = operation {
            event["operation"] = json!(operation);
        }
        self.send(event);
    }
}

fn truncate_chars(value: String, maximum: usize) -> String {
    if value.chars().count() <= maximum {
        return value;
    }
    let mut result = value.chars().take(maximum).collect::<String>();
    result.push('…');
    result
}

fn ensure_relay_size(value: &impl Serialize, label: &str) -> Result<usize, Error> {
    let bytes = serde_json::to_vec(value)?.len();
    if bytes > MAX_AGENT_RELAY_BYTES {
        return Err(Error::invalid_request().data(format!(
            "Agent {label} exceeds {MAX_AGENT_RELAY_BYTES} bytes"
        )));
    }
    Ok(bytes)
}

fn semantic_error(message: impl Into<String>) -> Error {
    Error::invalid_request().data(message.into())
}

fn validate_agent_session_id(session_id: &str) -> Result<(), String> {
    if session_id.is_empty() || session_id.encode_utf16().count() > 1_024 {
        return Err("Agent session ID must contain between 1 and 1024 characters".to_string());
    }
    Ok(())
}

#[derive(Clone)]
struct ActiveSession {
    cwd: PathBuf,
    modes: Option<Value>,
    config_options: Value,
}

struct PendingPermission {
    session_id: String,
    tool_call_id: String,
    option_ids: HashSet<String>,
    sender: oneshot::Sender<RequestPermissionResponse>,
}

struct PendingElicitation {
    session_id: Option<String>,
    url_elicitation_id: Option<String>,
    request: Value,
    sender: oneshot::Sender<CreateElicitationResponse>,
}

struct ActiveUrlElicitation {
    session_id: Option<String>,
}

#[derive(Default)]
struct BridgeState {
    agent_capabilities: Option<AgentCapabilities>,
    auth_methods: HashMap<String, AuthMethod>,
    active_sessions: HashMap<String, ActiveSession>,
    listed_sessions: HashMap<String, SessionInfo>,
    listed_next_cursor: Option<String>,
    listed_cursors: HashSet<String>,
    listed_session_bytes: usize,
    session_list_in_flight: bool,
    prompts: HashSet<String>,
    pending_forks: HashSet<String>,
    pending_closes: HashSet<String>,
    pending_controls: HashSet<String>,
    pending_deletions: HashSet<String>,
    pending_creations: usize,
    pending_attachments: HashSet<String>,
    early_updates: HashMap<String, Vec<SessionNotification>>,
    early_update_count: usize,
    early_update_bytes: usize,
    session_updates: HashMap<String, SessionUpdateSemanticState>,
    permissions: HashMap<String, PendingPermission>,
    elicitations: HashMap<String, PendingElicitation>,
    url_elicitations: HashMap<String, ActiveUrlElicitation>,
    seen_url_elicitation_ids: HashSet<String>,
}

#[derive(Default)]
struct PromptLifecycle {
    starting: AtomicUsize,
    changed: Notify,
}

impl PromptLifecycle {
    fn register_start(self: &Arc<Self>) -> PromptStartGuard {
        self.starting.fetch_add(1, Ordering::AcqRel);
        PromptStartGuard {
            lifecycle: self.clone(),
            pending: true,
        }
    }

    async fn wait_until_started(&self) {
        loop {
            let changed = self.changed.notified();
            if self.starting.load(Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }

    fn prompt_finished(&self) {
        self.changed.notify_one();
    }
}

struct PromptStartGuard {
    lifecycle: Arc<PromptLifecycle>,
    pending: bool,
}

impl PromptStartGuard {
    fn finish(&mut self) {
        if !self.pending {
            return;
        }
        self.pending = false;
        self.lifecycle.starting.fetch_sub(1, Ordering::AcqRel);
        self.lifecycle.changed.notify_one();
    }
}

impl Drop for PromptStartGuard {
    fn drop(&mut self) {
        self.finish();
    }
}

fn response_controls(response: &Value) -> Result<(Option<Value>, Value), Error> {
    let modes = response
        .get("modes")
        .filter(|value| !value.is_null())
        .cloned();
    let config_options = response
        .get("configOptions")
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    validate_session_controls(modes.as_ref(), Some(&config_options)).map_err(semantic_error)?;
    Ok((modes, config_options))
}

fn track_session(
    state: &mut BridgeState,
    session_id: &str,
    cwd: PathBuf,
    response: &Value,
    allowed_pending_attachment: Option<&str>,
) -> Result<Vec<SessionNotification>, Error> {
    validate_agent_session_id(session_id).map_err(semantic_error)?;
    if state.active_sessions.contains_key(session_id)
        || state.pending_deletions.contains(session_id)
        || (state.pending_attachments.contains(session_id)
            && allowed_pending_attachment != Some(session_id))
    {
        return Err(semantic_error(format!(
            "Agent returned a duplicate active session ID: {session_id}"
        )));
    }
    if state.active_sessions.len() >= MAX_TRACKED_SESSIONS {
        return Err(semantic_error(format!(
            "Active session limit reached ({MAX_TRACKED_SESSIONS})"
        )));
    }
    let (modes, config_options) = response_controls(response)?;
    let validation = state
        .session_updates
        .entry(session_id.to_string())
        .or_default();
    if let Some(reason) = &validation.invalid_reason {
        return Err(semantic_error(format!(
            "Agent session replay was invalid: {reason}"
        )));
    }
    state.active_sessions.insert(
        session_id.to_string(),
        ActiveSession {
            cwd,
            modes,
            config_options,
        },
    );
    let early_updates = state.early_updates.remove(session_id).unwrap_or_default();
    let early_bytes = early_updates
        .iter()
        .filter_map(|notification| serde_json::to_vec(notification).ok())
        .map(|bytes| bytes.len())
        .sum::<usize>();
    state.early_update_count = state.early_update_count.saturating_sub(early_updates.len());
    state.early_update_bytes = state.early_update_bytes.saturating_sub(early_bytes);
    Ok(early_updates)
}

fn clear_pending_creation_replays(state: &mut BridgeState) {
    let session_ids = state.early_updates.keys().cloned().collect::<Vec<_>>();
    state.early_updates.clear();
    state.early_update_count = 0;
    state.early_update_bytes = 0;
    for session_id in session_ids {
        if !state.active_sessions.contains_key(&session_id)
            && !state.pending_attachments.contains(&session_id)
        {
            state.session_updates.remove(&session_id);
        }
    }
}

async fn close_rejected_chat_session(
    connection: &ConnectionTo<Agent>,
    session_id: &str,
    supported: bool,
) {
    if supported && !session_id.is_empty() {
        let _ = connection
            .send_request(CloseSessionRequest::new(session_id.to_string()))
            .block_task()
            .await;
    }
}

fn can_close_rejected_chat_session(state: &BridgeState, session_id: &str) -> bool {
    !state.active_sessions.contains_key(session_id)
        && state
            .agent_capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.session_capabilities.close.is_some())
}

#[derive(Clone)]
struct CommandContext {
    options: Arc<Options>,
    state: Arc<Mutex<BridgeState>>,
    sink: EventSink,
    terminals: Option<TerminalManager>,
    filesystem: Option<Arc<WorkspaceFileSystem>>,
    auth_terminal: AuthTerminalManager,
    prompt_lifecycle: Arc<PromptLifecycle>,
}

#[derive(Clone, Copy)]
enum SessionOperation {
    Prompt,
    Fork,
    Close,
    Control,
}

pub(crate) enum BridgeInput {
    Command(String),
    SubscribersGone,
    Restart,
    Shutdown,
}

pub async fn run(
    options: Arc<Options>,
    commands: mpsc::Receiver<BridgeInput>,
    events: mpsc::UnboundedSender<String>,
) {
    let sink = EventSink { tx: events };
    let mcp_servers = options
        .mcp_servers
        .iter()
        .map(|server| json!({ "name": server_name(server), "type": server_type(server) }))
        .collect::<Vec<_>>();
    sink.send(json!({
        "type": "bridge/hello",
        "transport": options.transport.as_str(),
        "command": options.command,
        "cwd": options.cwd,
        "readOnly": options.read_only,
        "additionalDirectories": options.additional_directories,
        "mcpServers": mcp_servers,
    }));
    sink.send(json!({ "type": "bridge/phase", "phase": "starting" }));

    let result = match options.transport {
        Transport::Stdio => {
            let config = AcpAgentConfig::new(&options.command[0])
                .args(options.command.iter().skip(1).cloned());
            let debug_sink = sink.clone();
            let (fatal_tx, mut fatal_rx) = mpsc::unbounded_channel();
            let agent = BoundedAcpAgent::new(config)
                .on_stderr(move |chunk| {
                    debug_sink.send(json!({
                        "type": "bridge/stderr",
                        "chunk": chunk,
                    }));
                })
                .on_fatal(move |error| {
                    let _ = fatal_tx.send(error);
                });
            tokio::select! {
                biased;
                error = fatal_rx.recv() => Err(error.unwrap_or_else(|| {
                    Error::internal_error().data("Agent process monitor stopped unexpectedly")
                })),
                result = run_connection(agent, options.clone(), commands, sink.clone()) => result,
            }
        }
        Transport::Http | Transport::Ws => match HttpClient::with_endpoint(&options.command[0]) {
            Ok(client) => run_connection(client, options.clone(), commands, sink.clone()).await,
            Err(error) => Err(Error::invalid_params().data(error.to_string())),
        },
    };

    match result {
        Ok(()) => sink.send(json!({ "type": "bridge/phase", "phase": "stopped" })),
        Err(error) => {
            sink.acp_error(error, None, None);
            sink.send(json!({ "type": "bridge/phase", "phase": "error" }));
        }
    }
}

async fn run_connection<T>(
    transport: T,
    options: Arc<Options>,
    mut commands: mpsc::Receiver<BridgeInput>,
    sink: EventSink,
) -> Result<(), Error>
where
    T: ConnectTo<Client>,
{
    let state = Arc::new(Mutex::new(BridgeState::default()));
    let filesystem = (options.transport == Transport::Stdio)
        .then(|| {
            WorkspaceFileSystem::new(
                &options.cwd,
                options.read_only,
                &options.additional_directories,
            )
        })
        .transpose()?
        .map(Arc::new);
    let terminals = filesystem
        .clone()
        .map(|filesystem| TerminalManager::new(filesystem, sink.tx.clone()));
    let mcp = McpManager::new(
        options.cwd.clone(),
        options.acp_mcp_providers.clone(),
        sink.tx.clone(),
    );
    let auth_terminal = AuthTerminalManager::new(sink.tx.clone());
    let prompt_lifecycle = Arc::new(PromptLifecycle::default());

    let builder = Client
        .builder()
        .on_receive_notification(
            {
                let sink = sink.clone();
                let state = state.clone();
                let terminals = terminals.clone();
                async move |notification: SessionNotification, _connection| {
                    handle_session_update(notification, &state, &sink, terminals.as_ref()).await;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let sink = sink.clone();
                let state = state.clone();
                let terminals = terminals.clone();
                async move |request: RequestPermissionRequest, responder, _connection| {
                    ensure_relay_size(&request, "permission request")?;
                    let permission_id = Uuid::new_v4().to_string();
                    let session_id = request.session_id.0.to_string();
                    let request_value = serde_json::to_value(&request)?;
                    let tool_call = request_value
                        .get("toolCall")
                        .ok_or_else(|| Error::invalid_request().data("missing permission tool call"))?;
                    for terminal_id in terminal_references(tool_call) {
                        let Some(terminals) = &terminals else {
                            return Err(Error::invalid_request().data(format!(
                                "unknown terminal: {terminal_id}"
                            )));
                        };
                        terminals.assert_reference(&terminal_id, &session_id).await?;
                    }
                    let tool_call_id = request.tool_call.tool_call_id.0.to_string();
                    let (sender, receiver) = oneshot::channel();
                    {
                        let mut state = state.lock().await;
                        if !state.active_sessions.contains_key(&session_id) {
                            return Err(Error::invalid_params().data(format!(
                                "unknown or inactive session: {session_id}"
                            )));
                        }
                        let option_ids = validate_permission_request(
                            state.session_updates.entry(session_id.clone()).or_default(),
                            &request,
                        )
                        .map_err(semantic_error)?;
                        if state.permissions.len() >= MAX_PENDING_INTERACTIONS {
                            return responder.respond(RequestPermissionResponse::new(
                                RequestPermissionOutcome::Cancelled,
                            ));
                        }
                        if state.permissions.values().any(|pending| {
                            pending.session_id == session_id
                                && pending.tool_call_id == tool_call_id
                        }) {
                            return Err(Error::invalid_request().data(format!(
                                "A permission request is already pending for tool call: {tool_call_id}"
                            )));
                        }
                        state
                            .permissions
                            .insert(permission_id.clone(), PendingPermission {
                                session_id,
                                tool_call_id,
                                option_ids,
                                sender,
                            });
                    }
                    sink.send(json!({
                        "type": "acp/permission_request",
                        "permissionId": permission_id,
                        "request": request,
                    }));
                    let response = receiver.await.unwrap_or_else(|_| {
                        RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
                    });
                    responder.respond(response)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sink = sink.clone();
                let state = state.clone();
                async move |request: CreateElicitationRequest, responder, _connection| {
                    ensure_relay_size(&request, "elicitation request")?;
                    let request_value =
                        validate_elicitation_request(&request).map_err(semantic_error)?;
                    let elicitation_id = Uuid::new_v4().to_string();
                    let url_elicitation_id = match &request.mode {
                        ElicitationMode::Url(mode) => {
                            Some(mode.elicitation_id.0.to_string())
                        }
                        _ => None,
                    };
                    let session_id = match request.scope() {
                        ElicitationScope::Session(scope) => {
                            Some(scope.session_id.0.to_string())
                        }
                        ElicitationScope::Request(_) => None,
                        _ => None,
                    };
                    let (sender, receiver) = oneshot::channel();
                    {
                        let mut state = state.lock().await;
                        if let Some(session_id) = &session_id
                            && !state.active_sessions.contains_key(session_id)
                        {
                            return Err(Error::invalid_params().data(
                                "elicitation references an unknown or inactive session",
                            ));
                        }
                        if state.elicitations.len() >= MAX_PENDING_INTERACTIONS {
                            return responder.respond(CreateElicitationResponse::new(
                                ElicitationAction::Cancel,
                            ));
                        }
                        if let Some(url_elicitation_id) = &url_elicitation_id {
                            if url_elicitation_id.is_empty() || url_elicitation_id.len() > 1_024 {
                                return Err(Error::invalid_params()
                                    .data("Agent returned an invalid URL elicitation ID"));
                            }
                            if !state
                                .seen_url_elicitation_ids
                                .insert(url_elicitation_id.clone())
                            {
                                return Err(Error::invalid_params().data(format!(
                                    "Agent reused a URL elicitation ID: {url_elicitation_id}"
                                )));
                            }
                            if state.seen_url_elicitation_ids.len() > MAX_URL_ELICITATION_IDS {
                                state.seen_url_elicitation_ids.remove(url_elicitation_id);
                                return Err(Error::invalid_request().data(format!(
                                    "Agent exceeded {MAX_URL_ELICITATION_IDS} URL elicitation IDs"
                                )));
                            }
                        }
                        state
                            .elicitations
                            .insert(elicitation_id.clone(), PendingElicitation {
                                session_id,
                                url_elicitation_id,
                                request: request_value,
                                sender,
                            });
                    }
                    sink.send(json!({
                        "type": "acp/elicitation_request",
                        "elicitationId": elicitation_id,
                        "request": request,
                    }));
                    let response = receiver.await.unwrap_or_else(|_| {
                        CreateElicitationResponse::new(ElicitationAction::Cancel)
                    });
                    responder.respond(response)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let sink = sink.clone();
                let state = state.clone();
                async move |notification: CompleteElicitationNotification, _connection| {
                    ensure_relay_size(&notification, "elicitation completion")?;
                    let elicitation_id = notification.elicitation_id.0.to_string();
                    if state
                        .lock()
                        .await
                        .url_elicitations
                        .remove(&elicitation_id)
                        .is_none()
                    {
                        return Err(Error::invalid_params().data(format!(
                            "URL elicitation was not accepted or is no longer active: {elicitation_id}"
                        )));
                    }
                    sink.send(json!({
                        "type": "acp/elicitation_complete",
                        "notification": notification,
                    }));
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let filesystem = filesystem.clone();
                let state = state.clone();
                async move |request: ReadTextFileRequest, responder, connection| {
                    let filesystem = filesystem.clone();
                    let state = state.clone();
                    let cancellation = responder.cancellation();
                    connection.spawn(async move {
                        let result = match filesystem {
                            Some(filesystem) => {
                                let session_id = request.session_id.0.to_string();
                                match require_active(&session_id, &state).await {
                                    Ok(()) => {
                                        filesystem.read_cancellable(request, cancellation).await
                                    }
                                    Err(error) => Err(error),
                                }
                            }
                            None => Err(Error::method_not_found()
                                .data("filesystem methods are unavailable for remote transports")),
                        };
                        responder.respond_with_result(result)
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let filesystem = filesystem.clone();
                let state = state.clone();
                async move |request: WriteTextFileRequest, responder, connection| {
                    let filesystem = filesystem.clone();
                    let state = state.clone();
                    let cancellation = responder.cancellation();
                    connection.spawn(async move {
                        let result = match filesystem {
                            Some(filesystem) => {
                                let session_id = request.session_id.0.to_string();
                                match require_active(&session_id, &state).await {
                                    Ok(()) => {
                                        filesystem.write_cancellable(request, cancellation).await
                                    }
                                    Err(error) => Err(error),
                                }
                            }
                            None => Err(Error::method_not_found()
                                .data("filesystem methods are unavailable for remote transports")),
                        };
                        responder.respond_with_result(result)
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let terminals = terminals.clone();
                let state = state.clone();
                async move |request: CreateTerminalRequest, responder, connection| {
                    let terminals = terminals.clone();
                    let state = state.clone();
                    connection.spawn(async move {
                        let result = match terminals {
                            Some(terminals) => {
                                let session_id = request.session_id.0.to_string();
                                match require_active(&session_id, &state).await {
                                    Ok(()) => terminals.create(request).await,
                                    Err(error) => Err(error),
                                }
                            }
                            None => Err(Error::method_not_found()
                                .data("terminal methods are unavailable for remote transports")),
                        };
                        responder.respond_with_result(result)
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let terminals = terminals.clone();
                async move |request: TerminalOutputRequest, responder, connection| {
                    let terminals = terminals.clone();
                    connection.spawn(async move {
                        let result = match terminals {
                            Some(terminals) => terminals.output(request).await,
                            None => Err(Error::method_not_found()
                                .data("terminal methods are unavailable for remote transports")),
                        };
                        responder.respond_with_result(result)
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let terminals = terminals.clone();
                async move |request: WaitForTerminalExitRequest, responder, connection| {
                    let terminals = terminals.clone();
                    let cancellation = responder.cancellation();
                    connection.spawn(async move {
                        let result = match terminals {
                            Some(terminals) => terminals.wait_for_exit(request, cancellation).await,
                            None => Err(Error::method_not_found()
                                .data("terminal methods are unavailable for remote transports")),
                        };
                        responder.respond_with_result(result)
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let terminals = terminals.clone();
                async move |request: KillTerminalRequest, responder, connection| {
                    let terminals = terminals.clone();
                    connection.spawn(async move {
                        let result = match terminals {
                            Some(terminals) => terminals.kill(request).await,
                            None => Err(Error::method_not_found()
                                .data("terminal methods are unavailable for remote transports")),
                        };
                        responder.respond_with_result(result)
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let terminals = terminals.clone();
                async move |request: ReleaseTerminalRequest, responder, connection| {
                    let terminals = terminals.clone();
                    connection.spawn(async move {
                        let result = match terminals {
                            Some(terminals) => terminals.release(request).await,
                            None => Err(Error::method_not_found()
                                .data("terminal methods are unavailable for remote transports")),
                        };
                        responder.respond_with_result(result)
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let mcp = mcp.clone();
                async move |request: ConnectMcpRequest, responder, connection| {
                    let manager = mcp.clone();
                    let acp = connection.clone();
                    let cancellation = responder.cancellation();
                    connection.spawn(async move {
                        responder.respond_with_result(
                            manager.connect(request, acp, cancellation).await,
                        )
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let mcp = mcp.clone();
                async move |request: MessageMcpRequest, responder, connection| {
                    let manager = mcp.clone();
                    let cancellation = responder.cancellation();
                    connection.spawn(async move {
                        responder.respond_with_result(manager.message(request, cancellation).await)
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let mcp = mcp.clone();
                async move |notification: MessageMcpNotification, connection| {
                    let manager = mcp.clone();
                    connection.spawn(async move { manager.notify(notification).await })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let mcp = mcp.clone();
                async move |request: DisconnectMcpRequest, responder, connection| {
                    let manager = mcp.clone();
                    connection.spawn(async move {
                        responder.respond_with_result(manager.disconnect(request).await)
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        );

    builder
        .connect_with(transport, async move |connection| {
            sink.send(json!({ "type": "bridge/phase", "phase": "initializing" }));
            let initialize = InitializeRequest::new(ProtocolVersion::V1)
                .client_capabilities(client_capabilities(&options))
                .client_info(
                    Implementation::new("attyd", env!("CARGO_PKG_VERSION"))
                        .title("attyd web client"),
                );
            let response = connection.send_request(initialize).block_task().await?;
            ensure_relay_size(&response, "initialize response")?;
            if response.protocol_version != ProtocolVersion::V1 {
                return Err(Error::invalid_request().data(format!(
                    "unsupported ACP protocol version: {}",
                    response.protocol_version
                )));
            }
            validate_auth_methods(
                &response.auth_methods,
                &response.agent_capabilities,
                options.transport == Transport::Stdio,
            )?;
            validate_configured_capabilities(&response.agent_capabilities, &options)?;
            {
                let mut state = state.lock().await;
                state.agent_capabilities = Some(response.agent_capabilities.clone());
                state.auth_methods = response
                    .auth_methods
                    .iter()
                    .map(|method| (method.id().0.to_string(), method.clone()))
                    .collect();
            }
            sink.typed("acp/initialized", &response);
            sink.send(json!({ "type": "bridge/phase", "phase": "ready" }));

            loop {
                let raw = match commands.recv().await {
                    Some(BridgeInput::Command(raw)) => raw,
                    Some(BridgeInput::SubscribersGone) => {
                        cancel_prompts_on_disconnect(&connection, &state, &sink, &prompt_lifecycle)
                            .await;
                        detach_browser_sessions(&state).await;
                        auth_terminal.close();
                        continue;
                    }
                    Some(BridgeInput::Restart | BridgeInput::Shutdown) | None => break,
                };
                if raw.len() > MAX_BRIDGE_MESSAGE_BYTES {
                    sink.error(
                        format!("WebSocket command exceeds {MAX_BRIDGE_MESSAGE_BYTES} bytes"),
                        None,
                        None,
                    );
                    continue;
                }
                let command = match serde_json::from_str::<Value>(&raw) {
                    Ok(Value::Object(command)) => Value::Object(command),
                    Ok(_) => {
                        sink.error("WebSocket command must be a JSON object", None, None);
                        continue;
                    }
                    Err(error) => {
                        sink.error(format!("Invalid WebSocket command: {error}"), None, None);
                        continue;
                    }
                };
                let operation = command
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if operation == "bridge/ping" {
                    if let Some(nonce) = command.get("nonce").and_then(Value::as_str) {
                        sink.send(json!({ "type": "bridge/pong", "nonce": nonce }));
                    } else {
                        sink.error("bridge/ping requires nonce", None, Some(&operation));
                    }
                    continue;
                }
                let task_connection = connection.clone();
                let prompt_start =
                    (operation == "session/prompt").then(|| prompt_lifecycle.register_start());
                let context = CommandContext {
                    options: options.clone(),
                    state: state.clone(),
                    sink: sink.clone(),
                    terminals: terminals.clone(),
                    filesystem: filesystem.clone(),
                    auth_terminal: auth_terminal.clone(),
                    prompt_lifecycle: prompt_lifecycle.clone(),
                };
                connection.spawn(async move {
                    let request_id = command
                        .get("requestId")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    let task_sink = context.sink.clone();
                    if let Err(error) =
                        handle_command(command, task_connection, context, prompt_start).await
                    {
                        task_sink.acp_error(error, request_id.as_deref(), Some(&operation));
                    }
                    Ok(())
                })?;
            }
            cancel_prompts_on_disconnect(&connection, &state, &sink, &prompt_lifecycle).await;
            if let Some(terminals) = terminals {
                terminals.close_all().await;
            }
            mcp.close_all().await;
            auth_terminal.close();
            Ok(())
        })
        .await
}

fn client_capabilities(options: &Options) -> ClientCapabilities {
    let local = options.transport == Transport::Stdio;
    ClientCapabilities::new()
        .fs(FileSystemCapabilities::new()
            .read_text_file(local)
            .write_text_file(local && !options.read_only))
        .terminal(local)
        .session(
            ClientSessionCapabilities::new()
                .compaction(CompactionCapabilities::new())
                .config_options(
                    SessionConfigOptionsCapabilities::new()
                        .boolean(BooleanConfigOptionCapabilities::new()),
                ),
        )
        .plan(PlanCapabilities::new())
        .auth(AuthCapabilities::new().terminal(local))
        .elicitation(
            ElicitationCapabilities::new()
                .form(ElicitationFormCapabilities::new())
                .url(ElicitationUrlCapabilities::new()),
        )
}

async fn handle_session_update(
    notification: SessionNotification,
    state: &Arc<Mutex<BridgeState>>,
    sink: &EventSink,
    terminals: Option<&TerminalManager>,
) {
    if let Err(error) = ensure_relay_size(&notification, "session update") {
        sink.acp_error(error, None, Some("session/update"));
        return;
    }
    let session_id = notification.session_id.0.to_string();
    let update = match serde_json::to_value(&notification.update) {
        Ok(update) => update,
        Err(error) => {
            sink.acp_error(error.into(), None, Some("session/update"));
            return;
        }
    };
    let tracked = {
        let mut state = state.lock().await;
        if state.active_sessions.contains_key(&session_id)
            || state.pending_attachments.contains(&session_id)
        {
            true
        } else if state.pending_creations > 0 {
            if let Err(message) = validate_agent_session_id(&session_id) {
                sink.acp_error(semantic_error(message), None, Some("session/update"));
                return;
            }
            if !state.session_updates.contains_key(&session_id)
                && state.early_updates.len() >= state.pending_creations
            {
                sink.acp_error(
                    semantic_error(
                        "Agent sent updates for more session IDs than pending session creations",
                    ),
                    None,
                    Some("session/update"),
                );
                return;
            }
            state.session_updates.entry(session_id.clone()).or_default();
            state.early_updates.entry(session_id.clone()).or_default();
            true
        } else {
            // Late updates for closed or failed sessions must not leak into the visible thread.
            false
        }
    };
    if !tracked {
        return;
    }

    let tool_update = matches!(
        update.get("sessionUpdate").and_then(Value::as_str),
        Some("tool_call" | "tool_call_update")
    );
    if tool_update {
        for terminal_id in terminal_references(&update) {
            let result = match terminals {
                Some(terminals) => terminals.assert_reference(&terminal_id, &session_id).await,
                None => {
                    Err(Error::invalid_request().data(format!("unknown terminal: {terminal_id}")))
                }
            };
            if let Err(error) = result {
                let mut state = state.lock().await;
                if !state.active_sessions.contains_key(&session_id)
                    && let Some(validation) = state.session_updates.get_mut(&session_id)
                {
                    validation.invalid_reason.get_or_insert_with(|| {
                        error
                            .data
                            .as_ref()
                            .map_or_else(|| error.message.clone(), Value::to_string)
                    });
                }
                drop(state);
                sink.acp_error(error, None, Some("session/update"));
                return;
            }
        }
    }

    let outcome = {
        let mut state = state.lock().await;
        let active = state.active_sessions.contains_key(&session_id);
        let attachment = state.pending_attachments.contains(&session_id);
        let early_creation = !active && !attachment && state.pending_creations > 0;
        if !active && !attachment && !early_creation {
            return;
        }
        let validation = state.session_updates.entry(session_id.clone()).or_default();
        if let Err(message) = validate_and_track_session_update(validation, &update) {
            if !active {
                validation.invalid_reason.get_or_insert(message.clone());
            }
            Err(semantic_error(message))
        } else if early_creation {
            let notification_bytes = serde_json::to_vec(&notification)
                .map(|bytes| bytes.len())
                .unwrap_or(MAX_EARLY_UPDATE_BYTES + 1);
            let replay_error = if state.early_update_count >= MAX_EARLY_UPDATES {
                Some(format!(
                    "Agent exceeded {MAX_EARLY_UPDATES} updates before completing session creation"
                ))
            } else if state.early_update_bytes.saturating_add(notification_bytes)
                > MAX_EARLY_UPDATE_BYTES
            {
                Some(format!(
                    "Agent session creation replay exceeds {MAX_EARLY_UPDATE_BYTES} bytes"
                ))
            } else {
                None
            };
            if let Some(message) = replay_error {
                if let Some(validation) = state.session_updates.get_mut(&session_id) {
                    validation.invalid_reason.get_or_insert(message.clone());
                }
                Err(semantic_error(message))
            } else {
                state
                    .early_updates
                    .entry(session_id.clone())
                    .or_default()
                    .push(notification.clone());
                state.early_update_count += 1;
                state.early_update_bytes += notification_bytes;
                Ok(false)
            }
        } else {
            if let Some(session) = state.active_sessions.get_mut(&session_id) {
                match update.get("sessionUpdate").and_then(Value::as_str) {
                    Some("current_mode_update") => {
                        if let (Some(modes), Some(mode_id)) = (
                            session.modes.as_mut().and_then(Value::as_object_mut),
                            update.get("currentModeId").and_then(Value::as_str),
                        ) {
                            modes.insert("currentModeId".to_string(), json!(mode_id));
                        }
                    }
                    Some("config_option_update") => {
                        session.config_options = update
                            .get("configOptions")
                            .cloned()
                            .unwrap_or_else(|| Value::Array(Vec::new()));
                    }
                    _ => {}
                }
            }
            Ok(true)
        }
    };
    match outcome {
        Ok(true) => {
            sink.send(json!({ "type": "acp/session_update", "notification": notification }));
        }
        Ok(false) => {}
        Err(error) => sink.acp_error(error, None, Some("session/update")),
    }
}

async fn cancel_prompts_on_disconnect(
    connection: &ConnectionTo<Agent>,
    state: &Arc<Mutex<BridgeState>>,
    sink: &EventSink,
    lifecycle: &PromptLifecycle,
) {
    // A browser can disconnect immediately after sending session/prompt. Wait
    // until every accepted prompt command has either failed validation or
    // published its ACP request so cancellation cannot overtake the prompt.
    lifecycle.wait_until_started().await;

    let session_ids = state.lock().await.prompts.clone();
    if session_ids.is_empty() {
        return;
    }

    for session_id in &session_ids {
        let _ = connection.send_notification(CancelNotification::new(session_id.clone()));
        cancel_interactions(session_id, "client_disconnected", state, sink).await;
    }

    // ACP cancellation completes through the original session/prompt response.
    // Keep the connection alive briefly so the notification reaches the Agent
    // and that response can be consumed before the transport is released.
    let wait_for_completion = async {
        loop {
            let changed = lifecycle.changed.notified();
            let all_finished = {
                let state = state.lock().await;
                session_ids
                    .iter()
                    .all(|session_id| !state.prompts.contains(session_id))
            };
            if all_finished {
                return;
            }
            changed.await;
        }
    };
    let _ = tokio::time::timeout(DISCONNECT_CANCEL_GRACE_PERIOD, wait_for_completion).await;
}

async fn detach_browser_sessions(state: &Arc<Mutex<BridgeState>>) {
    let (permissions, elicitations) = {
        let mut state = state.lock().await;
        state.active_sessions.clear();
        state.session_updates.clear();
        state.url_elicitations.clear();
        (
            std::mem::take(&mut state.permissions),
            std::mem::take(&mut state.elicitations),
        )
    };
    for (_, pending) in permissions {
        let _ = pending.sender.send(RequestPermissionResponse::new(
            RequestPermissionOutcome::Cancelled,
        ));
    }
    for (_, pending) in elicitations {
        let _ = pending
            .sender
            .send(CreateElicitationResponse::new(ElicitationAction::Cancel));
    }
}

async fn handle_command(
    command: Value,
    connection: ConnectionTo<Agent>,
    context: CommandContext,
    mut prompt_start: Option<PromptStartGuard>,
) -> Result<(), Error> {
    let CommandContext {
        options,
        state,
        sink,
        terminals,
        filesystem,
        auth_terminal,
        prompt_lifecycle,
    } = context;
    let operation = bounded_string_field(&command, "type", MAX_BRIDGE_TYPE_LENGTH)?;
    match operation {
        "auth/authenticate" => {
            let request_id = string_field(&command, "requestId")?;
            let method_id = string_field(&command, "methodId")?;
            let method = state
                .lock()
                .await
                .auth_methods
                .get(method_id)
                .cloned()
                .ok_or_else(|| {
                    Error::invalid_params().data(format!(
                        "authentication method was not offered: {method_id}"
                    ))
                })?;
            if matches!(method, AuthMethod::Terminal(_)) {
                return Err(Error::invalid_request()
                    .data("terminal authentication methods must use auth/terminal_start"));
            }
            let response = connection
                .send_request(AuthenticateRequest::new(method_id.to_string()))
                .block_task()
                .await?;
            ensure_relay_size(&response, "authentication response")?;
            sink.send(json!({
                "type": "acp/authenticated",
                "requestId": request_id,
                "methodId": method_id,
                "response": response,
            }));
        }
        "auth/logout" => {
            let request_id = string_field(&command, "requestId")?;
            require_agent_method("auth/logout", &state).await?;
            let response = connection
                .send_request(LogoutRequest::new())
                .block_task()
                .await?;
            ensure_relay_size(&response, "logout response")?;
            sink.send(json!({
                "type": "acp/logged_out",
                "requestId": request_id,
                "response": response,
            }));
        }
        "session/new" => {
            let request_id = string_field(&command, "requestId")?.to_string();
            let cwd = new_session_cwd(&command, &options)?;
            {
                let mut state = state.lock().await;
                if state.active_sessions.len()
                    + state.pending_creations
                    + state.pending_attachments.len()
                    >= MAX_TRACKED_SESSIONS
                {
                    return Err(semantic_error(format!(
                        "Active session limit reached ({MAX_TRACKED_SESSIONS})"
                    )));
                }
                state.pending_creations += 1;
            }
            let request = NewSessionRequest::new(&cwd)
                .additional_directories(local_additional_directories(&options))
                .mcp_servers(options.mcp_servers.clone());
            let result = connection.send_request(request).block_task().await;
            let mut state = state.lock().await;
            state.pending_creations = state.pending_creations.saturating_sub(1);
            match result {
                Ok(response) => {
                    let session_id = response.session_id.0.to_string();
                    let response_value = serde_json::to_value(&response)?;
                    let tracked =
                        ensure_relay_size(&response, "session/new response").and_then(|_| {
                            track_session(
                                &mut state,
                                &session_id,
                                cwd.clone(),
                                &response_value,
                                None,
                            )
                        });
                    let early_updates = match tracked {
                        Ok(updates) => updates,
                        Err(error) => {
                            let can_close = can_close_rejected_chat_session(&state, &session_id);
                            if state.pending_creations == 0 {
                                clear_pending_creation_replays(&mut state);
                            }
                            drop(state);
                            close_rejected_chat_session(&connection, &session_id, can_close).await;
                            return Err(error);
                        }
                    };
                    drop(state);
                    let mut event = json!({
                        "type": "acp/session_created",
                        "requestId": request_id,
                        "cwd": cwd,
                        "response": response,
                    });
                    if !early_updates.is_empty() {
                        event["earlyUpdates"] = json!(early_updates);
                    }
                    sink.send(event);
                }
                Err(error) => {
                    if state.pending_creations == 0 {
                        clear_pending_creation_replays(&mut state);
                    }
                    return Err(error);
                }
            }
        }
        "session/list" => {
            let request_id = string_field(&command, "requestId")?.to_string();
            require_agent_method("session/list", &state).await?;
            let cursor = match command.get("cursor") {
                None | Some(Value::Null) => None,
                Some(Value::String(cursor))
                    if !cursor.is_empty() && cursor.encode_utf16().count() <= 4_096 =>
                {
                    Some(cursor.clone())
                }
                Some(_) => {
                    return Err(Error::invalid_params()
                        .data("session/list cursor must be a non-empty bounded string"));
                }
            };
            {
                let mut state = state.lock().await;
                if state.session_list_in_flight {
                    return Err(
                        Error::invalid_request().data("a session/list request is already running")
                    );
                }
                if cursor.is_some() && cursor != state.listed_next_cursor {
                    return Err(Error::invalid_params()
                        .data("session/list cursor was not offered by the Agent"));
                }
                state.session_list_in_flight = true;
            }
            let requested_cwd =
                (options.transport == Transport::Stdio).then(|| options.cwd.clone());
            let result = connection
                .send_request(
                    ListSessionsRequest::new()
                        .cwd(requested_cwd)
                        .cursor(cursor.clone()),
                )
                .block_task()
                .await;
            let mut state = state.lock().await;
            state.session_list_in_flight = false;
            let response = result?;
            let page_bytes = validate_session_list_page(&response, cursor.as_deref(), &state)?;
            if cursor.is_none() {
                state.listed_sessions.clear();
                state.listed_cursors.clear();
                state.listed_session_bytes = 0;
            }
            for session in &response.sessions {
                state
                    .listed_sessions
                    .insert(session.session_id.0.to_string(), session.clone());
            }
            state.listed_next_cursor = response.next_cursor.clone();
            if let Some(next_cursor) = &response.next_cursor {
                state.listed_cursors.insert(next_cursor.clone());
            }
            state.listed_session_bytes = state.listed_session_bytes.saturating_add(page_bytes);
            drop(state);
            sink.send(json!({
                "type": "acp/sessions_listed",
                "requestId": request_id,
                "cursor": cursor,
                "response": response,
            }));
        }
        "session/load" | "session/resume" => {
            let request_id = string_field(&command, "requestId")?.to_string();
            let session_id = string_field(&command, "sessionId")?.to_string();
            require_agent_method(operation, &state).await?;
            let cwd = reserve_attachment(&session_id, &state).await?;
            let result = if operation == "session/load" {
                connection
                    .send_request(
                        LoadSessionRequest::new(session_id.clone(), &cwd)
                            .additional_directories(local_additional_directories(&options))
                            .mcp_servers(options.mcp_servers.clone()),
                    )
                    .block_task()
                    .await
                    .map(|response| {
                        serde_json::to_value(response).expect("ACP response serializes")
                    })
            } else {
                connection
                    .send_request(
                        ResumeSessionRequest::new(session_id.clone(), &cwd)
                            .additional_directories(local_additional_directories(&options))
                            .mcp_servers(options.mcp_servers.clone()),
                    )
                    .block_task()
                    .await
                    .map(|response| {
                        serde_json::to_value(response).expect("ACP response serializes")
                    })
            };
            let mut state = state.lock().await;
            let response = match result {
                Ok(response) => response,
                Err(error) => {
                    state.pending_attachments.remove(&session_id);
                    state.session_updates.remove(&session_id);
                    return Err(error);
                }
            };
            if let Err(error) = ensure_relay_size(&response, "session attachment response") {
                state.pending_attachments.remove(&session_id);
                state.session_updates.remove(&session_id);
                return Err(error);
            }
            if let Err(error) = track_session(
                &mut state,
                &session_id,
                cwd.clone(),
                &response,
                Some(&session_id),
            ) {
                state.pending_attachments.remove(&session_id);
                state.session_updates.remove(&session_id);
                return Err(error);
            }
            state.pending_attachments.remove(&session_id);
            drop(state);
            sink.send(json!({
                "type": "acp/session_attached",
                "requestId": request_id,
                "method": if operation == "session/load" { "load" } else { "resume" },
                "sessionId": session_id,
                "cwd": cwd,
                "response": response,
            }));
        }
        "session/fork" => {
            let request_id = string_field(&command, "requestId")?.to_string();
            let source_id = string_field(&command, "sessionId")?.to_string();
            require_agent_method("session/fork", &state).await?;
            let cwd = {
                let mut state = state.lock().await;
                let cwd = state
                    .active_sessions
                    .get(&source_id)
                    .map(|session| session.cwd.clone())
                    .ok_or_else(|| {
                        Error::invalid_params()
                            .data(format!("unknown or inactive session: {source_id}"))
                    })?;
                reserve_session_operation(&mut state, &source_id, SessionOperation::Fork)?;
                if state.active_sessions.len()
                    + state.pending_creations
                    + state.pending_attachments.len()
                    >= MAX_TRACKED_SESSIONS
                {
                    release_session_operation(&mut state, &source_id, SessionOperation::Fork);
                    return Err(semantic_error(format!(
                        "Active session limit reached ({MAX_TRACKED_SESSIONS})"
                    )));
                }
                state.pending_creations += 1;
                cwd
            };
            let result = connection
                .send_request(
                    ForkSessionRequest::new(source_id.clone(), &cwd)
                        .additional_directories(local_additional_directories(&options))
                        .mcp_servers(options.mcp_servers.clone()),
                )
                .block_task()
                .await;
            let mut state = state.lock().await;
            state.pending_creations = state.pending_creations.saturating_sub(1);
            release_session_operation(&mut state, &source_id, SessionOperation::Fork);
            let response = match result {
                Ok(response) => response,
                Err(error) => {
                    if state.pending_creations == 0 {
                        clear_pending_creation_replays(&mut state);
                    }
                    return Err(error);
                }
            };
            let session_id = response.session_id.0.to_string();
            let response_value = serde_json::to_value(&response)?;
            let tracked = ensure_relay_size(&response, "session/fork response").and_then(|_| {
                if session_id == source_id {
                    Err(Error::invalid_request()
                        .data("Agent returned the source session ID for session/fork"))
                } else {
                    track_session(&mut state, &session_id, cwd.clone(), &response_value, None)
                }
            });
            let early_updates = match tracked {
                Ok(updates) => updates,
                Err(error) => {
                    let can_close = can_close_rejected_chat_session(&state, &session_id);
                    if state.pending_creations == 0 {
                        clear_pending_creation_replays(&mut state);
                    }
                    drop(state);
                    close_rejected_chat_session(&connection, &session_id, can_close).await;
                    return Err(error);
                }
            };
            drop(state);
            let mut event = json!({
                "type": "acp/session_forked",
                "requestId": request_id,
                "sourceSessionId": source_id,
                "cwd": cwd,
                "response": response,
            });
            if !early_updates.is_empty() {
                event["earlyUpdates"] = json!(early_updates);
            }
            sink.send(event);
        }
        "session/close" => {
            let request_id = string_field(&command, "requestId")?;
            let session_id = string_field(&command, "sessionId")?;
            require_agent_method("session/close", &state).await?;
            {
                let mut state = state.lock().await;
                reserve_session_operation(&mut state, session_id, SessionOperation::Close)?;
            }
            let result = connection
                .send_request(CloseSessionRequest::new(session_id.to_string()))
                .block_task()
                .await;
            {
                let mut state = state.lock().await;
                release_session_operation(&mut state, session_id, SessionOperation::Close);
            }
            let response = result?;
            ensure_relay_size(&response, "session/close response")?;
            cancel_interactions(session_id, "session_closed", &state, &sink).await;
            if let Some(terminals) = &terminals {
                terminals.release_session(session_id).await;
            }
            {
                let mut state = state.lock().await;
                state.active_sessions.remove(session_id);
                state.session_updates.remove(session_id);
            }
            sink.send(json!({
                "type": "acp/session_closed",
                "requestId": request_id,
                "sessionId": session_id,
            }));
        }
        "session/delete" => {
            let request_id = string_field(&command, "requestId")?;
            let session_id = string_field(&command, "sessionId")?;
            require_agent_method("session/delete", &state).await?;
            {
                let mut state = state.lock().await;
                reserve_deletion(&mut state, session_id)?;
            }
            let result = connection
                .send_request(DeleteSessionRequest::new(session_id.to_string()))
                .block_task()
                .await;
            state.lock().await.pending_deletions.remove(session_id);
            let response = result?;
            ensure_relay_size(&response, "session/delete response")?;
            cancel_interactions(session_id, "session_closed", &state, &sink).await;
            if let Some(terminals) = &terminals {
                terminals.release_session(session_id).await;
            }
            {
                let mut state = state.lock().await;
                state.active_sessions.remove(session_id);
                state.listed_sessions.remove(session_id);
                state.session_updates.remove(session_id);
            }
            sink.send(json!({
                "type": "acp/session_deleted",
                "requestId": request_id,
                "sessionId": session_id,
            }));
        }
        "session/prompt" => {
            let request_id = string_field(&command, "requestId")?.to_string();
            let session_id = string_field(&command, "sessionId")?.to_string();
            let prompt_value = command
                .get("prompt")
                .cloned()
                .ok_or_else(|| Error::invalid_params().data("session/prompt requires prompt"))?;
            let blocks = prompt_value
                .as_array()
                .ok_or_else(|| Error::invalid_params().data("session/prompt must be an array"))?;
            for block in blocks {
                validate_content_block(block, "Prompt content").map_err(semantic_error)?;
            }
            {
                let state = state.lock().await;
                validate_prompt_capabilities(blocks, &state)?;
            }
            let prompt: Vec<ContentBlock> = serde_json::from_value(prompt_value)?;
            {
                let mut state = state.lock().await;
                reserve_session_operation(&mut state, &session_id, SessionOperation::Prompt)?;
            }
            let request = connection.send_request(PromptRequest::new(session_id.clone(), prompt));
            if let Some(prompt_start) = prompt_start.as_mut() {
                prompt_start.finish();
            }
            let result = request.block_task().await;
            {
                let mut state = state.lock().await;
                release_session_operation(&mut state, &session_id, SessionOperation::Prompt);
            }
            prompt_lifecycle.prompt_finished();
            let response = result?;
            validate_prompt_response(&response).map_err(semantic_error)?;
            sink.send(json!({
                "type": "acp/prompt_complete",
                "requestId": request_id,
                "sessionId": session_id,
                "response": response,
            }));
        }
        "session/cancel" => {
            let session_id = string_field(&command, "sessionId")?;
            require_active(session_id, &state).await?;
            connection.send_notification(CancelNotification::new(session_id.to_string()))?;
            cancel_interactions(session_id, "session_cancelled", &state, &sink).await;
        }
        "session/set_mode" => {
            let request_id = string_field(&command, "requestId")?;
            let session_id = string_field(&command, "sessionId")?;
            let mode_id = string_field(&command, "modeId")?;
            {
                let mut state = state.lock().await;
                let session = state.active_sessions.get(session_id).ok_or_else(|| {
                    Error::invalid_params()
                        .data(format!("unknown or inactive session: {session_id}"))
                })?;
                validate_session_mode_reference(session.modes.as_ref(), mode_id)
                    .map_err(semantic_error)?;
                reserve_session_operation(&mut state, session_id, SessionOperation::Control)?;
            }
            let result = connection
                .send_request(SetSessionModeRequest::new(
                    session_id.to_string(),
                    mode_id.to_string(),
                ))
                .block_task()
                .await;
            {
                let mut state = state.lock().await;
                release_session_operation(&mut state, session_id, SessionOperation::Control);
            }
            let response = result?;
            ensure_relay_size(&response, "session mode response")?;
            if let Some(modes) = state
                .lock()
                .await
                .active_sessions
                .get_mut(session_id)
                .and_then(|session| session.modes.as_mut())
                .and_then(Value::as_object_mut)
            {
                modes.insert("currentModeId".to_string(), json!(mode_id));
            }
            sink.send(json!({
                "type": "acp/mode_changed",
                "requestId": request_id,
                "sessionId": session_id,
                "modeId": mode_id,
            }));
        }
        "session/set_config_option" => {
            let request_id = string_field(&command, "requestId")?;
            let session_id = string_field(&command, "sessionId")?;
            let config_id = string_field(&command, "configId")?;
            let value = command
                .get("value")
                .ok_or_else(|| Error::invalid_params().data("config value is required"))?;
            let request = if let Some(value) = value.as_bool() {
                SetSessionConfigOptionRequest::new(
                    session_id.to_string(),
                    config_id.to_string(),
                    value,
                )
            } else if let Some(value) = value.as_str() {
                SetSessionConfigOptionRequest::new(
                    session_id.to_string(),
                    config_id.to_string(),
                    SessionConfigValueId::new(value.to_string()),
                )
            } else {
                return Err(Error::invalid_params().data("config value must be string or boolean"));
            };
            {
                let mut state = state.lock().await;
                let session = state.active_sessions.get(session_id).ok_or_else(|| {
                    Error::invalid_params()
                        .data(format!("unknown or inactive session: {session_id}"))
                })?;
                validate_session_config_reference(&session.config_options, config_id, value)
                    .map_err(semantic_error)?;
                reserve_session_operation(&mut state, session_id, SessionOperation::Control)?;
            }
            let result = connection.send_request(request).block_task().await;
            {
                let mut state = state.lock().await;
                release_session_operation(&mut state, session_id, SessionOperation::Control);
            }
            let response = result?;
            ensure_relay_size(&response, "session config response")?;
            let response_value = serde_json::to_value(&response)?;
            let config_options = response_value
                .get("configOptions")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new()));
            validate_session_config_options(&config_options).map_err(semantic_error)?;
            if let Some(session) = state.lock().await.active_sessions.get_mut(session_id) {
                session.config_options = config_options;
            }
            sink.send(json!({
                "type": "acp/config_changed",
                "requestId": request_id,
                "sessionId": session_id,
                "configId": config_id,
                "value": value,
                "response": response,
            }));
        }
        "permission/respond" => {
            let request_id = string_field(&command, "requestId")?;
            let permission_id = string_field(&command, "permissionId")?;
            let response: RequestPermissionResponse = serde_json::from_value(json!({
                "outcome": command.get("outcome").cloned().ok_or_else(|| {
                    Error::invalid_params().data("permission outcome is required")
                })?,
            }))?;
            let mut state = state.lock().await;
            let pending = state.permissions.get(permission_id).ok_or_else(|| {
                Error::invalid_params().data("permission request is no longer pending")
            })?;
            if let RequestPermissionOutcome::Selected(selected) = &response.outcome
                && !pending.option_ids.contains(selected.option_id.0.as_ref())
            {
                return Err(
                    Error::invalid_params().data("permission option was not offered by the Agent")
                );
            }
            let pending = state
                .permissions
                .remove(permission_id)
                .expect("pending permission checked above");
            drop(state);
            pending
                .sender
                .send(response)
                .map_err(|_| Error::request_cancelled())?;
            sink.send(json!({
                "type": "acp/permission_resolved",
                "permissionId": permission_id,
                "requestId": request_id,
            }));
        }
        "elicitation/respond" => {
            let request_id = string_field(&command, "requestId")?;
            let elicitation_id = string_field(&command, "elicitationId")?;
            let response_value = command
                .get("response")
                .cloned()
                .ok_or_else(|| Error::invalid_params().data("elicitation response is required"))?;
            let mut state = state.lock().await;
            let request = state
                .elicitations
                .get(elicitation_id)
                .map(|pending| pending.request.clone())
                .ok_or_else(|| Error::invalid_params().data("elicitation is no longer pending"))?;
            validate_elicitation_response_value(&request, &response_value)
                .map_err(semantic_error)?;
            let response: CreateElicitationResponse = serde_json::from_value(response_value)?;
            let pending = state.elicitations.remove(elicitation_id);
            let Some(pending) = pending else {
                return Err(Error::invalid_params().data("elicitation is no longer pending"));
            };
            if matches!(&response.action, ElicitationAction::Accept(_))
                && let Some(url_elicitation_id) = &pending.url_elicitation_id
            {
                state.url_elicitations.insert(
                    url_elicitation_id.clone(),
                    ActiveUrlElicitation {
                        session_id: pending.session_id.clone(),
                    },
                );
            }
            drop(state);
            pending
                .sender
                .send(response.clone())
                .map_err(|_| Error::request_cancelled())?;
            sink.send(json!({
                "type": "acp/elicitation_resolved",
                "elicitationId": elicitation_id,
                "response": response,
                "requestId": request_id,
            }));
        }
        "context/search" => {
            let request_id = string_field(&command, "requestId")?;
            let query = string_field_allow_empty(&command, "query", 256)?;
            let filesystem = filesystem.ok_or_else(|| {
                Error::method_not_found()
                    .data("workspace context is unavailable for remote transports")
            })?;
            let matches = filesystem.search_context(query).await?;
            sink.send(json!({
                "type": "bridge/context_search_result",
                "requestId": request_id,
                "query": query,
                "matches": matches,
            }));
        }
        "context/read" => {
            let request_id = string_field(&command, "requestId")?;
            let session_id = string_field(&command, "sessionId")?;
            let path = bounded_string_field(&command, "path", MAX_BRIDGE_PATH_LENGTH)?;
            require_active(session_id, &state).await?;
            let filesystem = filesystem.ok_or_else(|| {
                Error::method_not_found()
                    .data("workspace context is unavailable for remote transports")
            })?;
            let attachment = filesystem
                .read_context(PathBuf::from(path).as_path())
                .await?;
            sink.send(json!({
                "type": "bridge/context_attached",
                "requestId": request_id,
                "sessionId": session_id,
                "attachment": attachment,
            }));
        }
        "auth/terminal_start" => {
            if options.transport != Transport::Stdio {
                return Err(Error::method_not_found()
                    .data("terminal authentication is unavailable for remote transports"));
            }
            let request_id = string_field(&command, "requestId")?.to_string();
            let method_id = string_field(&command, "methodId")?;
            let cols = u16_field(&command, "cols")?;
            let rows = u16_field(&command, "rows")?;
            let method = state
                .lock()
                .await
                .auth_methods
                .get(method_id)
                .cloned()
                .ok_or_else(|| {
                    Error::invalid_params().data(format!(
                        "authentication method was not offered: {method_id}"
                    ))
                })?;
            let AuthMethod::Terminal(method) = method else {
                return Err(Error::invalid_request()
                    .data("authentication method is handled by the Agent, not a terminal"));
            };
            auth_terminal.start(
                request_id,
                method,
                &options.command,
                &options.cwd,
                cols,
                rows,
            )?;
        }
        "auth/terminal_input" => {
            let request_id = string_field(&command, "requestId")?;
            let data = string_field_allow_empty(&command, "data", MAX_BRIDGE_MESSAGE_BYTES)?;
            auth_terminal.write(request_id, data)?;
        }
        "auth/terminal_resize" => {
            let request_id = string_field(&command, "requestId")?;
            auth_terminal.resize(
                request_id,
                u16_field(&command, "cols")?,
                u16_field(&command, "rows")?,
            )?;
        }
        "auth/terminal_cancel" => {
            auth_terminal.cancel(string_field(&command, "requestId")?)?;
        }
        operation if operation.starts_with("nes/") || operation.starts_with("document/") => {
            return Err(Error::method_not_found()
                .data("attyd does not advertise the ACP NES/editor surface"));
        }
        _ => {
            return Err(
                Error::method_not_found().data(format!("unknown WebSocket command: {operation}"))
            );
        }
    }
    Ok(())
}

fn string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str, Error> {
    bounded_string_field(value, field, MAX_BRIDGE_IDENTIFIER_LENGTH)
}

fn bounded_string_field<'a>(
    value: &'a Value,
    field: &str,
    maximum: usize,
) -> Result<&'a str, Error> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.encode_utf16().count() <= maximum)
        .ok_or_else(|| {
            Error::invalid_params().data(format!(
                "{field} must contain between 1 and {maximum} characters"
            ))
        })
}

fn string_field_allow_empty<'a>(
    value: &'a Value,
    field: &str,
    maximum: usize,
) -> Result<&'a str, Error> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| value.encode_utf16().count() <= maximum)
        .ok_or_else(|| Error::invalid_params().data(format!("{field} must be a string")))
}

fn u16_field(value: &Value, field: &str) -> Result<u16, Error> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| Error::invalid_params().data(format!("{field} must be a positive uint16")))
}

fn validate_session_list_page(
    response: &ListSessionsResponse,
    cursor: Option<&str>,
    state: &BridgeState,
) -> Result<usize, Error> {
    let page_bytes = ensure_relay_size(response, "session/list response")?;
    let accumulated = if cursor.is_none() {
        page_bytes
    } else {
        state.listed_session_bytes.saturating_add(page_bytes)
    };
    if accumulated > MAX_SESSION_LIST_TOTAL_BYTES {
        return Err(Error::invalid_request().data(format!(
            "accumulated session/list responses exceed {MAX_SESSION_LIST_TOTAL_BYTES} bytes"
        )));
    }
    if response.sessions.len() > MAX_LISTED_SESSIONS {
        return Err(Error::invalid_request().data(format!(
            "Agent returned more than {MAX_LISTED_SESSIONS} sessions in one page"
        )));
    }

    let mut session_ids = if cursor.is_none() {
        HashSet::new()
    } else {
        state.listed_sessions.keys().cloned().collect()
    };
    for session in &response.sessions {
        let session_id = session.session_id.0.as_ref();
        if validate_agent_session_id(session_id).is_err() {
            return Err(
                Error::invalid_request().data("Agent returned an invalid listed session ID")
            );
        }
        let cwd = session.cwd.to_string_lossy();
        if !valid_session_path(&cwd) {
            return Err(Error::invalid_request().data(format!(
                "Agent returned an invalid absolute session cwd: {session_id}"
            )));
        }
        if session.additional_directories.len() > 256 {
            return Err(Error::invalid_request().data(format!(
                "Agent listed session {session_id} with more than 256 additional directories"
            )));
        }
        for directory in &session.additional_directories {
            let directory = directory.to_string_lossy();
            if !valid_session_path(&directory) {
                return Err(Error::invalid_request().data(format!(
                    "Agent returned an invalid absolute additional directory for session {session_id}"
                )));
            }
        }
        validate_session_metadata(
            session.title.as_ref().map(|title| json!(title)).as_ref(),
            session
                .updated_at
                .as_ref()
                .map(|updated_at| json!(updated_at))
                .as_ref(),
            &format!("Agent listed session {session_id}"),
        )
        .map_err(semantic_error)?;
        session_ids.insert(session_id.to_string());
    }
    if session_ids.len() > MAX_LISTED_SESSIONS {
        return Err(Error::invalid_request().data(format!(
            "session/list returned more than {MAX_LISTED_SESSIONS} unique sessions"
        )));
    }

    if let Some(next_cursor) = &response.next_cursor {
        if next_cursor.is_empty() || next_cursor.encode_utf16().count() > 4_096 {
            return Err(
                Error::invalid_request().data("Agent returned an invalid session/list cursor")
            );
        }
        if cursor.is_some() && state.listed_cursors.contains(next_cursor) {
            return Err(Error::invalid_request()
                .data(format!("Agent reused session/list cursor: {next_cursor}")));
        }
    }
    Ok(page_bytes)
}

fn valid_session_path(value: &str) -> bool {
    !value.is_empty()
        && value.encode_utf16().count() <= 16_384
        && !value.contains('\0')
        && is_portable_absolute_path(value)
}

fn validate_prompt_capabilities(blocks: &[Value], state: &BridgeState) -> Result<(), Error> {
    let capabilities = state
        .agent_capabilities
        .as_ref()
        .map(|capabilities| &capabilities.prompt_capabilities);
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("image") if !capabilities.is_some_and(|capabilities| capabilities.image) => {
                return Err(semantic_error(
                    "Agent did not advertise image prompt support",
                ));
            }
            Some("audio") if !capabilities.is_some_and(|capabilities| capabilities.audio) => {
                return Err(semantic_error(
                    "Agent did not advertise audio prompt support",
                ));
            }
            Some("resource")
                if !capabilities.is_some_and(|capabilities| capabilities.embedded_context) =>
            {
                return Err(semantic_error(
                    "Agent did not advertise embedded context support",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn is_portable_absolute_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    value.starts_with('/')
        || value.starts_with("\\\\")
        || value.starts_with("//")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
}

fn new_session_cwd(command: &Value, options: &Options) -> Result<PathBuf, Error> {
    let cwd = match command.get("cwd") {
        Some(Value::String(cwd)) => PathBuf::from(cwd),
        Some(_) => return Err(Error::invalid_params().data("session cwd must be a string")),
        None if options.transport == Transport::Stdio => options.cwd.clone(),
        None => {
            return Err(Error::invalid_params()
                .data("session/new requires an absolute Agent workspace for remote transports"));
        }
    };
    if !cwd.is_absolute() {
        return Err(Error::invalid_params().data("session cwd must be an absolute path"));
    }
    Ok(cwd)
}

fn local_additional_directories(options: &Options) -> Vec<PathBuf> {
    if options.transport == Transport::Stdio {
        options.additional_directories.clone()
    } else {
        Vec::new()
    }
}

async fn reserve_attachment(
    session_id: &str,
    state: &Arc<Mutex<BridgeState>>,
) -> Result<PathBuf, Error> {
    let mut state = state.lock().await;
    if state.pending_deletions.contains(session_id) {
        return Err(Error::invalid_request().data("session deletion is already running"));
    }
    if state.active_sessions.contains_key(session_id) {
        return Err(
            Error::invalid_request().data(format!("session is already active: {session_id}"))
        );
    }
    let listed = state.listed_sessions.get(session_id).ok_or_else(|| {
        Error::invalid_params().data(format!(
            "session was not returned by session/list: {session_id}"
        ))
    })?;
    let cwd = listed.cwd.clone();
    if state.active_sessions.len() + state.pending_creations + state.pending_attachments.len()
        >= MAX_TRACKED_SESSIONS
    {
        return Err(semantic_error(format!(
            "Active session limit reached ({MAX_TRACKED_SESSIONS})"
        )));
    }
    if !state.pending_attachments.insert(session_id.to_string()) {
        return Err(Error::invalid_request().data("session attachment is already running"));
    }
    state.session_updates.insert(
        session_id.to_string(),
        SessionUpdateSemanticState::default(),
    );
    Ok(cwd)
}

fn reserve_session_operation(
    state: &mut BridgeState,
    session_id: &str,
    operation: SessionOperation,
) -> Result<(), Error> {
    if !state.active_sessions.contains_key(session_id) {
        return Err(
            Error::invalid_params().data(format!("unknown or inactive session: {session_id}"))
        );
    }
    if state.prompts.contains(session_id)
        || state.pending_forks.contains(session_id)
        || state.pending_closes.contains(session_id)
        || state.pending_controls.contains(session_id)
        || state.pending_deletions.contains(session_id)
    {
        return Err(
            Error::invalid_request().data("another prompt or session mutation is already running")
        );
    }
    match operation {
        SessionOperation::Prompt => &mut state.prompts,
        SessionOperation::Fork => &mut state.pending_forks,
        SessionOperation::Close => &mut state.pending_closes,
        SessionOperation::Control => &mut state.pending_controls,
    }
    .insert(session_id.to_string());
    Ok(())
}

fn release_session_operation(
    state: &mut BridgeState,
    session_id: &str,
    operation: SessionOperation,
) {
    match operation {
        SessionOperation::Prompt => &mut state.prompts,
        SessionOperation::Fork => &mut state.pending_forks,
        SessionOperation::Close => &mut state.pending_closes,
        SessionOperation::Control => &mut state.pending_controls,
    }
    .remove(session_id);
}

fn reserve_deletion(state: &mut BridgeState, session_id: &str) -> Result<(), Error> {
    if state.pending_attachments.contains(session_id)
        || state.prompts.contains(session_id)
        || state.pending_forks.contains(session_id)
        || state.pending_closes.contains(session_id)
        || state.pending_controls.contains(session_id)
    {
        return Err(
            Error::invalid_request().data("another prompt or session mutation is already running")
        );
    }
    if state.pending_deletions.len() >= MAX_TRACKED_SESSIONS {
        return Err(semantic_error(format!(
            "Pending session deletion limit reached ({MAX_TRACKED_SESSIONS})"
        )));
    }
    if !state.pending_deletions.insert(session_id.to_string()) {
        return Err(Error::invalid_request().data("session deletion is already running"));
    }
    Ok(())
}

async fn require_active(session_id: &str, state: &Arc<Mutex<BridgeState>>) -> Result<(), Error> {
    if !state.lock().await.active_sessions.contains_key(session_id) {
        return Err(
            Error::invalid_params().data(format!("unknown or inactive session: {session_id}"))
        );
    }
    Ok(())
}

async fn cancel_interactions(
    session_id: &str,
    reason: &str,
    state: &Arc<Mutex<BridgeState>>,
    sink: &EventSink,
) {
    let mut state = state.lock().await;
    let permission_ids = state
        .permissions
        .iter()
        .filter(|(_, pending)| pending.session_id == session_id)
        .map(|(id, _)| id)
        .cloned()
        .collect::<Vec<_>>();
    for id in permission_ids {
        if let Some(pending) = state.permissions.remove(&id) {
            let _ = pending.sender.send(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ));
            sink.send(json!({
                "type": "acp/permission_resolved",
                "permissionId": id,
            }));
        }
    }
    let elicitation_ids = state
        .elicitations
        .iter()
        .filter(|(_, pending)| pending.session_id.as_deref() == Some(session_id))
        .map(|(id, _)| id)
        .cloned()
        .collect::<Vec<_>>();
    for id in elicitation_ids {
        if let Some(pending) = state.elicitations.remove(&id) {
            let response = CreateElicitationResponse::new(ElicitationAction::Cancel);
            let _ = pending.sender.send(response.clone());
            sink.send(json!({
                "type": "acp/elicitation_resolved",
                "elicitationId": id,
                "response": response,
            }));
        }
    }
    let url_elicitation_ids = state
        .url_elicitations
        .iter()
        .filter(|(_, tracked)| tracked.session_id.as_deref() == Some(session_id))
        .map(|(id, _)| id)
        .cloned()
        .collect::<Vec<_>>();
    for elicitation_id in url_elicitation_ids {
        state.url_elicitations.remove(&elicitation_id);
        sink.send(json!({
            "type": "acp/elicitation_aborted",
            "elicitationId": elicitation_id,
            "sessionId": session_id,
            "reason": reason,
        }));
    }
}

fn validate_configured_capabilities(
    capabilities: &AgentCapabilities,
    options: &Options,
) -> Result<(), Error> {
    if !options.additional_directories.is_empty()
        && capabilities
            .session_capabilities
            .additional_directories
            .is_none()
    {
        return Err(Error::invalid_request()
            .data("Agent does not support configured additional directories"));
    }
    for server in &options.mcp_servers {
        let supported = match server {
            McpServer::Stdio(_) => true,
            McpServer::Http(_) => capabilities.mcp_capabilities.http,
            McpServer::Sse(_) => capabilities.mcp_capabilities.sse,
            McpServer::Acp(_) => capabilities.mcp_capabilities.acp,
            _ => false,
        };
        if !supported {
            return Err(Error::invalid_request().data(format!(
                "Agent does not support configured {} MCP server {}",
                server_type(server),
                server_name(server),
            )));
        }
    }
    Ok(())
}

fn validate_auth_methods(
    methods: &[AuthMethod],
    capabilities: &AgentCapabilities,
    terminal_supported: bool,
) -> Result<(), Error> {
    if methods.len() > MAX_AUTH_METHODS {
        return Err(semantic_error(format!(
            "Agent advertised more than {MAX_AUTH_METHODS} authentication methods"
        )));
    }
    let mut ids = HashSet::new();
    for method in methods {
        let id = method.id().0.as_ref();
        validate_agent_session_id(id).map_err(semantic_error)?;
        if !ids.insert(id) {
            return Err(semantic_error(format!(
                "Agent advertised duplicate authentication method ID: {id}"
            )));
        }
        let name = method.name();
        if name.is_empty() || name.encode_utf16().count() > MAX_AUTH_METHOD_NAME_LENGTH {
            return Err(semantic_error(format!(
                "Agent authentication method name must contain between 1 and {MAX_AUTH_METHOD_NAME_LENGTH} characters"
            )));
        }
        if method.description().is_some_and(|description| {
            description.encode_utf16().count() > MAX_AUTH_METHOD_DESCRIPTION_LENGTH
        }) {
            return Err(semantic_error(format!(
                "Agent authentication method description exceeds {MAX_AUTH_METHOD_DESCRIPTION_LENGTH} characters"
            )));
        }
        if let AuthMethod::Terminal(method) = method {
            if !terminal_supported {
                return Err(semantic_error(
                    "Agent advertised terminal authentication although the remote transport cannot reproduce its invocation",
                ));
            }
            validate_terminal_auth_method(method)?;
        }
    }
    if methods.is_empty() && capabilities.auth.logout.is_some() {
        return Err(semantic_error(
            "Agent advertised logout without any authentication methods",
        ));
    }
    Ok(())
}

async fn require_agent_method(
    operation: &str,
    state: &Arc<Mutex<BridgeState>>,
) -> Result<(), Error> {
    let capabilities = state
        .lock()
        .await
        .agent_capabilities
        .clone()
        .ok_or_else(|| Error::internal_error().data("Agent capabilities are unavailable"))?;
    let supported = match operation {
        "auth/logout" => capabilities.auth.logout.is_some(),
        "session/list" => capabilities.session_capabilities.list.is_some(),
        "session/load" => capabilities.load_session,
        "session/resume" => capabilities.session_capabilities.resume.is_some(),
        "session/fork" => capabilities.session_capabilities.fork.is_some(),
        "session/close" => capabilities.session_capabilities.close.is_some(),
        "session/delete" => capabilities.session_capabilities.delete.is_some(),
        _ => true,
    };
    if supported {
        Ok(())
    } else {
        Err(Error::method_not_found()
            .data(format!("Agent did not advertise support for {operation}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn options(arguments: &[&str]) -> Options {
        Options::try_parse_from(arguments)
            .unwrap()
            .normalized()
            .unwrap()
    }

    fn session_info(session_id: &str, cwd: &str) -> SessionInfo {
        serde_json::from_value(json!({
            "sessionId": session_id,
            "cwd": cwd,
            "title": format!("Session {session_id}"),
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn relays_a_fatal_oversized_stdio_line_before_initialization() {
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
            "--oversized-stdout-line",
        ])
        .unwrap()
        .normalized()
        .unwrap();
        let (_commands, command_rx) = mpsc::channel(1);
        let (event_tx, mut events) = mpsc::unbounded_channel();
        tokio::spawn(run(Arc::new(options), command_rx, event_tx));

        let mut received = Vec::new();
        let error = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let event = events.recv().await.expect("bridge event channel closed");
                let value: Value = serde_json::from_str(&event).unwrap();
                received.push(value.clone());
                if value["type"] == "bridge/error" {
                    return value;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("bridge did not relay fatal transport error: {received:?}"));
        assert!(
            error
                .to_string()
                .contains("Agent NDJSON line exceeds 8000000 bytes"),
            "unexpected bridge error: {error}"
        );
        assert!(
            !received
                .iter()
                .any(|event| event["type"] == "acp/initialized")
        );
    }

    #[test]
    fn advertises_agent_interaction_capabilities_without_editor_features() {
        // Selected from Zed's `client_capabilities_include_elicitation_without_acp_beta`
        // contract: form and URL elicitation are stable client behavior.
        let local = client_capabilities(&options(&["attyd"]));
        assert!(
            local
                .elicitation
                .as_ref()
                .and_then(|value| value.form.as_ref())
                .is_some()
        );
        assert!(
            local
                .elicitation
                .as_ref()
                .and_then(|value| value.url.as_ref())
                .is_some()
        );
        assert!(local.fs.read_text_file);
        assert!(local.terminal);
        let serialized = serde_json::to_value(&local).unwrap();
        assert!(serialized.get("nes").is_none());
        assert!(serialized.get("positionEncodings").is_none());

        let remote = client_capabilities(&options(&[
            "attyd",
            "--transport",
            "ws",
            "--",
            "ws://127.0.0.1:3284/acp",
        ]));
        assert!(!remote.fs.read_text_file);
        assert!(!remote.terminal);
        assert!(!remote.auth.terminal);
    }

    #[tokio::test]
    async fn buffers_pre_response_updates_and_drops_late_unknown_sessions() {
        // Zed's ACP regression suite requires load replay notifications arriving before
        // the RPC response to survive. New/fork need the same ordering guarantee here.
        let state = Arc::new(Mutex::new(BridgeState {
            pending_creations: 1,
            ..BridgeState::default()
        }));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx };
        let notification: SessionNotification = serde_json::from_value(json!({
            "sessionId": "new-session",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": "early-message",
                "content": { "type": "text", "text": "before response" }
            }
        }))
        .unwrap();

        handle_session_update(notification.clone(), &state, &sink, None).await;
        assert_eq!(state.lock().await.early_updates["new-session"].len(), 1);
        assert!(rx.try_recv().is_err());

        state.lock().await.pending_creations = 0;
        handle_session_update(notification.clone(), &state, &sink, None).await;
        assert!(
            rx.try_recv().is_err(),
            "late unknown updates must stay hidden"
        );

        state.lock().await.active_sessions.insert(
            "new-session".to_string(),
            ActiveSession {
                cwd: PathBuf::from("/workspace"),
                modes: None,
                config_options: Value::Array(Vec::new()),
            },
        );
        handle_session_update(notification, &state, &sink, None).await;
        let relayed: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(relayed["type"], "acp/session_update");
        assert_eq!(relayed["notification"]["sessionId"], "new-session");
    }

    #[tokio::test]
    async fn rejects_invalid_active_updates_transactionally_then_relays_recovery() {
        let state = Arc::new(Mutex::new(BridgeState::default()));
        state.lock().await.active_sessions.insert(
            "session".to_string(),
            ActiveSession {
                cwd: PathBuf::from("/workspace"),
                modes: None,
                config_options: Value::Array(Vec::new()),
            },
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx };
        let invalid: SessionNotification = serde_json::from_value(json!({
            "sessionId": "session",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": "same-message",
                "content": { "type": "image", "data": "AA==", "mimeType": "text/html" }
            }
        }))
        .unwrap();
        handle_session_update(invalid, &state, &sink, None).await;
        let error: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(error["type"], "bridge/error");
        assert!(error["data"].as_str().unwrap().contains("image/* family"));
        assert_eq!(
            state.lock().await.session_updates["session"].update_count,
            0
        );

        let recovery: SessionNotification = serde_json::from_value(json!({
            "sessionId": "session",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": "same-message",
                "content": { "type": "text", "text": "connection survived" }
            }
        }))
        .unwrap();
        handle_session_update(recovery, &state, &sink, None).await;
        let event: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(event["type"], "acp/session_update");
        assert_eq!(
            state.lock().await.session_updates["session"].update_count,
            1
        );
    }

    #[tokio::test]
    async fn invalid_early_updates_prevent_session_commit_and_clear_cleanly() {
        let state = Arc::new(Mutex::new(BridgeState {
            pending_creations: 1,
            ..BridgeState::default()
        }));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx };
        let invalid: SessionNotification = serde_json::from_value(json!({
            "sessionId": "new-session",
            "update": {
                "sessionUpdate": "compaction_summary_chunk",
                "compactionId": "missing",
                "content": { "type": "text", "text": "orphan" }
            }
        }))
        .unwrap();
        handle_session_update(invalid, &state, &sink, None).await;
        let event: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(event["type"], "bridge/error");

        let mut state = state.lock().await;
        let response = json!({
            "sessionId": "new-session",
            "modes": {
                "currentModeId": "build",
                "availableModes": [{ "id": "build", "name": "Build" }]
            }
        });
        let error = track_session(
            &mut state,
            "new-session",
            PathBuf::from("/workspace"),
            &response,
            None,
        )
        .unwrap_err();
        assert!(
            error
                .data
                .unwrap()
                .to_string()
                .contains("replay was invalid")
        );
        assert!(!state.active_sessions.contains_key("new-session"));
        state.pending_creations = 0;
        clear_pending_creation_replays(&mut state);
        assert!(!state.session_updates.contains_key("new-session"));
    }

    #[test]
    fn bounds_structured_acp_errors_before_browser_relay() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx };
        sink.acp_error(
            Error::internal_error()
                .data(json!({ "payload": "x".repeat(MAX_BRIDGE_ERROR_DATA_BYTES + 1) })),
            Some("request"),
            Some("session/prompt"),
        );
        let event: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(event["type"], "bridge/error");
        assert_eq!(event["dataTruncated"], true);
        assert!(event.get("data").is_none());
        assert_eq!(event["requestId"], "request");
    }

    #[test]
    fn bounds_agent_relay_values_and_browser_event_fallbacks() {
        assert!(ensure_relay_size(&json!({ "ok": true }), "fixture").is_ok());
        assert!(
            ensure_relay_size(
                &json!({ "padding": "x".repeat(MAX_AGENT_RELAY_BYTES) }),
                "fixture",
            )
            .is_err()
        );

        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx };
        sink.send(json!({ "padding": "x".repeat(MAX_BRIDGE_MESSAGE_BYTES) }));
        let event: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(event["type"], "bridge/error");
        assert!(
            event["message"]
                .as_str()
                .unwrap()
                .contains("browser event exceeds")
        );
    }

    #[test]
    fn validates_bounded_browser_fields_and_terminal_dimensions() {
        let command = json!({
            "requestId": "request",
            "empty": "",
            "tooLong": "x".repeat(MAX_BRIDGE_IDENTIFIER_LENGTH + 1),
            "wideIdentifier": "😀".repeat(MAX_BRIDGE_IDENTIFIER_LENGTH / 2),
            "tooWideIdentifier": "😀".repeat(MAX_BRIDGE_IDENTIFIER_LENGTH / 2 + 1),
            "path": "x".repeat(MAX_BRIDGE_PATH_LENGTH),
            "cols": 80,
            "zero": 0,
        });
        assert_eq!(string_field(&command, "requestId").unwrap(), "request");
        assert!(string_field(&command, "empty").is_err());
        assert!(string_field(&command, "tooLong").is_err());
        assert!(string_field(&command, "wideIdentifier").is_ok());
        assert!(string_field(&command, "tooWideIdentifier").is_err());
        assert!(bounded_string_field(&command, "path", MAX_BRIDGE_PATH_LENGTH).is_ok());
        assert_eq!(string_field_allow_empty(&command, "empty", 4).unwrap(), "");
        assert_eq!(u16_field(&command, "cols").unwrap(), 80);
        assert!(u16_field(&command, "zero").is_err());
        assert!(u16_field(&json!({ "cols": 65_536 }), "cols").is_err());
    }

    #[test]
    fn selects_session_cwd_by_transport_and_rejects_relative_paths() {
        let local = options(&["attyd", "--cwd", "/tmp/local-workspace"]);
        assert_eq!(
            new_session_cwd(&json!({}), &local).unwrap(),
            PathBuf::from("/tmp/local-workspace")
        );
        assert!(new_session_cwd(&json!({ "cwd": "relative" }), &local).is_err());

        let remote = options(&["attyd", "--transport", "ws", "ws://127.0.0.1:3284/acp"]);
        assert!(new_session_cwd(&json!({}), &remote).is_err());
        assert_eq!(
            new_session_cwd(&json!({ "cwd": "/agent/workspace" }), &remote).unwrap(),
            PathBuf::from("/agent/workspace")
        );
        assert!(local_additional_directories(&remote).is_empty());
    }

    #[tokio::test]
    async fn reserves_listed_sessions_once_and_rejects_active_attachments() {
        let state = Arc::new(Mutex::new(BridgeState::default()));
        state.lock().await.listed_sessions.insert(
            "saved".to_string(),
            session_info("saved", "/agent/workspace"),
        );
        assert_eq!(
            reserve_attachment("saved", &state).await.unwrap(),
            PathBuf::from("/agent/workspace")
        );
        assert!(reserve_attachment("saved", &state).await.is_err());

        let mut locked = state.lock().await;
        locked.pending_attachments.clear();
        locked.active_sessions.insert(
            "saved".to_string(),
            ActiveSession {
                cwd: PathBuf::from("/agent/workspace"),
                modes: None,
                config_options: Value::Array(Vec::new()),
            },
        );
        drop(locked);
        assert!(reserve_attachment("saved", &state).await.is_err());
        assert!(reserve_attachment("unknown", &state).await.is_err());
    }

    #[test]
    fn serializes_prompts_lifecycle_controls_and_deletion_attachment_races() {
        let mut state = BridgeState::default();
        state.active_sessions.insert(
            "active".to_string(),
            ActiveSession {
                cwd: PathBuf::from("/agent/workspace"),
                modes: None,
                config_options: Value::Array(Vec::new()),
            },
        );
        reserve_session_operation(&mut state, "active", SessionOperation::Prompt).unwrap();
        assert!(reserve_session_operation(&mut state, "active", SessionOperation::Prompt).is_err());
        assert!(reserve_session_operation(&mut state, "active", SessionOperation::Fork).is_err());
        assert!(reserve_session_operation(&mut state, "active", SessionOperation::Close).is_err());
        assert!(
            reserve_session_operation(&mut state, "active", SessionOperation::Control).is_err()
        );
        release_session_operation(&mut state, "active", SessionOperation::Prompt);
        reserve_session_operation(&mut state, "active", SessionOperation::Control).unwrap();
        assert!(reserve_session_operation(&mut state, "active", SessionOperation::Fork).is_err());
        release_session_operation(&mut state, "active", SessionOperation::Control);
        reserve_session_operation(&mut state, "active", SessionOperation::Close).unwrap();
        release_session_operation(&mut state, "active", SessionOperation::Close);

        state.listed_sessions.insert(
            "saved".to_string(),
            session_info("saved", "/agent/workspace"),
        );
        state.pending_attachments.insert("saved".to_string());
        assert!(reserve_deletion(&mut state, "saved").is_err());
        state.pending_attachments.clear();
        reserve_deletion(&mut state, "saved").unwrap();
        assert!(reserve_deletion(&mut state, "saved").is_err());
        assert!(reserve_session_operation(&mut state, "active", SessionOperation::Prompt).is_ok());
        release_session_operation(&mut state, "active", SessionOperation::Prompt);

        state.pending_deletions.clear();
        reserve_deletion(&mut state, "active").unwrap();
        assert!(reserve_session_operation(&mut state, "active", SessionOperation::Prompt).is_err());
        state.pending_deletions.clear();

        // Session identity is owned by the Agent. The bridge must not reject a
        // just-closed or remotely known session merely because it was not in a
        // local session/list snapshot.
        reserve_deletion(&mut state, "agent-owned-session").unwrap();
    }

    #[test]
    fn rejected_session_cleanup_never_closes_an_active_session_id() {
        let mut state = BridgeState {
            agent_capabilities: Some(AgentCapabilities::new().session_capabilities(
                SessionCapabilities::new().close(SessionCloseCapabilities::new()),
            )),
            ..BridgeState::default()
        };
        assert!(can_close_rejected_chat_session(&state, "new-allocation"));

        state.active_sessions.insert(
            "source-session".to_string(),
            ActiveSession {
                cwd: PathBuf::from("/agent/workspace"),
                modes: None,
                config_options: Value::Array(Vec::new()),
            },
        );
        assert!(!can_close_rejected_chat_session(&state, "source-session"));

        state.agent_capabilities = Some(AgentCapabilities::new());
        assert!(!can_close_rejected_chat_session(&state, "new-allocation"));
    }

    #[test]
    fn rejects_cyclic_session_list_cursors_and_unbounded_rows() {
        let mut state = BridgeState::default();
        state.listed_sessions.insert(
            "saved".to_string(),
            session_info("saved", "/agent/workspace"),
        );
        state
            .listed_cursors
            .extend(["cursor-a".to_string(), "cursor-b".to_string()]);
        state.listed_session_bytes = 128;

        let cyclic: ListSessionsResponse = serde_json::from_value(json!({
            "sessions": [{
                "sessionId": "saved",
                "cwd": "/agent/workspace",
                "title": "Duplicate is a safe upsert"
            }],
            "nextCursor": "cursor-a"
        }))
        .unwrap();
        let error = validate_session_list_page(&cyclic, Some("cursor-b"), &state).unwrap_err();
        assert!(
            error
                .data
                .unwrap()
                .to_string()
                .contains("reused session/list cursor")
        );

        let relative: ListSessionsResponse = serde_json::from_value(json!({
            "sessions": [{ "sessionId": "bad", "cwd": "relative" }]
        }))
        .unwrap();
        assert!(validate_session_list_page(&relative, None, &state).is_err());

        let fresh: ListSessionsResponse = serde_json::from_value(json!({
            "sessions": [{ "sessionId": "fresh", "cwd": "C:\\agent\\workspace" }],
            "nextCursor": "cursor-a"
        }))
        .unwrap();
        assert!(validate_session_list_page(&fresh, None, &state).is_ok());

        let too_many_roots: ListSessionsResponse = serde_json::from_value(json!({
            "sessions": [{
                "sessionId": "roots",
                "cwd": "/agent/workspace",
                "additionalDirectories": (0..257)
                    .map(|index| format!("/agent/root-{index}"))
                    .collect::<Vec<_>>()
            }]
        }))
        .unwrap();
        assert!(validate_session_list_page(&too_many_roots, None, &state).is_err());

        let invalid_metadata: ListSessionsResponse = serde_json::from_value(json!({
            "sessions": [{
                "sessionId": "metadata",
                "cwd": "/agent/workspace",
                "title": "x".repeat(16_385)
            }]
        }))
        .unwrap();
        assert!(validate_session_list_page(&invalid_metadata, None, &state).is_err());

        let invalid_cursor: ListSessionsResponse = serde_json::from_value(json!({
            "sessions": [], "nextCursor": "x".repeat(4_097)
        }))
        .unwrap();
        assert!(validate_session_list_page(&invalid_cursor, None, &state).is_err());
    }

    #[tokio::test]
    async fn capability_gates_optional_methods_and_configured_features() {
        let state = Arc::new(Mutex::new(BridgeState {
            agent_capabilities: Some(AgentCapabilities::new()),
            ..BridgeState::default()
        }));
        for operation in [
            "auth/logout",
            "session/list",
            "session/load",
            "session/resume",
            "session/fork",
            "session/close",
            "session/delete",
        ] {
            assert!(require_agent_method(operation, &state).await.is_err());
        }

        state.lock().await.agent_capabilities = Some(
            AgentCapabilities::new()
                .load_session(true)
                .session_capabilities(
                    SessionCapabilities::new()
                        .list(SessionListCapabilities::new())
                        .resume(SessionResumeCapabilities::new())
                        .fork(SessionForkCapabilities::new())
                        .close(SessionCloseCapabilities::new())
                        .delete(SessionDeleteCapabilities::new()),
                )
                .auth(AgentAuthCapabilities::new().logout(LogoutCapabilities::new())),
        );
        for operation in [
            "auth/logout",
            "session/list",
            "session/load",
            "session/resume",
            "session/fork",
            "session/close",
            "session/delete",
        ] {
            assert!(require_agent_method(operation, &state).await.is_ok());
        }

        let configured_roots = options(&["attyd", "--add-dir", "/tmp/additional"]);
        assert!(
            validate_configured_capabilities(&AgentCapabilities::new(), &configured_roots).is_err()
        );
        let supports_roots = AgentCapabilities::new().session_capabilities(
            SessionCapabilities::new()
                .additional_directories(SessionAdditionalDirectoriesCapabilities::new()),
        );
        assert!(validate_configured_capabilities(&supports_roots, &configured_roots).is_ok());

        let mut configured_mcp = options(&["attyd"]);
        configured_mcp.mcp_servers = vec![McpServer::Http(McpServerHttp::new(
            "remote",
            "https://example.test/mcp",
        ))];
        assert!(
            validate_configured_capabilities(&AgentCapabilities::new(), &configured_mcp).is_err()
        );
        assert!(
            validate_configured_capabilities(
                &AgentCapabilities::new().mcp_capabilities(McpCapabilities::new().http(true)),
                &configured_mcp,
            )
            .is_ok()
        );

        let mut prompt_state = BridgeState {
            agent_capabilities: Some(AgentCapabilities::new()),
            ..BridgeState::default()
        };
        assert!(
            validate_prompt_capabilities(
                &[json!({ "type": "text", "text": "hello" })],
                &prompt_state,
            )
            .is_ok()
        );

        let agent_method = AuthMethod::Agent(AuthMethodAgent::new("login", "Login"));
        assert!(
            validate_auth_methods(
                std::slice::from_ref(&agent_method),
                &AgentCapabilities::new(),
                true,
            )
            .is_ok()
        );
        assert!(
            validate_auth_methods(
                &[agent_method.clone(), agent_method],
                &AgentCapabilities::new(),
                true,
            )
            .is_err()
        );
        assert!(
            validate_auth_methods(
                &[],
                &AgentCapabilities::new()
                    .auth(AgentAuthCapabilities::new().logout(LogoutCapabilities::new())),
                true,
            )
            .is_err()
        );
        assert!(
            validate_auth_methods(
                &[AuthMethod::Terminal(AuthMethodTerminal::new(
                    "terminal", "Terminal",
                ))],
                &AgentCapabilities::new(),
                false,
            )
            .is_err()
        );
        assert!(
            validate_prompt_capabilities(
                &[json!({ "type": "image", "data": "AA==", "mimeType": "image/png" })],
                &prompt_state,
            )
            .is_err()
        );
        prompt_state.agent_capabilities = Some(
            AgentCapabilities::new().prompt_capabilities(
                PromptCapabilities::new()
                    .image(true)
                    .audio(true)
                    .embedded_context(true),
            ),
        );
        assert!(
            validate_prompt_capabilities(
                &[
                    json!({ "type": "image" }),
                    json!({ "type": "audio" }),
                    json!({ "type": "resource" }),
                ],
                &prompt_state,
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn cancels_only_session_scoped_interactions_and_url_flows() {
        let (permission_sender, permission_receiver) = oneshot::channel();
        let (session_sender, session_receiver) = oneshot::channel();
        let (request_sender, _request_receiver) = oneshot::channel();
        let state = Arc::new(Mutex::new(BridgeState::default()));
        {
            let mut state = state.lock().await;
            state.permissions.insert(
                "permission".to_string(),
                PendingPermission {
                    session_id: "session".to_string(),
                    tool_call_id: "tool".to_string(),
                    option_ids: HashSet::new(),
                    sender: permission_sender,
                },
            );
            state.elicitations.insert(
                "session-elicitation".to_string(),
                PendingElicitation {
                    session_id: Some("session".to_string()),
                    url_elicitation_id: None,
                    request: json!({
                        "sessionId": "session", "mode": "form", "message": "test",
                        "requestedSchema": { "type": "object", "properties": {} }
                    }),
                    sender: session_sender,
                },
            );
            state.elicitations.insert(
                "request-elicitation".to_string(),
                PendingElicitation {
                    session_id: None,
                    url_elicitation_id: None,
                    request: json!({
                        "requestId": 1, "mode": "form", "message": "test",
                        "requestedSchema": { "type": "object", "properties": {} }
                    }),
                    sender: request_sender,
                },
            );
            state.url_elicitations.insert(
                "session-url".to_string(),
                ActiveUrlElicitation {
                    session_id: Some("session".to_string()),
                },
            );
            state.url_elicitations.insert(
                "request-url".to_string(),
                ActiveUrlElicitation { session_id: None },
            );
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx };
        cancel_interactions("session", "session_cancelled", &state, &sink).await;

        assert!(matches!(
            permission_receiver.await.unwrap().outcome,
            RequestPermissionOutcome::Cancelled
        ));
        assert!(matches!(
            session_receiver.await.unwrap().action,
            ElicitationAction::Cancel
        ));
        let state = state.lock().await;
        assert!(state.permissions.is_empty());
        assert!(state.elicitations.contains_key("request-elicitation"));
        assert!(!state.url_elicitations.contains_key("session-url"));
        assert!(state.url_elicitations.contains_key("request-url"));
        drop(state);
        let events = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|event| serde_json::from_str::<Value>(&event).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 3);
        assert!(events.iter().any(|event| {
            event["type"] == "acp/permission_resolved" && event["permissionId"] == "permission"
        }));
        assert!(events.iter().any(|event| {
            event["type"] == "acp/elicitation_resolved"
                && event["elicitationId"] == "session-elicitation"
                && event["response"]["action"] == "cancel"
        }));
        assert!(events.iter().any(|event| {
            event["type"] == "acp/elicitation_aborted" && event["reason"] == "session_cancelled"
        }));
    }

    #[tokio::test]
    async fn caps_early_update_retention_and_marks_invalid_replays() {
        let state = Arc::new(Mutex::new(BridgeState {
            pending_creations: 1,
            ..BridgeState::default()
        }));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx };
        for index in 0..(MAX_EARLY_UPDATES + 16) {
            let notification: SessionNotification = serde_json::from_value(json!({
                "sessionId": "new-session",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": format!("message-{index}"),
                    "content": { "type": "text", "text": "early" }
                }
            }))
            .unwrap();
            handle_session_update(notification, &state, &sink, None).await;
        }
        let retained = state
            .lock()
            .await
            .early_updates
            .values()
            .map(Vec::len)
            .sum::<usize>();
        let state = state.lock().await;
        assert!(retained <= MAX_EARLY_UPDATES);
        assert!(state.early_update_bytes <= MAX_EARLY_UPDATE_BYTES);
        assert!(
            state.session_updates["new-session"]
                .invalid_reason
                .is_some()
        );
        drop(state);
        while let Ok(event) = rx.try_recv() {
            let event: Value = serde_json::from_str(&event).unwrap();
            assert_eq!(event["type"], "bridge/error");
        }
    }
}
