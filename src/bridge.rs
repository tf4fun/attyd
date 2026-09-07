use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::{ProtocolVersion, v1::*};
use agent_client_protocol::{
    AcpAgentConfig, Agent, Client, ConnectTo, ConnectionTo, is_incoming_transport_closed,
};
use agent_client_protocol_http::HttpClient;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::agent_process::BoundedAcpAgent;
use crate::auth_terminal::{AuthTerminalManager, validate_method as validate_terminal_auth_method};
use crate::elicitation_validation::{
    validate_elicitation_request, validate_elicitation_response_value,
};
use crate::event_queue::EventSender;
use crate::filesystem::WorkspaceFileSystem;
use crate::mcp::McpManager;
use crate::mcp_config::{server_name, server_type};
use crate::options::{Options, Transport};
use crate::runtime_state::{
    RuntimeEffect, RuntimeState, SessionLifecycle,
    SessionOperationKind as RuntimeSessionOperationKind, UrlFlowStatus,
};
use crate::semantic::{
    SessionUpdateSemanticState, terminal_references, validate_and_track_session_update,
    validate_content_block, validate_permission_request, validate_prompt_response,
    validate_session_config_options, validate_session_config_reference, validate_session_controls,
    validate_session_metadata, validate_session_mode_reference,
};
use crate::session_mirror::{MirrorError, MirrorPhase, SessionMirror, TurnAdmission};
use crate::terminal::{TerminalManager, TerminalSnapshot};

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
const SHUTDOWN_CANCEL_GRACE_PERIOD: Duration = Duration::from_secs(2);
const TERMINAL_SNAPSHOT_QUEUE_CAPACITY: usize = 64;
const RECONCILE_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const RECONCILE_MAX_BACKOFF: Duration = Duration::from_secs(8);
const RECONCILE_MAX_ATTEMPTS: usize = 8;
const MAX_SESSION_VIEW_WAITERS_PER_SESSION: usize = 64;

#[derive(Clone)]
struct EventSink {
    tx: EventSender,
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

    fn send_to(&self, subscriber_id: u64, event: Value) {
        self.send(json!({
            "type": "bridge/internal_direct",
            "subscriberId": subscriber_id,
            "event": event,
        }));
    }

    fn internal_typed(&self, kind: &str, value: impl Serialize) {
        match serde_json::to_value(value) {
            Ok(value) => {
                let _ = self.tx.send(
                    json!({
                        "type": kind,
                        "value": value,
                    })
                    .to_string(),
                );
            }
            Err(error) => self.error(
                format!("failed to serialize canonical runtime state: {error}"),
                None,
                Some("canonical/runtime"),
            ),
        }
    }

    fn error(&self, message: impl Into<String>, request_id: Option<&str>, operation: Option<&str>) {
        self.send(error_event(message, request_id, operation));
    }

    fn acp_error(&self, error: Error, request_id: Option<&str>, operation: Option<&str>) {
        self.send(acp_error_event(error, request_id, operation));
    }

    fn acp_error_to(
        &self,
        subscriber_id: u64,
        error: Error,
        request_id: Option<&str>,
        operation: Option<&str>,
    ) {
        self.send_to(subscriber_id, acp_error_event(error, request_id, operation));
    }
}

fn error_event(
    message: impl Into<String>,
    request_id: Option<&str>,
    operation: Option<&str>,
) -> Value {
    let mut event = json!({ "type": "bridge/error", "message": message.into() });
    if let Some(request_id) = request_id {
        event["requestId"] = json!(request_id);
    }
    if let Some(operation) = operation {
        event["operation"] = json!(operation);
    }
    event
}

fn relay_interaction_resolution(
    sink: &EventSink,
    event: Value,
    delivered: bool,
    interaction: &str,
) -> Result<(), Error> {
    if !delivered {
        return Err(Error::request_cancelled().data(format!(
            "Agent cancelled the {interaction} request before delivery"
        )));
    }
    sink.send(event);
    Ok(())
}

fn acp_error_event(error: Error, request_id: Option<&str>, operation: Option<&str>) -> Value {
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
    event
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
    incarnation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttachmentReservation {
    Fresh { cwd: PathBuf },
    Reload { cwd: PathBuf, incarnation: u64 },
}

impl AttachmentReservation {
    fn cwd(&self) -> &PathBuf {
        match self {
            Self::Fresh { cwd } | Self::Reload { cwd, .. } => cwd,
        }
    }

    fn is_reload(&self) -> bool {
        matches!(self, Self::Reload { .. })
    }
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
struct SyncControlCandidate {
    updates: Vec<Value>,
    bytes: usize,
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
    materializations_in_flight: HashMap<String, String>,
    attachment_subscribers: HashMap<String, u64>,
    attachment_update_counts: HashMap<String, usize>,
    attachment_update_bytes: HashMap<String, usize>,
    sync_control_candidates: HashMap<String, SyncControlCandidate>,
    early_updates: HashMap<String, Vec<SessionNotification>>,
    early_update_count: usize,
    early_update_bytes: usize,
    session_updates: HashMap<String, SessionUpdateSemanticState>,
    session_view_waiters: HashMap<String, Vec<oneshot::Sender<Result<Value, SessionViewError>>>>,
    permissions: HashMap<String, PendingPermission>,
    elicitations: HashMap<String, PendingElicitation>,
    url_elicitations: HashMap<String, ActiveUrlElicitation>,
    seen_url_elicitation_ids: HashSet<String>,
    in_flight_request_ids: HashSet<String>,
    runtime: RuntimeState,
    session_mirror: Option<SessionMirror>,
    published_runtime_seq: u64,
}

fn session_mirror(state: &mut BridgeState) -> &mut SessionMirror {
    let epoch = state.runtime.epoch().to_string();
    state
        .session_mirror
        .get_or_insert_with(|| SessionMirror::new(epoch))
}

fn session_view_value(
    state: &mut BridgeState,
    session_id: &str,
    incarnation: u64,
) -> Result<Value, Error> {
    let mut live = state
        .runtime
        .session(session_id)
        .filter(|session| session.incarnation == incarnation)
        .map(serde_json::to_value)
        .transpose()?
        .unwrap_or(Value::Null);
    let mut view = session_mirror(state)
        .view_value(session_id, incarnation)
        .map_err(mirror_error)?;
    if let Some(live) = live.as_object_mut() {
        live.remove("activeTurn");
    }
    view["live"] = live;
    Ok(view)
}

fn session_delta_value(
    state: &BridgeState,
    session_id: &str,
    incarnation: u64,
    change: Value,
) -> Result<Value, Error> {
    let session = state
        .session_mirror
        .as_ref()
        .and_then(|mirror| mirror.state(session_id))
        .filter(|session| session.incarnation == incarnation)
        .ok_or_else(|| runtime_state_error("missing authoritative session projection"))?;
    Ok(json!({
        "type": "bridge/session_delta",
        "bridgeEpoch": state.runtime.epoch(),
        "sessionId": session_id,
        "sessionIncarnation": incarnation,
        "fromRevision": session.view_revision.saturating_sub(1),
        "viewRevision": session.view_revision,
        "change": change,
    }))
}

fn session_turn_outcome_value(
    state: &mut BridgeState,
    event_type: &str,
    session_id: &str,
    incarnation: u64,
    operation_id: &str,
    extra: Value,
) -> Result<Value, Error> {
    let bridge_epoch = state.runtime.epoch().to_string();
    let session = state
        .session_mirror
        .as_ref()
        .and_then(|mirror| mirror.state(session_id))
        .filter(|session| session.incarnation == incarnation)
        .ok_or_else(|| Error::internal_error().data("missing turn outcome projection"))?;
    let mut event = json!({
        "type": event_type,
        "bridgeEpoch": bridge_epoch,
        "sessionId": session_id,
        "sessionIncarnation": session.incarnation,
        "viewRevision": session.view_revision,
        "historyRevision": session.history_revision,
        "phase": session.phase,
        "operationId": operation_id,
    });
    let Some(event_fields) = event.as_object_mut() else {
        return Err(Error::internal_error().data("turn outcome must be an object"));
    };
    let Some(extra_fields) = extra.as_object() else {
        return Err(Error::internal_error().data("turn outcome fields must be an object"));
    };
    event_fields.extend(extra_fields.clone());
    Ok(event)
}

fn advance_session_delta(
    state: &mut BridgeState,
    session_id: &str,
    incarnation: u64,
    change: Value,
) -> Result<Value, Error> {
    session_mirror(state)
        .touch(session_id, incarnation)
        .map_err(mirror_error)?;
    session_delta_value(state, session_id, incarnation, change)
}

fn mirror_error(error: MirrorError) -> Error {
    match error {
        MirrorError::StaleHistory => Error::invalid_request().data("history revision is stale"),
        MirrorError::IdempotencyConflict => {
            Error::invalid_request().data("client intent ID was reused with a different turn")
        }
        MirrorError::WrongPhase => {
            Error::invalid_request().data("session is not ready to accept this operation")
        }
        MirrorError::UnknownSession | MirrorError::StaleIncarnation => {
            Error::invalid_params().data("unknown or stale session incarnation")
        }
        MirrorError::OperationMismatch => {
            Error::invalid_request().data("session operation no longer matches")
        }
        MirrorError::InconsistentHistory => {
            Error::invalid_request().data("session history is inconsistent with the completed turn")
        }
        MirrorError::History(error) => {
            Error::invalid_request().data(format!("history cache rejected snapshot: {error:?}"))
        }
    }
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

fn validate_replay_control_references(
    validation: &SessionUpdateSemanticState,
    modes: Option<&Value>,
) -> Result<(), Error> {
    if let Some(current_mode_id) = validation.current_mode_id.as_deref() {
        validate_session_mode_reference(modes, current_mode_id).map_err(semantic_error)?;
    }
    Ok(())
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
        || state.pending_closes.contains(session_id)
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
    validate_replay_control_references(validation, modes.as_ref())?;
    state.active_sessions.insert(
        session_id.to_string(),
        ActiveSession {
            cwd,
            modes,
            config_options,
            incarnation: 0,
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

fn notification_values(updates: &[SessionNotification]) -> Vec<Value> {
    updates
        .iter()
        .filter_map(|notification| serde_json::to_value(notification).ok())
        .collect()
}

fn serialized_value_len(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

fn conversation_notification_values(updates: &[SessionNotification]) -> Vec<Value> {
    updates
        .iter()
        .filter_map(|notification| {
            let value = serde_json::to_value(notification).ok()?;
            is_conversation_update(value.get("update").unwrap_or(&Value::Null)).then_some(value)
        })
        .collect()
}

fn is_conversation_update(update: &Value) -> bool {
    matches!(
        update.get("sessionUpdate").and_then(Value::as_str),
        Some(
            "user_message_chunk"
                | "agent_message_chunk"
                | "agent_thought_chunk"
                | "tool_call"
                | "tool_call_update"
                | "plan"
                | "plan_update"
                | "plan_removed"
                | "compaction_update"
                | "compaction_summary_chunk"
        )
    )
}

fn apply_legacy_control_update(session: &mut ActiveSession, update: &Value) {
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

fn runtime_state_error(error: impl std::fmt::Debug) -> Error {
    Error::internal_error().data(format!("canonical runtime transition failed: {error:?}"))
}

fn agent_error_value(error: &Error) -> Value {
    json!({
        "code": i32::from(error.code),
        "message": error.message,
        "data": error.data,
    })
}

fn settle_runtime_operation_request_error(
    state: &mut BridgeState,
    session_id: &str,
    incarnation: u64,
    operation_id: &str,
    kind: RuntimeSessionOperationKind,
    error: &Error,
) -> Result<(), Error> {
    let epoch = state.runtime.epoch().to_string();
    if is_incoming_transport_closed(error) {
        state
            .runtime
            .mark_operation_uncertain(
                &epoch,
                session_id,
                incarnation,
                operation_id,
                error.message.clone(),
            )
            .map_err(runtime_state_error)
    } else {
        state
            .runtime
            .fail_operation(
                &epoch,
                session_id,
                incarnation,
                operation_id,
                kind,
                agent_error_value(error),
            )
            .map_err(runtime_state_error)
    }
}

fn settle_runtime_prompt_request_error(
    state: &mut BridgeState,
    session_id: &str,
    incarnation: u64,
    operation_id: &str,
    error: &Error,
) -> Result<(), Error> {
    let epoch = state.runtime.epoch().to_string();
    if is_incoming_transport_closed(error) {
        state
            .runtime
            .mark_prompt_uncertain(
                &epoch,
                session_id,
                incarnation,
                operation_id,
                error.message.clone(),
            )
            .map_err(runtime_state_error)
    } else {
        state
            .runtime
            .fail_prompt(
                &epoch,
                session_id,
                incarnation,
                operation_id,
                agent_error_value(error),
            )
            .map_err(runtime_state_error)
    }
}

fn settle_runtime_attachment_request_error(
    state: &mut BridgeState,
    session_id: &str,
    incarnation: u64,
    operation_id: &str,
    kind: RuntimeSessionOperationKind,
    reload: bool,
    error: &Error,
) -> Result<(), Error> {
    let epoch = state.runtime.epoch().to_string();
    if is_incoming_transport_closed(error) {
        state
            .runtime
            .mark_operation_uncertain(
                &epoch,
                session_id,
                incarnation,
                operation_id,
                error.message.clone(),
            )
            .map_err(runtime_state_error)
    } else if reload {
        state
            .runtime
            .fail_reload(
                &epoch,
                session_id,
                incarnation,
                operation_id,
                agent_error_value(error),
            )
            .map_err(runtime_state_error)
    } else {
        state
            .runtime
            .fail_attachment(
                &epoch,
                session_id,
                incarnation,
                operation_id,
                kind,
                agent_error_value(error),
            )
            .map_err(runtime_state_error)
    }
}

fn fail_runtime_attachment(
    state: &mut BridgeState,
    session_id: &str,
    incarnation: u64,
    operation_id: &str,
    kind: RuntimeSessionOperationKind,
    reload: bool,
    error: &Error,
) -> Result<(), Error> {
    let epoch = state.runtime.epoch().to_string();
    if reload {
        state
            .runtime
            .fail_reload(
                &epoch,
                session_id,
                incarnation,
                operation_id,
                json!({ "message": error.message.clone(), "data": error.data.clone() }),
            )
            .map_err(runtime_state_error)
    } else {
        state
            .runtime
            .fail_attachment(
                &epoch,
                session_id,
                incarnation,
                operation_id,
                kind,
                json!({ "message": error.message.clone(), "data": error.data.clone() }),
            )
            .map_err(runtime_state_error)
    }
}

fn clear_attachment_tracking(state: &mut BridgeState, session_id: &str, clear_validation: bool) {
    state.pending_attachments.remove(session_id);
    state.attachment_subscribers.remove(session_id);
    state.attachment_update_counts.remove(session_id);
    state.attachment_update_bytes.remove(session_id);
    if clear_validation {
        state.session_updates.remove(session_id);
    }
}

fn fail_mirror_attachment(
    state: &mut BridgeState,
    session_id: &str,
    incarnation: u64,
    attempt_id: &str,
    reload: bool,
    retain_cold: bool,
    retryable: bool,
    message: &str,
) {
    if reload || retain_cold {
        let _ = session_mirror(state).fail_load(
            session_id,
            incarnation,
            attempt_id,
            message,
            retryable,
        );
    } else {
        session_mirror(state).remove(session_id, incarnation);
    }
}

fn flush_runtime(state: &mut BridgeState, sink: &EventSink) {
    match state.runtime.deltas_after(state.published_runtime_seq) {
        Some(deltas) => {
            for delta in deltas {
                state.published_runtime_seq = delta.seq;
                sink.internal_typed("bridge/internal_runtime_delta", delta);
            }
        }
        None => {
            let snapshot = state.runtime.snapshot();
            state.published_runtime_seq = snapshot.through_seq;
            sink.internal_typed("bridge/internal_runtime_snapshot", snapshot);
        }
    }
}

fn apply_terminal_snapshot(state: &mut BridgeState, snapshot: TerminalSnapshot) -> bool {
    let Some(session_id) = snapshot
        .value
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return false;
    };
    let Some(terminal_id) = snapshot
        .value
        .get("terminalId")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return false;
    };
    if live_session_incarnation(state, &session_id) != Some(snapshot.incarnation) {
        return false;
    }
    let retained = if snapshot
        .value
        .get("released")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let terminal = crate::runtime_state::fold_terminal_snapshot(
            state
                .runtime
                .session(&session_id)
                .and_then(|session| session.terminals.get(&terminal_id)),
            &snapshot.value,
        );
        session_mirror(state)
            .retain_terminal_output(&session_id, snapshot.incarnation, &terminal_id, &terminal)
            .unwrap_or(false)
    } else {
        false
    };
    let epoch = state.runtime.epoch().to_string();
    let updated = state
        .runtime
        .upsert_terminal(
            &epoch,
            &session_id,
            snapshot.incarnation,
            terminal_id,
            snapshot.value,
        )
        .is_ok_and(|result| {
            matches!(
                result,
                crate::runtime_state::TerminalUpsert::Inserted
                    | crate::runtime_state::TerminalUpsert::Updated
            )
        });
    retained || updated
}

fn publish_terminal_snapshot(
    state: &mut BridgeState,
    sink: &EventSink,
    snapshot: TerminalSnapshot,
) {
    let Some(session_id) = snapshot
        .value
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return;
    };
    let prior_revision = state
        .session_mirror
        .as_ref()
        .and_then(|mirror| mirror.state(&session_id))
        .map(|session| session.view_revision);
    let terminal = snapshot.value.clone();
    let incarnation = snapshot.incarnation;
    if !apply_terminal_snapshot(state, snapshot) {
        return;
    }
    flush_runtime(state, sink);
    let Some(current_revision) = state
        .session_mirror
        .as_ref()
        .and_then(|mirror| mirror.state(&session_id))
        .map(|session| session.view_revision)
    else {
        return;
    };
    if Some(current_revision) == prior_revision {
        if session_mirror(state)
            .touch(&session_id, incarnation)
            .is_err()
        {
            return;
        }
    }
    if let Ok(delta) = session_delta_value(
        state,
        &session_id,
        incarnation,
        json!({ "kind": "terminal_update", "terminal": terminal }),
    ) {
        sink.send(delta);
    }
}

fn live_session_incarnation(state: &BridgeState, session_id: &str) -> Option<u64> {
    state
        .active_sessions
        .get(session_id)
        .map(|session| session.incarnation)
        .or_else(|| {
            state.pending_attachments.contains(session_id).then(|| {
                state
                    .runtime
                    .session(session_id)
                    .filter(|session| session.lifecycle == SessionLifecycle::Attaching)
                    .map(|session| session.incarnation)
            })?
        })
}

fn set_active_incarnation(state: &mut BridgeState, session_id: &str, incarnation: u64) {
    if let Some(session) = state.active_sessions.get_mut(session_id) {
        session.incarnation = incarnation;
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
        && !state.listed_sessions.contains_key(session_id)
        && !state.pending_attachments.contains(session_id)
        && !state.pending_forks.contains(session_id)
        && !state.pending_closes.contains(session_id)
        && !state.pending_controls.contains(session_id)
        && !state.pending_deletions.contains(session_id)
        && state.runtime.session(session_id).is_none()
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
    cancellation: CancellationToken,
}

#[derive(Clone, Copy)]
enum SessionOperation {
    Prompt,
    Fork,
    Close,
    Control,
}

pub(crate) enum BridgeInput {
    RuntimeSnapshotRequest,
    SessionViewRequest {
        session_id: String,
        response: oneshot::Sender<Result<Value, SessionViewError>>,
    },
    RetireIdleSession {
        session_id: String,
    },
    TurnRequest {
        session_id: String,
        history_revision: String,
        client_intent_id: String,
        prompt: Vec<Value>,
        response: oneshot::Sender<Result<Value, String>>,
    },
    BusinessRequest {
        command: Value,
        response: oneshot::Sender<Result<Value, BridgeRequestError>>,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum SessionViewError {
    NotFound,
    Unavailable(String),
}

impl SessionViewError {
    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }
}

impl From<Error> for SessionViewError {
    fn from(error: Error) -> Self {
        Self::Unavailable(error_message(error))
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BridgeRequestError {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl BridgeRequestError {
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
            data: None,
        }
    }

    fn from_acp(error: &Error) -> Self {
        Self {
            message: error.message.clone(),
            code: Some(i32::from(error.code)),
            data: error.data.clone(),
        }
    }
}

#[derive(Clone)]
struct TurnResponder {
    sender: Arc<std::sync::Mutex<Option<oneshot::Sender<Result<Value, String>>>>>,
}

#[derive(Clone)]
struct BusinessResponder {
    sender: Arc<std::sync::Mutex<Option<oneshot::Sender<Result<Value, BridgeRequestError>>>>>,
}

impl BusinessResponder {
    fn new(sender: oneshot::Sender<Result<Value, BridgeRequestError>>) -> Self {
        Self {
            sender: Arc::new(std::sync::Mutex::new(Some(sender))),
        }
    }

    fn success(&self, value: Value) {
        if let Some(sender) = self
            .sender
            .lock()
            .expect("business responder poisoned")
            .take()
        {
            let _ = sender.send(Ok(value));
        }
    }

    fn error(&self, error: &Error) {
        if let Some(sender) = self
            .sender
            .lock()
            .expect("business responder poisoned")
            .take()
        {
            let _ = sender.send(Err(BridgeRequestError::from_acp(error)));
        }
    }
}

impl TurnResponder {
    fn new(sender: oneshot::Sender<Result<Value, String>>) -> Self {
        Self {
            sender: Arc::new(std::sync::Mutex::new(Some(sender))),
        }
    }

    fn success(&self, value: Value) {
        if let Some(sender) = self.sender.lock().expect("turn responder poisoned").take() {
            let _ = sender.send(Ok(value));
        }
    }

    fn error(&self, error: &Error) {
        if let Some(sender) = self.sender.lock().expect("turn responder poisoned").take() {
            let message = error
                .data
                .as_ref()
                .map_or_else(|| error.message.clone(), Value::to_string);
            let _ = sender.send(Err(message));
        }
    }
}

pub async fn run_with_cancellation(
    options: Arc<Options>,
    commands: mpsc::Receiver<BridgeInput>,
    events: EventSender,
    cancellation: CancellationToken,
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
            let (fatal_tx, mut fatal_rx) = mpsc::channel(1);
            let agent = BoundedAcpAgent::new(config)
                .on_stderr(move |chunk| {
                    debug_sink.send(json!({
                        "type": "bridge/stderr",
                        "chunk": chunk,
                    }));
                })
                .on_fatal(move |error| {
                    let _ = fatal_tx.try_send(error);
                });
            let connection = run_connection(
                agent,
                options.clone(),
                commands,
                sink.clone(),
                cancellation.clone(),
            );
            tokio::pin!(connection);
            tokio::select! {
                biased;
                error = fatal_rx.recv() => {
                    let error = error.unwrap_or_else(|| {
                        Error::internal_error().data("Agent process monitor stopped unexpectedly")
                    });
                    cancellation.cancel();
                    // A fatal child/decoder signal must not skip the normal
                    // terminal, MCP, auth, interaction and prompt cleanup in
                    // run_connection. Bound the wait so a broken peer cannot
                    // hang bridge generation teardown forever.
                    let _ = tokio::time::timeout(
                        SHUTDOWN_CANCEL_GRACE_PERIOD,
                        &mut connection,
                    )
                    .await;
                    Err(error)
                },
                result = &mut connection => result,
            }
        }
        Transport::Http | Transport::Ws => match HttpClient::with_endpoint(&options.command[0]) {
            Ok(client) => {
                run_connection(
                    client,
                    options.clone(),
                    commands,
                    sink.clone(),
                    cancellation.clone(),
                )
                .await
            }
            Err(error) => Err(Error::invalid_params().data(error.to_string())),
        },
    };

    match result {
        Ok(()) => sink.send(json!({ "type": "bridge/phase", "phase": "stopped" })),
        Err(error) => {
            tracing::error!(error = ?error, "ACP bridge failed");
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
    cancellation: CancellationToken,
) -> Result<(), Error>
where
    T: ConnectTo<Client>,
{
    let state = Arc::new(Mutex::new(BridgeState::default()));
    sink.internal_typed(
        "bridge/internal_runtime_snapshot",
        state.lock().await.runtime.snapshot(),
    );
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
    let (terminal_snapshot_tx, mut terminal_snapshot_rx) =
        mpsc::channel(TERMINAL_SNAPSHOT_QUEUE_CAPACITY);
    let terminals = filesystem.clone().map(|filesystem| {
        TerminalManager::new_with_snapshots(filesystem, sink.tx.clone(), Some(terminal_snapshot_tx))
    });
    {
        let state = state.clone();
        let sink = sink.clone();
        tokio::spawn(async move {
            while let Some(snapshot) = terminal_snapshot_rx.recv().await {
                let mut state = state.lock().await;
                publish_terminal_snapshot(&mut state, &sink, snapshot);
            }
        });
    }
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
                    let cancellation = responder.cancellation();
                    let (incarnation, business_delta) = {
                        let mut state = state.lock().await;
                        let incarnation =
                            live_session_incarnation(&state, &session_id).ok_or_else(|| {
                                Error::invalid_params().data(format!(
                                    "unknown or inactive session: {session_id}"
                                ))
                            })?;
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
                        let epoch = state.runtime.epoch().to_string();
                        state
                            .runtime
                            .upsert_permission(
                                &epoch,
                                &session_id,
                                incarnation,
                                permission_id.clone(),
                                request_value.clone(),
                            )
                            .map_err(runtime_state_error)?;
                        state
                            .permissions
                            .insert(permission_id.clone(), PendingPermission {
                                session_id: session_id.clone(),
                                tool_call_id,
                                option_ids,
                                sender,
                            });
                        let business_delta = advance_session_delta(
                            &mut state,
                            &session_id,
                            incarnation,
                            json!({
                                "kind": "interaction_upsert",
                                "interaction": {
                                    "interactionId": permission_id,
                                    "type": "permission",
                                    "request": request_value,
                                },
                            }),
                        )?;
                        flush_runtime(&mut state, &sink);
                        (incarnation, business_delta)
                    };
                    sink.send(business_delta);
                    sink.send(json!({
                        "type": "acp/permission_request",
                        "permissionId": permission_id,
                        "request": request,
                    }));
                    tokio::select! {
                        biased;
                        response = receiver => responder.respond(response.unwrap_or_else(|_| {
                            RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
                        })),
                        _ = cancellation.cancelled() => {
                            let (removed, business_delta) = {
                                let mut state = state.lock().await;
                                let removed = state.permissions.remove(&permission_id).is_some();
                                let mut business_delta = None;
                                if removed {
                                    let epoch = state.runtime.epoch().to_string();
                                    state.runtime.resolve_permission(
                                        &epoch,
                                        &session_id,
                                        incarnation,
                                        &permission_id,
                                    ).map_err(runtime_state_error)?;
                                    business_delta = Some(advance_session_delta(
                                        &mut state,
                                        &session_id,
                                        incarnation,
                                        json!({
                                            "kind": "interaction_remove",
                                            "interactionId": permission_id,
                                        }),
                                    )?);
                                    flush_runtime(&mut state, &sink);
                                }
                                (removed, business_delta)
                            };
                            if let Some(delta) = business_delta {
                                sink.send(delta);
                            }
                            if removed {
                                sink.send(json!({
                                    "type": "acp/permission_resolved",
                                    "permissionId": permission_id,
                                }));
                            }
                            Err(Error::request_cancelled())
                        }
                    }
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
                    let cancellation = responder.cancellation();
                    let (runtime_scope, business_delta) = {
                        let mut state = state.lock().await;
                        let runtime_scope = match &session_id {
                            Some(session_id) => Some((
                                session_id.clone(),
                                live_session_incarnation(&state, session_id).ok_or_else(|| {
                                        Error::invalid_params().data(
                                            "elicitation references an unknown or inactive session",
                                        )
                                    })?,
                            )),
                            None => None,
                        };
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
                        let epoch = state.runtime.epoch().to_string();
                        state
                            .runtime
                            .upsert_elicitation(
                                &epoch,
                                runtime_scope
                                    .as_ref()
                                    .map(|(session_id, incarnation)| {
                                        (session_id.as_str(), *incarnation)
                                    }),
                                elicitation_id.clone(),
                                request_value.clone(),
                            )
                            .map_err(runtime_state_error)?;
                        state
                            .elicitations
                            .insert(elicitation_id.clone(), PendingElicitation {
                                session_id: session_id.clone(),
                                url_elicitation_id: url_elicitation_id.clone(),
                                    request: request_value.clone(),
                                sender,
                            });
                        let business_delta = if let Some((session_id, incarnation)) = &runtime_scope
                        {
                            Some(advance_session_delta(
                                &mut state,
                                session_id,
                                *incarnation,
                                json!({
                                    "kind": "interaction_upsert",
                                    "interaction": {
                                        "interactionId": elicitation_id,
                                        "type": "elicitation",
                                        "request": request_value,
                                    },
                                }),
                            )?)
                        } else {
                            None
                        };
                        flush_runtime(&mut state, &sink);
                        (runtime_scope, business_delta)
                    };
                    if let Some(delta) = business_delta {
                        sink.send(delta);
                    }
                    sink.send(json!({
                        "type": "acp/elicitation_request",
                        "elicitationId": elicitation_id,
                        "request": request,
                    }));
                    tokio::select! {
                        biased;
                        response = receiver => responder.respond(response.unwrap_or_else(|_| {
                            CreateElicitationResponse::new(ElicitationAction::Cancel)
                        })),
                        _ = cancellation.cancelled() => {
                            let (removed, business_delta) = {
                                let mut state = state.lock().await;
                                let removed = state.elicitations.remove(&elicitation_id).is_some();
                                let mut business_delta = None;
                                if removed {
                                    let epoch = state.runtime.epoch().to_string();
                                    state.runtime.resolve_elicitation(
                                        &epoch,
                                        runtime_scope.as_ref().map(|(session_id, incarnation)| {
                                            (session_id.as_str(), *incarnation)
                                        }),
                                        &elicitation_id,
                                        None,
                                    ).map_err(runtime_state_error)?;
                                    if let Some((session_id, incarnation)) = &runtime_scope {
                                        business_delta = Some(advance_session_delta(
                                            &mut state,
                                            session_id,
                                            *incarnation,
                                            json!({
                                                "kind": "interaction_remove",
                                                "interactionId": elicitation_id,
                                            }),
                                        )?);
                                    }
                                    flush_runtime(&mut state, &sink);
                                }
                                (removed, business_delta)
                            };
                            if let Some(delta) = business_delta {
                                sink.send(delta);
                            }
                            if removed {
                                sink.send(json!({
                                    "type": "acp/elicitation_resolved",
                                    "elicitationId": elicitation_id,
                                    "response": CreateElicitationResponse::new(
                                        ElicitationAction::Cancel,
                                    ),
                                }));
                            }
                            Err(Error::request_cancelled())
                        }
                    }
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
                    {
                        let mut state = state.lock().await;
                        let tracked = state
                            .url_elicitations
                            .get(&elicitation_id)
                            .ok_or_else(|| {
                                Error::invalid_params().data(format!(
                                    "URL elicitation was not accepted or is no longer active: {elicitation_id}"
                                ))
                            })?;
                        let runtime_scope = match &tracked.session_id {
                            Some(session_id) => Some((
                                session_id.clone(),
                                state
                                    .active_sessions
                                    .get(session_id)
                                    .map(|session| session.incarnation)
                                    .ok_or_else(|| {
                                        Error::invalid_params().data(
                                            "URL elicitation references an inactive session",
                                        )
                                    })?,
                            )),
                            None => None,
                        };
                        let epoch = state.runtime.epoch().to_string();
                        state
                            .runtime
                            .settle_url_flow(
                                &epoch,
                                runtime_scope
                                    .as_ref()
                                    .map(|(session_id, incarnation)| {
                                        (session_id.as_str(), *incarnation)
                                    }),
                                &elicitation_id,
                                UrlFlowStatus::Completed,
                            )
                            .map_err(runtime_state_error)?;
                        state.url_elicitations.remove(&elicitation_id);
                        flush_runtime(&mut state, &sink);
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
                                match require_live_session(&session_id, &state).await {
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
                                match require_live_session(&session_id, &state).await {
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
                                match require_live_session_incarnation(&session_id, &state).await {
                                    Ok(incarnation) => {
                                        let result = terminals
                                            .create_for_incarnation(request, incarnation)
                                            .await;
                                        match result {
                                            Ok(response)
                                                if require_live_session_incarnation(
                                                    &session_id,
                                                    &state,
                                                )
                                                    .await
                                                    .ok()
                                                    != Some(incarnation) =>
                                            {
                                                let _ = terminals
                                                    .release(ReleaseTerminalRequest::new(
                                                        session_id,
                                                        response.terminal_id,
                                                    ))
                                                    .await;
                                                Err(Error::request_cancelled().data(
                                                    "session closed while terminal was being created",
                                                ))
                                            }
                                            result => result,
                                        }
                                    }
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
                let state = state.clone();
                let sink = sink.clone();
                async move |request: ReleaseTerminalRequest, responder, connection| {
                    let terminals = terminals.clone();
                    let state = state.clone();
                    let sink = sink.clone();
                    connection.spawn(async move {
                        let result = match terminals {
                            Some(terminals) => match terminals.release_with_snapshot(request).await {
                                Ok((response, snapshot)) => {
                                    let mut state = state.lock().await;
                                    publish_terminal_snapshot(&mut state, &sink, snapshot);
                                    Ok(response)
                                }
                                Err(error) => Err(error),
                            },
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
                let input = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => None,
                    input = commands.recv() => input,
                };
                let (command, turn_responder, business_responder) = match input {
                    Some(BridgeInput::RuntimeSnapshotRequest) => {
                        let mut state = state.lock().await;
                        let snapshot = state.runtime.snapshot();
                        state.published_runtime_seq = snapshot.through_seq;
                        sink.internal_typed("bridge/internal_runtime_snapshot", snapshot);
                        continue;
                    }
                    Some(BridgeInput::RetireIdleSession { session_id }) => {
                        let ready = {
                            let state = state.lock().await;
                            state
                                .active_sessions
                                .get(&session_id)
                                .is_some_and(|session| {
                                    state
                                        .session_mirror
                                        .as_ref()
                                        .and_then(|mirror| mirror.state(&session_id))
                                        .is_some_and(|mirror| {
                                            mirror.incarnation == session.incarnation
                                                && mirror.phase == MirrorPhase::Ready
                                        })
                                })
                        };
                        if !ready {
                            continue;
                        }
                        (
                            json!({
                                "type": "session/close",
                                "requestId": format!("bridge-idle-close-{}", Uuid::new_v4()),
                                "sessionId": session_id,
                            }),
                            None,
                            None,
                        )
                    }
                    Some(BridgeInput::SessionViewRequest {
                        session_id,
                        response,
                    }) => {
                        let materialize = {
                            let mut state = state.lock().await;
                            if let Some(incarnation) = state
                                .active_sessions
                                .get(&session_id)
                                .map(|session| session.incarnation)
                            {
                                let result =
                                    session_view_value(&mut state, &session_id, incarnation)
                                        .map_err(SessionViewError::from);
                                let _ = response.send(result);
                                None
                            } else if let Some(incarnation) = state
                                .session_mirror
                                .as_ref()
                                .and_then(|mirror| mirror.state(&session_id))
                                .filter(|session| session.phase == MirrorPhase::Blocked)
                                .map(|session| session.incarnation)
                            {
                                let result =
                                    session_view_value(&mut state, &session_id, incarnation)
                                        .map_err(SessionViewError::from);
                                let _ = response.send(result);
                                None
                            } else if !state.listed_sessions.contains_key(&session_id) {
                                let _ = response.send(Err(SessionViewError::NotFound));
                                None
                            } else {
                                let already_loading =
                                    state.materializations_in_flight.contains_key(&session_id);
                                let waiters = state
                                    .session_view_waiters
                                    .entry(session_id.clone())
                                    .or_default();
                                waiters.retain(|waiter| !waiter.is_closed());
                                if waiters.len() >= MAX_SESSION_VIEW_WAITERS_PER_SESSION {
                                    let _ = response.send(Err(SessionViewError::unavailable(
                                        "too many observers are waiting for this session",
                                    )));
                                    None
                                } else {
                                    waiters.push(response);
                                    (!already_loading).then(|| {
                                        let materialization_id = Uuid::new_v4().to_string();
                                        state
                                            .materializations_in_flight
                                            .insert(session_id.clone(), materialization_id.clone());
                                        (session_id, materialization_id)
                                    })
                                }
                            }
                        };
                        let Some((session_id, materialization_id)) = materialize else {
                            continue;
                        };
                        (
                            json!({
                                "type": "session/load",
                                "requestId": format!("bridge-materialize-{}", Uuid::new_v4()),
                                "sessionId": session_id,
                                "bridgeManagedMaterialization": true,
                                "bridgeMaterializationId": materialization_id,
                            }),
                            None,
                            None,
                        )
                    }
                    Some(BridgeInput::TurnRequest {
                        session_id,
                        history_revision,
                        client_intent_id,
                        prompt,
                        response,
                    }) => (
                        json!({
                            "type": "session/prompt",
                            "requestId": client_intent_id,
                            "clientIntentId": client_intent_id,
                            "historyRevision": history_revision,
                            "sessionId": session_id,
                            "prompt": prompt,
                        }),
                        Some(TurnResponder::new(response)),
                        None,
                    ),
                    Some(BridgeInput::BusinessRequest { command, response }) => {
                        (command, None, Some(BusinessResponder::new(response)))
                    }
                    None => break,
                };
                let operation = command
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
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
                    cancellation: cancellation.clone(),
                };
                connection.spawn(async move {
                    let request_id = command
                        .get("requestId")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    let intent_session_id = command
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    let bridge_managed_materialization = command
                        .get("bridgeManagedMaterialization")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let bridge_materialization_id = command
                        .get("bridgeMaterializationId")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    let task_sink = context.sink.clone();
                    if operation != "session/prompt"
                        && let Some(request_id) = request_id.as_deref()
                    {
                        let mut state = context.state.lock().await;
                        if !state.in_flight_request_ids.insert(request_id.to_string()) {
                            drop(state);
                            if let Some(responder) = &business_responder {
                                responder.error(
                                    &Error::invalid_request()
                                        .data("requestId is already in flight on this bridge"),
                                );
                            }
                            return Ok(());
                        }
                    }
                    let result = if bridge_managed_materialization {
                        let mut command = command;
                        let mut backoff = RECONCILE_INITIAL_BACKOFF;
                        let mut attempt = 0_usize;
                        loop {
                            attempt += 1;
                            command["bridgeMaterializationFinalAttempt"] =
                                json!(attempt >= RECONCILE_MAX_ATTEMPTS);
                            let result = handle_command(
                                command.clone(),
                                task_connection.clone(),
                                context.clone(),
                                None,
                                None,
                                None,
                            )
                            .await;
                            match result {
                                Err(error)
                                    if retryable_reconcile_error(&error)
                                        && attempt < RECONCILE_MAX_ATTEMPTS
                                        && !context.cancellation.is_cancelled() =>
                                {
                                    task_sink.send(json!({
                                        "type": "bridge/session_sync",
                                        "sessionId": intent_session_id,
                                        "phase": "retrying",
                                        "message": error.message,
                                        "retryAfterMs": backoff.as_millis(),
                                    }));
                                    tokio::select! {
                                        _ = tokio::time::sleep(backoff) => {}
                                        _ = context.cancellation.cancelled() => {
                                            break Err(Error::request_cancelled());
                                        }
                                    }
                                    backoff = backoff.saturating_mul(2).min(RECONCILE_MAX_BACKOFF);
                                    command["requestId"] =
                                        json!(format!("bridge-materialize-{}", Uuid::new_v4()));
                                }
                                result => break result,
                            }
                        }
                    } else {
                        handle_command(
                            command,
                            task_connection,
                            context.clone(),
                            prompt_start,
                            turn_responder.clone(),
                            business_responder.clone(),
                        )
                        .await
                    };
                    if operation == "session/load"
                        && bridge_managed_materialization
                        && let Some(session_id) = intent_session_id.as_deref()
                        && let Some(materialization_id) = bridge_materialization_id.as_deref()
                    {
                        resolve_session_view_waiters(
                            &context.state,
                            session_id,
                            materialization_id,
                            &result,
                        )
                        .await;
                    }
                    if let (Some(responder), Err(error)) = (&turn_responder, &result) {
                        responder.error(error);
                    }
                    match (&business_responder, &result) {
                        (Some(responder), Ok(())) => {
                            responder.success(json!({ "status": "ok" }));
                        }
                        (Some(responder), Err(error)) => responder.error(error),
                        (None, _) => {}
                    }
                    apply_runtime_effects(&context.state, &task_sink, context.terminals.as_ref())
                        .await;
                    {
                        let mut state = context.state.lock().await;
                        flush_runtime(&mut state, &task_sink);
                        if let Some(request_id) = request_id.as_deref() {
                            state.in_flight_request_ids.remove(request_id);
                        }
                    }
                    Ok(())
                })?;
            }
            cancel_prompts_on_shutdown(&connection, &state, &sink, &prompt_lifecycle).await;
            if let Some(terminals) = terminals {
                terminals.close_all().await;
            }
            mcp.close_all().await;
            auth_terminal.close();
            Ok(())
        })
        .await
}

fn error_message(error: Error) -> String {
    error.data.map_or(error.message, |data| data.to_string())
}

async fn resolve_session_view_waiters(
    state: &Arc<Mutex<BridgeState>>,
    session_id: &str,
    materialization_id: &str,
    load_result: &Result<(), Error>,
) {
    let (waiters, result) = {
        let mut state = state.lock().await;
        if state
            .materializations_in_flight
            .get(session_id)
            .is_none_or(|owner| owner != materialization_id)
        {
            return;
        }
        let waiters = state
            .session_view_waiters
            .remove(session_id)
            .unwrap_or_default();
        state.materializations_in_flight.remove(session_id);
        let result = match load_result {
            Ok(()) => state
                .active_sessions
                .get(session_id)
                .map(|session| session.incarnation)
                .ok_or_else(|| {
                    SessionViewError::unavailable(
                        "session/load completed without materializing the session",
                    )
                })
                .and_then(|incarnation| {
                    session_view_value(&mut state, session_id, incarnation)
                        .map_err(SessionViewError::from)
                }),
            Err(error) => Err(SessionViewError::Unavailable(
                error
                    .data
                    .clone()
                    .map_or_else(|| error.message.clone(), |data| data.to_string()),
            )),
        };
        (waiters, result)
    };
    for waiter in waiters {
        let _ = waiter.send(result.clone());
    }
}

async fn apply_runtime_effects(
    state: &Arc<Mutex<BridgeState>>,
    sink: &EventSink,
    terminals: Option<&TerminalManager>,
) {
    let terminal_releases = {
        let mut state = state.lock().await;
        let effects = state.runtime.take_effects();
        let mut terminal_releases = Vec::new();
        for effect in effects {
            match effect {
                RuntimeEffect::CancelPermissionResponder {
                    session_id,
                    interaction_id,
                } => {
                    if state
                        .permissions
                        .get(&interaction_id)
                        .is_some_and(|pending| pending.session_id == session_id)
                    {
                        let pending = state
                            .permissions
                            .remove(&interaction_id)
                            .expect("matching permission checked above");
                        let _ = pending.sender.send(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Cancelled,
                        ));
                        sink.send(json!({
                            "type": "acp/permission_resolved",
                            "permissionId": interaction_id,
                        }));
                    }
                }
                RuntimeEffect::CancelElicitationResponder {
                    session_id,
                    interaction_id,
                } => {
                    if state
                        .elicitations
                        .get(&interaction_id)
                        .is_some_and(|pending| pending.session_id == session_id)
                    {
                        let pending = state
                            .elicitations
                            .remove(&interaction_id)
                            .expect("matching elicitation checked above");
                        let response = CreateElicitationResponse::new(ElicitationAction::Cancel);
                        let _ = pending.sender.send(response.clone());
                        sink.send(json!({
                            "type": "acp/elicitation_resolved",
                            "elicitationId": interaction_id,
                            "response": response,
                        }));
                    }
                }
                RuntimeEffect::AbortUrlFlow {
                    session_id,
                    elicitation_id,
                } => {
                    if state
                        .url_elicitations
                        .get(&elicitation_id)
                        .is_some_and(|tracked| tracked.session_id == session_id)
                    {
                        state.url_elicitations.remove(&elicitation_id);
                        sink.send(json!({
                            "type": "acp/elicitation_aborted",
                            "elicitationId": elicitation_id,
                            "sessionId": session_id,
                            "reason": "runtime_terminal",
                        }));
                    }
                }
                RuntimeEffect::ReleaseTerminal {
                    session_id,
                    terminal_id,
                } => terminal_releases.push((session_id, terminal_id)),
            }
        }
        terminal_releases
    };
    if let Some(terminals) = terminals {
        for (session_id, terminal_id) in terminal_releases {
            let _ = terminals
                .release(ReleaseTerminalRequest::new(session_id, terminal_id))
                .await;
        }
    }
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
    enum Delivery {
        None,
        Broadcast,
        Direct(u64),
    }
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
    let replaying = if tool_update {
        let state = state.lock().await;
        state.pending_attachments.contains(&session_id)
            || state
                .active_sessions
                .get(&session_id)
                .and_then(|session| {
                    state
                        .session_mirror
                        .as_ref()
                        .and_then(|mirror| mirror.load_attempt(&session_id, session.incarnation))
                })
                .is_some()
    } else {
        false
    };
    if tool_update && !replaying {
        for terminal_id in terminal_references(&update) {
            let result = match terminals {
                Some(terminals) => terminals.assert_reference(&terminal_id, &session_id).await,
                None => {
                    Err(Error::invalid_request().data(format!("unknown terminal: {terminal_id}")))
                }
            };
            if let Err(error) = result {
                let mut state = state.lock().await;
                let attachment = state.pending_attachments.contains(&session_id);
                let reconciling = state
                    .active_sessions
                    .get(&session_id)
                    .and_then(|session| {
                        state.session_mirror.as_ref().and_then(|mirror| {
                            mirror.load_attempt(&session_id, session.incarnation)
                        })
                    })
                    .is_some();
                let attachment_subscriber = state.attachment_subscribers.get(&session_id).copied();
                if (!state.active_sessions.contains_key(&session_id) || attachment || reconciling)
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
                if let Some(subscriber_id) = attachment_subscriber {
                    sink.acp_error_to(subscriber_id, error, None, Some("session/update"));
                } else {
                    sink.acp_error(error, None, Some("session/update"));
                }
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
        let active_incarnation = state
            .active_sessions
            .get(&session_id)
            .map(|session| session.incarnation);
        let mirror_phase = active_incarnation.and_then(|incarnation| {
            state
                .session_mirror
                .as_ref()
                .and_then(|mirror| mirror.state(&session_id))
                .filter(|session| session.incarnation == incarnation)
                .map(|session| session.phase)
        });
        let mirror_replay_attempt = active_incarnation.and_then(|incarnation| {
            state
                .session_mirror
                .as_ref()
                .and_then(|mirror| mirror.load_attempt(&session_id, incarnation))
                .map(str::to_string)
        });
        let late_canonical_conversation = active
            && !attachment
            && mirror_replay_attempt.is_none()
            && is_conversation_update(&update)
            && state
                .active_sessions
                .get(&session_id)
                .is_some_and(|session| session.incarnation != 0)
            && state
                .runtime
                .session(&session_id)
                .is_none_or(|session| session.active_turn.is_none());
        if late_canonical_conversation {
            return;
        }
        let attachment_subscriber = attachment
            .then(|| state.attachment_subscribers.get(&session_id).copied())
            .flatten();
        let validation = state.session_updates.entry(session_id.clone()).or_default();
        let result = if let Err(message) = validate_and_track_session_update(validation, &update) {
            if !active || attachment || mirror_replay_attempt.is_some() {
                validation.invalid_reason.get_or_insert(message.clone());
            }
            Err(semantic_error(message))
        } else if attachment {
            let replay_bytes = state
                .attachment_update_bytes
                .get(&session_id)
                .copied()
                .unwrap_or(0);
            let replay_len = state
                .attachment_update_counts
                .get(&session_id)
                .copied()
                .unwrap_or(0);
            let update_bytes = serialized_value_len(&update);
            if replay_len >= MAX_EARLY_UPDATES
                || replay_bytes.saturating_add(update_bytes) > MAX_EARLY_UPDATE_BYTES
            {
                Err(semantic_error(format!(
                    "Agent session attachment replay exceeds {MAX_EARLY_UPDATE_BYTES} bytes or {MAX_EARLY_UPDATES} updates"
                )))
            } else {
                let incarnation = state
                    .runtime
                    .session(&session_id)
                    .map(|session| session.incarnation)
                    .ok_or_else(|| runtime_state_error("missing canonical attachment transaction"));
                let runtime_result = incarnation.and_then(|incarnation| {
                    if is_conversation_update(&update) {
                        let attempt = session_mirror(&mut state)
                            .load_attempt(&session_id, incarnation)
                            .map(str::to_string);
                        if let Some(attempt) = attempt {
                            session_mirror(&mut state)
                                .append_load_update(
                                    &session_id,
                                    incarnation,
                                    &attempt,
                                    update.clone(),
                                )
                                .map_err(mirror_error)?;
                        }
                    }
                    let epoch = state.runtime.epoch().to_string();
                    state
                        .runtime
                        .append_attachment_candidate(
                            &epoch,
                            &session_id,
                            incarnation,
                            update.clone(),
                        )
                        .map_err(runtime_state_error)
                });
                match runtime_result {
                    Ok(()) => {
                        state
                            .attachment_update_counts
                            .insert(session_id.clone(), replay_len.saturating_add(1));
                        state.attachment_update_bytes.insert(
                            session_id.clone(),
                            replay_bytes.saturating_add(update_bytes),
                        );
                        Ok(state
                            .attachment_subscribers
                            .get(&session_id)
                            .copied()
                            .map_or(Delivery::Broadcast, Delivery::Direct))
                    }
                    Err(error) => Err(error),
                }
            }
        } else if active
            && !attachment
            && matches!(
                mirror_phase,
                Some(MirrorPhase::Loading | MirrorPhase::Reconciling)
            )
            && mirror_replay_attempt.is_some()
        {
            let incarnation = active_incarnation.expect("active session has an incarnation");
            let mirrored = if is_conversation_update(&update) {
                session_mirror(&mut state)
                    .append_load_update(
                        &session_id,
                        incarnation,
                        mirror_replay_attempt
                            .as_deref()
                            .expect("reconciliation has a load attempt"),
                        update.clone(),
                    )
                    .map_err(mirror_error)
            } else {
                let bytes = serialized_value_len(&update);
                let candidate = state
                    .sync_control_candidates
                    .entry(session_id.clone())
                    .or_default();
                if candidate.updates.len() >= MAX_EARLY_UPDATES
                    || candidate.bytes.saturating_add(bytes) > MAX_EARLY_UPDATE_BYTES
                {
                    Err(semantic_error(format!(
                        "Agent control replay exceeds {MAX_EARLY_UPDATE_BYTES} bytes or {MAX_EARLY_UPDATES} updates"
                    )))
                } else {
                    candidate.bytes = candidate.bytes.saturating_add(bytes);
                    candidate.updates.push(update.clone());
                    Ok(())
                }
            };
            mirrored.map(|()| Delivery::None)
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
                Ok(Delivery::None)
            }
        } else {
            let incarnation = state
                .active_sessions
                .get(&session_id)
                .map(|session| session.incarnation)
                .unwrap_or(0);
            if incarnation != 0 {
                let epoch = state.runtime.epoch().to_string();
                let mut business_delta = None;
                let runtime_result: Result<(), Error> = if is_conversation_update(&update) {
                    let mirrored = if mirror_phase == Some(MirrorPhase::Running)
                        && let Some(operation_id) = session_mirror(&mut state)
                            .active_operation_id(&session_id, incarnation)
                            .map(str::to_string)
                    {
                        let mirrored = session_mirror(&mut state)
                            .append_turn_update(
                                &session_id,
                                incarnation,
                                &operation_id,
                                update.clone(),
                            )
                            .map_err(mirror_error);
                        if mirrored.is_ok() {
                            business_delta = Some(
                                session_delta_value(
                                    &state,
                                    &session_id,
                                    incarnation,
                                    json!({ "kind": "turn_update", "update": update }),
                                )
                                .expect("a mirrored turn update has a session identity"),
                            );
                        }
                        mirrored
                    } else {
                        Ok(())
                    };
                    mirrored.and_then(|()| {
                        if state
                            .runtime
                            .session(&session_id)
                            .and_then(|session| session.active_turn.as_ref())
                            .is_some()
                        {
                            state
                                .runtime
                                .append_turn_update(
                                    &epoch,
                                    &session_id,
                                    incarnation,
                                    update.clone(),
                                )
                                .map_err(runtime_state_error)
                        } else {
                            Ok(())
                        }
                    })
                } else {
                    let key = update
                        .get("sessionUpdate")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    state
                        .runtime
                        .update_control_state(&epoch, &session_id, incarnation, key, update.clone())
                        .map_err(runtime_state_error)
                };
                if let Err(error) = runtime_result {
                    Err(error)
                } else {
                    if let Some(delta) = business_delta {
                        sink.send(delta);
                    }
                    if let Some(session) = state.active_sessions.get_mut(&session_id) {
                        apply_legacy_control_update(session, &update);
                    }
                    Ok(Delivery::Broadcast)
                }
            } else {
                if let Some(session) = state.active_sessions.get_mut(&session_id) {
                    apply_legacy_control_update(session, &update);
                }
                Ok(Delivery::Broadcast)
            }
        };
        if let Err(error) = &result
            && (attachment || mirror_replay_attempt.is_some())
            && let Some(validation) = state.session_updates.get_mut(&session_id)
        {
            validation.invalid_reason.get_or_insert_with(|| {
                error
                    .data
                    .as_ref()
                    .map_or_else(|| error.message.clone(), Value::to_string)
            });
        }
        if result.is_err()
            && let Some((incarnation, attempt_id)) =
                state.session_mirror.as_ref().and_then(|mirror| {
                    let incarnation = mirror.state(&session_id)?.incarnation;
                    mirror
                        .load_attempt(&session_id, incarnation)
                        .map(|attempt| (incarnation, attempt.to_string()))
                })
        {
            let _ =
                session_mirror(&mut state).invalidate_load(&session_id, incarnation, &attempt_id);
        }
        flush_runtime(&mut state, sink);
        (
            result,
            attachment_subscriber,
            mirror_replay_attempt.is_some(),
        )
    };
    match outcome {
        (Ok(Delivery::Broadcast), _, _) => {
            sink.send(json!({ "type": "acp/session_update", "notification": notification }));
        }
        (Ok(Delivery::Direct(subscriber_id)), _, _) => sink.send_to(
            subscriber_id,
            json!({ "type": "acp/session_update", "notification": notification }),
        ),
        (Ok(Delivery::None), _, _) => {}
        (Err(error), Some(subscriber_id), _) => {
            sink.acp_error_to(subscriber_id, error, None, Some("session/update"));
        }
        (Err(_), None, true) => {}
        (Err(error), None, false) => sink.acp_error(error, None, Some("session/update")),
    }
}

async fn cancel_prompts_on_shutdown(
    connection: &ConnectionTo<Agent>,
    state: &Arc<Mutex<BridgeState>>,
    sink: &EventSink,
    lifecycle: &PromptLifecycle,
) {
    // A bridge shutdown can race with a just-accepted browser command. Wait
    // until every prompt command has either failed validation or published
    // its ACP request so cancellation cannot overtake the prompt.
    lifecycle.wait_until_started().await;

    let session_ids = {
        let state = state.lock().await;
        state.prompts.clone()
    };
    if session_ids.is_empty() {
        return;
    }

    for session_id in &session_ids {
        let _ = connection.send_notification(CancelNotification::new(session_id.clone()));
        cancel_interactions(session_id, "session_cancelled", state, sink).await;
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
    let _ = tokio::time::timeout(SHUTDOWN_CANCEL_GRACE_PERIOD, wait_for_completion).await;
}

async fn handle_command(
    command: Value,
    connection: ConnectionTo<Agent>,
    context: CommandContext,
    mut prompt_start: Option<PromptStartGuard>,
    turn_responder: Option<TurnResponder>,
    business_responder: Option<BusinessResponder>,
) -> Result<(), Error> {
    if serialized_value_len(&command) > MAX_BRIDGE_MESSAGE_BYTES {
        return Err(Error::invalid_params().data(format!(
            "Bridge command exceeds {MAX_BRIDGE_MESSAGE_BYTES} bytes"
        )));
    }
    let CommandContext {
        options,
        state,
        sink,
        terminals,
        filesystem,
        auth_terminal,
        prompt_lifecycle,
        cancellation,
    } = context;
    let bridge_state = state.clone();
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
            if let Some(responder) = &business_responder {
                responder.success(serde_json::to_value(&response)?);
            }
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
            if let Some(responder) = &business_responder {
                responder.success(serde_json::to_value(&response)?);
            }
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
                    if state.pending_creations == 0 {
                        clear_pending_creation_replays(&mut state);
                    }
                    let replay = notification_values(&early_updates);
                    let epoch = state.runtime.epoch().to_string();
                    let runtime_result = state.runtime.open_new_with_replay(
                        &epoch,
                        session_id.clone(),
                        cwd.to_string_lossy(),
                        response_value,
                        replay,
                    );
                    let incarnation = match runtime_result {
                        Ok(incarnation) => incarnation,
                        Err(error) => {
                            state.active_sessions.remove(&session_id);
                            state.session_updates.remove(&session_id);
                            let can_close = can_close_rejected_chat_session(&state, &session_id);
                            drop(state);
                            close_rejected_chat_session(&connection, &session_id, can_close).await;
                            return Err(runtime_state_error(error));
                        }
                    };
                    set_active_incarnation(&mut state, &session_id, incarnation);
                    session_mirror(&mut state).register_new(session_id.clone(), incarnation);
                    let mirror_view = session_view_value(&mut state, &session_id, incarnation)?;
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
                    if let Some(responder) = &business_responder {
                        responder.success(json!({
                            "sessionId": session_id,
                            "cwd": cwd,
                            "view": mirror_view,
                        }));
                    }
                    sink.send(event);
                    sink.send(json!({
                        "type": "bridge/session_view",
                        "sessionId": session_id,
                        "view": mirror_view,
                    }));
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
            // Startup cwd is the new-session default, not a discovery filter.
            // Existing sessions retain the cwd returned by the Agent.
            let result = connection
                .send_request(ListSessionsRequest::new().cursor(cursor.clone()))
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
            if let Some(responder) = &business_responder {
                responder.success(serde_json::to_value(&response)?);
            }
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
            let bridge_managed_materialization = command
                .get("bridgeManagedMaterialization")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let bridge_materialization_final_attempt = command
                .get("bridgeMaterializationFinalAttempt")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            require_agent_method(operation, &state).await?;
            let attachment_kind = if operation == "session/load" {
                RuntimeSessionOperationKind::Load
            } else {
                RuntimeSessionOperationKind::Resume
            };
            let reservation = reserve_attachment(&session_id, attachment_kind, &state).await?;
            let cwd = reservation.cwd().clone();
            let reload = reservation.is_reload();
            state
                .lock()
                .await
                .attachment_subscribers
                .insert(session_id.clone(), 0);
            let attachment_incarnation = {
                let mut state = state.lock().await;
                let epoch = state.runtime.epoch().to_string();
                let started = match reservation {
                    AttachmentReservation::Fresh { .. } => state.runtime.start_attachment(
                        &epoch,
                        session_id.clone(),
                        cwd.to_string_lossy(),
                        request_id.clone(),
                        attachment_kind,
                    ),
                    AttachmentReservation::Reload { incarnation, .. } => state
                        .runtime
                        .start_reload(
                            &epoch,
                            &session_id,
                            incarnation,
                            request_id.clone(),
                            attachment_kind,
                        )
                        .map(|()| incarnation),
                };
                let incarnation = match started {
                    Ok(incarnation) => incarnation,
                    Err(error) => {
                        clear_attachment_tracking(&mut state, &session_id, false);
                        return Err(runtime_state_error(error));
                    }
                };
                if !reload {
                    session_mirror(&mut state).register_cold(session_id.clone(), incarnation);
                }
                if operation == "session/load" {
                    let mirror = session_mirror(&mut state);
                    if let Err(error) = mirror.begin_load(&session_id, incarnation, &request_id) {
                        let error = mirror_error(error);
                        fail_runtime_attachment(
                            &mut state,
                            &session_id,
                            incarnation,
                            &request_id,
                            attachment_kind,
                            reload,
                            &error,
                        )?;
                        if !reload {
                            session_mirror(&mut state).remove(&session_id, incarnation);
                        }
                        clear_attachment_tracking(&mut state, &session_id, false);
                        return Err(error);
                    }
                }
                flush_runtime(&mut state, &sink);
                incarnation
            };
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
                    settle_runtime_attachment_request_error(
                        &mut state,
                        &session_id,
                        attachment_incarnation,
                        &request_id,
                        attachment_kind,
                        reload,
                        &error,
                    )?;
                    if operation == "session/load" {
                        fail_mirror_attachment(
                            &mut state,
                            &session_id,
                            attachment_incarnation,
                            &request_id,
                            reload,
                            bridge_managed_materialization,
                            retryable_reconcile_error(&error)
                                && !bridge_materialization_final_attempt,
                            &error.message,
                        );
                    }
                    clear_attachment_tracking(&mut state, &session_id, true);
                    return Err(error);
                }
            };
            if let Err(error) = ensure_relay_size(&response, "session attachment response") {
                fail_runtime_attachment(
                    &mut state,
                    &session_id,
                    attachment_incarnation,
                    &request_id,
                    attachment_kind,
                    reload,
                    &error,
                )?;
                if operation == "session/load" {
                    fail_mirror_attachment(
                        &mut state,
                        &session_id,
                        attachment_incarnation,
                        &request_id,
                        reload,
                        bridge_managed_materialization,
                        false,
                        &error.message,
                    );
                }
                clear_attachment_tracking(&mut state, &session_id, true);
                return Err(error);
            }
            let tracked = if reload {
                let result = response_controls(&response).and_then(|(modes, config_options)| {
                    let validation = state.session_updates.get(&session_id).ok_or_else(|| {
                        runtime_state_error("missing same-session reload validation state")
                    })?;
                    if let Some(reason) = &validation.invalid_reason {
                        return Err(semantic_error(format!(
                            "Agent session replay was invalid: {reason}"
                        )));
                    }
                    validate_replay_control_references(validation, modes.as_ref())?;
                    let active = state.active_sessions.get(&session_id).ok_or_else(|| {
                        runtime_state_error("same-session reload lost its active session")
                    })?;
                    if active.incarnation != attachment_incarnation {
                        return Err(runtime_state_error(
                            "same-session reload incarnation changed before commit",
                        ));
                    }
                    Ok((modes, config_options))
                });
                result.map(Some)
            } else {
                track_session(
                    &mut state,
                    &session_id,
                    cwd.clone(),
                    &response,
                    Some(&session_id),
                )
                .map(|_| None)
            };
            let replacement_controls = match tracked {
                Ok(controls) => controls,
                Err(error) => {
                    fail_runtime_attachment(
                        &mut state,
                        &session_id,
                        attachment_incarnation,
                        &request_id,
                        attachment_kind,
                        reload,
                        &error,
                    )?;
                    if operation == "session/load" {
                        fail_mirror_attachment(
                            &mut state,
                            &session_id,
                            attachment_incarnation,
                            &request_id,
                            reload,
                            bridge_managed_materialization,
                            false,
                            &error.message,
                        );
                    }
                    clear_attachment_tracking(&mut state, &session_id, true);
                    return Err(error);
                }
            };
            let epoch = state.runtime.epoch().to_string();
            if operation == "session/load" {
                session_mirror(&mut state)
                    .commit_load(&session_id, attachment_incarnation, &request_id)
                    .map_err(mirror_error)?;
            }
            let completed = if reload {
                state.runtime.complete_reload(
                    &epoch,
                    &session_id,
                    attachment_incarnation,
                    &request_id,
                    response.clone(),
                )
            } else {
                state.runtime.complete_attachment(
                    &epoch,
                    &session_id,
                    attachment_incarnation,
                    &request_id,
                    attachment_kind,
                    response.clone(),
                )
            };
            if let Err(error) = completed {
                clear_attachment_tracking(&mut state, &session_id, true);
                return Err(runtime_state_error(error));
            }
            if let Some((modes, config_options)) = replacement_controls {
                let active = state
                    .active_sessions
                    .get_mut(&session_id)
                    .expect("same-session reload was validated above");
                active.modes = modes;
                active.config_options = config_options;
            } else {
                set_active_incarnation(&mut state, &session_id, attachment_incarnation);
            }
            if let Some(validation) = state.session_updates.get_mut(&session_id) {
                validation.retire_turn();
            }
            state.pending_attachments.remove(&session_id);
            let attachment_subscriber = state
                .attachment_subscribers
                .remove(&session_id)
                .unwrap_or(0);
            state.attachment_update_counts.remove(&session_id);
            state.attachment_update_bytes.remove(&session_id);
            let mirror_view = (operation == "session/load")
                .then(|| session_view_value(&mut state, &session_id, attachment_incarnation))
                .transpose()?;
            drop(state);
            if let Some(view) = mirror_view {
                sink.send(json!({
                    "type": "bridge/session_view",
                    "sessionId": session_id,
                    "view": view,
                }));
            }
            if operation == "session/resume" {
                synchronize_authoritative_history(
                    &connection,
                    &options,
                    &bridge_state,
                    &sink,
                    &cancellation,
                    &session_id,
                    attachment_incarnation,
                )
                .await?;
            }
            sink.send_to(
                attachment_subscriber,
                json!({
                    "type": "acp/session_attached",
                    "requestId": request_id,
                    "method": if operation == "session/load" { "load" } else { "resume" },
                    "sessionId": session_id,
                    "cwd": cwd,
                    "response": response,
                }),
            );
        }
        "session/fork" => {
            let request_id = string_field(&command, "requestId")?.to_string();
            let source_id = string_field(&command, "sessionId")?.to_string();
            require_agent_method("session/fork", &state).await?;
            let (cwd, source_incarnation) = {
                let mut state = state.lock().await;
                let source = state.active_sessions.get(&source_id).ok_or_else(|| {
                    Error::invalid_params()
                        .data(format!("unknown or inactive session: {source_id}"))
                })?;
                let cwd = source.cwd.clone();
                let source_incarnation = source.incarnation;
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
                let epoch = state.runtime.epoch().to_string();
                if let Err(error) = state.runtime.start_operation(
                    &epoch,
                    &source_id,
                    source_incarnation,
                    request_id.clone(),
                    RuntimeSessionOperationKind::Fork,
                    "forking",
                ) {
                    release_session_operation(&mut state, &source_id, SessionOperation::Fork);
                    return Err(runtime_state_error(error));
                }
                flush_runtime(&mut state, &sink);
                state.pending_creations += 1;
                (cwd, source_incarnation)
            };
            sink.send(json!({
                "type": "bridge/session_operation_started",
                "requestId": request_id,
                "sessionId": source_id,
                "operation": "fork",
            }));
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
            let response = match result {
                Ok(response) => response,
                Err(error) => {
                    settle_runtime_operation_request_error(
                        &mut state,
                        &source_id,
                        source_incarnation,
                        &request_id,
                        RuntimeSessionOperationKind::Fork,
                        &error,
                    )?;
                    release_session_operation(&mut state, &source_id, SessionOperation::Fork);
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
                    let epoch = state.runtime.epoch().to_string();
                    state
                        .runtime
                        .fail_operation(
                            &epoch,
                            &source_id,
                            source_incarnation,
                            &request_id,
                            RuntimeSessionOperationKind::Fork,
                            json!({
                                "code": i32::from(error.code),
                                "message": error.message.clone(),
                                "data": error.data.clone(),
                            }),
                        )
                        .map_err(runtime_state_error)?;
                    release_session_operation(&mut state, &source_id, SessionOperation::Fork);
                    let can_close = can_close_rejected_chat_session(&state, &session_id);
                    if state.pending_creations == 0 {
                        clear_pending_creation_replays(&mut state);
                    }
                    drop(state);
                    close_rejected_chat_session(&connection, &session_id, can_close).await;
                    return Err(error);
                }
            };
            if state.pending_creations == 0 {
                clear_pending_creation_replays(&mut state);
            }
            let target_replay = conversation_notification_values(&early_updates);
            let target_replay = (!target_replay.is_empty()).then_some(target_replay);
            let epoch = state.runtime.epoch().to_string();
            let runtime_result = state.runtime.open_forked(
                &epoch,
                session_id.clone(),
                cwd.to_string_lossy(),
                response_value,
                (&source_id, source_incarnation),
                target_replay,
            );
            let incarnation = match runtime_result {
                Ok(incarnation) => incarnation,
                Err(error) => {
                    state.active_sessions.remove(&session_id);
                    state.session_updates.remove(&session_id);
                    state
                        .runtime
                        .fail_operation(
                            &epoch,
                            &source_id,
                            source_incarnation,
                            &request_id,
                            RuntimeSessionOperationKind::Fork,
                            json!({ "message": format!("{error:?}") }),
                        )
                        .map_err(runtime_state_error)?;
                    release_session_operation(&mut state, &source_id, SessionOperation::Fork);
                    let can_close = can_close_rejected_chat_session(&state, &session_id);
                    drop(state);
                    close_rejected_chat_session(&connection, &session_id, can_close).await;
                    return Err(runtime_state_error(error));
                }
            };
            set_active_incarnation(&mut state, &session_id, incarnation);
            session_mirror(&mut state).register_cold(session_id.clone(), incarnation);
            state
                .runtime
                .complete_operation(
                    &epoch,
                    &source_id,
                    source_incarnation,
                    &request_id,
                    RuntimeSessionOperationKind::Fork,
                    serde_json::to_value(&response)?,
                )
                .map_err(runtime_state_error)?;
            release_session_operation(&mut state, &source_id, SessionOperation::Fork);
            drop(state);
            synchronize_authoritative_history(
                &connection,
                &options,
                &bridge_state,
                &sink,
                &cancellation,
                &session_id,
                incarnation,
            )
            .await?;
            if let Some(responder) = &business_responder {
                let view = {
                    let mut state = bridge_state.lock().await;
                    session_view_value(&mut state, &session_id, incarnation)?
                };
                responder.success(json!({
                    "sessionId": session_id,
                    "sourceSessionId": source_id,
                    "cwd": cwd,
                    "view": view,
                }));
            }
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
            let request_id = string_field(&command, "requestId")?.to_string();
            let session_id = string_field(&command, "sessionId")?.to_string();
            require_agent_method("session/close", &state).await?;
            {
                let mut state = state.lock().await;
                reserve_session_operation(&mut state, &session_id, SessionOperation::Close)?;
                let incarnation = state.active_sessions[&session_id].incarnation;
                let epoch = state.runtime.epoch().to_string();
                if let Err(error) = state.runtime.start_operation(
                    &epoch,
                    &session_id,
                    incarnation,
                    request_id.clone(),
                    RuntimeSessionOperationKind::Close,
                    "closing",
                ) {
                    release_session_operation(&mut state, &session_id, SessionOperation::Close);
                    return Err(runtime_state_error(error));
                }
                flush_runtime(&mut state, &sink);
            }
            sink.send(json!({
                "type": "bridge/session_operation_started",
                "requestId": request_id,
                "sessionId": session_id,
                "operation": "close",
            }));
            let result = connection
                .send_request(CloseSessionRequest::new(session_id.clone()))
                .block_task()
                .await;
            if let Err(error) = result {
                let mut state = state.lock().await;
                let incarnation = state.active_sessions[&session_id].incarnation;
                settle_runtime_operation_request_error(
                    &mut state,
                    &session_id,
                    incarnation,
                    &request_id,
                    RuntimeSessionOperationKind::Close,
                    &error,
                )?;
                release_session_operation(&mut state, &session_id, SessionOperation::Close);
                return Err(error);
            }
            {
                let mut state = state.lock().await;
                let incarnation = state.active_sessions[&session_id].incarnation;
                let epoch = state.runtime.epoch().to_string();
                state
                    .runtime
                    .close_session(&epoch, &session_id, incarnation, &request_id)
                    .map_err(runtime_state_error)?;
                session_mirror(&mut state).remove(&session_id, incarnation);
                state.sync_control_candidates.remove(&session_id);
                state.active_sessions.remove(&session_id);
                state.session_updates.remove(&session_id);
            }
            cancel_interactions(&session_id, "session_closed", &state, &sink).await;
            if let Some(terminals) = &terminals {
                terminals.release_session(&session_id).await;
            }
            {
                let mut state = state.lock().await;
                release_session_operation(&mut state, &session_id, SessionOperation::Close);
            }
            sink.send(json!({
                "type": "acp/session_closed",
                "requestId": request_id,
                "sessionId": session_id,
            }));
        }
        "session/delete" => {
            let request_id = string_field(&command, "requestId")?.to_string();
            let session_id = string_field(&command, "sessionId")?.to_string();
            require_agent_method("session/delete", &state).await?;
            let (close_before_delete, runtime_incarnation) = {
                let mut state = state.lock().await;
                reserve_deletion(&mut state, &session_id)?;
                let close_before_delete = state.active_sessions.contains_key(&session_id)
                    && state
                        .agent_capabilities
                        .as_ref()
                        .is_some_and(|capabilities| {
                            capabilities.session_capabilities.close.is_some()
                        });
                let runtime_incarnation = match begin_runtime_delete(
                    &mut state,
                    &session_id,
                    &request_id,
                    close_before_delete,
                ) {
                    Ok(incarnation) => incarnation,
                    Err(error) => {
                        state.pending_deletions.remove(&session_id);
                        return Err(error);
                    }
                };
                if runtime_incarnation.is_some() {
                    flush_runtime(&mut state, &sink);
                }
                (close_before_delete, runtime_incarnation)
            };
            sink.send(json!({
                "type": "bridge/session_operation_started",
                "requestId": request_id,
                "sessionId": session_id,
                "operation": "delete",
            }));
            if close_before_delete {
                let close_result = connection
                    .send_request(CloseSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await;
                if let Err(error) = close_result {
                    let mut state = bridge_state.lock().await;
                    if let Some(incarnation) = runtime_incarnation {
                        settle_runtime_operation_request_error(
                            &mut state,
                            &session_id,
                            incarnation,
                            &request_id,
                            RuntimeSessionOperationKind::Delete,
                            &error,
                        )?;
                        session_mirror(&mut state).remove(&session_id, incarnation);
                        state.sync_control_candidates.remove(&session_id);
                    }
                    state.pending_deletions.remove(&session_id);
                    return Err(error);
                }
                {
                    let mut state = state.lock().await;
                    if let Some(incarnation) = runtime_incarnation {
                        commit_runtime_delete_close_success(
                            &mut state,
                            &session_id,
                            incarnation,
                            &request_id,
                            &sink,
                        )?;
                    }
                    state.active_sessions.remove(&session_id);
                    state.session_updates.remove(&session_id);
                }
                cancel_interactions(&session_id, "session_closed", &state, &sink).await;
                if let Some(terminals) = &terminals {
                    terminals.release_session(&session_id).await;
                }
                sink.send(json!({
                    "type": "acp/session_closed",
                    "requestId": request_id,
                    "sessionId": session_id,
                }));
            }
            let result = connection
                .send_request(DeleteSessionRequest::new(session_id.clone()))
                .block_task()
                .await;
            if let Err(error) = result {
                let mut state = state.lock().await;
                if let Some(incarnation) = runtime_incarnation {
                    settle_runtime_operation_request_error(
                        &mut state,
                        &session_id,
                        incarnation,
                        &request_id,
                        RuntimeSessionOperationKind::Delete,
                        &error,
                    )?;
                }
                state.pending_deletions.remove(&session_id);
                return Err(error);
            }
            if let Some(incarnation) = runtime_incarnation {
                let mut state = state.lock().await;
                let epoch = state.runtime.epoch().to_string();
                state
                    .runtime
                    .complete_delete(&epoch, &session_id, incarnation, &request_id)
                    .map_err(runtime_state_error)?;
                session_mirror(&mut state).remove(&session_id, incarnation);
                state.sync_control_candidates.remove(&session_id);
            }
            if !close_before_delete {
                {
                    let mut state = state.lock().await;
                    state.active_sessions.remove(&session_id);
                    state.session_updates.remove(&session_id);
                }
                cancel_interactions(&session_id, "session_closed", &state, &sink).await;
                if let Some(terminals) = &terminals {
                    terminals.release_session(&session_id).await;
                }
            }
            {
                let mut state = state.lock().await;
                state.pending_deletions.remove(&session_id);
                state.listed_sessions.remove(&session_id);
            }
            sink.send(json!({
                "type": "acp/session_deleted",
                "requestId": request_id,
                "sessionId": session_id,
            }));
        }
        "session/prompt" => {
            let turn_responder = turn_responder.ok_or_else(|| {
                Error::invalid_request()
                    .data("turns require a session revision and intent admission")
            })?;
            let request_id = string_field(&command, "requestId")?.to_string();
            let runtime_operation_id = request_id.clone();
            let client_intent_id = string_field(&command, "clientIntentId")?.to_string();
            let expected_history_revision = string_field(&command, "historyRevision")?;
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
            let runtime_prompt = blocks.clone();
            {
                let state = state.lock().await;
                validate_prompt_capabilities(blocks, &state)?;
            }
            let prompt: Vec<ContentBlock> = serde_json::from_value(prompt_value)?;
            let (incarnation, mirror_operation_id, running_view) = {
                let mut state = state.lock().await;
                let incarnation = state
                    .active_sessions
                    .get(&session_id)
                    .map(|session| session.incarnation)
                    .ok_or_else(|| {
                        Error::invalid_params()
                            .data(format!("unknown or inactive session: {session_id}"))
                    })?;
                let mirror_operation_id = match session_mirror(&mut state)
                    .start_turn(
                        &session_id,
                        incarnation,
                        expected_history_revision,
                        &client_intent_id,
                        runtime_prompt.clone(),
                    )
                    .map_err(mirror_error)?
                {
                    TurnAdmission::Accepted { operation_id } => operation_id,
                    TurnAdmission::Duplicate { operation_id } => {
                        turn_responder.success(json!({
                            "operationId": operation_id,
                            "disposition": "duplicate",
                        }));
                        return Ok(());
                    }
                };
                if let Err(error) =
                    reserve_session_operation(&mut state, &session_id, SessionOperation::Prompt)
                {
                    let _ = session_mirror(&mut state).abort_turn(
                        &session_id,
                        incarnation,
                        &mirror_operation_id,
                    );
                    return Err(error);
                }
                let epoch = state.runtime.epoch().to_string();
                if let Err(error) = state.runtime.start_prompt(
                    &epoch,
                    &session_id,
                    incarnation,
                    runtime_operation_id.clone(),
                    runtime_prompt,
                ) {
                    release_session_operation(&mut state, &session_id, SessionOperation::Prompt);
                    let _ = session_mirror(&mut state).abort_turn(
                        &session_id,
                        incarnation,
                        &mirror_operation_id,
                    );
                    return Err(runtime_state_error(error));
                }
                flush_runtime(&mut state, &sink);
                let view = session_view_value(&mut state, &session_id, incarnation)?;
                (incarnation, mirror_operation_id, view)
            };
            turn_responder.success(json!({
                "operationId": mirror_operation_id,
                "disposition": "accepted",
            }));
            sink.send(json!({
                "type": "bridge/session_view",
                "sessionId": session_id,
                "view": running_view,
            }));
            sink.send(json!({
                "type": "acp/prompt_started",
                "requestId": request_id,
                "sessionId": session_id,
                "prompt": prompt,
            }));
            let prompt_for_outcome = prompt.clone();
            let request = connection.send_request(PromptRequest::new(session_id.clone(), prompt));
            if let Some(prompt_start) = prompt_start.as_mut() {
                prompt_start.finish();
            }
            let result = request.block_task().await;
            let response = match result {
                Ok(response) => response,
                Err(error) => {
                    let mut state = state.lock().await;
                    settle_runtime_prompt_request_error(
                        &mut state,
                        &session_id,
                        incarnation,
                        &runtime_operation_id,
                        &error,
                    )?;
                    if is_incoming_transport_closed(&error) {
                        session_mirror(&mut state)
                            .block_turn(
                                &session_id,
                                incarnation,
                                &mirror_operation_id,
                                error.message.clone(),
                            )
                            .map_err(mirror_error)?;
                        let outcome = session_turn_outcome_value(
                            &mut state,
                            "bridge/session_turn_failed",
                            &session_id,
                            incarnation,
                            &mirror_operation_id,
                            json!({
                            "clientIntentId": client_intent_id,
                            "prompt": prompt_for_outcome,
                            "error": agent_error_value(&error),
                            }),
                        )?;
                        sink.send(outcome);
                        finish_prompt_operation(&mut state, &session_id, &prompt_lifecycle);
                        return Err(error);
                    }
                    session_mirror(&mut state)
                        .complete_turn(
                            &session_id,
                            incarnation,
                            &mirror_operation_id,
                            json!({ "error": agent_error_value(&error) }),
                        )
                        .map_err(mirror_error)?;
                    flush_runtime(&mut state, &sink);
                    let reconciling_view =
                        session_view_value(&mut state, &session_id, incarnation)?;
                    drop(state);
                    sink.send(json!({
                        "type": "bridge/session_view",
                        "sessionId": session_id,
                        "view": reconciling_view,
                    }));
                    let commit = commit_completed_turn_history(
                        &bridge_state,
                        &sink,
                        &session_id,
                        incarnation,
                        &mirror_operation_id,
                    )
                    .await;
                    let mut state = bridge_state.lock().await;
                    finish_prompt_operation(&mut state, &session_id, &prompt_lifecycle);
                    if let Err(sync_error) = commit {
                        tracing::warn!(
                            session_id,
                            error = ?sync_error,
                            "failed to commit history after an Agent prompt error"
                        );
                    }
                    let outcome = session_turn_outcome_value(
                        &mut state,
                        "bridge/session_turn_failed",
                        &session_id,
                        incarnation,
                        &mirror_operation_id,
                        json!({
                        "clientIntentId": client_intent_id,
                        "prompt": prompt_for_outcome,
                        "error": agent_error_value(&error),
                        }),
                    )?;
                    drop(state);
                    sink.send(outcome);
                    return Err(error);
                }
            };
            if let Err(message) = validate_prompt_response(&response) {
                let error = semantic_error(message);
                let reconciling_view = {
                    let mut state = state.lock().await;
                    let epoch = state.runtime.epoch().to_string();
                    state
                        .runtime
                        .fail_prompt(
                            &epoch,
                            &session_id,
                            incarnation,
                            &runtime_operation_id,
                            json!({ "message": error.message.clone(), "data": error.data.clone() }),
                        )
                        .map_err(runtime_state_error)?;
                    session_mirror(&mut state)
                        .complete_turn(
                            &session_id,
                            incarnation,
                            &mirror_operation_id,
                            json!({ "invalidPromptResponse": error.message.clone() }),
                        )
                        .map_err(mirror_error)?;
                    flush_runtime(&mut state, &sink);
                    session_view_value(&mut state, &session_id, incarnation)?
                };
                sink.send(json!({
                    "type": "bridge/session_view",
                    "sessionId": session_id,
                    "view": reconciling_view,
                }));
                let commit = commit_completed_turn_history(
                    &bridge_state,
                    &sink,
                    &session_id,
                    incarnation,
                    &mirror_operation_id,
                )
                .await;
                let mut state = state.lock().await;
                finish_prompt_operation(&mut state, &session_id, &prompt_lifecycle);
                if let Err(sync_error) = commit {
                    tracing::warn!(
                        session_id,
                        error = ?sync_error,
                        "failed to commit history after an invalid prompt response"
                    );
                }
                let outcome = session_turn_outcome_value(
                    &mut state,
                    "bridge/session_turn_failed",
                    &session_id,
                    incarnation,
                    &mirror_operation_id,
                    json!({
                    "clientIntentId": client_intent_id,
                    "prompt": prompt_for_outcome,
                    "error": agent_error_value(&error),
                    }),
                )?;
                drop(state);
                sink.send(outcome);
                return Err(error);
            }
            let reconciling_view = {
                let mut state = state.lock().await;
                let epoch = state.runtime.epoch().to_string();
                let response_value = serde_json::to_value(&response)?;
                state
                    .runtime
                    .complete_prompt(
                        &epoch,
                        &session_id,
                        incarnation,
                        &runtime_operation_id,
                        response_value.clone(),
                    )
                    .map_err(runtime_state_error)?;
                session_mirror(&mut state)
                    .complete_turn(
                        &session_id,
                        incarnation,
                        &mirror_operation_id,
                        response_value,
                    )
                    .map_err(mirror_error)?;
                flush_runtime(&mut state, &sink);
                session_view_value(&mut state, &session_id, incarnation)?
            };
            sink.send(json!({
                "type": "bridge/session_view",
                "sessionId": session_id,
                "view": reconciling_view,
            }));
            let commit = commit_completed_turn_history(
                &bridge_state,
                &sink,
                &session_id,
                incarnation,
                &mirror_operation_id,
            )
            .await;
            {
                let mut state = state.lock().await;
                finish_prompt_operation(&mut state, &session_id, &prompt_lifecycle);
            }
            commit?;
            let outcome = {
                let mut state = state.lock().await;
                session_turn_outcome_value(
                    &mut state,
                    "bridge/session_turn_complete",
                    &session_id,
                    incarnation,
                    &mirror_operation_id,
                    json!({
                        "clientIntentId": client_intent_id,
                        "response": response,
                    }),
                )?
            };
            sink.send(outcome);
            sink.send(json!({
                "type": "acp/prompt_complete",
                "requestId": request_id,
                "sessionId": session_id,
                "response": response,
            }));
        }
        "session/cancel" => {
            let session_id = string_field(&command, "sessionId")?;
            let expected_operation_id = command.get("expectedOperationId").and_then(Value::as_str);
            require_active(session_id, &state).await?;
            {
                let mut state = state.lock().await;
                let incarnation = state.active_sessions[session_id].incarnation;
                let epoch = state.runtime.epoch().to_string();
                if let Some(expected_operation_id) = expected_operation_id {
                    let mirror = session_mirror(&mut state)
                        .state(session_id)
                        .filter(|session| session.incarnation == incarnation)
                        .filter(|session| session.phase == MirrorPhase::Running)
                        .and_then(|session| session.active_turn.as_ref());
                    if mirror.is_none_or(|turn| turn.operation_id != expected_operation_id) {
                        return Err(Error::invalid_request()
                            .data("turn operation is stale or no longer running"));
                    }
                }
                if state
                    .runtime
                    .session(session_id)
                    .and_then(|session| session.active_turn.as_ref())
                    .is_none()
                {
                    return Err(Error::invalid_request().data("session has no active prompt"));
                }
                connection.send_notification(CancelNotification::new(session_id.to_string()))?;
                state
                    .runtime
                    .request_cancel(&epoch, session_id, incarnation)
                    .map_err(runtime_state_error)?;
            }
            cancel_interactions(session_id, "session_cancelled", &state, &sink).await;
        }
        "session/set_mode" => {
            let request_id = string_field(&command, "requestId")?.to_string();
            let session_id = string_field(&command, "sessionId")?.to_string();
            let mode_id = string_field(&command, "modeId")?.to_string();
            {
                let mut state = state.lock().await;
                let session = state.active_sessions.get(&session_id).ok_or_else(|| {
                    Error::invalid_params()
                        .data(format!("unknown or inactive session: {session_id}"))
                })?;
                validate_session_mode_reference(session.modes.as_ref(), &mode_id)
                    .map_err(semantic_error)?;
                let incarnation = session.incarnation;
                reserve_session_operation(&mut state, &session_id, SessionOperation::Control)?;
                let epoch = state.runtime.epoch().to_string();
                if let Err(error) = state.runtime.start_operation(
                    &epoch,
                    &session_id,
                    incarnation,
                    request_id.clone(),
                    RuntimeSessionOperationKind::SetMode,
                    "setting_mode",
                ) {
                    release_session_operation(&mut state, &session_id, SessionOperation::Control);
                    return Err(runtime_state_error(error));
                }
                flush_runtime(&mut state, &sink);
            }
            sink.send(json!({
                "type": "bridge/session_operation_started",
                "requestId": request_id,
                "sessionId": session_id,
                "operation": "mode",
            }));
            let result = connection
                .send_request(SetSessionModeRequest::new(
                    session_id.clone(),
                    mode_id.clone(),
                ))
                .block_task()
                .await;
            let response = match result {
                Ok(response) => response,
                Err(error) => {
                    let mut state = state.lock().await;
                    let incarnation = state.active_sessions[&session_id].incarnation;
                    settle_runtime_operation_request_error(
                        &mut state,
                        &session_id,
                        incarnation,
                        &request_id,
                        RuntimeSessionOperationKind::SetMode,
                        &error,
                    )?;
                    release_session_operation(&mut state, &session_id, SessionOperation::Control);
                    return Err(error);
                }
            };
            if let Err(error) = ensure_relay_size(&response, "session mode response") {
                let mut state = state.lock().await;
                let incarnation = state.active_sessions[&session_id].incarnation;
                let epoch = state.runtime.epoch().to_string();
                state
                    .runtime
                    .fail_operation(
                        &epoch,
                        &session_id,
                        incarnation,
                        &request_id,
                        RuntimeSessionOperationKind::SetMode,
                        json!({ "message": error.message.clone(), "data": error.data.clone() }),
                    )
                    .map_err(runtime_state_error)?;
                release_session_operation(&mut state, &session_id, SessionOperation::Control);
                return Err(error);
            }
            let business_delta = {
                let mut state = state.lock().await;
                let incarnation = state.active_sessions[&session_id].incarnation;
                let epoch = state.runtime.epoch().to_string();
                state
                    .runtime
                    .complete_control_operation(
                        &epoch,
                        &session_id,
                        incarnation,
                        &request_id,
                        RuntimeSessionOperationKind::SetMode,
                        "current_mode_update",
                        json!({
                            "sessionUpdate": "current_mode_update",
                            "currentModeId": mode_id,
                        }),
                        serde_json::to_value(&response)?,
                    )
                    .map_err(runtime_state_error)?;
                if let Some(modes) = state
                    .active_sessions
                    .get_mut(&session_id)
                    .and_then(|session| session.modes.as_mut())
                    .and_then(Value::as_object_mut)
                {
                    modes.insert("currentModeId".to_string(), json!(mode_id));
                }
                release_session_operation(&mut state, &session_id, SessionOperation::Control);
                advance_session_delta(
                    &mut state,
                    &session_id,
                    incarnation,
                    json!({
                        "kind": "control_update",
                        "control": "mode",
                        "modeId": mode_id,
                    }),
                )?
            };
            sink.send(business_delta);
            sink.send(json!({
                "type": "acp/mode_changed",
                "requestId": request_id,
                "sessionId": session_id,
                "modeId": mode_id,
            }));
        }
        "session/set_config_option" => {
            let request_id = string_field(&command, "requestId")?.to_string();
            let session_id = string_field(&command, "sessionId")?.to_string();
            let config_id = string_field(&command, "configId")?.to_string();
            let value = command
                .get("value")
                .cloned()
                .ok_or_else(|| Error::invalid_params().data("config value is required"))?;
            let request = if let Some(value) = value.as_bool() {
                SetSessionConfigOptionRequest::new(session_id.clone(), config_id.clone(), value)
            } else if let Some(value) = value.as_str() {
                SetSessionConfigOptionRequest::new(
                    session_id.clone(),
                    config_id.clone(),
                    SessionConfigValueId::new(value.to_string()),
                )
            } else {
                return Err(Error::invalid_params().data("config value must be string or boolean"));
            };
            {
                let mut state = state.lock().await;
                let session = state.active_sessions.get(&session_id).ok_or_else(|| {
                    Error::invalid_params()
                        .data(format!("unknown or inactive session: {session_id}"))
                })?;
                validate_session_config_reference(&session.config_options, &config_id, &value)
                    .map_err(semantic_error)?;
                let incarnation = session.incarnation;
                reserve_session_operation(&mut state, &session_id, SessionOperation::Control)?;
                let epoch = state.runtime.epoch().to_string();
                if let Err(error) = state.runtime.start_operation(
                    &epoch,
                    &session_id,
                    incarnation,
                    request_id.clone(),
                    RuntimeSessionOperationKind::SetConfig,
                    "setting_config",
                ) {
                    release_session_operation(&mut state, &session_id, SessionOperation::Control);
                    return Err(runtime_state_error(error));
                }
                flush_runtime(&mut state, &sink);
            }
            sink.send(json!({
                "type": "bridge/session_operation_started",
                "requestId": request_id,
                "sessionId": session_id,
                "operation": "config",
            }));
            let result = connection.send_request(request).block_task().await;
            let response = match result {
                Ok(response) => response,
                Err(error) => {
                    let mut state = state.lock().await;
                    let incarnation = state.active_sessions[&session_id].incarnation;
                    settle_runtime_operation_request_error(
                        &mut state,
                        &session_id,
                        incarnation,
                        &request_id,
                        RuntimeSessionOperationKind::SetConfig,
                        &error,
                    )?;
                    release_session_operation(&mut state, &session_id, SessionOperation::Control);
                    return Err(error);
                }
            };
            if let Err(error) = ensure_relay_size(&response, "session config response") {
                let mut state = state.lock().await;
                let incarnation = state.active_sessions[&session_id].incarnation;
                let epoch = state.runtime.epoch().to_string();
                state
                    .runtime
                    .fail_operation(
                        &epoch,
                        &session_id,
                        incarnation,
                        &request_id,
                        RuntimeSessionOperationKind::SetConfig,
                        json!({ "message": error.message.clone(), "data": error.data.clone() }),
                    )
                    .map_err(runtime_state_error)?;
                release_session_operation(&mut state, &session_id, SessionOperation::Control);
                return Err(error);
            }
            let response_value = serde_json::to_value(&response)?;
            let config_options = response_value
                .get("configOptions")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new()));
            if let Err(message) = validate_session_config_options(&config_options) {
                let error = semantic_error(message);
                let mut state = state.lock().await;
                let incarnation = state.active_sessions[&session_id].incarnation;
                let epoch = state.runtime.epoch().to_string();
                state
                    .runtime
                    .fail_operation(
                        &epoch,
                        &session_id,
                        incarnation,
                        &request_id,
                        RuntimeSessionOperationKind::SetConfig,
                        json!({ "message": error.message.clone(), "data": error.data.clone() }),
                    )
                    .map_err(runtime_state_error)?;
                release_session_operation(&mut state, &session_id, SessionOperation::Control);
                return Err(error);
            }
            let business_delta = {
                let mut state = state.lock().await;
                let incarnation = state.active_sessions[&session_id].incarnation;
                let epoch = state.runtime.epoch().to_string();
                state
                    .runtime
                    .complete_control_operation(
                        &epoch,
                        &session_id,
                        incarnation,
                        &request_id,
                        RuntimeSessionOperationKind::SetConfig,
                        "config_option_update",
                        json!({
                            "sessionUpdate": "config_option_update",
                            "configOptions": config_options,
                        }),
                        response_value,
                    )
                    .map_err(runtime_state_error)?;
                if let Some(session) = state.active_sessions.get_mut(&session_id) {
                    session.config_options = config_options.clone();
                }
                release_session_operation(&mut state, &session_id, SessionOperation::Control);
                advance_session_delta(
                    &mut state,
                    &session_id,
                    incarnation,
                    json!({
                        "kind": "control_update",
                        "control": "configuration",
                        "configId": config_id,
                        "value": value,
                        "configOptions": config_options,
                    }),
                )?
            };
            sink.send(business_delta);
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
            let request_id = string_field(&command, "requestId")?.to_string();
            let permission_id = string_field(&command, "permissionId")?.to_string();
            let response: RequestPermissionResponse = serde_json::from_value(json!({
                "outcome": command.get("outcome").cloned().ok_or_else(|| {
                    Error::invalid_params().data("permission outcome is required")
                })?,
            }))?;
            let mut locked = state.lock().await;
            let pending = locked.permissions.get(&permission_id).ok_or_else(|| {
                Error::invalid_params().data("permission request is no longer pending")
            })?;
            if let Some(expected_session_id) = command.get("sessionId").and_then(Value::as_str)
                && expected_session_id != pending.session_id
            {
                return Err(Error::invalid_params()
                    .data("permission does not belong to the requested session"));
            }
            if let RequestPermissionOutcome::Selected(selected) = &response.outcome
                && !pending.option_ids.contains(selected.option_id.0.as_ref())
            {
                return Err(
                    Error::invalid_params().data("permission option was not offered by the Agent")
                );
            }
            let permission_session_id = pending.session_id.clone();
            let incarnation = live_session_incarnation(&locked, &permission_session_id)
                .ok_or_else(|| {
                    Error::invalid_params().data("permission references an inactive session")
                })?;
            let epoch = locked.runtime.epoch().to_string();
            locked
                .runtime
                .begin_permission_response(
                    &epoch,
                    &permission_session_id,
                    incarnation,
                    &permission_id,
                    request_id.clone(),
                )
                .map_err(runtime_state_error)?;
            let pending = locked
                .permissions
                .remove(&permission_id)
                .expect("pending permission checked above");
            flush_runtime(&mut locked, &sink);
            drop(locked);
            let delivered = pending.sender.send(response).is_ok();
            let business_delta = {
                let mut state = state.lock().await;
                if state
                    .runtime
                    .session(&permission_session_id)
                    .is_some_and(|session| session.incarnation == incarnation)
                {
                    let epoch = state.runtime.epoch().to_string();
                    state
                        .runtime
                        .complete_permission_response(
                            &epoch,
                            &permission_session_id,
                            incarnation,
                            &permission_id,
                            &request_id,
                        )
                        .map_err(runtime_state_error)?;
                    let delta = advance_session_delta(
                        &mut state,
                        &permission_session_id,
                        incarnation,
                        json!({
                            "kind": "interaction_remove",
                            "interactionId": permission_id,
                        }),
                    )?;
                    flush_runtime(&mut state, &sink);
                    Some(delta)
                } else {
                    None
                }
            };
            if let Some(delta) = business_delta {
                sink.send(delta);
            }
            relay_interaction_resolution(
                &sink,
                json!({
                    "type": "acp/permission_resolved",
                    "permissionId": permission_id,
                    "requestId": request_id,
                }),
                delivered,
                "permission",
            )?;
        }
        "elicitation/respond" => {
            let request_id = string_field(&command, "requestId")?.to_string();
            let elicitation_id = string_field(&command, "elicitationId")?.to_string();
            let response_value = command
                .get("response")
                .cloned()
                .ok_or_else(|| Error::invalid_params().data("elicitation response is required"))?;
            let mut locked = state.lock().await;
            let request = locked
                .elicitations
                .get(&elicitation_id)
                .map(|pending| pending.request.clone())
                .ok_or_else(|| Error::invalid_params().data("elicitation is no longer pending"))?;
            if let Some(expected_session_id) = command.get("sessionId").and_then(Value::as_str)
                && locked
                    .elicitations
                    .get(&elicitation_id)
                    .and_then(|pending| pending.session_id.as_deref())
                    != Some(expected_session_id)
            {
                return Err(Error::invalid_params()
                    .data("elicitation does not belong to the requested session"));
            }
            validate_elicitation_response_value(&request, &response_value)
                .map_err(semantic_error)?;
            let response: CreateElicitationResponse = serde_json::from_value(response_value)?;
            let pending_scope = locked
                .elicitations
                .get(&elicitation_id)
                .and_then(|pending| pending.session_id.clone());
            let runtime_scope = match &pending_scope {
                Some(session_id) => Some((
                    session_id.clone(),
                    locked
                        .active_sessions
                        .get(session_id)
                        .map(|session| session.incarnation)
                        .ok_or_else(|| {
                            Error::invalid_params()
                                .data("elicitation references an inactive session")
                        })?,
                )),
                None => None,
            };
            let accepted_url_id = locked
                .elicitations
                .get(&elicitation_id)
                .and_then(|pending| {
                    matches!(&response.action, ElicitationAction::Accept(_))
                        .then(|| pending.url_elicitation_id.clone())
                        .flatten()
                });
            let epoch = locked.runtime.epoch().to_string();
            locked
                .runtime
                .begin_elicitation_response(
                    &epoch,
                    runtime_scope
                        .as_ref()
                        .map(|(session_id, incarnation)| (session_id.as_str(), *incarnation)),
                    &elicitation_id,
                    request_id.clone(),
                )
                .map_err(runtime_state_error)?;
            let pending = locked.elicitations.remove(&elicitation_id);
            let Some(pending) = pending else {
                return Err(Error::invalid_params().data("elicitation is no longer pending"));
            };
            flush_runtime(&mut locked, &sink);
            drop(locked);
            let delivered = pending.sender.send(response.clone()).is_ok();
            let business_delta = {
                let mut state = state.lock().await;
                let scope_still_exists =
                    runtime_scope
                        .as_ref()
                        .is_none_or(|(session_id, incarnation)| {
                            state
                                .runtime
                                .session(session_id)
                                .is_some_and(|session| session.incarnation == *incarnation)
                        });
                if scope_still_exists {
                    let epoch = state.runtime.epoch().to_string();
                    state
                        .runtime
                        .complete_elicitation_response(
                            &epoch,
                            runtime_scope.as_ref().map(|(session_id, incarnation)| {
                                (session_id.as_str(), *incarnation)
                            }),
                            &elicitation_id,
                            &request_id,
                            delivered.then_some(accepted_url_id.as_deref()).flatten(),
                        )
                        .map_err(runtime_state_error)?;
                    if delivered
                        && matches!(&response.action, ElicitationAction::Accept(_))
                        && let Some(url_elicitation_id) = &pending.url_elicitation_id
                    {
                        state.url_elicitations.insert(
                            url_elicitation_id.clone(),
                            ActiveUrlElicitation {
                                session_id: pending.session_id.clone(),
                            },
                        );
                    }
                    let delta = if let Some((session_id, incarnation)) = &runtime_scope {
                        Some(advance_session_delta(
                            &mut state,
                            session_id,
                            *incarnation,
                            json!({
                                "kind": "interaction_remove",
                                "interactionId": elicitation_id,
                            }),
                        )?)
                    } else {
                        None
                    };
                    flush_runtime(&mut state, &sink);
                    delta
                } else {
                    None
                }
            };
            if let Some(delta) = business_delta {
                sink.send(delta);
            }
            relay_interaction_resolution(
                &sink,
                json!({
                    "type": "acp/elicitation_resolved",
                    "elicitationId": elicitation_id,
                    "response": response,
                    "requestId": request_id,
                }),
                delivered,
                "elicitation",
            )?;
        }
        "context/search" => {
            let request_id = string_field(&command, "requestId")?;
            let query = string_field_allow_empty(&command, "query", 256)?;
            let filesystem = filesystem.ok_or_else(|| {
                Error::method_not_found()
                    .data("workspace context is unavailable for remote transports")
            })?;
            let matches = filesystem.search_context(query).await?;
            if let Some(responder) = &business_responder {
                responder.success(json!({ "matches": matches }));
            }
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
            if let Some(responder) = &business_responder {
                responder.success(json!({ "attachment": attachment }));
            }
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
        _ => {
            return Err(
                Error::method_not_found().data(format!("unknown bridge operation: {operation}"))
            );
        }
    }
    Ok(())
}

fn retryable_reconcile_error(error: &Error) -> bool {
    if is_incoming_transport_closed(error) {
        return false;
    }
    !matches!(i32::from(error.code), -32600 | -32601 | -32602 | -32002)
}

async fn commit_completed_turn_history(
    state: &Arc<Mutex<BridgeState>>,
    sink: &EventSink,
    session_id: &str,
    incarnation: u64,
    operation_id: &str,
) -> Result<(), Error> {
    let (result, view) = {
        let mut state = state.lock().await;
        let result = session_mirror(&mut state)
            .commit_completed_turn_from_memory(session_id, incarnation, operation_id)
            .map(|_| ())
            .map_err(mirror_error);
        if let Err(error) = &result {
            session_mirror(&mut state)
                .block_turn(session_id, incarnation, operation_id, error.message.clone())
                .map_err(mirror_error)?;
        } else if let Some(validation) = state.session_updates.get_mut(session_id) {
            validation.retire_turn();
        }
        let view = session_view_value(&mut state, session_id, incarnation)?;
        (result, view)
    };
    sink.send(json!({
        "type": "bridge/session_view",
        "sessionId": session_id,
        "view": view,
    }));
    match result {
        Ok(()) => {
            sink.send(json!({
                "type": "bridge/session_sync",
                "sessionId": session_id,
                "phase": "ready",
                "source": "bridge_memory",
            }));
            Ok(())
        }
        Err(error) => {
            sink.send(json!({
                "type": "bridge/session_sync",
                "sessionId": session_id,
                "phase": "blocked",
                "message": error.message,
                "source": "bridge_memory",
            }));
            Err(error)
        }
    }
}

async fn synchronize_authoritative_history(
    connection: &ConnectionTo<Agent>,
    options: &Options,
    state: &Arc<Mutex<BridgeState>>,
    sink: &EventSink,
    cancellation: &CancellationToken,
    session_id: &str,
    incarnation: u64,
) -> Result<(), Error> {
    let cwd = {
        let state = state.lock().await;
        state
            .active_sessions
            .get(session_id)
            .filter(|session| session.incarnation == incarnation)
            .map(|session| session.cwd.clone())
            .ok_or_else(|| {
                Error::invalid_params().data("session disappeared before reconciliation")
            })?
    };
    let mut backoff = RECONCILE_INITIAL_BACKOFF;
    let mut attempt = 0_usize;
    loop {
        attempt += 1;
        let attempt_id = Uuid::new_v4().to_string();
        let sync_phase = {
            let mut state = state.lock().await;
            session_mirror(&mut state)
                .begin_load(session_id, incarnation, attempt_id.clone())
                .map_err(mirror_error)?;
            state
                .session_updates
                .entry(session_id.to_string())
                .or_default()
                .retire_turn();
            state
                .sync_control_candidates
                .insert(session_id.to_string(), SyncControlCandidate::default());
            match session_mirror(&mut state)
                .state(session_id)
                .map(|session| session.phase)
            {
                Some(MirrorPhase::Loading) => "loading",
                _ => "reconciling",
            }
        };
        sink.send(json!({
            "type": "bridge/session_sync",
            "sessionId": session_id,
            "phase": sync_phase,
            "attemptId": attempt_id,
        }));

        let result = connection
            .send_request(
                LoadSessionRequest::new(session_id.to_string(), &cwd)
                    .additional_directories(local_additional_directories(options))
                    .mcp_servers(options.mcp_servers.clone()),
            )
            .block_task()
            .await;

        let failure = {
            let mut state = state.lock().await;
            match result {
                Ok(response) => {
                    let response_value = serde_json::to_value(&response)?;
                    let validation_error = ensure_relay_size(&response, "session/load response")
                        .err()
                        .or_else(|| response_controls(&response_value).err())
                        .or_else(|| {
                            let modes =
                                response_value.get("modes").filter(|value| !value.is_null());
                            state
                                .session_updates
                                .get(session_id)
                                .and_then(|validation| {
                                    validate_replay_control_references(validation, modes).err()
                                })
                        })
                        .or_else(|| {
                            state
                                .session_updates
                                .get(session_id)
                                .and_then(|validation| validation.invalid_reason.clone())
                                .map(semantic_error)
                        });
                    if let Some(error) = validation_error {
                        state.sync_control_candidates.remove(session_id);
                        session_mirror(&mut state)
                            .fail_load(
                                session_id,
                                incarnation,
                                &attempt_id,
                                error.message.clone(),
                                false,
                            )
                            .map_err(mirror_error)?;
                        let delta = session_delta_value(
                            &state,
                            session_id,
                            incarnation,
                            json!({
                                "kind": "sync_state",
                                "phase": "blocked",
                                "message": error.message,
                            }),
                        )?;
                        Some((error, false, delta))
                    } else {
                        match session_mirror(&mut state).commit_load(
                            session_id,
                            incarnation,
                            &attempt_id,
                        ) {
                            Ok(_) => {
                                let controls = state
                                    .sync_control_candidates
                                    .remove(session_id)
                                    .unwrap_or_default()
                                    .updates;
                                let epoch = state.runtime.epoch().to_string();
                                state
                                    .runtime
                                    .synchronize_loaded_session(
                                        &epoch,
                                        session_id,
                                        incarnation,
                                        response_value.clone(),
                                        controls,
                                    )
                                    .map_err(runtime_state_error)?;
                                let (modes, config_options) = response_controls(&response_value)
                                    .expect("load controls were validated before commit");
                                if let Some(active) = state.active_sessions.get_mut(session_id) {
                                    active.modes = modes;
                                    active.config_options = config_options;
                                }
                                let view = session_view_value(&mut state, session_id, incarnation)?;
                                if let Some(validation) = state.session_updates.get_mut(session_id)
                                {
                                    validation.retire_turn();
                                }
                                sink.send(json!({
                                    "type": "bridge/session_view",
                                    "sessionId": session_id,
                                    "view": view,
                                }));
                                sink.send(json!({
                                    "type": "bridge/session_sync",
                                    "sessionId": session_id,
                                    "phase": "ready",
                                }));
                                return Ok(());
                            }
                            Err(error) => {
                                state.sync_control_candidates.remove(session_id);
                                let error = mirror_error(error);
                                session_mirror(&mut state)
                                    .fail_load(
                                        session_id,
                                        incarnation,
                                        &attempt_id,
                                        error.message.clone(),
                                        false,
                                    )
                                    .map_err(mirror_error)?;
                                let delta = session_delta_value(
                                    &state,
                                    session_id,
                                    incarnation,
                                    json!({
                                        "kind": "sync_state",
                                        "phase": "blocked",
                                        "message": error.message,
                                    }),
                                )?;
                                Some((error, false, delta))
                            }
                        }
                    }
                }
                Err(error) => {
                    state.sync_control_candidates.remove(session_id);
                    let retryable =
                        retryable_reconcile_error(&error) && attempt < RECONCILE_MAX_ATTEMPTS;
                    session_mirror(&mut state)
                        .fail_load(
                            session_id,
                            incarnation,
                            &attempt_id,
                            error.message.clone(),
                            retryable,
                        )
                        .map_err(mirror_error)?;
                    let delta = session_delta_value(
                        &state,
                        session_id,
                        incarnation,
                        json!({
                            "kind": "sync_state",
                            "phase": if retryable { "retrying" } else { "blocked" },
                            "message": error.message,
                        }),
                    )?;
                    Some((error, retryable, delta))
                }
            }
        };

        let Some((error, retryable, delta)) = failure else {
            unreachable!("successful reconciliation returns from the state transaction")
        };
        sink.send(delta);
        if !retryable {
            sink.send(json!({
                "type": "bridge/session_sync",
                "sessionId": session_id,
                "phase": "blocked",
                "message": error.message,
            }));
            return Err(error);
        }
        sink.send(json!({
            "type": "bridge/session_sync",
            "sessionId": session_id,
            "phase": "retrying",
            "message": error.message,
            "retryAfterMs": backoff.as_millis(),
        }));
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = cancellation.cancelled() => return Err(Error::request_cancelled()),
        }
        backoff = backoff.saturating_mul(2).min(RECONCILE_MAX_BACKOFF);
    }
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
    kind: RuntimeSessionOperationKind,
    state: &Arc<Mutex<BridgeState>>,
) -> Result<AttachmentReservation, Error> {
    let mut state = state.lock().await;
    if state.prompts.contains(session_id)
        || state.pending_attachments.contains(session_id)
        || state.pending_forks.contains(session_id)
        || state.pending_closes.contains(session_id)
        || state.pending_controls.contains(session_id)
        || state.pending_deletions.contains(session_id)
    {
        return Err(
            Error::invalid_request().data("another prompt or session mutation is already running")
        );
    }
    let tracked = state.active_sessions.get(session_id).map(|session| {
        let runtime = state.runtime.session(session_id);
        let idle = runtime.is_some_and(|runtime| {
            runtime.active_turn.is_none()
                && runtime.operation.is_none()
                && runtime.lifecycle == SessionLifecycle::Active
        });
        (session.cwd.clone(), session.incarnation, idle)
    });
    let reservation = match tracked {
        Some((cwd, incarnation, true)) => {
            if kind != RuntimeSessionOperationKind::Load {
                return Err(Error::invalid_request()
                    .data("an active tracked session can only be reloaded with session/load"));
            }
            AttachmentReservation::Reload { cwd, incarnation }
        }
        Some(_) => {
            return Err(Error::invalid_request()
                .data("session/load requires the tracked session to be idle"));
        }
        None => {
            let listed = state.listed_sessions.get(session_id).ok_or_else(|| {
                Error::invalid_params().data(format!(
                    "session was not returned by session/list: {session_id}"
                ))
            })?;
            if state.active_sessions.len()
                + state.pending_creations
                + state.pending_attachments.len()
                >= MAX_TRACKED_SESSIONS
            {
                return Err(semantic_error(format!(
                    "Active session limit reached ({MAX_TRACKED_SESSIONS})"
                )));
            }
            AttachmentReservation::Fresh {
                cwd: listed.cwd.clone(),
            }
        }
    };
    state.pending_attachments.insert(session_id.to_string());
    state
        .session_updates
        .entry(session_id.to_string())
        .or_default()
        .retire_turn();
    state
        .attachment_update_counts
        .insert(session_id.to_string(), 0);
    state
        .attachment_update_bytes
        .insert(session_id.to_string(), 0);
    Ok(reservation)
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
        || state.pending_attachments.contains(session_id)
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

fn finish_prompt_operation(state: &mut BridgeState, session_id: &str, lifecycle: &PromptLifecycle) {
    release_session_operation(state, session_id, SessionOperation::Prompt);
    if let Some(validation) = state.session_updates.get_mut(session_id) {
        validation.retire_turn();
    }
    // Notify only after the exclusion guard is gone. A shutdown waiter that
    // wakes on this edge must observe the terminal prompt state instead of
    // sleeping again with no later notification.
    lifecycle.prompt_finished();
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

fn begin_runtime_delete(
    state: &mut BridgeState,
    session_id: &str,
    operation_id: &str,
    close_before_delete: bool,
) -> Result<Option<u64>, Error> {
    let active_incarnation = state
        .active_sessions
        .get(session_id)
        .map(|session| session.incarnation);
    let closed_incarnation = state.runtime.session(session_id).and_then(|session| {
        (session.lifecycle == SessionLifecycle::Closed).then_some(session.incarnation)
    });
    let Some(incarnation) = active_incarnation.or(closed_incarnation) else {
        return Ok(None);
    };
    let epoch = state.runtime.epoch().to_string();
    let result = if close_before_delete || closed_incarnation.is_some() {
        state
            .runtime
            .start_delete(&epoch, session_id, incarnation, operation_id.to_string())
    } else {
        state
            .runtime
            .start_direct_delete(&epoch, session_id, incarnation, operation_id.to_string())
    };
    result.map_err(runtime_state_error)?;
    Ok(Some(incarnation))
}

fn commit_runtime_delete_close_success(
    state: &mut BridgeState,
    session_id: &str,
    incarnation: u64,
    operation_id: &str,
    sink: &EventSink,
) -> Result<(), Error> {
    let epoch = state.runtime.epoch().to_string();
    state
        .runtime
        .delete_close_succeeded(&epoch, session_id, incarnation, operation_id)
        .map_err(runtime_state_error)?;
    flush_runtime(state, sink);
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

async fn require_live_session(
    session_id: &str,
    state: &Arc<Mutex<BridgeState>>,
) -> Result<(), Error> {
    require_live_session_incarnation(session_id, state)
        .await
        .map(|_| ())
}

async fn require_live_session_incarnation(
    session_id: &str,
    state: &Arc<Mutex<BridgeState>>,
) -> Result<u64, Error> {
    let state = state.lock().await;
    live_session_incarnation(&state, session_id).ok_or_else(|| {
        Error::invalid_params().data(format!("unknown or inactive session: {session_id}"))
    })
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
    let runtime_incarnation = state
        .active_sessions
        .get(session_id)
        .map(|session| session.incarnation);
    for id in permission_ids {
        if let Some(pending) = state.permissions.remove(&id) {
            if let Some(incarnation) = runtime_incarnation {
                let epoch = state.runtime.epoch().to_string();
                if let Err(error) =
                    state
                        .runtime
                        .resolve_permission(&epoch, session_id, incarnation, &id)
                {
                    sink.acp_error(runtime_state_error(error), None, Some("permission/cancel"));
                }
            }
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
            if let Some(incarnation) = runtime_incarnation {
                let epoch = state.runtime.epoch().to_string();
                if let Err(error) = state.runtime.resolve_elicitation(
                    &epoch,
                    Some((session_id, incarnation)),
                    &id,
                    None,
                ) {
                    sink.acp_error(runtime_state_error(error), None, Some("elicitation/cancel"));
                }
            }
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
        if let Some(incarnation) = runtime_incarnation {
            let epoch = state.runtime.epoch().to_string();
            if let Err(error) = state.runtime.settle_url_flow(
                &epoch,
                Some((session_id, incarnation)),
                &elicitation_id,
                UrlFlowStatus::Cancelled,
            ) {
                sink.acp_error(runtime_state_error(error), None, Some("elicitation/abort"));
            }
        }
        sink.send(json!({
            "type": "acp/elicitation_aborted",
            "elicitationId": elicitation_id,
            "sessionId": session_id,
            "reason": reason,
        }));
    }
    flush_runtime(&mut state, sink);
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

    async fn request(
        commands: &mpsc::Sender<BridgeInput>,
        command: Value,
    ) -> Result<Value, String> {
        if command["type"] == "session/prompt" {
            let session_id = command["sessionId"].as_str().unwrap().to_string();
            let history_revision = if let Some(revision) = command["historyRevision"].as_str() {
                revision.to_string()
            } else {
                let (response, result) = oneshot::channel();
                commands
                    .send(BridgeInput::SessionViewRequest {
                        session_id: session_id.clone(),
                        response,
                    })
                    .await
                    .unwrap();
                let view = result.await.unwrap().unwrap();
                view["session"]["historyRevision"]
                    .as_str()
                    .unwrap()
                    .to_string()
            };
            let (response, result) = oneshot::channel();
            commands
                .send(BridgeInput::TurnRequest {
                    session_id,
                    history_revision,
                    client_intent_id: command["requestId"].as_str().unwrap().to_string(),
                    prompt: command["prompt"].as_array().unwrap().clone(),
                    response,
                })
                .await
                .unwrap();
            result.await.unwrap()
        } else {
            let (response, result) = oneshot::channel();
            commands
                .send(BridgeInput::BusinessRequest { command, response })
                .await
                .unwrap();
            result.await.unwrap().map_err(|error| {
                error.message + &error.data.map_or(String::new(), |data| data.to_string())
            })
        }
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
        tokio::spawn(run_with_cancellation(
            Arc::new(options),
            command_rx,
            event_tx.into(),
            CancellationToken::new(),
        ));

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

    #[tokio::test]
    async fn running_turn_rejects_bridge_owned_queue_and_accepts_after_reconcile() {
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
            "--slow-control",
        ])
        .unwrap()
        .normalized()
        .unwrap();
        let (commands, command_rx) = mpsc::channel(16);
        let (event_tx, mut events) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let bridge = tokio::spawn(run_with_cancellation(
            Arc::new(options),
            command_rx,
            event_tx.into(),
            cancellation.clone(),
        ));

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = events.recv().await.expect("bridge stopped before ready");
                let event: Value = serde_json::from_str(&event).unwrap();
                if event["type"] == "bridge/phase" && event["phase"] == "ready" {
                    break;
                }
            }
        })
        .await
        .expect("bridge did not initialize");
        request(
            &commands,
            json!({
                "type": "session/new",
                "requestId": "new",
                "cwd": cwd,
            }),
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = events
                    .recv()
                    .await
                    .expect("bridge stopped before creating a session");
                let event: Value = serde_json::from_str(&event).unwrap();
                if event["type"] == "acp/session_created" {
                    assert_eq!(event["response"]["sessionId"], "test-session");
                    break;
                }
            }
        })
        .await
        .expect("session/new did not complete");

        let first = request(
            &commands,
            json!({
                "type": "session/prompt",
                "requestId": "first",
                "sessionId": "test-session",
                "prompt": [{ "type": "text", "text": "stream-follow-flow" }],
            }),
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = events
                    .recv()
                    .await
                    .expect("bridge stopped before first prompt started");
                let event: Value = serde_json::from_str(&event).unwrap();
                if event["type"] == "acp/prompt_started" && event["requestId"] == "first" {
                    break;
                }
            }
        })
        .await
        .expect("first prompt did not start");

        let duplicate = request(
            &commands,
            json!({
                "type": "session/prompt",
                "requestId": "first",
                "sessionId": "test-session",
                "prompt": [{ "type": "text", "text": "stream-follow-flow" }],
            }),
        )
        .await
        .unwrap();
        assert_eq!(duplicate["disposition"], "duplicate");
        assert_eq!(duplicate["operationId"], first["operationId"]);
        let collision = request(
            &commands,
            json!({
                "type": "session/prompt",
                "requestId": "first",
                "sessionId": "test-session",
                "prompt": [{ "type": "text", "text": "different payload" }],
            }),
        )
        .await
        .unwrap_err();
        assert!(collision.contains("different turn"), "{collision}");

        // A queued prompt is browser-local. Submitting while the previous turn
        // is running must be rejected before it reaches the Agent.
        let rejection = request(
            &commands,
            json!({
                "type": "session/prompt",
                "requestId": "second",
                "sessionId": "test-session",
                "prompt": [{ "type": "text", "text": "message-actions-flow" }],
            }),
        )
        .await
        .unwrap_err();
        assert!(rejection.contains("not ready"), "{rejection}");

        let mut trace = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let raw = events
                    .recv()
                    .await
                    .expect("bridge stopped before rejecting the concurrent prompt");
                let event: Value = serde_json::from_str(&raw).unwrap();
                trace.push(json!({
                    "type": event.get("type"),
                    "requestId": event.get("requestId"),
                    "event": event.get("event"),
                }));
                assert_ne!(
                    event["type"], "acp/prompt_started",
                    "duplicate, conflicting or concurrent prompt reached the Agent"
                );
                if event["type"] == "acp/prompt_complete" && event["requestId"] == "first" {
                    break;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("concurrent rejection/reconcile did not finish: {trace:?}"));

        let duplicate = request(
            &commands,
            json!({
                "type": "session/prompt",
                "requestId": "first",
                "sessionId": "test-session",
                "prompt": [{ "type": "text", "text": "stream-follow-flow" }],
            }),
        )
        .await
        .unwrap();
        assert_eq!(duplicate["disposition"], "duplicate");
        assert_eq!(duplicate["operationId"], first["operationId"]);

        request(
            &commands,
            json!({
                "type": "session/prompt",
                "requestId": "after-reconcile",
                "sessionId": "test-session",
                "prompt": [{ "type": "text", "text": "message-actions-flow" }],
            }),
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event: Value = serde_json::from_str(
                    &events
                        .recv()
                        .await
                        .expect("bridge stopped before follow-up"),
                )
                .unwrap();
                if event["type"] == "acp/prompt_complete" && event["requestId"] == "after-reconcile"
                {
                    break;
                }
            }
        })
        .await
        .expect("browser-resubmitted prompt did not complete after reconciliation");

        // Business requests also report an in-flight request collision through
        // their REST responder; a private subscriber event cannot resolve HTTP.
        let mut replies = Vec::new();
        for _ in 0..2 {
            let (response, result) = oneshot::channel();
            commands
                .send(BridgeInput::BusinessRequest {
                    command: json!({
                        "type": "session/set_mode",
                        "requestId": "same-control-request",
                        "sessionId": "test-session",
                        "modeId": "plan",
                    }),
                    response,
                })
                .await
                .unwrap();
            replies.push(result);
        }
        let results = tokio::time::timeout(Duration::from_secs(5), async {
            let mut results = Vec::new();
            for reply in replies {
                results.push(reply.await.unwrap());
            }
            results
        })
        .await
        .expect("a duplicate business request lost its response");
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let collision = results.into_iter().find_map(Result::err).unwrap();
        assert!(
            collision
                .data
                .unwrap()
                .to_string()
                .contains("already in flight")
        );

        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(5), bridge)
            .await
            .expect("bridge did not stop")
            .unwrap();
    }

    #[tokio::test]
    async fn terminal_turn_with_load_support_commits_observed_history_without_reloading() {
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
            "--fail-load-once",
        ])
        .unwrap()
        .normalized()
        .unwrap();
        let (commands, command_rx) = mpsc::channel(16);
        let (event_tx, mut events) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let bridge = tokio::spawn(run_with_cancellation(
            Arc::new(options),
            command_rx,
            event_tx.into(),
            cancellation.clone(),
        ));

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event: Value = serde_json::from_str(&events.recv().await.unwrap()).unwrap();
                if event["type"] == "bridge/phase" && event["phase"] == "ready" {
                    break;
                }
            }
        })
        .await
        .expect("bridge did not initialize");
        request(
            &commands,
            json!({
                "type": "session/new",
                "requestId": "new",
                "cwd": cwd,
            }),
        )
        .await
        .unwrap();
        let initial_revision = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event: Value = serde_json::from_str(&events.recv().await.unwrap()).unwrap();
                if event["type"] == "bridge/session_view" && event["sessionId"] == "test-session" {
                    break event["view"]["session"]["historyRevision"]
                        .as_str()
                        .unwrap()
                        .to_string();
                }
            }
        })
        .await
        .expect("new session view was not published");

        request(
            &commands,
            json!({
                "type": "session/prompt",
                "requestId": "turn",
                "clientIntentId": "stable-turn-intent",
                "historyRevision": initial_revision.clone(),
                "sessionId": "test-session",
                "prompt": [{ "type": "text", "text": "message-actions-flow" }],
            }),
        )
        .await
        .unwrap();

        let mut prompt_starts = 0;
        let mut saw_post_turn_load = false;
        let successor = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event: Value = serde_json::from_str(&events.recv().await.unwrap()).unwrap();
                if event["type"] == "acp/prompt_started" && event["requestId"] == "turn" {
                    prompt_starts += 1;
                }
                if event["type"] == "bridge/session_sync"
                    && event["sessionId"] == "test-session"
                    && matches!(event["phase"].as_str(), Some("loading" | "retrying"))
                {
                    saw_post_turn_load = true;
                }
                if event["type"] == "bridge/session_view"
                    && event["sessionId"] == "test-session"
                    && event["view"]["session"]["phase"] == "ready"
                    && event["view"]["baseline"]["updates"]
                        .as_array()
                        .is_some_and(|updates| !updates.is_empty())
                {
                    break event["view"]["session"]["historyRevision"]
                        .as_str()
                        .unwrap()
                        .to_string();
                }
            }
        })
        .await
        .expect("in-memory commit did not publish a successor view");

        assert!(
            !saw_post_turn_load,
            "a completed turn must stay in memory instead of issuing session/load"
        );
        assert_eq!(prompt_starts, 1, "history commit redispatched the prompt");
        assert_ne!(successor, initial_revision);

        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(5), bridge)
            .await
            .expect("bridge did not stop")
            .unwrap();
    }

    #[tokio::test]
    async fn agent_without_load_keeps_completed_turns_in_the_bridge_memory_baseline() {
        async fn next_event(events: &mut mpsc::UnboundedReceiver<String>) -> Value {
            let raw = tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .expect("timed out waiting for bridge event")
                .expect("bridge stopped unexpectedly");
            serde_json::from_str(&raw).expect("bridge event must be JSON")
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
            "--minimal",
        ])
        .unwrap()
        .normalized()
        .unwrap();
        let (commands, command_rx) = mpsc::channel(16);
        let (event_tx, mut events) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let bridge = tokio::spawn(run_with_cancellation(
            Arc::new(options),
            command_rx,
            event_tx.into(),
            cancellation.clone(),
        ));

        loop {
            let event = next_event(&mut events).await;
            assert_ne!(
                event["type"], "bridge/error",
                "unexpected startup error: {event}"
            );
            if event["type"] == "bridge/phase" && event["phase"] == "ready" {
                break;
            }
        }
        request(
            &commands,
            json!({
                "type": "session/new",
                "requestId": "new",
                "cwd": cwd,
            }),
        )
        .await
        .unwrap();
        let initial_revision = loop {
            let event = next_event(&mut events).await;
            if event["type"] == "bridge/session_view"
                && event["sessionId"] == "test-session"
                && event["view"]["session"]["phase"] == "ready"
            {
                break event["view"]["session"]["historyRevision"]
                    .as_str()
                    .unwrap()
                    .to_string();
            }
        };

        request(
            &commands,
            json!({
                "type": "session/prompt",
                "requestId": "turn",
                "clientIntentId": "no-load-turn",
                "historyRevision": initial_revision,
                "sessionId": "test-session",
                "prompt": [{ "type": "text", "text": "message-actions-flow" }],
            }),
        )
        .await
        .unwrap();

        let completed = loop {
            let event = next_event(&mut events).await;
            if event["type"] == "bridge/session_sync" {
                assert_ne!(
                    event["phase"], "loading",
                    "an Agent without loadSession must never receive session/load"
                );
            }
            if event["type"] == "bridge/session_view"
                && event["sessionId"] == "test-session"
                && event["view"]["session"]["phase"] == "ready"
                && event["view"]["baseline"]["updates"]
                    .as_array()
                    .is_some_and(|updates| !updates.is_empty())
            {
                break event["view"].clone();
            }
        };
        assert_ne!(
            completed["session"]["historyRevision"],
            Value::String(initial_revision)
        );
        let updates = completed["baseline"]["updates"].as_array().unwrap();
        assert!(updates.iter().any(|update| {
            update["sessionUpdate"] == "user_message_chunk"
                && update["content"]["text"] == "message-actions-flow"
        }));
        assert!(updates.iter().any(|update| {
            update["sessionUpdate"] == "agent_message_chunk"
                && update["content"]["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("Context menu response"))
        }));

        let (response_tx, response_rx) = oneshot::channel();
        commands
            .send(BridgeInput::SessionViewRequest {
                session_id: "test-session".to_string(),
                response: response_tx,
            })
            .await
            .unwrap();
        let rebuilt = response_rx.await.unwrap().unwrap();
        assert_eq!(rebuilt["baseline"], completed["baseline"]);
        assert_eq!(rebuilt["session"]["phase"], "ready");

        let successor_revision = rebuilt["session"]["historyRevision"]
            .as_str()
            .unwrap()
            .to_string();
        request(
            &commands,
            json!({
                "type": "session/prompt",
                "requestId": "second-turn",
                "clientIntentId": "no-load-second-turn",
                "historyRevision": successor_revision,
                "sessionId": "test-session",
                "prompt": [{ "type": "text", "text": "attachment-input-flow" }],
            }),
        )
        .await
        .unwrap();
        let second = loop {
            let event = next_event(&mut events).await;
            if event["type"] == "bridge/session_view"
                && event["sessionId"] == "test-session"
                && event["view"]["session"]["phase"] == "ready"
                && event["view"]["baseline"]["updates"]
                    .as_array()
                    .is_some_and(|updates| {
                        updates
                            .iter()
                            .filter(|update| update["sessionUpdate"] == "user_message_chunk")
                            .count()
                            == 2
                    })
            {
                break event["view"].clone();
            }
        };
        let updates = second["baseline"]["updates"].as_array().unwrap();
        assert!(updates.iter().any(|update| {
            update["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("Context menu response"))
        }));
        assert!(updates.iter().any(|update| {
            update["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("Received prompt blocks"))
        }));

        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(5), bridge)
            .await
            .expect("bridge did not stop")
            .unwrap();
    }

    #[tokio::test]
    async fn rejected_same_session_load_rolls_back_and_following_prompt_runs() {
        async fn next_event(events: &mut mpsc::UnboundedReceiver<String>) -> Value {
            let raw = tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .expect("timed out waiting for bridge event")
                .expect("bridge stopped unexpectedly");
            serde_json::from_str(&raw).expect("bridge event must be JSON")
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
            "--fail-load-once",
        ])
        .unwrap()
        .normalized()
        .unwrap();
        let (commands, command_rx) = mpsc::channel(16);
        let (event_tx, mut events) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let bridge = tokio::spawn(run_with_cancellation(
            Arc::new(options),
            command_rx,
            event_tx.into(),
            cancellation.clone(),
        ));

        loop {
            let event = next_event(&mut events).await;
            if event["type"] == "bridge/phase" && event["phase"] == "ready" {
                break;
            }
        }
        request(
            &commands,
            json!({
                "type": "session/new",
                "requestId": "new",
                "cwd": cwd,
            }),
        )
        .await
        .unwrap();
        loop {
            if next_event(&mut events).await["type"] == "acp/session_created" {
                break;
            }
        }

        let rejection = request(
            &commands,
            json!({
                "type": "session/load",
                "requestId": "reload",
                "sessionId": "test-session",
            }),
        )
        .await
        .unwrap_err();
        assert!(rejection.contains("Synthetic load failure"), "{rejection}");
        while let Ok(raw) = events.try_recv() {
            let event: Value = serde_json::from_str(&raw).unwrap();
            assert_ne!(
                event["type"], "acp/session_attached",
                "a rejected load must not commit replacement state"
            );
        }

        request(
            &commands,
            json!({
                "type": "session/prompt",
                "requestId": "after-rejected-load",
                "sessionId": "test-session",
                "prompt": [{ "type": "text", "text": "message-actions-flow" }],
            }),
        )
        .await
        .unwrap();
        loop {
            let event = next_event(&mut events).await;
            if event["type"] == "acp/prompt_complete" && event["requestId"] == "after-rejected-load"
            {
                break;
            }
        }

        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(5), bridge)
            .await
            .expect("bridge did not stop")
            .unwrap();
    }

    #[test]
    fn advertises_agent_interaction_capabilities_without_editor_features() {
        // Selected from Zed's `client_capabilities_include_elicitation_without_acp_beta`
        // contract: form and URL elicitation are stable client behavior.
        let local = client_capabilities(&options(&["attyd", "--", "fixture-agent"]));
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

    #[test]
    fn failed_interaction_delivery_is_never_reported_as_resolved() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        let success = json!({
            "type": "acp/elicitation_resolved",
            "elicitationId": "elicitation",
            "requestId": "response",
        });

        let error =
            relay_interaction_resolution(&sink, success.clone(), false, "elicitation").unwrap_err();
        assert_eq!(error.code, ErrorCode::RequestCancelled);
        assert!(
            rx.try_recv().is_err(),
            "an Agent-side cancellation must not look like a successful user response"
        );

        relay_interaction_resolution(&sink, success.clone(), true, "elicitation").unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&rx.try_recv().unwrap()).unwrap(),
            success
        );
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
        let sink = EventSink { tx: tx.into() };
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
                incarnation: 0,
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
                incarnation: 0,
            },
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
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
    async fn canonical_runtime_routes_conversation_and_control_updates_separately() {
        let mut bridge_state = BridgeState::default();
        let epoch = bridge_state.runtime.epoch().to_string();
        let incarnation = bridge_state
            .runtime
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        bridge_state
            .runtime
            .start_prompt(
                &epoch,
                "session",
                incarnation,
                "prompt",
                vec![json!({ "type": "text", "text": "hello" })],
            )
            .unwrap();
        bridge_state.active_sessions.insert(
            "session".to_string(),
            ActiveSession {
                cwd: PathBuf::from("/workspace"),
                modes: Some(json!({ "currentModeId": "build" })),
                config_options: Value::Array(Vec::new()),
                incarnation,
            },
        );
        bridge_state.published_runtime_seq = bridge_state.runtime.seq();
        let state = Arc::new(Mutex::new(bridge_state));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        let answer: SessionNotification = serde_json::from_value(json!({
            "sessionId": "session",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": "answer",
                "content": { "type": "text", "text": "world" }
            }
        }))
        .unwrap();
        let mode: SessionNotification = serde_json::from_value(json!({
            "sessionId": "session",
            "update": {
                "sessionUpdate": "current_mode_update",
                "currentModeId": "plan"
            }
        }))
        .unwrap();

        handle_session_update(answer, &state, &sink, None).await;
        handle_session_update(mode, &state, &sink, None).await;

        let state = state.lock().await;
        let runtime = state.runtime.session("session").unwrap();
        assert_eq!(runtime.active_turn.as_ref().unwrap().updates.len(), 1);
        assert_eq!(
            runtime.control_state["current_mode_update"]["currentModeId"],
            "plan",
        );
        assert_eq!(
            state.active_sessions["session"].modes.as_ref().unwrap()["currentModeId"],
            "plan",
        );
        drop(state);
        let types = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|event| serde_json::from_str::<Value>(&event).unwrap()["type"].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            vec![
                json!("bridge/internal_runtime_delta"),
                json!("acp/session_update"),
                json!("bridge/internal_runtime_delta"),
                json!("acp/session_update"),
            ],
        );
    }

    #[tokio::test]
    async fn late_turn_update_without_active_turn_is_quarantined_from_all_business_streams() {
        let mut bridge_state = BridgeState::default();
        let epoch = bridge_state.runtime.epoch().to_string();
        let incarnation = bridge_state
            .runtime
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        bridge_state
            .runtime
            .start_prompt(&epoch, "session", incarnation, "finished", Vec::new())
            .unwrap();
        bridge_state
            .runtime
            .complete_prompt(
                &epoch,
                "session",
                incarnation,
                "finished",
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        bridge_state.active_sessions.insert(
            "session".to_string(),
            ActiveSession {
                cwd: PathBuf::from("/workspace"),
                modes: None,
                config_options: Value::Array(Vec::new()),
                incarnation,
            },
        );
        bridge_state.published_runtime_seq = bridge_state.runtime.seq();
        let before = bridge_state.runtime.snapshot();
        let state = Arc::new(Mutex::new(bridge_state));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        let late: SessionNotification = serde_json::from_value(json!({
            "sessionId": "session",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": "late-answer",
                "content": { "type": "text", "text": "too late" }
            }
        }))
        .unwrap();

        handle_session_update(late, &state, &sink, None).await;

        let state = state.lock().await;
        assert_eq!(state.runtime.snapshot(), before);
        assert!(
            !state.session_updates.contains_key("session"),
            "a quarantined update must not poison validation state for the next turn"
        );
        drop(state);
        assert!(
            rx.try_recv().is_err(),
            "a late conversation update must not leak through the legacy business stream"
        );
    }

    #[tokio::test]
    async fn attachment_updates_stay_candidate_only_until_agent_response() {
        let mut bridge_state = BridgeState::default();
        let epoch = bridge_state.runtime.epoch().to_string();
        let incarnation = bridge_state
            .runtime
            .start_attachment(
                &epoch,
                "session",
                "/workspace",
                "load",
                RuntimeSessionOperationKind::Load,
            )
            .unwrap();
        bridge_state
            .pending_attachments
            .insert("session".to_string());
        bridge_state
            .attachment_update_counts
            .insert("session".to_string(), 0);
        bridge_state
            .attachment_update_bytes
            .insert("session".to_string(), 0);
        let state = Arc::new(Mutex::new(bridge_state));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        let replay: SessionNotification = serde_json::from_value(json!({
            "sessionId": "session",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": "loaded-answer",
                "content": { "type": "text", "text": "candidate" }
            }
        }))
        .unwrap();

        handle_session_update(replay, &state, &sink, None).await;

        let mut state = state.lock().await;
        assert_eq!(
            live_session_incarnation(&state, "session"),
            Some(incarnation)
        );
        assert!(apply_terminal_snapshot(
            &mut state,
            TerminalSnapshot {
                incarnation,
                value: json!({
                    "sessionId": "session",
                    "terminalId": "pre-response-terminal",
                    "released": false,
                }),
            },
        ));
        assert!(
            !serde_json::to_string(&state.runtime.snapshot())
                .unwrap()
                .contains("candidate")
        );
        state
            .runtime
            .complete_attachment(
                &epoch,
                "session",
                incarnation,
                "load",
                RuntimeSessionOperationKind::Load,
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        assert!(
            !serde_json::to_string(&state.runtime.snapshot())
                .unwrap()
                .contains("candidate"),
            "successful load replay is transient browser delivery, not bridge history",
        );
        assert!(
            state
                .runtime
                .session("session")
                .unwrap()
                .terminals
                .contains_key("pre-response-terminal")
        );
        drop(state);
        let event = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|event| serde_json::from_str::<Value>(&event).unwrap())
            .find(|event| event["type"] == "acp/session_update")
            .expect("legacy update was not relayed");
        assert_eq!(event["notification"]["sessionId"], "session");
    }

    #[tokio::test]
    async fn invalid_same_session_reload_update_is_private_and_poisoned_for_rollback() {
        let mut bridge_state = BridgeState::default();
        let epoch = bridge_state.runtime.epoch().to_string();
        let incarnation = bridge_state
            .runtime
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        bridge_state
            .runtime
            .start_reload(
                &epoch,
                "session",
                incarnation,
                "load",
                RuntimeSessionOperationKind::Load,
            )
            .unwrap();
        bridge_state.active_sessions.insert(
            "session".to_string(),
            ActiveSession {
                cwd: PathBuf::from("/workspace"),
                modes: None,
                config_options: Value::Array(Vec::new()),
                incarnation,
            },
        );
        bridge_state
            .pending_attachments
            .insert("session".to_string());
        bridge_state
            .attachment_subscribers
            .insert("session".to_string(), 42);
        bridge_state
            .attachment_update_counts
            .insert("session".to_string(), 0);
        bridge_state
            .attachment_update_bytes
            .insert("session".to_string(), 0);
        let state = Arc::new(Mutex::new(bridge_state));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        let invalid: SessionNotification = serde_json::from_value(json!({
            "sessionId": "session",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": "invalid-loaded-answer",
                "content": { "type": "image", "data": "AA==", "mimeType": "text/html" }
            }
        }))
        .unwrap();

        handle_session_update(invalid, &state, &sink, None).await;

        let state = state.lock().await;
        assert!(
            state.session_updates["session"].invalid_reason.is_some(),
            "invalid replacement input must force the later load response to roll back"
        );
        assert_eq!(
            state.runtime.session("session").unwrap().incarnation,
            incarnation
        );
        drop(state);
        let envelopes = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|event| serde_json::from_str::<Value>(&event).unwrap())
            .collect::<Vec<_>>();
        let envelope = envelopes
            .iter()
            .find(|event| event["type"] == "bridge/internal_direct")
            .expect("invalid reload update error was not routed to its requester");
        assert_eq!(envelope["type"], "bridge/internal_direct");
        assert_eq!(envelope["subscriberId"], 42);
        assert_eq!(envelope["event"]["type"], "bridge/error");
        assert!(
            envelopes.iter().all(|event| {
                !matches!(
                    event["type"].as_str(),
                    Some("bridge/error" | "acp/session_update")
                )
            }),
            "invalid reload update escaped the private delivery path"
        );
    }

    #[tokio::test]
    async fn invalid_early_updates_prevent_session_commit_and_clear_cleanly() {
        let state = Arc::new(Mutex::new(BridgeState {
            pending_creations: 1,
            ..BridgeState::default()
        }));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
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
        let sink = EventSink { tx: tx.into() };
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
        let sink = EventSink { tx: tx.into() };
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
        let local = options(&[
            "attyd",
            "--cwd",
            "/tmp/local-workspace",
            "--",
            "fixture-agent",
        ]);
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
            reserve_attachment("saved", RuntimeSessionOperationKind::Load, &state)
                .await
                .unwrap(),
            AttachmentReservation::Fresh {
                cwd: PathBuf::from("/agent/workspace")
            }
        );
        assert!(
            reserve_attachment("saved", RuntimeSessionOperationKind::Load, &state)
                .await
                .is_err()
        );

        let mut locked = state.lock().await;
        locked.pending_attachments.clear();
        locked.active_sessions.insert(
            "saved".to_string(),
            ActiveSession {
                cwd: PathBuf::from("/agent/workspace"),
                modes: None,
                config_options: Value::Array(Vec::new()),
                incarnation: 0,
            },
        );
        drop(locked);
        assert!(
            reserve_attachment("saved", RuntimeSessionOperationKind::Load, &state)
                .await
                .is_err()
        );
        assert!(
            reserve_attachment("unknown", RuntimeSessionOperationKind::Load, &state)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn reserves_an_idle_tracked_session_for_best_effort_authoritative_reload() {
        let mut bridge = BridgeState::default();
        let epoch = bridge.runtime.epoch().to_string();
        let incarnation = bridge
            .runtime
            .open_new(
                &epoch,
                "session",
                "/agent/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        bridge.active_sessions.insert(
            "session".to_string(),
            ActiveSession {
                cwd: PathBuf::from("/agent/workspace"),
                modes: None,
                config_options: Value::Array(Vec::new()),
                incarnation,
            },
        );
        let state = Arc::new(Mutex::new(bridge));

        let reservation = reserve_attachment("session", RuntimeSessionOperationKind::Load, &state)
            .await
            .expect("an idle tracked session must reach the Agent's session/load implementation");

        assert_eq!(
            reservation,
            AttachmentReservation::Reload {
                cwd: PathBuf::from("/agent/workspace"),
                incarnation,
            }
        );
        assert!(state.lock().await.pending_attachments.contains("session"));

        {
            let mut state = state.lock().await;
            state.pending_attachments.clear();
            // Reconciliation deliberately keeps the prompt exclusion guard
            // after RuntimeState has retired its active turn. A second load
            // must still be rejected during that window.
            state.prompts.insert("session".to_string());
        }
        assert!(
            reserve_attachment("session", RuntimeSessionOperationKind::Load, &state)
                .await
                .is_err()
        );
        assert!(!state.lock().await.pending_attachments.contains("session"));
    }

    #[tokio::test]
    async fn only_the_owner_can_settle_materialization_waiters() {
        let state = Arc::new(Mutex::new(BridgeState::default()));
        let (sender, mut receiver) = oneshot::channel();
        {
            let mut state = state.lock().await;
            state
                .materializations_in_flight
                .insert("session".to_string(), "owner".to_string());
            state
                .session_view_waiters
                .insert("session".to_string(), vec![sender]);
        }

        let failed = Err(Error::internal_error().data("unrelated load"));
        resolve_session_view_waiters(&state, "session", "not-owner", &failed).await;
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            state
                .lock()
                .await
                .materializations_in_flight
                .get("session")
                .map(String::as_str),
            Some("owner")
        );

        resolve_session_view_waiters(&state, "session", "owner", &failed).await;
        assert!(receiver.await.unwrap().is_err());
        assert!(
            !state
                .lock()
                .await
                .materializations_in_flight
                .contains_key("session")
        );
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
                incarnation: 0,
            },
        );
        state.active_sessions.insert(
            "other".to_string(),
            ActiveSession {
                cwd: PathBuf::from("/agent/other-workspace"),
                modes: None,
                config_options: Value::Array(Vec::new()),
                incarnation: 0,
            },
        );
        reserve_session_operation(&mut state, "active", SessionOperation::Prompt).unwrap();
        reserve_session_operation(&mut state, "other", SessionOperation::Prompt).unwrap();
        assert_eq!(state.prompts.len(), 2);
        assert!(reserve_session_operation(&mut state, "active", SessionOperation::Prompt).is_err());
        assert!(reserve_session_operation(&mut state, "active", SessionOperation::Fork).is_err());
        assert!(reserve_session_operation(&mut state, "active", SessionOperation::Close).is_err());
        assert!(
            reserve_session_operation(&mut state, "active", SessionOperation::Control).is_err()
        );
        release_session_operation(&mut state, "other", SessionOperation::Prompt);
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
    fn rejected_session_cleanup_never_closes_a_known_or_mutating_session_id() {
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
                incarnation: 0,
            },
        );
        assert!(!can_close_rejected_chat_session(&state, "source-session"));

        state.listed_sessions.insert(
            "listed-session".to_string(),
            session_info("listed-session", "/agent/workspace"),
        );
        assert!(!can_close_rejected_chat_session(&state, "listed-session"));

        state
            .pending_deletions
            .insert("deleting-session".to_string());
        assert!(!can_close_rejected_chat_session(&state, "deleting-session"));

        state.agent_capabilities = Some(AgentCapabilities::new());
        assert!(!can_close_rejected_chat_session(&state, "new-allocation"));
    }

    #[tokio::test]
    async fn prompt_completion_releases_exclusion_before_notifying_shutdown() {
        let state = Arc::new(Mutex::new(BridgeState::default()));
        {
            let mut state = state.lock().await;
            state.active_sessions.insert(
                "session".to_string(),
                ActiveSession {
                    cwd: PathBuf::from("/workspace"),
                    modes: None,
                    config_options: Value::Array(Vec::new()),
                    incarnation: 1,
                },
            );
            reserve_session_operation(&mut state, "session", SessionOperation::Prompt).unwrap();
        }
        let lifecycle = Arc::new(PromptLifecycle::default());
        let waiter = {
            let state = state.clone();
            let lifecycle = lifecycle.clone();
            tokio::spawn(async move {
                loop {
                    let changed = lifecycle.changed.notified();
                    if !state.lock().await.prompts.contains("session") {
                        return;
                    }
                    changed.await;
                }
            })
        };
        tokio::task::yield_now().await;
        {
            let mut state = state.lock().await;
            finish_prompt_operation(&mut state, "session", &lifecycle);
        }
        tokio::time::timeout(Duration::from_millis(100), waiter)
            .await
            .expect("shutdown waiter missed the prompt terminal edge")
            .unwrap();
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

        let configured_roots = options(&[
            "attyd",
            "--add-dir",
            "/tmp/additional",
            "--",
            "fixture-agent",
        ]);
        assert!(
            validate_configured_capabilities(&AgentCapabilities::new(), &configured_roots).is_err()
        );
        let supports_roots = AgentCapabilities::new().session_capabilities(
            SessionCapabilities::new()
                .additional_directories(SessionAdditionalDirectoriesCapabilities::new()),
        );
        assert!(validate_configured_capabilities(&supports_roots, &configured_roots).is_ok());

        let mut configured_mcp = options(&["attyd", "--", "fixture-agent"]);
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
    async fn prompt_terminal_effects_cancel_owned_responders_in_the_adapter() {
        let (permission_sender, permission_receiver) = oneshot::channel();
        let (elicitation_sender, elicitation_receiver) = oneshot::channel();
        let state = Arc::new(Mutex::new(BridgeState::default()));
        {
            let mut state = state.lock().await;
            let epoch = state.runtime.epoch().to_string();
            let incarnation = state
                .runtime
                .open_new(
                    &epoch,
                    "session",
                    "/workspace",
                    json!({ "sessionId": "session" }),
                )
                .unwrap();
            state
                .runtime
                .start_prompt(&epoch, "session", incarnation, "prompt", Vec::new())
                .unwrap();
            state
                .runtime
                .upsert_permission(
                    &epoch,
                    "session",
                    incarnation,
                    "permission",
                    json!({ "sessionId": "session" }),
                )
                .unwrap();
            state
                .runtime
                .upsert_elicitation(
                    &epoch,
                    Some(("session", incarnation)),
                    "elicitation",
                    json!({ "sessionId": "session", "mode": "form" }),
                )
                .unwrap();
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
                "elicitation".to_string(),
                PendingElicitation {
                    session_id: Some("session".to_string()),
                    url_elicitation_id: None,
                    request: json!({ "sessionId": "session", "mode": "form" }),
                    sender: elicitation_sender,
                },
            );
            state
                .runtime
                .complete_prompt(
                    &epoch,
                    "session",
                    incarnation,
                    "prompt",
                    json!({ "stopReason": "end_turn" }),
                )
                .unwrap();
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        apply_runtime_effects(&state, &EventSink { tx: tx.into() }, None).await;

        assert!(matches!(
            permission_receiver.await.unwrap().outcome,
            RequestPermissionOutcome::Cancelled
        ));
        assert!(matches!(
            elicitation_receiver.await.unwrap().action,
            ElicitationAction::Cancel
        ));
        let state = state.lock().await;
        assert!(state.permissions.is_empty());
        assert!(state.elicitations.is_empty());
        drop(state);
        assert_eq!(
            std::iter::from_fn(|| rx.try_recv().ok()).count(),
            2,
            "each responder gets one public terminal event"
        );
    }

    #[test]
    fn retrying_a_partially_failed_delete_reuses_the_closed_runtime_incarnation() {
        let mut state = BridgeState::default();
        let epoch = state.runtime.epoch().to_string();
        let incarnation = state
            .runtime
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        state
            .runtime
            .start_delete(&epoch, "session", incarnation, "first-delete")
            .unwrap();
        state
            .runtime
            .delete_close_succeeded(&epoch, "session", incarnation, "first-delete")
            .unwrap();
        state
            .runtime
            .fail_operation(
                &epoch,
                "session",
                incarnation,
                "first-delete",
                RuntimeSessionOperationKind::Delete,
                json!("delete failed"),
            )
            .unwrap();

        assert_eq!(
            begin_runtime_delete(&mut state, "session", "retry-delete", false).unwrap(),
            Some(incarnation)
        );
        state
            .runtime
            .complete_delete(&epoch, "session", incarnation, "retry-delete")
            .unwrap();
        assert!(state.runtime.session("session").is_none());
    }

    #[test]
    fn close_before_delete_publishes_deleting_before_the_delete_rpc_wait() {
        let mut state = BridgeState::default();
        let epoch = state.runtime.epoch().to_string();
        let incarnation = state
            .runtime
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        state
            .runtime
            .start_delete(&epoch, "session", incarnation, "delete")
            .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        flush_runtime(&mut state, &sink);
        while rx.try_recv().is_ok() {}

        commit_runtime_delete_close_success(&mut state, "session", incarnation, "delete", &sink)
            .unwrap();

        assert_eq!(state.published_runtime_seq, state.runtime.seq());
        let event = serde_json::from_str::<Value>(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(event["type"], "bridge/internal_runtime_delta");
        assert_eq!(event["value"]["change"]["session"]["lifecycle"], "deleting");
    }

    #[tokio::test]
    async fn close_cleanup_tombstone_blocks_same_id_reattachment() {
        let state = Arc::new(Mutex::new(BridgeState::default()));
        {
            let mut state = state.lock().await;
            state.pending_closes.insert("session".to_string());
            state.listed_sessions.insert(
                "session".to_string(),
                serde_json::from_value(json!({
                    "sessionId": "session",
                    "cwd": "/workspace",
                }))
                .unwrap(),
            );
        }

        let error = reserve_attachment("session", RuntimeSessionOperationKind::Load, &state)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("another prompt or session mutation")
        );
        assert!(!state.lock().await.pending_attachments.contains("session"));
    }

    #[test]
    fn terminal_business_deltas_and_replacement_view_share_the_same_output() {
        let mut state = BridgeState::default();
        let epoch = state.runtime.epoch().to_string();
        let incarnation = state
            .runtime
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        state.active_sessions.insert(
            "session".to_string(),
            ActiveSession {
                cwd: PathBuf::from("/workspace"),
                modes: None,
                config_options: json!([]),
                incarnation,
            },
        );
        let mirror = session_mirror(&mut state);
        mirror.register_new("session", incarnation);
        mirror.begin_load("session", incarnation, "load").unwrap();
        mirror
            .append_load_update(
                "session",
                incarnation,
                "load",
                json!({
                    "sessionUpdate": "tool_call", "toolCallId": "tool", "title": "run",
                    "content": [{ "type": "terminal", "terminalId": "terminal" }],
                    "rawOutput": { "agent": "original" },
                }),
            )
            .unwrap();
        mirror.commit_load("session", incarnation, "load").unwrap();
        let revision = mirror.state("session").unwrap().view_revision;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        for (output, released) in [("hello", false), (" world", false), ("", true)] {
            publish_terminal_snapshot(
                &mut state,
                &sink,
                TerminalSnapshot {
                    incarnation,
                    value: json!({
                        "sessionId": "session", "terminalId": "terminal", "output": output,
                        "outputAppend": true, "truncated": false, "released": released,
                        "exitStatus": if released { json!({ "exitCode": 0 }) } else { Value::Null },
                    }),
                },
            );
        }
        let deltas = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|event| serde_json::from_str::<Value>(&event).unwrap())
            .filter(|event| event["type"] == "bridge/session_delta")
            .collect::<Vec<_>>();
        assert_eq!(deltas.len(), 3);
        for (index, delta) in deltas.iter().enumerate() {
            assert_eq!(delta["fromRevision"], revision + index as u64);
            assert_eq!(delta["viewRevision"], revision + index as u64 + 1);
            assert_eq!(delta["change"]["kind"], "terminal_update");
        }
        assert!(
            state
                .runtime
                .session("session")
                .unwrap()
                .terminals
                .is_empty()
        );
        let view = session_view_value(&mut state, "session", incarnation).unwrap();
        assert_eq!(view["terminals"]["terminal"]["output"], "hello world");
        assert_eq!(view["terminals"]["terminal"]["exitStatus"]["exitCode"], 0);
        assert_eq!(
            view["baseline"]["updates"][0]["rawOutput"],
            json!({ "agent": "original" })
        );
    }

    #[test]
    fn late_terminal_snapshot_from_an_old_incarnation_is_ignored() {
        let mut state = BridgeState::default();
        let epoch = state.runtime.epoch().to_string();
        let first = state
            .runtime
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        state.active_sessions.insert(
            "session".to_string(),
            ActiveSession {
                cwd: PathBuf::from("/workspace"),
                modes: None,
                config_options: json!([]),
                incarnation: first,
            },
        );
        assert!(apply_terminal_snapshot(
            &mut state,
            TerminalSnapshot {
                incarnation: first,
                value: json!({
                    "sessionId": "session",
                    "terminalId": "old-terminal",
                    "released": false,
                }),
            },
        ));
        state
            .runtime
            .start_operation(
                &epoch,
                "session",
                first,
                "close",
                RuntimeSessionOperationKind::Close,
                "closing",
            )
            .unwrap();
        state
            .runtime
            .close_session(&epoch, "session", first, "close")
            .unwrap();
        state.active_sessions.remove("session");
        let second = state
            .runtime
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        state.active_sessions.insert(
            "session".to_string(),
            ActiveSession {
                cwd: PathBuf::from("/workspace"),
                modes: None,
                config_options: json!([]),
                incarnation: second,
            },
        );

        assert!(!apply_terminal_snapshot(
            &mut state,
            TerminalSnapshot {
                incarnation: first,
                value: json!({
                    "sessionId": "session",
                    "terminalId": "old-terminal",
                    "released": true,
                }),
            },
        ));
        assert!(
            state
                .runtime
                .session("session")
                .unwrap()
                .terminals
                .is_empty()
        );
    }

    #[test]
    fn transport_eof_is_uncertain_but_an_agent_error_is_definite_failure() {
        let mut uncertain = BridgeState::default();
        let epoch = uncertain.runtime.epoch().to_string();
        let incarnation = uncertain
            .runtime
            .open_new(
                &epoch,
                "uncertain",
                "/workspace",
                json!({ "sessionId": "uncertain" }),
            )
            .unwrap();
        uncertain
            .runtime
            .start_prompt(&epoch, "uncertain", incarnation, "prompt", Vec::new())
            .unwrap();
        let transport_error = Error::internal_error().data(json!({
            "reason": "incoming_transport_closed",
            "method": "session/prompt",
        }));
        settle_runtime_prompt_request_error(
            &mut uncertain,
            "uncertain",
            incarnation,
            "prompt",
            &transport_error,
        )
        .unwrap();
        assert_eq!(
            uncertain.runtime.session("uncertain").unwrap().lifecycle,
            SessionLifecycle::Uncertain
        );
        let uncertain_session = uncertain.runtime.session("uncertain").unwrap();
        assert!(uncertain_session.active_turn.is_none());

        let mut failed = BridgeState::default();
        let epoch = failed.runtime.epoch().to_string();
        let incarnation = failed
            .runtime
            .open_new(
                &epoch,
                "failed",
                "/workspace",
                json!({ "sessionId": "failed" }),
            )
            .unwrap();
        failed
            .runtime
            .start_prompt(&epoch, "failed", incarnation, "prompt", Vec::new())
            .unwrap();
        settle_runtime_prompt_request_error(
            &mut failed,
            "failed",
            incarnation,
            "prompt",
            &Error::resource_not_found(None),
        )
        .unwrap();
        assert_eq!(
            failed.runtime.session("failed").unwrap().lifecycle,
            SessionLifecycle::Active
        );
        let failed_session = failed.runtime.session("failed").unwrap();
        assert!(failed_session.active_turn.is_none());
    }

    #[tokio::test]
    async fn cancels_only_session_scoped_interactions_and_url_flows() {
        let (permission_sender, permission_receiver) = oneshot::channel();
        let (session_sender, session_receiver) = oneshot::channel();
        let (request_sender, _request_receiver) = oneshot::channel();
        let state = Arc::new(Mutex::new(BridgeState::default()));
        {
            let mut state = state.lock().await;
            let epoch = state.runtime.epoch().to_string();
            let incarnation = state
                .runtime
                .open_new(
                    &epoch,
                    "session",
                    "/workspace",
                    json!({ "sessionId": "session" }),
                )
                .unwrap();
            state.active_sessions.insert(
                "session".to_string(),
                ActiveSession {
                    cwd: PathBuf::from("/workspace"),
                    modes: None,
                    config_options: json!([]),
                    incarnation,
                },
            );
            state
                .runtime
                .upsert_permission(
                    &epoch,
                    "session",
                    incarnation,
                    "permission",
                    json!({ "sessionId": "session" }),
                )
                .unwrap();
            state
                .runtime
                .upsert_elicitation(
                    &epoch,
                    Some(("session", incarnation)),
                    "session-elicitation",
                    json!({ "sessionId": "session", "mode": "form" }),
                )
                .unwrap();
            state
                .runtime
                .upsert_elicitation(
                    &epoch,
                    Some(("session", incarnation)),
                    "accepted-session-url",
                    json!({ "sessionId": "session", "mode": "url" }),
                )
                .unwrap();
            state
                .runtime
                .resolve_elicitation(
                    &epoch,
                    Some(("session", incarnation)),
                    "accepted-session-url",
                    Some("session-url"),
                )
                .unwrap();
            state
                .runtime
                .upsert_elicitation(
                    &epoch,
                    None,
                    "request-elicitation",
                    json!({ "requestId": 1, "mode": "form" }),
                )
                .unwrap();
            state
                .runtime
                .upsert_elicitation(
                    &epoch,
                    None,
                    "accepted-request-url",
                    json!({ "requestId": 2, "mode": "url" }),
                )
                .unwrap();
            state
                .runtime
                .resolve_elicitation(&epoch, None, "accepted-request-url", Some("request-url"))
                .unwrap();
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
        let sink = EventSink { tx: tx.into() };
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
        let runtime = state.runtime.snapshot();
        let runtime_session = runtime.sessions.get("session").unwrap();
        assert!(runtime_session.permissions.is_empty());
        assert!(runtime_session.elicitations.is_empty());
        assert!(
            !runtime_session.url_flows.contains_key("session-url"),
            "a terminal URL flow must be removed instead of retained as completed state"
        );
        assert!(
            runtime
                .request_elicitations
                .contains_key("request-elicitation")
        );
        assert_eq!(
            runtime.request_url_flows["request-url"].status,
            UrlFlowStatus::Waiting
        );
        drop(state);
        let events = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|event| serde_json::from_str::<Value>(&event).unwrap())
            .filter(|event| {
                !event["type"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("bridge/internal_"))
            })
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
        let sink = EventSink { tx: tx.into() };
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
