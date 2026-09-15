use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::{ProtocolVersion, v1::*};
use agent_client_protocol::{
    AcpAgentConfig, Agent, Client, ConnectTo, ConnectionTo, JsonRpcRequest,
    is_incoming_transport_closed,
};
use agent_client_protocol_http::HttpClient;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use self::scheduling::{ExecutionTurn, IngressSender};
use crate::agent_process::StdioAcpAgent;
use crate::auth_terminal::{AuthTerminalManager, validate_method as validate_terminal_auth_method};
use crate::elicitation_validation::{
    validate_elicitation_request, validate_elicitation_response_value,
};
use crate::event_queue::EventSender;
use crate::filesystem::WorkspaceFileSystem;
use crate::mcp::McpManager;
use crate::mcp_config::{server_name, server_type};
use crate::options::{Options, Transport};
use crate::ordered_ingress::{RequestClass, RequestOwner};
use crate::runtime_state::{
    RuntimeEffect, SessionLifecycle, SessionOperationKind as RuntimeSessionOperationKind,
    SessionTurnError, UrlFlowResolution, UrlFlowStatus,
};
use crate::semantic::{
    SessionUpdateSemanticState, terminal_references, validate_and_track_session_update,
    validate_content_block, validate_permission_request, validate_prompt_response,
    validate_session_config_options, validate_session_config_reference, validate_session_controls,
    validate_session_metadata, validate_session_mode_reference,
};
use crate::session_mirror::{MirrorError, MirrorPhase, TurnAdmission};
use crate::session_observation::ObservationLease;
use crate::session_registry::SessionRegistry;
use crate::session_resources::{
    AttachmentDelivery, ElicitationResponder, PermissionResponder, ReplayValidationBackup,
    SessionResourceOwner, SessionResources, SessionUpdateOwner, SyncControlCandidate,
    UrlRegistration,
};
use crate::session_state::SessionAdmission;
use crate::terminal::{TerminalManager, TerminalSnapshot};

mod agent_dispatch;
mod coordinator;
#[cfg(test)]
mod coordinator_tests;
mod inbound_requests;
mod scheduling;

const SHUTDOWN_CANCEL_GRACE_PERIOD: Duration = Duration::from_secs(2);
const RECONCILE_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const RECONCILE_MAX_BACKOFF: Duration = Duration::from_secs(8);

#[derive(Clone)]
struct EventSink {
    tx: EventSender,
}

impl EventSink {
    fn send(&self, event: Value) {
        let serialized = event.to_string();
        let _ = self.tx.send(serialized);
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
    let message = error.message;
    let mut event = json!({
        "type": "bridge/error",
        "message": message,
        "code": i32::from(error.code),
    });
    if let Some(data) = error.data {
        event["dataBytes"] = json!(serde_json::to_vec(&data).map_or(0, |encoded| encoded.len()));
        event["data"] = data;
    }
    if let Some(request_id) = request_id {
        event["requestId"] = json!(request_id);
    }
    if let Some(operation) = operation {
        event["operation"] = json!(operation);
    }
    event
}

fn relay_bytes(value: &impl Serialize) -> Result<usize, Error> {
    let bytes = serde_json::to_vec(value)?.len();
    Ok(bytes)
}

fn semantic_error(message: impl Into<String>) -> Error {
    Error::invalid_request().data(message.into())
}

fn validate_agent_session_id(session_id: &str) -> Result<(), String> {
    if session_id.is_empty() {
        return Err("Agent session ID must not be empty".to_string());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttachmentReservation {
    Fresh { cwd: PathBuf, incarnation: u64 },
    Reload { cwd: PathBuf, incarnation: u64 },
}

impl AttachmentReservation {
    fn cwd(&self) -> &PathBuf {
        match self {
            Self::Fresh { cwd, .. } | Self::Reload { cwd, .. } => cwd,
        }
    }

    fn incarnation(&self) -> u64 {
        match self {
            Self::Fresh { incarnation, .. } | Self::Reload { incarnation, .. } => *incarnation,
        }
    }

    fn is_reload(&self) -> bool {
        matches!(self, Self::Reload { .. })
    }
}

/// A cold attachment is allocated by the coordinator before its real session
/// queue exists. This token travels out of band, never in browser JSON.
#[derive(Clone)]
pub(crate) struct PreparedAttachment {
    operation_id: String,
    reservation: AttachmentReservation,
}

#[derive(Default)]
struct CreationStaging {
    validation: SessionUpdateSemanticState,
    early_notifications: Vec<SessionNotification>,
    bytes: usize,
}

impl SessionUpdateOwner {
    fn matches(&self, state: &BridgeState, session_id: &str) -> bool {
        if let Some(incarnation) = self.incarnation {
            return state
                .sessions
                .resources(session_id, incarnation)
                .ok()
                .and_then(|resources| resources.validation.as_ref())
                .is_some_and(|validation| Arc::ptr_eq(&validation.allocation, &self.allocation));
        }
        // An unknown creation keeps the same allocation when its real ID is
        // installed. A continuation from any other attempt cannot follow the ID.
        session_validation(state, session_id)
            .is_some_and(|validation| Arc::ptr_eq(&validation.allocation, &self.allocation))
            || (live_session_incarnation(state, session_id).is_none()
                && state
                    .creation_staging
                    .get(session_id)
                    .is_some_and(|staging| {
                        Arc::ptr_eq(&staging.validation.allocation, &self.allocation)
                    }))
    }
}

#[derive(Default)]
struct BridgeState {
    agent_capabilities: Option<AgentCapabilities>,
    auth_methods: HashMap<String, AuthMethod>,
    listed_sessions: HashMap<String, SessionInfo>,
    // Catalog-only deletions have no SessionRuntime or session execution queue.
    catalog_deletions: HashMap<String, CatalogDeletion>,
    catalog_revision: u64,
    session_list_gate: Arc<Mutex<()>>,
    pending_creations: usize,
    creation_staging: HashMap<String, CreationStaging>,
    early_update_count: usize,
    early_update_bytes: usize,
    request_elicitations: HashMap<String, ElicitationResponder>,
    in_flight_request_ids: HashSet<String>,
    sessions: SessionRegistry,
    published_runtime_seq: u64,
    observer_events: Option<mpsc::UnboundedSender<BridgeInput>>,
    observer_timeout: Option<i64>,
    observers_stopped: bool,
}

fn session_ingest<'a>(state: &'a BridgeState, session_id: &str) -> Option<&'a SessionResources> {
    let incarnation = state.sessions.state(session_id)?.incarnation;
    state.sessions.resources(session_id, incarnation).ok()
}

fn session_ingest_mut<'a>(
    state: &'a mut BridgeState,
    session_id: &str,
) -> Option<&'a mut SessionResources> {
    let incarnation = state.sessions.state(session_id)?.incarnation;
    state.sessions.resources_mut(session_id, incarnation).ok()
}

fn session_validation<'a>(
    state: &'a BridgeState,
    session_id: &str,
) -> Option<&'a SessionUpdateSemanticState> {
    session_ingest(state, session_id)?.validation.as_ref()
}

fn session_validation_mut<'a>(
    state: &'a mut BridgeState,
    session_id: &str,
) -> Option<&'a mut SessionUpdateSemanticState> {
    session_ingest_mut(state, session_id)?.validation.as_mut()
}

fn ensure_session_validation<'a>(
    state: &'a mut BridgeState,
    session_id: &str,
    incarnation: u64,
) -> Result<&'a mut SessionUpdateSemanticState, Error> {
    Ok(state
        .sessions
        .resources_mut(session_id, incarnation)
        .map_err(mirror_error)?
        .validation
        .get_or_insert_with(SessionUpdateSemanticState::default))
}

fn owner_validation_mut<'a>(
    state: &'a mut BridgeState,
    session_id: &str,
    owner: &SessionUpdateOwner,
) -> Option<&'a mut SessionUpdateSemanticState> {
    if !owner.matches(state, session_id) {
        return None;
    }
    let staged = owner.incarnation.is_none()
        && state
            .creation_staging
            .get(session_id)
            .is_some_and(|staging| Arc::ptr_eq(&staging.validation.allocation, &owner.allocation));
    if staged {
        Some(&mut state.creation_staging.get_mut(session_id)?.validation)
    } else {
        session_validation_mut(state, session_id)
    }
}

fn replay_validation_backup<'a>(
    state: &'a BridgeState,
    session_id: &str,
) -> Option<&'a ReplayValidationBackup> {
    session_ingest(state, session_id)?
        .replay_validation_backup
        .as_ref()
}

fn clear_attachment_delivery(
    state: &mut BridgeState,
    session_id: &str,
    incarnation: u64,
) -> Option<u64> {
    let resources = state.sessions.resources_mut(session_id, incarnation).ok()?;
    std::mem::take(&mut resources.attachment).subscriber
}

fn clear_ingest_resources(state: &mut BridgeState, session_id: &str, incarnation: u64) {
    if let Ok(resources) = state.sessions.resources_mut(session_id, incarnation) {
        resources.validation = None;
        resources.replay_validation_backup = None;
        resources.attachment = AttachmentDelivery::default();
    }
}

fn take_sync_control_candidate(state: &mut BridgeState, session_id: &str) -> SyncControlCandidate {
    session_ingest_mut(state, session_id)
        .map(|resources| std::mem::take(&mut resources.attachment.control_candidate))
        .unwrap_or_default()
}

fn interaction_owner_is_live(state: &BridgeState, owner: &SessionResourceOwner) -> bool {
    state.sessions.owns_resources(owner)
        && state.sessions.resource_session(&owner.session_id).is_some()
}

fn pending_elicitation<'a>(
    state: &'a BridgeState,
    interaction_id: &str,
) -> Option<(Option<&'a SessionResourceOwner>, &'a ElicitationResponder)> {
    if let Some(pending) = state.request_elicitations.get(interaction_id) {
        return Some((None, pending));
    }
    state
        .sessions
        .elicitation(interaction_id)
        .map(|(owner, pending)| (Some(owner), pending))
}

fn take_pending_elicitation(
    state: &mut BridgeState,
    interaction_id: &str,
    owner: Option<&SessionResourceOwner>,
) -> Option<ElicitationResponder> {
    match owner {
        Some(owner) => state.sessions.take_elicitation(interaction_id, owner),
        None => state.request_elicitations.remove(interaction_id),
    }
}

fn pending_elicitation_count(state: &BridgeState) -> usize {
    state.request_elicitations.len() + state.sessions.elicitation_owners.len()
}

fn pending_url_elicitation_in_use(state: &BridgeState, url_id: &str) -> bool {
    state
        .request_elicitations
        .values()
        .any(|pending| pending.url_elicitation_id.as_deref() == Some(url_id))
        || state.sessions.elicitation_owners.keys().any(|id| {
            state
                .sessions
                .elicitation(id)
                .is_some_and(|(_, pending)| pending.url_elicitation_id.as_deref() == Some(url_id))
        })
}

fn session_mirror(state: &mut BridgeState) -> &mut SessionRegistry {
    &mut state.sessions
}

fn session_view_value(
    state: &mut BridgeState,
    session_id: &str,
    incarnation: u64,
) -> Result<Value, Error> {
    let mut live = state
        .sessions
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
        .sessions
        .state(session_id)
        .filter(|session| session.incarnation == incarnation)
        .ok_or_else(|| runtime_state_error("missing authoritative session projection"))?;
    Ok(json!({
        "type": "bridge/session_delta",
        "bridgeEpoch": state.sessions.epoch(),
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
    let bridge_epoch = state.sessions.epoch().to_string();
    let session = state
        .sessions
        .state(session_id)
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

fn prepare_session_response(
    state: &mut BridgeState,
    session_id: &str,
    _cwd: PathBuf,
    response: &Value,
    allowed_attachment: Option<(u64, &str)>,
) -> Result<Vec<SessionNotification>, Error> {
    validate_agent_session_id(session_id).map_err(semantic_error)?;
    let attaching = allowed_attachment.is_some_and(|(incarnation, operation_id)| {
        state.sessions.state(session_id).is_some_and(|session| {
            session.incarnation == incarnation
                && session
                    .live
                    .as_ref()
                    .is_some_and(|live| live.lifecycle == SessionLifecycle::Attaching)
                && session.operation.as_ref().is_some_and(|operation| {
                    operation.kind.admission() == SessionAdmission::Attachment
                        && operation.operation_id == operation_id
                })
        })
    });
    if state.catalog_deletions.contains_key(session_id)
        || allowed_attachment.is_some() && !attaching
        || !attaching
            && state.sessions.state(session_id).is_some_and(|session| {
                session.operation.is_some()
                    || session
                        .live
                        .as_ref()
                        .is_some_and(|live| live.lifecycle != SessionLifecycle::Closed)
            })
    {
        return Err(semantic_error(format!(
            "Agent returned a duplicate active session ID: {session_id}"
        )));
    }
    let (modes, _) = response_controls(response)?;
    let validation = if attaching {
        let incarnation = allowed_attachment.expect("attachment was checked").0;
        ensure_session_validation(state, session_id, incarnation)?
    } else {
        &mut state
            .creation_staging
            .entry(session_id.to_string())
            .or_default()
            .validation
    };
    if let Some(reason) = &validation.invalid_reason {
        return Err(semantic_error(format!(
            "Agent session replay was invalid: {reason}"
        )));
    }
    validate_replay_control_references(validation, modes.as_ref())?;
    Ok(if attaching {
        Vec::new()
    } else {
        take_creation_notifications(state, session_id)
    })
}

fn clear_pending_creation_replays(state: &mut BridgeState) {
    let session_ids = state.creation_staging.keys().cloned().collect::<Vec<_>>();
    for session_id in session_ids {
        discard_creation_staging(state, &session_id);
    }
}

fn take_creation_notifications(
    state: &mut BridgeState,
    session_id: &str,
) -> Vec<SessionNotification> {
    let Some(staging) = state.creation_staging.get_mut(session_id) else {
        return Vec::new();
    };
    let notifications = std::mem::take(&mut staging.early_notifications);
    state.early_update_count = state.early_update_count.saturating_sub(notifications.len());
    state.early_update_bytes = state
        .early_update_bytes
        .saturating_sub(std::mem::take(&mut staging.bytes));
    notifications
}

fn discard_creation_staging(state: &mut BridgeState, session_id: &str) {
    if let Some(staging) = state.creation_staging.remove(session_id) {
        state.early_update_count = state
            .early_update_count
            .saturating_sub(staging.early_notifications.len());
        state.early_update_bytes = state.early_update_bytes.saturating_sub(staging.bytes);
    }
}

fn promote_creation_validation(
    state: &mut BridgeState,
    session_id: &str,
    incarnation: u64,
) -> Result<(), Error> {
    if incarnation == 0
        || state
            .sessions
            .active_session(session_id)
            .is_none_or(|session| session.state.incarnation != incarnation)
    {
        return Err(Error::invalid_request()
            .data("creation requires its newly installed active incarnation"));
    }
    let resources = state
        .sessions
        .resources(session_id, incarnation)
        .map_err(mirror_error)?;
    if resources.validation.is_some() {
        return Err(
            Error::invalid_request().data("creation cannot replace an existing validation owner")
        );
    }
    let staging = state
        .creation_staging
        .remove(session_id)
        .ok_or_else(|| Error::invalid_request().data("creation validation owner disappeared"))?;
    state.early_update_count = state
        .early_update_count
        .saturating_sub(staging.early_notifications.len());
    state.early_update_bytes = state.early_update_bytes.saturating_sub(staging.bytes);
    state
        .sessions
        .resources_mut(session_id, incarnation)
        .map_err(mirror_error)?
        .validation = Some(staging.validation);
    Ok(())
}

fn notification_values(updates: &[SessionNotification]) -> Vec<Value> {
    updates
        .iter()
        .filter_map(|notification| serde_json::to_value(notification).ok())
        .collect()
}

fn serialized_value_len(value: &impl Serialize) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).map_or(usize::MAX, |()| counter.0)
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
    let epoch = state.sessions.epoch().to_string();
    if is_incoming_transport_closed(error) {
        state
            .sessions
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
            .sessions
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
    let epoch = state.sessions.epoch().to_string();
    if is_incoming_transport_closed(error) {
        state
            .sessions
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
            .sessions
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
    let epoch = state.sessions.epoch().to_string();
    if is_incoming_transport_closed(error) {
        state
            .sessions
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
            .sessions
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
            .sessions
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
    let epoch = state.sessions.epoch().to_string();
    if reload {
        state
            .sessions
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
            .sessions
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

fn clear_attachment_tracking(
    state: &mut BridgeState,
    session_id: &str,
    owner: &SessionUpdateOwner,
    clear_validation: bool,
) {
    if !owner.matches(state, session_id) {
        return;
    }
    settle_attachment(state, session_id);
    if let Some(incarnation) = owner.incarnation {
        clear_attachment_delivery(state, session_id, incarnation);
    }
    if replay_validation_backup(state, session_id).is_some() {
        rollback_replay_validation(state, session_id, owner);
    } else if clear_validation {
        if let Some(resources) = session_ingest_mut(state, session_id) {
            resources.validation = None;
        }
    }
}

fn begin_replay_validation(state: &mut BridgeState, session_id: &str) -> SessionUpdateOwner {
    let validation = SessionUpdateSemanticState::default();
    let incarnation = state
        .sessions
        .state(session_id)
        .expect("replay has a canonical session owner")
        .incarnation;
    let owner = SessionUpdateOwner {
        incarnation: Some(incarnation),
        allocation: validation.allocation.clone(),
    };
    let resources = state
        .sessions
        .resources_mut(session_id, incarnation)
        .expect("validated replay owner");
    let previous = resources.validation.replace(validation);
    resources.replay_validation_backup = Some(ReplayValidationBackup {
        owner: owner.clone(),
        previous,
    });
    owner
}

fn take_replay_validation_backup(
    state: &mut BridgeState,
    session_id: &str,
    owner: &SessionUpdateOwner,
) -> Option<ReplayValidationBackup> {
    let resources = state
        .sessions
        .resources_mut(session_id, owner.incarnation?)
        .ok()?;
    let backup = resources.replay_validation_backup.as_ref()?;
    if !Arc::ptr_eq(&backup.owner.allocation, &owner.allocation) {
        return None;
    }
    resources.replay_validation_backup.take()
}

fn commit_replay_validation(state: &mut BridgeState, session_id: &str, owner: &SessionUpdateOwner) {
    if take_replay_validation_backup(state, session_id, owner).is_none()
        || !owner.matches(state, session_id)
    {
        return;
    }
    if let Some(validation) = session_validation_mut(state, session_id) {
        validation.retire_turn();
    }
}

fn rollback_replay_validation(
    state: &mut BridgeState,
    session_id: &str,
    owner: &SessionUpdateOwner,
) {
    if let Some(backup) = take_replay_validation_backup(state, session_id, owner) {
        if !owner.matches(state, session_id) {
            return;
        }
        session_ingest_mut(state, session_id)
            .expect("validated replay owner")
            .validation = backup.previous;
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
    match state.sessions.deltas_after(state.published_runtime_seq) {
        Some(deltas) => {
            for delta in deltas {
                state.published_runtime_seq = delta.seq;
                sink.internal_typed("bridge/internal_runtime_delta", delta);
            }
        }
        None => {
            let snapshot = state.sessions.snapshot();
            state.published_runtime_seq = snapshot.through_seq;
            sink.internal_typed("bridge/internal_runtime_snapshot", snapshot);
        }
    }
    refresh_observer_timers(state);
}

fn refresh_observer_timers(state: &mut BridgeState) {
    let Some(events) = state.observer_events.clone() else {
        return;
    };
    let timeout = if state.observers_stopped {
        -1
    } else {
        state.observer_timeout.unwrap_or(-1)
    };
    let owners = state
        .sessions
        .iter_states()
        .map(|session| {
            (
                session.session_id.clone(),
                session.incarnation,
                session.live.as_ref().map(|live| live.lifecycle.clone()),
            )
        })
        .collect::<Vec<_>>();
    for (session_id, incarnation, lifecycle) in owners {
        let Ok(resources) = state.sessions.resources_mut(&session_id, incarnation) else {
            continue;
        };
        if state.observers_stopped {
            resources.observers.stop_absence();
            continue;
        }
        let Some(timer) =
            resources
                .observers
                .refresh(lifecycle.as_ref(), timeout, tokio::time::Instant::now())
        else {
            continue;
        };
        let events = events.clone();
        tokio::spawn(async move {
            if !timer.wait().await {
                return;
            }
            while timer.permit.pending() {
                if timer.permit.cancelled.is_cancelled()
                    || events
                        .send(BridgeInput::RetireUnobservedSession {
                            session_id: session_id.clone(),
                            incarnation,
                            absence_id: timer.id,
                            permit: timer.permit.clone(),
                        })
                        .is_err()
                {
                    return;
                }
                // A control/load may still own admission. Preserve the original
                // absence interval while retrying its close intent.
                tokio::select! {
                    _ = timer.permit.cancelled.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
            }
        });
    }
}

fn finish_session_observers(
    state: &mut BridgeState,
    sink: &EventSink,
    session_id: &str,
    incarnation: u64,
) {
    let epoch = state.sessions.epoch().to_string();
    let Ok(resources) = state.sessions.resources_mut(session_id, incarnation) else {
        return;
    };
    for (observer_id, lease) in resources.observers.drain_for_finish() {
        sink.send(json!({
            "type": "bridge/internal_observer_end", "observerId": observer_id,
            "sessionId": session_id, "bridgeEpoch": epoch, "sessionIncarnation": incarnation,
        }));
        lease.finish();
    }
}

fn retire_session_observation(
    state: &mut BridgeState,
    sink: &EventSink,
    session_id: &str,
    incarnation: u64,
    reason: &str,
) {
    sink.send(json!({
        "type": "bridge/session_retired", "sessionId": session_id,
        "bridgeEpoch": state.sessions.epoch(), "sessionIncarnation": incarnation,
        "reason": reason,
    }));
    finish_session_observers(state, sink, session_id, incarnation);
}

fn finish_session_view_waiter(
    state: &mut BridgeState,
    sink: &EventSink,
    session_id: &str,
    waiter: SessionViewWaiter,
    result: Result<Value, SessionViewError>,
) {
    match waiter {
        SessionViewWaiter::View(response) => {
            let _ = response.send(result);
        }
        SessionViewWaiter::Observe {
            observer_id,
            lease,
            reply,
        } => {
            if lease.is_cancelled() || reply.is_closed() {
                lease.cancel();
                return;
            }
            let view = match result {
                Ok(view) => view,
                Err(error) => {
                    lease.cancel();
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            let incarnation = view.pointer("/session/incarnation").and_then(Value::as_u64);
            let accepted = incarnation
                .and_then(|incarnation| state.sessions.resources_mut(session_id, incarnation).ok())
                .is_some_and(|resources| resources.observers.observe(observer_id, lease.clone()));
            if !accepted {
                lease.cancel();
                let _ = reply.send(Err(SessionViewError::unavailable(
                    "session observation owner no longer matches",
                )));
                return;
            }
            // Registration, revision capture and this marker share the same
            // owner lock/outbox as subsequent session changes.
            sink.send(json!({
                "type": "bridge/internal_observer_ready", "observerId": observer_id,
                "sessionId": session_id,
                "reset": {
                    "type": "bridge/session_reset", "bridgeEpoch": view["bridgeEpoch"],
                    "sessionId": session_id, "sessionIncarnation": incarnation,
                    "viewRevision": view["session"]["viewRevision"],
                    "historyRevision": view["session"]["historyRevision"],
                    "phase": view["session"]["phase"], "syncError": view["session"]["syncError"],
                },
            }));
            if reply.send(Ok(())).is_err() {
                lease.cancel();
                if let Ok(resources) = state
                    .sessions
                    .resources_mut(session_id, incarnation.unwrap())
                {
                    resources.observers.unobserve(observer_id, &lease);
                }
            } else if let Some(events) = state.observer_events.clone() {
                let session_id = session_id.to_string();
                tokio::spawn(async move {
                    tokio::select! {
                        biased;
                        _ = lease.finished() => return,
                        _ = lease.cancelled() => {}
                    }
                    let _ = events.send(BridgeInput::UnobserveSession {
                        session_id,
                        observer_id,
                        lease,
                    });
                });
            }
        }
    }
}

#[cfg(test)]
fn prepare_session_view_request(
    state: &mut BridgeState,
    sink: &EventSink,
    session_id: String,
    cwd: Option<String>,
    waiter: SessionViewWaiter,
) -> Option<Value> {
    prepare_session_view_request_for_owner(state, sink, session_id, cwd, None, waiter)
}

fn prepare_session_view_request_for_owner(
    state: &mut BridgeState,
    sink: &EventSink,
    session_id: String,
    cwd: Option<String>,
    expected_owner: Option<SessionResourceOwner>,
    waiter: SessionViewWaiter,
) -> Option<Value> {
    if let Some(owner) = expected_owner {
        // Resuming a view is read-only with respect to allocation. A retired
        // observer must never turn a transport retry into a fresh session/load.
        let current = owner.session_id == session_id
            && state.sessions.owns_resources(&owner)
            && state
                .sessions
                .state(&session_id)
                .is_some_and(|session| !session.observation_retired());
        let result = if owner.epoch != state.sessions.epoch() {
            Err(SessionViewError::ConnectionReplaced {
                owner,
                current_epoch: state.sessions.epoch().to_string(),
            })
        } else if current {
            session_view_value(state, &session_id, owner.incarnation)
                .map_err(SessionViewError::from)
        } else {
            Err(SessionViewError::Retired(owner))
        };
        finish_session_view_waiter(state, sink, &session_id, waiter, result);
        refresh_observer_timers(state);
        return None;
    }
    if state.catalog_deletions.contains_key(&session_id)
        || state
            .sessions
            .state(&session_id)
            .is_some_and(|session| session.operation.is_some() && session.observation_retired())
    {
        finish_session_view_waiter(
            state,
            sink,
            &session_id,
            waiter,
            Err(SessionViewError::unavailable("session is being deleted")),
        );
        return None;
    }
    if waiter.is_closed() {
        return None;
    }
    let incarnation = state
        .sessions
        .session_ref(&session_id)
        .filter(|session| {
            session.live.session.is_object() && session.live.lifecycle != SessionLifecycle::Closed
        })
        .map(|session| session.state.incarnation)
        .or_else(|| {
            state
                .sessions
                .state(&session_id)
                .filter(|session| session.phase == MirrorPhase::Blocked)
                .map(|session| session.incarnation)
        });
    if let Some(incarnation) = incarnation {
        let result =
            session_view_value(state, &session_id, incarnation).map_err(SessionViewError::from);
        finish_session_view_waiter(state, sink, &session_id, waiter, result);
        refresh_observer_timers(state);
        return None;
    }
    if !state.listed_sessions.contains_key(&session_id) && cwd.is_none() {
        finish_session_view_waiter(
            state,
            sink,
            &session_id,
            waiter,
            Err(SessionViewError::NotFound),
        );
        return None;
    }
    if let Some(materialization) = session_materialization_mut(state, &session_id) {
        materialization.waiters.retain(|waiter| !waiter.is_closed());
        materialization.waiters.push(waiter);
        return None;
    }
    let incarnation = if let Some(session) = state.sessions.state(&session_id) {
        session.incarnation
    } else {
        state.sessions.register_cold(&session_id, 0);
        0
    };
    let materialization_id = Uuid::new_v4().to_string();
    state
        .sessions
        .resources_mut(&session_id, incarnation)
        .expect("registered materialization owner")
        .materialization = Some(crate::session_resources::MaterializationResources {
        attempt_id: materialization_id.clone(),
        waiters: vec![waiter],
        cancellation: CancellationToken::new(),
    });
    let method = if state
        .agent_capabilities
        .as_ref()
        .is_some_and(|caps| caps.load_session)
    {
        "session/load"
    } else {
        "session/resume"
    };
    Some(json!({
        "type": method, "requestId": format!("bridge-materialize-{}", Uuid::new_v4()),
        "sessionId": session_id, "cwd": cwd, "bridgeManagedMaterialization": true,
        "bridgeMaterializationId": materialization_id,
    }))
}

fn session_materialization<'a>(
    state: &'a BridgeState,
    session_id: &str,
) -> Option<&'a crate::session_resources::MaterializationResources> {
    let incarnation = state.sessions.state(session_id)?.incarnation;
    state
        .sessions
        .resources(session_id, incarnation)
        .ok()?
        .materialization
        .as_ref()
}

fn session_materialization_mut<'a>(
    state: &'a mut BridgeState,
    session_id: &str,
) -> Option<&'a mut crate::session_resources::MaterializationResources> {
    let incarnation = state.sessions.state(session_id)?.incarnation;
    state
        .sessions
        .resources_mut(session_id, incarnation)
        .ok()?
        .materialization
        .as_mut()
}

fn take_session_materialization(
    state: &mut BridgeState,
    session_id: &str,
    attempt_id: &str,
) -> Option<crate::session_resources::MaterializationResources> {
    if session_materialization(state, session_id)?.attempt_id != attempt_id {
        return None;
    }
    let incarnation = state.sessions.state(session_id)?.incarnation;
    state
        .sessions
        .resources_mut(session_id, incarnation)
        .ok()?
        .materialization
        .take()
}

fn validate_terminal_references(
    state: &BridgeState,
    session_id: &str,
    incarnation: u64,
    tool_call: &Value,
) -> Result<(), Error> {
    let references = terminal_references(tool_call);
    if references.is_empty() {
        return Ok(());
    }
    let live = state
        .sessions
        .resource_session(session_id)
        .filter(|session| session.state.incarnation == incarnation)
        .ok_or_else(|| Error::invalid_params().data("terminal reference owner was retired"))?
        .live;
    for terminal_id in references {
        if !live.terminals.contains_key(&terminal_id) {
            return Err(Error::invalid_request().data(format!("unknown terminal: {terminal_id}")));
        }
    }
    Ok(())
}

/// A successful physical create must become a canonical reference before its
/// ACP response lets the Agent send a tool update using that terminal ID.
fn register_created_terminal_locked(
    state: &mut BridgeState,
    sink: &EventSink,
    owner: &SessionResourceOwner,
    terminal_id: &str,
) -> Result<(), Error> {
    if !state.sessions.owns_resources(owner)
        || live_session_incarnation(state, &owner.session_id) != Some(owner.incarnation)
    {
        return Err(Error::request_cancelled().data("terminal creation owner was retired"));
    }
    let present = |state: &BridgeState| {
        state
            .sessions
            .live(&owner.session_id)
            .is_some_and(|session| session.terminals.contains_key(terminal_id))
    };
    if present(state) {
        // Output or exit state may already have reached the owner. Identity
        // registration cannot replace that newer snapshot with an empty one.
        return Ok(());
    }
    publish_terminal_snapshot(
        state,
        sink,
        TerminalSnapshot {
            incarnation: owner.incarnation,
            value: json!({
                "sessionId": owner.session_id,
                "terminalId": terminal_id,
                "output": "",
                "outputBytes": "",
                "outputAppend": true,
                "retainedBytes": 0,
                "truncated": false,
                "exitStatus": null,
                "released": false,
            }),
        },
    );
    // Runtime terminal tombstones deliberately reject recreation. A delayed
    // local create completion must not return a handle which was already freed.
    if present(state) {
        Ok(())
    } else {
        Err(Error::request_cancelled().data("terminal was released before creation completed"))
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
                .sessions
                .live(&session_id)
                .and_then(|session| session.terminals.get(&terminal_id)),
            &snapshot.value,
        );
        session_mirror(state)
            .retain_terminal_output(&session_id, snapshot.incarnation, &terminal_id, &terminal)
            .unwrap_or(false)
    } else {
        false
    };
    let epoch = state.sessions.epoch().to_string();
    let updated = state
        .sessions
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
        .sessions
        .state(&session_id)
        .map(|session| session.view_revision);
    let terminal = snapshot.value.clone();
    let incarnation = snapshot.incarnation;
    if !apply_terminal_snapshot(state, snapshot) {
        return;
    }
    flush_runtime(state, sink);
    let Some(current_revision) = state
        .sessions
        .state(&session_id)
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
        .sessions
        .resource_session(session_id)
        .map(|session| session.state.incarnation)
}

fn attached_session_incarnation(state: &BridgeState, session_id: &str) -> Option<u64> {
    state
        .sessions
        .resource_session(session_id)
        .filter(|session| session.live.lifecycle != SessionLifecycle::Attaching)
        .map(|session| session.state.incarnation)
}

async fn close_rejected_chat_session(
    connection: &ConnectionTo<Agent>,
    session_id: &str,
    supported: bool,
    ingress: Option<&IngressSender>,
    execution: &mut Option<ExecutionTurn>,
    mut owner: RequestOwner,
) {
    if supported && !session_id.is_empty() {
        owner.attempt_id = Some(format!("rejected-cleanup-{}", Uuid::new_v4()));
        let _ = send_ordered(
            connection,
            ingress,
            execution,
            owner,
            RequestClass::Control,
            CloseSessionRequest::new(session_id.to_string()),
        )
        .await;
    }
}

fn can_close_rejected_chat_session(state: &BridgeState, session_id: &str) -> bool {
    !state.sessions.active_session(session_id).is_some()
        && !state.listed_sessions.contains_key(session_id)
        && !session_operation_pending(&state, session_id, SessionAdmission::Attachment)
        && !session_operation_pending(&state, session_id, SessionAdmission::Fork)
        && !session_operation_pending(&state, session_id, SessionAdmission::Close)
        && !session_operation_pending(&state, session_id, SessionAdmission::Control)
        && !session_operation_pending(&state, session_id, SessionAdmission::Delete)
        && state.sessions.live(session_id).is_none()
        && state
            .agent_capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.session_capabilities.close.is_some())
}

#[derive(Clone)]
struct CommandContext {
    auto_close: Option<crate::auto_close::AutoClosePermit>,
    prepared_attachment: Option<PreparedAttachment>,
    ingress: Option<scheduling::IngressSender>,
    options: Arc<Options>,
    state: Arc<Mutex<BridgeState>>,
    sink: EventSink,
    terminals: Option<TerminalManager>,
    filesystem: Option<Arc<WorkspaceFileSystem>>,
    auth_terminal: AuthTerminalManager,
    prompt_lifecycle: Arc<PromptLifecycle>,
    cancellation: CancellationToken,
}

pub(crate) enum BridgeInput {
    RuntimeSnapshotRequest,
    SessionViewRequest {
        session_id: String,
        cwd: Option<String>,
        expected_owner: Option<SessionResourceOwner>,
        response: oneshot::Sender<Result<Value, SessionViewError>>,
    },
    ObserveSession {
        session_id: String,
        cwd: Option<String>,
        expected_owner: Option<SessionResourceOwner>,
        observer_id: u64,
        lease: ObservationLease,
        reply: oneshot::Sender<Result<(), SessionViewError>>,
    },
    UnobserveSession {
        session_id: String,
        observer_id: u64,
        lease: ObservationLease,
    },
    RetireUnobservedSession {
        session_id: String,
        incarnation: u64,
        absence_id: u64,
        permit: crate::auto_close::AutoClosePermit,
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
    PreparedAttachment {
        command: Value,
        reservation: PreparedAttachment,
        response: Option<oneshot::Sender<Result<Value, BridgeRequestError>>>,
    },
}

impl BridgeInput {
    /// Resolve interaction replies by their resource owner. A caller-supplied
    /// session ID is validated by the reducer, never used to redirect a reply.
    fn session_id<'a>(&'a self, state: &'a BridgeState) -> Option<&'a str> {
        match self {
            Self::RuntimeSnapshotRequest => None,
            Self::SessionViewRequest { session_id, .. }
            | Self::ObserveSession { session_id, .. }
            | Self::UnobserveSession { session_id, .. }
            | Self::RetireUnobservedSession { session_id, .. }
            | Self::TurnRequest { session_id, .. } => Some(session_id),
            Self::BusinessRequest { command, .. } | Self::PreparedAttachment { command, .. } => {
                match command.get("type").and_then(Value::as_str)? {
                    "permission/respond" => state
                        .sessions
                        .permission(command.get("permissionId")?.as_str()?)
                        .map(|(owner, _)| owner.session_id.as_str()),
                    "elicitation/respond" => {
                        pending_elicitation(state, command.get("elicitationId")?.as_str()?)
                            .and_then(|(owner, _)| owner.map(|owner| owner.session_id.as_str()))
                    }
                    "session/load"
                    | "session/resume"
                    | "session/fork"
                    | "session/close"
                    | "session/delete"
                    | "session/prompt"
                    | "session/cancel"
                    | "session/set_mode"
                    | "session/set_config_option"
                    | "context/search"
                    | "context/read" => command.get("sessionId")?.as_str(),
                    _ => None,
                }
            }
        }
    }

    /// Charge retained payload plus bounded envelope/handle overhead without
    /// constructing a second serialized copy of a potentially large prompt.
    fn payload_bytes(&self) -> usize {
        let payload = match self {
            Self::RuntimeSnapshotRequest => 0,
            Self::SessionViewRequest {
                session_id,
                cwd,
                expected_owner,
                ..
            }
            | Self::ObserveSession {
                session_id,
                cwd,
                expected_owner,
                ..
            } => serialized_value_len(&(session_id, cwd)).saturating_add(
                expected_owner.as_ref().map_or(0, |owner| {
                    serialized_value_len(&(&owner.epoch, &owner.session_id, owner.incarnation))
                }),
            ),
            Self::UnobserveSession { session_id, .. }
            | Self::RetireUnobservedSession { session_id, .. } => serialized_value_len(session_id),
            Self::TurnRequest {
                session_id,
                history_revision,
                client_intent_id,
                prompt,
                ..
            } => serialized_value_len(&(session_id, history_revision, client_intent_id, prompt)),
            Self::BusinessRequest { command, .. } | Self::PreparedAttachment { command, .. } => {
                serialized_value_len(command)
            }
        };
        payload.saturating_add(256)
    }

    /// Capacity and stale-owner rejection must finish the original HTTP wait
    /// and withdraw its observation lease; dropping a sender alone loses the cause.
    fn reject(self, error: Error) {
        match self {
            Self::RuntimeSnapshotRequest => {}
            Self::SessionViewRequest { response, .. } => {
                let _ = response.send(Err(SessionViewError::from(error)));
            }
            Self::ObserveSession { lease, reply, .. } => {
                lease.cancel();
                let _ = reply.send(Err(SessionViewError::from(error)));
            }
            Self::UnobserveSession { lease, .. } => lease.cancel(),
            Self::RetireUnobservedSession { permit, .. } => {
                permit.cancel();
            }
            Self::TurnRequest { response, .. } => {
                let _ = response.send(Err(error_message(error)));
            }
            Self::BusinessRequest { response, .. } => {
                let _ = response.send(Err(BridgeRequestError::from_acp(&error)));
            }
            Self::PreparedAttachment { response, .. } => {
                if let Some(response) = response {
                    let _ = response.send(Err(BridgeRequestError::from_acp(&error)));
                }
            }
        }
    }
}

fn browser_traffic_class(input: &BridgeInput) -> crate::session_dispatch::TrafficClass {
    use crate::session_dispatch::TrafficClass;
    match input {
        BridgeInput::TurnRequest { .. } => TrafficClass::Ordinary,
        BridgeInput::BusinessRequest { command, .. }
            if matches!(
                command.get("type").and_then(Value::as_str),
                Some(
                    "session/new"
                        | "session/prompt"
                        | "session/fork"
                        | "session/load"
                        | "session/resume"
                        | "context/read"
                        | "context/search"
                )
            ) =>
        {
            TrafficClass::Ordinary
        }
        _ => TrafficClass::Reserved,
    }
}

pub(crate) enum SessionViewWaiter {
    View(oneshot::Sender<Result<Value, SessionViewError>>),
    Observe {
        observer_id: u64,
        lease: ObservationLease,
        reply: oneshot::Sender<Result<(), SessionViewError>>,
    },
}

impl SessionViewWaiter {
    pub(crate) fn is_closed(&self) -> bool {
        match self {
            Self::View(reply) => reply.is_closed(),
            Self::Observe { lease, reply, .. } => lease.is_cancelled() || reply.is_closed(),
        }
    }

    pub(crate) fn cancel(self, message: impl Into<String>) {
        let error = SessionViewError::unavailable(message);
        match self {
            Self::View(reply) => {
                let _ = reply.send(Err(error));
            }
            Self::Observe { lease, reply, .. } => {
                lease.cancel();
                let _ = reply.send(Err(error));
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum SessionViewError {
    NotFound,
    Retired(SessionResourceOwner),
    ConnectionReplaced {
        owner: SessionResourceOwner,
        current_epoch: String,
    },
    Unavailable(String),
}

impl SessionViewError {
    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }
}

impl From<Error> for SessionViewError {
    fn from(error: Error) -> Self {
        if error.code == agent_client_protocol::ErrorCode::ResourceNotFound {
            Self::NotFound
        } else {
            Self::Unavailable(error_message(error))
        }
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
    commands: mpsc::UnboundedReceiver<BridgeInput>,
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
            let (fatal_tx, mut fatal_rx) = mpsc::unbounded_channel();
            let agent = StdioAcpAgent::new(config)
                .on_stderr(move |chunk| {
                    debug_sink.send(json!({
                        "type": "bridge/stderr",
                        "chunk": chunk,
                    }));
                })
                .on_fatal(move |error| {
                    let _ = fatal_tx.send(error);
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
        Transport::Ws => {
            run_connection(
                crate::websocket_agent::WebSocketAgent::new(options.command[0].clone()),
                options.clone(),
                commands,
                sink.clone(),
                cancellation.clone(),
            )
            .await
        }
        Transport::Http => match HttpClient::with_endpoint(&options.command[0]) {
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
    mut commands: mpsc::UnboundedReceiver<BridgeInput>,
    sink: EventSink,
    cancellation: CancellationToken,
) -> Result<(), Error>
where
    T: ConnectTo<Client>,
{
    let state = Arc::new(Mutex::new(BridgeState::default()));
    let epoch = state.lock().await.sessions.epoch().to_string();
    let (mut scheduling, ingress) =
        scheduling::Scheduling::new(epoch.clone(), sink.clone(), cancellation.clone())?;
    let mut coordinator = coordinator::Coordinator::default();
    let (observer_events, mut observer_rx) = mpsc::unbounded_channel();
    {
        let mut state = state.lock().await;
        state.observer_events = Some(observer_events);
        state.observer_timeout = Some(options.session_unobserved_timeout);
    }
    sink.internal_typed(
        "bridge/internal_runtime_snapshot",
        state.lock().await.sessions.snapshot(),
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
    let (terminal_snapshot_tx, mut terminal_snapshot_rx) = mpsc::unbounded_channel();
    let terminals = filesystem.clone().map(|filesystem| {
        TerminalManager::new_with_snapshots(filesystem, sink.tx.clone(), Some(terminal_snapshot_tx))
    });
    let (terminal_barrier, mut terminal_barrier_rx) =
        mpsc::unbounded_channel::<oneshot::Sender<()>>();
    {
        let ingress = ingress.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    snapshot = terminal_snapshot_rx.recv() => {
                        let Some(snapshot) = snapshot else { break };
                        if ingress.try_terminal(snapshot).is_err() { break; }
                    }
                    barrier = terminal_barrier_rx.recv() => {
                        let Some(barrier) = barrier else { break };
                        while let Ok(snapshot) = terminal_snapshot_rx.try_recv() {
                            if ingress.try_terminal(snapshot).is_err() { return; }
                        }
                        let _ = barrier.send(());
                    }
                }
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

    let agent_services = agent_dispatch::AgentContext {
        state: state.clone(),
        sink: sink.clone(),
        terminals: terminals.clone(),
        filesystem: filesystem.clone(),
        mcp: mcp.clone(),
        ingress: ingress.clone(),
        owner: None,
        dispatch_owner: RequestOwner {
            epoch,
            session_id: None,
            incarnation: None,
            operation_id: "connection".to_string(),
            attempt_id: None,
        },
        url_registration: None,
        request_lease: None,
    };
    let builder = Client.builder().on_receive_dispatch(
        {
            let ingress = ingress.clone();
            async move |dispatch: agent_client_protocol::Dispatch, _connection| {
                ingress.receive_dispatch(dispatch).await
            }
        },
        agent_client_protocol::on_receive_dispatch!(),
    );

    builder
        .connect_with(transport, async move |connection| {
            let _auth_terminal_cleanup = auth_terminal.close_on_drop();
            sink.send(json!({ "type": "bridge/phase", "phase": "initializing" }));
            let initialize = InitializeRequest::new(ProtocolVersion::V1)
                .client_capabilities(client_capabilities(&options))
                .client_info(
                    Implementation::new("attyd", crate::VERSION).title("attyd web client"),
                );
            let response = connection.send_request(initialize).block_task().await?;
            relay_bytes(&response)?;
            if response.protocol_version != ProtocolVersion::V1 {
                return Err(Error::invalid_request().data(format!(
                    "unsupported ACP protocol version: {}",
                    response.protocol_version
                )));
            }
            validate_auth_methods(
                &response.auth_methods,
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
                let next = loop {
                    if cancellation.is_cancelled() { break None; }
                    coordinator.retire_missing(&*state.lock().await, &scheduling)?;
                    if let Some(delivery) = scheduling.pump()? {
                        if let Some(input) = coordinator::dispatch_ready(
                            delivery, &scheduling, &connection, &agent_services, false,
                        ).await? { break Some(input); }
                        continue;
                    }
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => break None,
                        item = scheduling.ingress_rx.recv() => {
                            let Some(item) = item else { break None };
                            coordinator.route(item, &scheduling, &ingress, &state, &sink).await?;
                        }
                        // Clean EOF fails pending RPCs but does not cancel the
                        // SDK foreground future. Stop admission and let the
                        // cleanup below drain already accepted deliveries.
                        _ = connection.incoming_closed() => break None,
                        input = observer_rx.recv() => {
                            if let Some(input) = input {
                                let _ = ingress.try_browser(input, crate::session_dispatch::TrafficClass::Reserved);
                            }
                        }
                        input = commands.recv() => {
                            let Some(input) = input else { cancellation.cancel(); break None };
                            let class = browser_traffic_class(&input);
                            let _ = ingress.try_browser(input, class);
                        }
                        _ = scheduling.wake.notified() => {}
                    }
                };
                let Some((input, execution)) = next else { break };
                let input = Some(input);
                let mut execution = Some(execution);
                let mut auto_close = None;
                let mut prepared_attachment = None;
                let (command, turn_responder, business_responder) = match input {
                    Some(BridgeInput::RuntimeSnapshotRequest) => {
                        let mut state = state.lock().await;
                        let snapshot = state.sessions.snapshot();
                        state.published_runtime_seq = snapshot.through_seq;
                        sink.internal_typed("bridge/internal_runtime_snapshot", snapshot);
                        continue;
                    }
                    Some(BridgeInput::RetireUnobservedSession {
                        session_id,
                        incarnation,
                        absence_id,
                        permit,
                    }) => {
                        if !permit.pending() {
                            continue;
                        }
                        let state = state.lock().await;
                        if !state
                            .sessions
                            .resources(&session_id, incarnation)
                            .is_ok_and(|resources| {
                                resources.observers.matches_absence(absence_id, &permit)
                            })
                            || state
                                .sessions
                                .active_session(&session_id)
                                .is_none_or(|session| session.state.incarnation != incarnation)
                            || !state
                                .agent_capabilities
                                .as_ref()
                                .is_some_and(|caps| caps.session_capabilities.close.is_some())
                        {
                            permit.cancel();
                            continue;
                        }
                        drop(state);
                        auto_close = Some(permit);
                        (
                            json!({
                                "type": "session/close",
                                "requestId": format!("bridge-unobserved-close-{}", Uuid::new_v4()),
                                "sessionId": session_id,
                                "expectedIncarnation": incarnation,
                            }),
                            None,
                            None,
                        )
                    }
                    Some(BridgeInput::SessionViewRequest {
                        session_id,
                        cwd,
                        expected_owner,
                        response,
                    }) => {
                        let command = {
                            let mut state = state.lock().await;
                            prepare_session_view_request_for_owner(
                                &mut state,
                                &sink,
                                session_id,
                                cwd,
                                expected_owner,
                                SessionViewWaiter::View(response),
                            )
                        };
                        let Some(command) = command else {
                            continue;
                        };
                        (command, None, None)
                    }
                    Some(BridgeInput::ObserveSession {
                        session_id,
                        cwd,
                        expected_owner,
                        observer_id,
                        lease,
                        reply,
                    }) => {
                        let command = {
                            let mut state = state.lock().await;
                            prepare_session_view_request_for_owner(
                                &mut state,
                                &sink,
                                session_id,
                                cwd,
                                expected_owner,
                                SessionViewWaiter::Observe {
                                    observer_id,
                                    lease,
                                    reply,
                                },
                            )
                        };
                        let Some(command) = command else {
                            continue;
                        };
                        (command, None, None)
                    }
                    Some(BridgeInput::UnobserveSession {
                        session_id,
                        observer_id,
                        lease,
                    }) => {
                        let mut state = state.lock().await;
                        if let Some(incarnation) = state
                            .sessions
                            .state(&session_id)
                            .map(|session| session.incarnation)
                            && let Ok(resources) =
                                state.sessions.resources_mut(&session_id, incarnation)
                        {
                            resources.observers.unobserve(observer_id, &lease);
                        }
                        refresh_observer_timers(&mut state);
                        continue;
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
                    Some(BridgeInput::PreparedAttachment { command, reservation, response }) => {
                        prepared_attachment = Some(reservation);
                        (command, None, response.map(BusinessResponder::new))
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
                    auto_close,
                    prepared_attachment,
                    ingress: Some(ingress.clone()),
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
                    if operation != "session/prompt"
                        && let Some(request_id) = request_id.as_deref()
                    {
                        let mut state = context.state.lock().await;
                        if let Err(error) = reserve_command_request_id(
                            &mut state, &context.sink, request_id, &command,
                            context.prepared_attachment.as_ref(),
                        ) {
                            if let Some(responder) = &business_responder {
                                responder.error(&error);
                            }
                            return Ok(());
                        }
                    }
                    let result = if bridge_managed_materialization {
                        let mut command = command;
                        let mut backoff = RECONCILE_INITIAL_BACKOFF;
                        let materialization_cancel = {
                            let state = context.state.lock().await;
                            intent_session_id.as_deref()
                                .and_then(|session_id| session_materialization(&state, session_id))
                                .filter(|owner| Some(owner.attempt_id.as_str()) == bridge_materialization_id.as_deref())
                                .map(|owner| owner.cancellation.clone())
                                .unwrap_or_else(|| { let token = CancellationToken::new(); token.cancel(); token })
                        };
                        loop {
                            if materialization_cancel.is_cancelled() { break Err(Error::request_cancelled()); }
                            command["bridgeMaterializationFinalAttempt"] =
                                json!(false);
                            command["bridgeMaterializationRetryAfterMs"] = json!(backoff.as_millis());
                            let result = tokio::select! {
                                _ = materialization_cancel.cancelled() => break Err(Error::request_cancelled()),
                                result = handle_command(
                                    command.clone(), task_connection.clone(), context.clone(), None, None, None, execution.take(),
                                ) => result,
                            };
                            match result {
                                Err(error)
                                    if retryable_reconcile_error(&error)
                                        && !context.cancellation.is_cancelled() =>
                                {
                                    tokio::select! {
                                        _ = tokio::time::sleep(backoff) => {}
                                        _ = materialization_cancel.cancelled() => break Err(Error::request_cancelled()),
                                        _ = context.cancellation.cancelled() => {
                                            break Err(Error::request_cancelled());
                                        }
                                    }
                                    backoff = backoff.saturating_mul(2).min(RECONCILE_MAX_BACKOFF);
                                    command["requestId"] =
                                        json!(format!("bridge-materialize-{}", Uuid::new_v4()));
                                    let owner = {
                                        let locked = context.state.lock().await;
                                        let session_id = intent_session_id.as_deref().expect("materialization session");
                                        let Some(incarnation) = locked.sessions.state(session_id).map(|session| session.incarnation) else {
                                            break Err(Error::request_cancelled().data("materialization owner retired"));
                                        };
                                        RequestOwner {
                                            epoch: locked.sessions.epoch().to_string(), session_id: Some(session_id.to_string()),
                                            incarnation: Some(incarnation), operation_id: command["requestId"].as_str().unwrap().to_string(),
                                            attempt_id: bridge_materialization_id.clone(),
                                        }
                                    };
                                    execution = match context.ingress.as_ref().expect("production ingress").continue_owner(owner).await {
                                        Ok(turn) => Some(turn), Err(error) => break Err(error),
                                    };
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
                            execution.take(),
                        )
                        .await
                    };
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
                    if let Some(request_id) = request_id.as_deref() {
                        context.state.lock().await.in_flight_request_ids.remove(request_id);
                    }
                    Ok(())
                })?;
            }
            {
                let mut state = state.lock().await;
                state.observers_stopped = true;
                refresh_observer_timers(&mut state);
            }
            let shutdown = cancel_prompts_on_shutdown(&connection, &state, &sink, &prompt_lifecycle);
            tokio::pin!(shutdown);
            loop {
                if futures::poll!(&mut shutdown).is_ready() { break; }
                if let Some(delivery) = scheduling.pump()? {
                    coordinator::dispatch_ready(delivery, &scheduling, &connection, &agent_services, true).await?;
                    continue;
                }
                tokio::select! {
                    _ = &mut shutdown => break,
                    item = scheduling.ingress_rx.recv() => {
                        let Some(item) = item else { break };
                        coordinator.route(item, &scheduling, &ingress, &state, &sink).await?;
                    }
                    _ = scheduling.wake.notified() => {}
                }
            }
            if let Some(terminals) = &terminals {
                terminals.close_all().await;
            }
            mcp.close_all().await;
            let (barrier, forwarded) = oneshot::channel();
            if terminal_barrier.send(barrier).is_ok() { let _ = forwarded.await; }
            let drain = async {
                loop {
                    while let Ok(item) = scheduling.ingress_rx.try_recv() {
                        coordinator.route(item, &scheduling, &ingress, &state, &sink).await?;
                    }
                    if let Some(delivery) = scheduling.pump()? {
                        coordinator::dispatch_ready(delivery, &scheduling, &connection, &agent_services, true).await?;
                        continue;
                    }
                    if !scheduling.has_local_work() { break; }
                    tokio::select! {
                        item = scheduling.ingress_rx.recv() => {
                            let Some(item) = item else { break };
                            coordinator.route(item, &scheduling, &ingress, &state, &sink).await?;
                        }
                        _ = scheduling.wake.notified() => {}
                    }
                }
                Ok::<(), Error>(())
            };
            let drain_deadline = async {
                // Natural EOF seals a finite input stream, not a processing
                // deadline. Only explicit cancellation may bound its cleanup.
                if connection.is_incoming_closed() {
                    cancellation.cancelled().await;
                }
                tokio::time::sleep(SHUTDOWN_CANCEL_GRACE_PERIOD).await;
            };
            tokio::select! {
                result = drain => result?,
                _ = drain_deadline => return Err(Error::internal_error().data("session transitions did not drain during shutdown")),
            }
            coordinator.close(&ingress, &sink)?;
            scheduling.close();
            {
                let mut state = state.lock().await;
                let owners = state.sessions.iter_states().map(|session| (session.session_id.clone(), session.incarnation)).collect::<Vec<_>>();
                for (session_id, incarnation) in owners {
                    finish_session_observers(&mut state, &sink, &session_id, incarnation);
                }
            }
            Ok(())
        })
        .await
}

fn error_message(error: Error) -> String {
    error.data.map_or(error.message, |data| data.to_string())
}

async fn resolve_session_view_waiters(
    state: &Arc<Mutex<BridgeState>>,
    sink: &EventSink,
    session_id: &str,
    materialization_id: &str,
    load_result: &Result<(), Error>,
) {
    let mut state = state.lock().await;
    resolve_session_view_waiters_locked(
        &mut state,
        sink,
        session_id,
        materialization_id,
        load_result,
    );
}

/// Complete materialization and its observation cut in the result owner's
/// local transition, before that session's next delivery is allowed to run.
fn resolve_session_view_waiters_locked(
    state: &mut BridgeState,
    sink: &EventSink,
    session_id: &str,
    materialization_id: &str,
    load_result: &Result<(), Error>,
) {
    let Some(materialization) = take_session_materialization(state, session_id, materialization_id)
    else {
        return;
    };
    materialization.cancellation.cancel();
    let result = match load_result {
        Ok(()) => state
            .sessions
            .active_session(session_id)
            .map(|session| session.state.incarnation)
            .ok_or_else(|| {
                SessionViewError::unavailable(
                    "session/load completed without materializing the session",
                )
            })
            .and_then(|incarnation| {
                session_view_value(state, session_id, incarnation).map_err(SessionViewError::from)
            }),
        Err(error) => Err(SessionViewError::from(error.clone())),
    };
    for waiter in materialization.waiters {
        finish_session_view_waiter(state, sink, session_id, waiter, result.clone());
    }
    if let Some(incarnation) = state
        .sessions
        .state(session_id)
        .filter(|session| {
            session.phase == MirrorPhase::Cold
                && session.live.is_none()
                && session.operation.is_none()
                && session.history_revision.is_none()
        })
        .map(|session| session.incarnation)
    {
        state.sessions.remove(session_id, incarnation);
    }
    refresh_observer_timers(state);
}

fn drain_runtime_effects(state: &mut BridgeState, sink: &EventSink) -> Vec<(String, String)> {
    let effects = state.sessions.take_effects();
    let mut terminal_releases = Vec::new();
    for effect in effects {
        match effect {
            RuntimeEffect::CancelPermissionResponder {
                session_id,
                incarnation,
                interaction_id,
            } => {
                let owner =
                    SessionResourceOwner::new(state.sessions.epoch(), &session_id, incarnation);
                if let Some(pending) = state.sessions.take_permission(&interaction_id, &owner) {
                    let _ = pending.sender.send(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ));
                    sink.send(json!({ "type": "acp/permission_resolved", "permissionId": interaction_id }));
                }
            }
            RuntimeEffect::CancelElicitationResponder {
                session_id,
                incarnation,
                interaction_id,
            } => {
                let owner = match (session_id, incarnation) {
                    (Some(session_id), Some(incarnation)) => Some(SessionResourceOwner::new(
                        state.sessions.epoch(),
                        session_id,
                        incarnation,
                    )),
                    (None, None) => None,
                    _ => continue,
                };
                if let Some(pending) =
                    take_pending_elicitation(state, &interaction_id, owner.as_ref())
                {
                    let response = CreateElicitationResponse::new(ElicitationAction::Cancel);
                    let _ = pending.sender.send(response.clone());
                    sink.send(json!({ "type": "acp/elicitation_resolved", "elicitationId": interaction_id, "response": response }));
                }
            }
            RuntimeEffect::AbortUrlFlow {
                session_id,
                incarnation,
                elicitation_id,
                registration_id,
            } => {
                let owner = match (session_id.clone(), incarnation) {
                    (Some(session_id), Some(incarnation)) => Some(SessionResourceOwner::new(
                        state.sessions.epoch(),
                        session_id,
                        incarnation,
                    )),
                    (None, None) => None,
                    _ => continue,
                };
                let registration = UrlRegistration {
                    owner,
                    registration_id,
                };
                if state
                    .sessions
                    .take_url_route(&elicitation_id, &registration)
                    .is_some()
                {
                    sink.send(json!({ "type": "acp/elicitation_aborted", "elicitationId": elicitation_id, "sessionId": session_id, "reason": "runtime_terminal" }));
                }
            }
            RuntimeEffect::ResourcesCancelled {
                session_id,
                incarnation: _,
                permission_ids,
                elicitation_ids,
                url_ids,
                reason,
            } => {
                // Physical retirement already sent ACP terminal responses and
                // removed routes synchronously. Do not terminate a reused ID.
                for id in permission_ids {
                    if !state.sessions.permission_owners.contains_key(&id) {
                        sink.send(json!({ "type": "acp/permission_resolved", "permissionId": id }));
                    }
                }
                for id in elicitation_ids {
                    if !state.sessions.elicitation_owners.contains_key(&id)
                        && !state.request_elicitations.contains_key(&id)
                    {
                        sink.send(json!({ "type": "acp/elicitation_resolved", "elicitationId": id, "response": CreateElicitationResponse::new(ElicitationAction::Cancel) }));
                    }
                }
                for (id, _registration_id) in url_ids {
                    if !state.sessions.url_owners.contains_key(&id) {
                        sink.send(json!({ "type": "acp/elicitation_aborted", "elicitationId": id, "sessionId": session_id, "reason": reason }));
                    }
                }
            }
            RuntimeEffect::ReleaseTerminal {
                session_id,
                terminal_id,
            } => terminal_releases.push((session_id, terminal_id)),
        }
    }
    terminal_releases
}

async fn apply_runtime_effects(
    state: &Arc<Mutex<BridgeState>>,
    sink: &EventSink,
    terminals: Option<&TerminalManager>,
) {
    let terminal_releases = {
        let mut locked = state.lock().await;
        drain_runtime_effects(&mut locked, sink)
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
    _terminals: Option<&TerminalManager>,
) {
    enum Delivery {
        None,
        Broadcast,
        Direct(u64),
    }
    if let Err(error) = relay_bytes(&notification) {
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
    let owner = {
        let mut state = state.lock().await;
        if let Some(incarnation) = live_session_incarnation(&state, &session_id) {
            let allocation = match ensure_session_validation(&mut state, &session_id, incarnation) {
                Ok(validation) => validation.allocation.clone(),
                Err(error) => {
                    sink.acp_error(error, None, Some("session/update"));
                    return;
                }
            };
            SessionUpdateOwner {
                incarnation: Some(incarnation),
                allocation,
            }
        } else if state.pending_creations > 0 {
            if let Err(message) = validate_agent_session_id(&session_id) {
                sink.acp_error(semantic_error(message), None, Some("session/update"));
                return;
            }
            if !state.creation_staging.contains_key(&session_id)
                && state.creation_staging.len() >= state.pending_creations
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
            let staging = state
                .creation_staging
                .entry(session_id.clone())
                .or_default();
            SessionUpdateOwner {
                incarnation: None,
                allocation: staging.validation.allocation.clone(),
            }
        } else {
            // Late updates for closed or failed sessions must not leak into the visible thread.
            return;
        }
    };

    let tool_update = matches!(
        update.get("sessionUpdate").and_then(Value::as_str),
        Some("tool_call" | "tool_call_update")
    );
    {
        let mut state = state.lock().await;
        if !owner.matches(&state, &session_id) {
            return;
        }
        let active = attached_session_incarnation(&state, &session_id).is_some();
        let attachment =
            session_operation_pending(&state, &session_id, SessionAdmission::Attachment);
        let early_creation = !active && !attachment && state.pending_creations > 0;
        if !active && !attachment && !early_creation {
            return;
        }
        let active_incarnation = attached_session_incarnation(&state, &session_id);
        let mirror_phase = active_incarnation.and_then(|incarnation| {
            state
                .sessions
                .state(&session_id)
                .filter(|session| session.incarnation == incarnation)
                .map(|session| session.phase)
        });
        let mirror_replay_attempt = active_incarnation.and_then(|incarnation| {
            state
                .sessions
                .load_attempt(&session_id, incarnation)
                .map(str::to_string)
        });
        let attachment_subscriber = attachment
            .then(|| {
                session_ingest(&state, &session_id)
                    .and_then(|resources| resources.attachment.subscriber)
            })
            .flatten();
        let reference_error = if tool_update && !attachment && mirror_replay_attempt.is_none() {
            validate_terminal_references(
                &state,
                &session_id,
                owner.incarnation.unwrap_or(0),
                &update,
            )
            .err()
        } else {
            None
        };
        let validation = owner_validation_mut(&mut state, &session_id, &owner)
            .expect("update allocation was checked");
        let result = if let Some(error) = reference_error {
            if !active || attachment || mirror_replay_attempt.is_some() {
                validation
                    .invalid_reason
                    .get_or_insert_with(|| error_message(error.clone()));
            }
            Err(error)
        } else if let Err(message) = if attachment || mirror_replay_attempt.is_some() {
            crate::semantic::validate_history_update(validation, &update)
        } else {
            validate_and_track_session_update(validation, &update)
        } {
            if !active || attachment || mirror_replay_attempt.is_some() {
                validation.invalid_reason.get_or_insert(message.clone());
            }
            Err(semantic_error(message))
        } else if attachment {
            let incarnation = state
                .sessions
                .state(&session_id)
                .map(|session| session.incarnation)
                .ok_or_else(|| runtime_state_error("missing canonical attachment transaction"));
            let runtime_result = incarnation.and_then(|incarnation| {
                if is_conversation_update(&update) {
                    let attempt = state
                        .sessions
                        .load_attempt(&session_id, incarnation)
                        .map(str::to_string)
                        .ok_or_else(|| runtime_state_error("attachment has no load attempt"))?;
                    state
                        .sessions
                        .append_load_update(&session_id, incarnation, &attempt, update.clone())
                        .map_err(mirror_error)
                } else {
                    let epoch = state.sessions.epoch().to_string();
                    state
                        .sessions
                        .append_attachment_candidate(
                            &epoch,
                            &session_id,
                            incarnation,
                            update.clone(),
                        )
                        .map_err(runtime_state_error)
                }
            });
            runtime_result.map(|()| {
                session_ingest(&state, &session_id)
                    .and_then(|resources| resources.attachment.subscriber)
                    .map_or(Delivery::Broadcast, Delivery::Direct)
            })
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
                let candidate = &mut session_ingest_mut(&mut state, &session_id)
                    .expect("replay has an owner")
                    .attachment
                    .control_candidate;
                candidate.bytes = candidate.bytes.saturating_add(bytes);
                candidate.updates.push(update.clone());
                Ok(())
            };
            mirrored.map(|()| Delivery::None)
        } else if early_creation {
            let notification_bytes = serde_json::to_vec(&notification)
                .expect("ACP notification serializes")
                .len();
            let staging = state
                .creation_staging
                .get_mut(&session_id)
                .expect("creation allocation was checked");
            staging.early_notifications.push(notification.clone());
            staging.bytes += notification_bytes;
            state.early_update_count += 1;
            state.early_update_bytes += notification_bytes;
            Ok(Delivery::None)
        } else {
            let incarnation = attached_session_incarnation(&state, &session_id).unwrap_or(0);
            if incarnation != 0 {
                let epoch = state.sessions.epoch().to_string();
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
                    } else if matches!(
                        mirror_phase,
                        Some(MirrorPhase::Ready | MirrorPhase::Reconciling)
                    ) {
                        session_mirror(&mut state).append_session_update(&session_id, incarnation, update.clone())
                            .map_err(mirror_error)
                            .and_then(|()| {
                                let view = session_view_value(&mut state, &session_id, incarnation)?;
                                business_delta = Some(json!({ "type": "bridge/session_view", "sessionId": session_id, "view": view }));
                                Ok(())
                            })
                    } else {
                        Ok(())
                    };
                    mirrored.and_then(|()| {
                        if state
                            .sessions
                            .state(&session_id)
                            .and_then(|session| session.active_turn.as_ref())
                            .and_then(|turn| turn.execution.as_ref())
                            .is_some()
                        {
                            let overlay = session_mirror(&mut state)
                                .state(&session_id)
                                .and_then(|session| session.active_turn.clone())
                                .ok_or_else(|| {
                                    runtime_state_error("missing authoritative turn overlay")
                                })?;
                            state
                                .sessions
                                .project_turn_update(
                                    &epoch,
                                    &session_id,
                                    incarnation,
                                    &overlay,
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
                        .sessions
                        .update_control_state(&epoch, &session_id, incarnation, key, update.clone())
                        .map_err(runtime_state_error)
                };
                if let Err(error) = runtime_result {
                    Err(error)
                } else {
                    if let Some(delta) = business_delta {
                        sink.send(delta);
                    }
                    Ok(Delivery::Broadcast)
                }
            } else {
                Ok(Delivery::Broadcast)
            }
        };
        if let Err(error) = &result
            && (attachment || mirror_replay_attempt.is_some())
            && let Some(validation) = owner_validation_mut(&mut state, &session_id, &owner)
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
                state.sessions.state(&session_id).and_then(|session| {
                    let incarnation = session.incarnation;
                    state
                        .sessions
                        .load_attempt(&session_id, incarnation)
                        .map(|attempt| (incarnation, attempt.to_string()))
                })
        {
            let _ =
                session_mirror(&mut state).invalidate_load(&session_id, incarnation, &attempt_id);
        }
        flush_runtime(&mut state, sink);
        match (
            result,
            attachment_subscriber,
            mirror_replay_attempt.is_some(),
        ) {
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
        state
            .sessions
            .iter_states()
            .filter(|session| session_prompt_pending(&state, &session.session_id))
            .map(|session| (session.session_id.clone(), session.incarnation))
            .collect::<Vec<_>>()
    };
    if session_ids.is_empty() {
        return;
    }

    for (session_id, incarnation) in &session_ids {
        // Incoming EOF already failed pending RPCs. Writing a cancellation to
        // an exited Agent can break the transport before their results drain.
        if !connection.is_incoming_closed() {
            let _ = connection.send_notification(CancelNotification::new(session_id.clone()));
        }
        cancel_interactions(session_id, *incarnation, "session_cancelled", state, sink).await;
    }

    // ACP cancellation completes through the original session/prompt response.
    // Keep the connection alive briefly so the notification reaches the Agent
    // and that response can be consumed before the transport is released.
    let wait_for_completion = async {
        loop {
            let changed = lifecycle.changed.notified();
            let all_finished = {
                let state = state.lock().await;
                session_ids.iter().all(|(session_id, incarnation)| {
                    state
                        .sessions
                        .state(session_id)
                        .is_none_or(|owner| owner.incarnation != *incarnation)
                        || !session_prompt_pending(&state, session_id)
                })
            };
            if all_finished {
                return;
            }
            changed.await;
        }
    };
    let _ = tokio::time::timeout(SHUTDOWN_CANCEL_GRACE_PERIOD, wait_for_completion).await;
}

fn reserve_command_request_id(
    state: &mut BridgeState,
    sink: &EventSink,
    request_id: &str,
    command: &Value,
    prepared: Option<&PreparedAttachment>,
) -> Result<(), Error> {
    if state.in_flight_request_ids.insert(request_id.to_string()) {
        return Ok(());
    }
    let error = Error::invalid_request().data("requestId is already in flight on this bridge");
    if let Some(prepared) = prepared {
        coordinator::reject_prepared_attachment(state, sink, command, prepared, &error);
    }
    Err(error)
}

fn rpc_owner(
    epoch: &str,
    session: Option<(&str, u64)>,
    operation_id: &str,
    attempt_id: Option<&str>,
) -> RequestOwner {
    RequestOwner {
        epoch: epoch.to_string(),
        session_id: session.map(|(id, _)| id.to_string()),
        incarnation: session.map(|(_, incarnation)| incarnation),
        operation_id: operation_id.to_string(),
        attempt_id: attempt_id.map(ToOwned::to_owned),
    }
}

async fn release_runtime_terminals(
    terminals: Option<&TerminalManager>,
    releases: Vec<(String, String)>,
) {
    if let Some(terminals) = terminals {
        for (session_id, terminal_id) in releases {
            let _ = terminals
                .release(ReleaseTerminalRequest::new(session_id, terminal_id))
                .await;
        }
    }
}

async fn continue_execution(
    ingress: Option<&IngressSender>,
    execution: &mut Option<ExecutionTurn>,
    owner: &RequestOwner,
) -> Result<(), Error> {
    if let Some(ingress) = ingress {
        drop(execution.take());
        *execution = Some(ingress.continue_owner(owner.clone()).await?);
    }
    Ok(())
}

// The outer error means the coordinator could not grant a completion ticket: no
// caller may roll back or publish without a ticket. A pre-send error is an inner
// error and leaves the original ticket held for the normal local rollback.
async fn send_ordered<Req: JsonRpcRequest>(
    connection: &ConnectionTo<Agent>,
    ingress: Option<&IngressSender>,
    execution: &mut Option<ExecutionTurn>,
    owner: RequestOwner,
    class: RequestClass,
    request: Req,
) -> Result<Result<Req::Response, Error>, Error> {
    send_ordered_then(connection, ingress, execution, owner, class, request, || {}).await
}

async fn send_ordered_then<Req: JsonRpcRequest>(
    connection: &ConnectionTo<Agent>,
    ingress: Option<&IngressSender>,
    execution: &mut Option<ExecutionTurn>,
    owner: RequestOwner,
    class: RequestClass,
    request: Req,
    sent: impl FnOnce(),
) -> Result<Result<Req::Response, Error>, Error> {
    send_ordered_with_replay(
        connection, ingress, execution, owner, class, request, sent, None,
    )
    .await
}

async fn send_ordered_with_replay<Req: JsonRpcRequest>(
    connection: &ConnectionTo<Agent>,
    ingress: Option<&IngressSender>,
    execution: &mut Option<ExecutionTurn>,
    owner: RequestOwner,
    class: RequestClass,
    request: Req,
    sent: impl FnOnce(),
    replay: Option<crate::history_replay::ReplayCandidate>,
) -> Result<Result<Req::Response, Error>, Error> {
    let Some(ingress) = ingress else {
        let pending = connection.send_request(request);
        sent();
        return Ok(pending.block_task().await);
    };
    let turn = execution.as_ref().ok_or_else(|| {
        Error::internal_error().data("ordered RPC requires its current execution ticket")
    })?;
    if !turn.matches_owner(&owner) {
        continue_execution(Some(ingress), execution, &owner).await?;
    }
    let handle = execution.as_ref().and_then(ExecutionTurn::handle).cloned();
    let pending = match ingress
        .prepare_rpc(class, owner, handle)
        .and_then(|prepared| prepared.with_replay(replay).send(connection, request))
    {
        Ok(pending) => pending,
        Err(error) => return Ok(Err(error)),
    };
    sent();
    drop(execution.take());
    let (result, turn) = pending.wait().await?;
    *execution = Some(turn);
    Ok(result)
}

/// The RPC owns lifecycle/ordering; the candidate owns replay data. The receive
/// hook seals that candidate at the response cut before any following live update.
async fn send_history<Req: JsonRpcRequest>(
    connection: &ConnectionTo<Agent>,
    ingress: Option<&IngressSender>,
    execution: &mut Option<ExecutionTurn>,
    state: &Arc<Mutex<BridgeState>>,
    owner: RequestOwner,
    request: Req,
) -> Result<Result<Req::Response, Error>, Error> {
    let session_id = owner
        .session_id
        .as_deref()
        .ok_or_else(|| runtime_state_error("load has no session"))?;
    let incarnation = owner
        .incarnation
        .ok_or_else(|| runtime_state_error("load has no incarnation"))?;
    let attempt_id = owner
        .attempt_id
        .as_deref()
        .ok_or_else(|| runtime_state_error("load has no attempt"))?;
    let replay = state
        .lock()
        .await
        .sessions
        .load_candidate(session_id, incarnation, attempt_id)
        .map_err(mirror_error)?;
    let result = send_ordered_with_replay(
        connection,
        ingress,
        execution,
        owner.clone(),
        RequestClass::Control,
        request,
        || {},
        Some(replay.clone()),
    )
    .await?;
    if ingress.is_some() && result.is_ok() {
        // Still holds the completion ticket. Following same-session events cannot
        // observe an intermediate mix of replay controls and the old baseline.
        let mut state = state.lock().await;
        state
            .sessions
            .load_candidate(session_id, incarnation, attempt_id)
            .map_err(mirror_error)?;
        let (mut validation, controls) = replay.take_controls();
        let current = session_validation_mut(&mut state, session_id)
            .ok_or_else(|| runtime_state_error("load lost its validation allocation"))?;
        validation.allocation = current.allocation.clone();
        *current = validation;
        if session_operation_pending(&state, session_id, SessionAdmission::Attachment) {
            let epoch = state.sessions.epoch().to_owned();
            for update in controls {
                state
                    .sessions
                    .append_attachment_candidate(&epoch, session_id, incarnation, update)
                    .map_err(runtime_state_error)?;
            }
        } else {
            let candidate = &mut session_ingest_mut(&mut state, session_id)
                .ok_or_else(|| runtime_state_error("load lost its resources"))?
                .attachment
                .control_candidate;
            candidate.bytes = controls.iter().map(serialized_value_len).sum();
            candidate.updates = controls;
        }
    }
    Ok(result)
}

// Every successful creation response has a provisional coordinator claim, even
// if response validation later fails. Drop rejects all early-return paths.
struct CreationFinish {
    ingress: Option<IngressSender>,
    owner: RequestOwner,
    target_id: Option<String>,
}

impl CreationFinish {
    fn new(
        ingress: Option<&IngressSender>,
        execution: &Option<ExecutionTurn>,
        owner: RequestOwner,
    ) -> Self {
        // Typed response decoding can fail after the raw response already claimed
        // a target. Read that raw boundary so rejection always releases its claim.
        let completion = execution.as_ref().and_then(ExecutionTurn::completion);
        let target_id = completion
            .and_then(|completion| completion.result.as_ref().ok())
            .and_then(|value| value.get("sessionId"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let owner = completion
            .map(|completion| completion.owner.clone())
            .unwrap_or(owner);
        Self {
            ingress: ingress.cloned(),
            owner,
            target_id,
        }
    }

    fn committed(&mut self, incarnation: u64) -> Result<(), Error> {
        if let (Some(ingress), Some(target_id)) = (&self.ingress, self.target_id.take()) {
            ingress.finish_creation(self.owner.clone(), target_id, Some(incarnation))?;
        }
        Ok(())
    }
}

impl Drop for CreationFinish {
    fn drop(&mut self) {
        if let (Some(ingress), Some(target_id)) = (&self.ingress, self.target_id.take()) {
            let _ = ingress.finish_creation(self.owner.clone(), target_id, None);
        }
    }
}

async fn handle_command(
    command: Value,
    connection: ConnectionTo<Agent>,
    context: CommandContext,
    prompt_start: Option<PromptStartGuard>,
    turn_responder: Option<TurnResponder>,
    business_responder: Option<BusinessResponder>,
    mut execution: Option<ExecutionTurn>,
) -> Result<(), Error> {
    let result = handle_command_inner(
        command.clone(),
        connection,
        context.clone(),
        prompt_start,
        turn_responder,
        business_responder,
        &mut execution,
    )
    .await;
    // Result publication, interaction retirement and the Observe materialization
    // cut are part of this local turn, including every early error path above.
    let terminal_releases = if context.ingress.is_none() || execution.is_some() {
        let mut state = context.state.lock().await;
        flush_runtime(&mut state, &context.sink);
        let releases = drain_runtime_effects(&mut state, &context.sink);
        let final_attempt = command
            .get("bridgeMaterializationFinalAttempt")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if command
            .get("bridgeManagedMaterialization")
            .and_then(Value::as_bool)
            == Some(true)
            && !final_attempt
            && !context.cancellation.is_cancelled()
            && let Err(error) = &result
            && retryable_reconcile_error(error)
        {
            context.sink.send(json!({
                "type": "bridge/session_sync",
                "sessionId": command.get("sessionId"),
                "phase": "retrying",
                "message": error.message,
                "retryAfterMs": command.get("bridgeMaterializationRetryAfterMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(RECONCILE_INITIAL_BACKOFF.as_millis() as u64),
            }));
        }
        if command
            .get("bridgeManagedMaterialization")
            .and_then(Value::as_bool)
            == Some(true)
            && (result
                .as_ref()
                .err()
                .is_none_or(|error| !retryable_reconcile_error(error))
                || final_attempt)
            && let (Some(session_id), Some(materialization_id)) = (
                command.get("sessionId").and_then(Value::as_str),
                command
                    .get("bridgeMaterializationId")
                    .and_then(Value::as_str),
            )
        {
            resolve_session_view_waiters_locked(
                &mut state,
                &context.sink,
                session_id,
                materialization_id,
                &result,
            );
        }
        releases
    } else {
        Vec::new()
    };
    drop(execution.take());
    if let Some(terminals) = &context.terminals {
        for (session_id, terminal_id) in terminal_releases {
            let _ = terminals
                .release(ReleaseTerminalRequest::new(session_id, terminal_id))
                .await;
        }
    }
    result
}

async fn handle_command_inner(
    command: Value,
    connection: ConnectionTo<Agent>,
    context: CommandContext,
    mut prompt_start: Option<PromptStartGuard>,
    turn_responder: Option<TurnResponder>,
    business_responder: Option<BusinessResponder>,
    execution: &mut Option<ExecutionTurn>,
) -> Result<(), Error> {
    if context.ingress.is_some() && execution.is_none() {
        return Err(Error::internal_error().data("ordered command has no execution ticket"));
    }
    let CommandContext {
        auto_close,
        options,
        prepared_attachment,
        ingress,
        state,
        sink,
        terminals,
        filesystem,
        auth_terminal,
        prompt_lifecycle,
        cancellation,
    } = context;
    let bridge_state = state.clone();
    let operation = nonempty_string_field(&command, "type")?;
    let epoch = state.lock().await.sessions.epoch().to_string();
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
            let response = send_ordered(
                &connection,
                ingress.as_ref(),
                execution,
                rpc_owner(&epoch, None, request_id, None),
                RequestClass::Control,
                AuthenticateRequest::new(method_id.to_string()),
            )
            .await??;
            relay_bytes(&response)?;
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
            let response = send_ordered(
                &connection,
                ingress.as_ref(),
                execution,
                rpc_owner(&epoch, None, request_id, None),
                RequestClass::Control,
                LogoutRequest::new(),
            )
            .await??;
            relay_bytes(&response)?;
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
                state.pending_creations += 1;
            }
            let request = NewSessionRequest::new(&cwd)
                .additional_directories(local_additional_directories(&options))
                .mcp_servers(options.mcp_servers.clone());
            let owner = rpc_owner(&epoch, None, &request_id, None);
            let result = send_ordered(
                &connection,
                ingress.as_ref(),
                execution,
                owner.clone(),
                RequestClass::Control,
                request,
            )
            .await?;
            let mut creation_finish = CreationFinish::new(ingress.as_ref(), execution, owner);
            let mut state = state.lock().await;
            state.pending_creations = state.pending_creations.saturating_sub(1);
            match result {
                Ok(response) => {
                    let session_id = response.session_id.0.to_string();
                    let response_value = serde_json::to_value(&response)?;
                    let tracked = relay_bytes(&response).and_then(|_| {
                        prepare_session_response(
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
                            close_rejected_chat_session(
                                &connection,
                                &session_id,
                                can_close,
                                ingress.as_ref(),
                                execution,
                                creation_finish.owner.clone(),
                            )
                            .await;
                            return Err(error);
                        }
                    };
                    let replay = notification_values(&early_updates);
                    let epoch = state.sessions.epoch().to_string();
                    let runtime_result = state.sessions.open_new_with_replay(
                        &epoch,
                        session_id.clone(),
                        cwd.clone(),
                        response_value,
                        replay,
                    );
                    let incarnation = match runtime_result {
                        Ok(incarnation) => incarnation,
                        Err(error) => {
                            discard_creation_staging(&mut state, &session_id);
                            let can_close = can_close_rejected_chat_session(&state, &session_id);
                            drop(state);
                            close_rejected_chat_session(
                                &connection,
                                &session_id,
                                can_close,
                                ingress.as_ref(),
                                execution,
                                creation_finish.owner.clone(),
                            )
                            .await;
                            return Err(runtime_state_error(error));
                        }
                    };
                    promote_creation_validation(&mut state, &session_id, incarnation)?;
                    if state.pending_creations == 0 {
                        clear_pending_creation_replays(&mut state);
                    }

                    session_mirror(&mut state).register_new(session_id.clone(), incarnation);
                    advance_catalog_revision(&mut state, &sink);
                    flush_runtime(&mut state, &sink);
                    let mirror_view = session_view_value(&mut state, &session_id, incarnation)?;
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
                    creation_finish.committed(incarnation)?;
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
                Some(Value::String(cursor)) if !cursor.is_empty() => Some(cursor.clone()),
                Some(_) => {
                    return Err(Error::invalid_params()
                        .data("session/list cursor must be a non-empty string"));
                }
            };
            let expected_revision = match command.get("expectedCatalogRevision") {
                None | Some(Value::Null) => None,
                Some(Value::String(revision)) if !revision.is_empty() => Some(revision.as_str()),
                Some(_) => {
                    return Err(Error::invalid_params()
                        .data("expectedCatalogRevision must be a non-empty string"));
                }
            };
            let list_gate = state.lock().await.session_list_gate.clone();
            let list_owner = rpc_owner(&epoch, None, &request_id, None);
            drop(execution.take());
            let _list_request = list_gate.lock().await;
            continue_execution(ingress.as_ref(), execution, &list_owner).await?;
            // Keep the Agent's cursor opaque. Business pagination separately binds
            // the whole chain to the directory revision of its first page.
            let catalog_revision = {
                let state = state.lock().await;
                if let Some(expected) = expected_revision {
                    require_catalog_revision(&state, expected)?;
                }
                session_catalog_revision(&state)
            };
            // Startup cwd is the new-session default, not a discovery filter.
            // Existing sessions retain the cwd returned by the Agent.
            let result = send_ordered(
                &connection,
                ingress.as_ref(),
                execution,
                rpc_owner(&epoch, None, &request_id, None),
                RequestClass::Control,
                ListSessionsRequest::new().cursor(cursor.clone()),
            )
            .await?;
            let mut state = state.lock().await;
            let response = result?;
            require_catalog_revision(&state, &catalog_revision)?;
            validate_session_list_page(&response, cursor.as_deref(), &state)?;
            if cursor.is_none() {
                state.listed_sessions.clear();
            }
            for session in &response.sessions {
                state
                    .listed_sessions
                    .insert(session.session_id.0.to_string(), session.clone());
            }
            drop(state);
            if let Some(responder) = &business_responder {
                let mut response = serde_json::to_value(&response)?;
                response["catalogRevision"] = json!(catalog_revision);
                responder.success(response);
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
            let requested_cwd = command.get("cwd").and_then(Value::as_str);
            let reservation = if let Some(prepared) = prepared_attachment
                .as_ref()
                .filter(|prepared| prepared.operation_id == request_id)
            {
                let state = state.lock().await;
                let incarnation = prepared.reservation.incarnation();
                let owns = state.sessions.state(&session_id).is_some_and(|session| {
                    session.incarnation == incarnation
                        && session.operation.as_ref().is_some_and(|owner| {
                            owner.operation_id == request_id
                                && owner.kind == attachment_kind
                                && matches!(owner.stage.as_str(), "attaching" | "reloading")
                        })
                });
                if !owns {
                    return Err(Error::invalid_request().data("prepared attachment owner changed"));
                }
                prepared.reservation.clone()
            } else {
                reserve_attachment_with_cwd(
                    &session_id,
                    attachment_kind,
                    requested_cwd,
                    &request_id,
                    command
                        .get("bridgeMaterializationId")
                        .and_then(Value::as_str),
                    &state,
                )
                .await?
            };
            let validation_owner = replay_validation_backup(&*state.lock().await, &session_id)
                .expect("attachment reservation owns validation")
                .owner
                .clone();
            let cwd = reservation.cwd().clone();
            let reload = reservation.is_reload();
            state
                .lock()
                .await
                .sessions
                .resources_mut(&session_id, reservation.incarnation())
                .map_err(mirror_error)?
                .attachment
                .subscriber = Some(0);
            let attachment_incarnation = {
                let mut state = state.lock().await;
                let incarnation = reservation.incarnation();
                {
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
                        clear_attachment_tracking(
                            &mut state,
                            &session_id,
                            &validation_owner,
                            false,
                        );
                        return Err(error);
                    }
                }
                flush_runtime(&mut state, &sink);
                incarnation
            };
            let owner = rpc_owner(
                &epoch,
                Some((&session_id, attachment_incarnation)),
                &request_id,
                Some(&request_id),
            );
            let result = if operation == "session/load" {
                send_history(
                    &connection,
                    ingress.as_ref(),
                    execution,
                    &state,
                    owner.clone(),
                    LoadSessionRequest::new(session_id.clone(), &cwd)
                        .additional_directories(local_additional_directories(&options))
                        .mcp_servers(options.mcp_servers.clone()),
                )
                .await?
                .map(|response| serde_json::to_value(response).expect("ACP response serializes"))
            } else {
                send_history(
                    &connection,
                    ingress.as_ref(),
                    execution,
                    &state,
                    owner.clone(),
                    ResumeSessionRequest::new(session_id.clone(), &cwd)
                        .additional_directories(local_additional_directories(&options))
                        .mcp_servers(options.mcp_servers.clone()),
                )
                .await?
                .map(|response| serde_json::to_value(response).expect("ACP response serializes"))
            };
            let mut state = state.lock().await;
            if !validation_owner.matches(&state, &session_id) {
                return Err(Error::invalid_request().data("session attachment owner changed"));
            }
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
                    {
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
                        if !reload
                            && error.code == agent_client_protocol::ErrorCode::ResourceNotFound
                        {
                            session_mirror(&mut state).remove(&session_id, attachment_incarnation);
                            state.listed_sessions.remove(&session_id);
                        }
                    }
                    clear_attachment_tracking(&mut state, &session_id, &validation_owner, true);
                    return Err(error);
                }
            };
            if let Err(error) = relay_bytes(&response) {
                fail_runtime_attachment(
                    &mut state,
                    &session_id,
                    attachment_incarnation,
                    &request_id,
                    attachment_kind,
                    reload,
                    &error,
                )?;
                {
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
                clear_attachment_tracking(&mut state, &session_id, &validation_owner, true);
                return Err(error);
            }
            let tracked = if reload {
                let result = response_controls(&response).and_then(|(modes, _)| {
                    let validation = session_validation(&state, &session_id).ok_or_else(|| {
                        runtime_state_error("missing same-session reload validation state")
                    })?;
                    if let Some(reason) = &validation.invalid_reason {
                        return Err(semantic_error(format!(
                            "Agent session replay was invalid: {reason}"
                        )));
                    }
                    validate_replay_control_references(validation, modes.as_ref())?;
                    let active = state.sessions.active_session(&session_id).ok_or_else(|| {
                        runtime_state_error("same-session reload lost its active session")
                    })?;
                    if active.state.incarnation != attachment_incarnation {
                        return Err(runtime_state_error(
                            "same-session reload incarnation changed before commit",
                        ));
                    }
                    Ok(())
                });
                result
            } else {
                prepare_session_response(
                    &mut state,
                    &session_id,
                    cwd.clone(),
                    &response,
                    Some((attachment_incarnation, request_id.as_str())),
                )
                .map(|_| ())
            };
            match tracked {
                Ok(()) => {}
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
                    {
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
                    clear_attachment_tracking(&mut state, &session_id, &validation_owner, true);
                    return Err(error);
                }
            };
            let epoch = state.sessions.epoch().to_string();
            let resume_cache = if operation == "session/load" {
                session_mirror(&mut state)
                    .commit_load(&session_id, attachment_incarnation, &request_id)
                    .map_err(mirror_error)?;
                None
            } else {
                Some(
                    session_mirror(&mut state)
                        .commit_attachment_cache(&session_id, attachment_incarnation, &request_id)
                        .map_err(mirror_error)?,
                )
            };
            let completed = if reload {
                state.sessions.complete_reload(
                    &epoch,
                    &session_id,
                    attachment_incarnation,
                    &request_id,
                    response.clone(),
                )
            } else {
                state.sessions.complete_attachment(
                    &epoch,
                    &session_id,
                    attachment_incarnation,
                    &request_id,
                    attachment_kind,
                    response.clone(),
                )
            };
            if let Err(error) = completed {
                clear_attachment_tracking(&mut state, &session_id, &validation_owner, true);
                return Err(runtime_state_error(error));
            }
            commit_replay_validation(&mut state, &session_id, &validation_owner);
            release_session_operation(
                &mut state,
                &session_id,
                attachment_incarnation,
                SessionAdmission::Attachment,
                &request_id,
            );
            flush_runtime(&mut state, &sink);
            let attachment_subscriber =
                clear_attachment_delivery(&mut state, &session_id, attachment_incarnation)
                    .unwrap_or(0);
            let mirror_view = (operation == "session/load")
                .then(|| session_view_value(&mut state, &session_id, attachment_incarnation))
                .transpose()?;
            if let Some(view) = mirror_view {
                sink.send(json!({
                    "type": "bridge/session_view",
                    "sessionId": session_id,
                    "view": view,
                }));
            }
            drop(state);
            if operation == "session/resume" {
                synchronize_attached_history(
                    &connection,
                    &options,
                    &bridge_state,
                    &sink,
                    &cancellation,
                    &session_id,
                    attachment_incarnation,
                    AttachmentHistoryFallback::Received {
                        updates: resume_cache.as_ref().map(|snapshot| snapshot.updates()).unwrap_or(&[]),
                        notice: "Only context received while resuming is available; earlier history may be missing.",
                    },
                    ingress.as_ref(), execution, &request_id,
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
            let (cwd, source_incarnation, source_baseline) = {
                let mut state = state.lock().await;
                let source = state.sessions.active_session(&source_id).ok_or_else(|| {
                    Error::invalid_params()
                        .data(format!("unknown or inactive session: {source_id}"))
                })?;
                let cwd = source.live.cwd.clone();
                let source_incarnation = source.state.incarnation;
                let source_baseline = session_mirror(&mut state)
                    .view(&source_id, source_incarnation)
                    .map_err(mirror_error)?
                    .baseline;
                reserve_session_operation(
                    &mut state,
                    &source_id,
                    RuntimeSessionOperationKind::Fork,
                    &request_id,
                )?;
                let epoch = state.sessions.epoch().to_string();
                if let Err(error) = state.sessions.start_operation(
                    &epoch,
                    &source_id,
                    source_incarnation,
                    request_id.clone(),
                    RuntimeSessionOperationKind::Fork,
                    "forking",
                ) {
                    release_session_operation(
                        &mut state,
                        &source_id,
                        source_incarnation,
                        SessionAdmission::Fork,
                        &request_id,
                    );
                    return Err(runtime_state_error(error));
                }
                flush_runtime(&mut state, &sink);
                state.pending_creations += 1;
                (cwd, source_incarnation, source_baseline)
            };
            sink.send(json!({
                "type": "bridge/session_operation_started",
                "requestId": request_id,
                "sessionId": source_id,
                "operation": "fork",
            }));
            let owner = rpc_owner(
                &epoch,
                Some((&source_id, source_incarnation)),
                &request_id,
                None,
            );
            let result = send_ordered(
                &connection,
                ingress.as_ref(),
                execution,
                owner.clone(),
                RequestClass::Control,
                ForkSessionRequest::new(source_id.clone(), &cwd)
                    .additional_directories(local_additional_directories(&options))
                    .mcp_servers(options.mcp_servers.clone()),
            )
            .await?;
            let mut creation_finish = CreationFinish::new(ingress.as_ref(), execution, owner);
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
                    release_session_operation(
                        &mut state,
                        &source_id,
                        source_incarnation,
                        SessionAdmission::Fork,
                        &request_id,
                    );
                    if state.pending_creations == 0 {
                        clear_pending_creation_replays(&mut state);
                    }
                    return Err(error);
                }
            };
            let session_id = response.session_id.0.to_string();
            let response_value = serde_json::to_value(&response)?;
            let tracked = relay_bytes(&response).and_then(|_| {
                if session_id == source_id {
                    Err(Error::invalid_request()
                        .data("Agent returned the source session ID for session/fork"))
                } else {
                    prepare_session_response(
                        &mut state,
                        &session_id,
                        cwd.clone(),
                        &response_value,
                        None,
                    )
                }
            });
            let early_updates = match tracked {
                Ok(updates) => updates,
                Err(error) => {
                    let epoch = state.sessions.epoch().to_string();
                    state
                        .sessions
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
                    release_session_operation(
                        &mut state,
                        &source_id,
                        source_incarnation,
                        SessionAdmission::Fork,
                        &request_id,
                    );
                    let can_close = can_close_rejected_chat_session(&state, &session_id);
                    if state.pending_creations == 0 {
                        clear_pending_creation_replays(&mut state);
                    }
                    drop(state);
                    close_rejected_chat_session(
                        &connection,
                        &session_id,
                        can_close,
                        ingress.as_ref(),
                        execution,
                        creation_finish.owner.clone(),
                    )
                    .await;
                    return Err(error);
                }
            };
            let target_replay = conversation_notification_values(&early_updates);
            let fork_cache = (!target_replay.is_empty()).then(|| target_replay.clone());
            let cache_notice = if target_replay.is_empty() {
                "Showing a snapshot of the source session from memory; branch history may differ."
            } else {
                "Only context received while forking is available; earlier branch history may be missing."
            };
            let target_replay = Some(notification_values(&early_updates));
            let epoch = state.sessions.epoch().to_string();
            let runtime_result = state.sessions.open_forked(
                &epoch,
                session_id.clone(),
                cwd.clone(),
                response_value,
                (&source_id, source_incarnation),
                target_replay,
            );
            let incarnation = match runtime_result {
                Ok(incarnation) => incarnation,
                Err(error) => {
                    discard_creation_staging(&mut state, &session_id);
                    state
                        .sessions
                        .fail_operation(
                            &epoch,
                            &source_id,
                            source_incarnation,
                            &request_id,
                            RuntimeSessionOperationKind::Fork,
                            json!({ "message": format!("{error:?}") }),
                        )
                        .map_err(runtime_state_error)?;
                    release_session_operation(
                        &mut state,
                        &source_id,
                        source_incarnation,
                        SessionAdmission::Fork,
                        &request_id,
                    );
                    let can_close = can_close_rejected_chat_session(&state, &session_id);
                    drop(state);
                    close_rejected_chat_session(
                        &connection,
                        &session_id,
                        can_close,
                        ingress.as_ref(),
                        execution,
                        creation_finish.owner.clone(),
                    )
                    .await;
                    return Err(runtime_state_error(error));
                }
            };
            promote_creation_validation(&mut state, &session_id, incarnation)?;
            if state.pending_creations == 0 {
                clear_pending_creation_replays(&mut state);
            }

            session_mirror(&mut state).register_cold(session_id.clone(), incarnation);
            // The response claim is about to release subsequent target messages.
            // Give those messages an installed baseline before making the target routable.
            let cached_updates = fork_cache
                .as_deref()
                .unwrap_or_else(|| source_baseline.updates());
            let notice = attachment_cache_notice(cached_updates, cache_notice);
            session_mirror(&mut state)
                .use_cached_history(&session_id, incarnation, cached_updates, notice.to_string())
                .map_err(mirror_error)?;
            state
                .sessions
                .complete_operation(
                    &epoch,
                    &source_id,
                    source_incarnation,
                    &request_id,
                    RuntimeSessionOperationKind::Fork,
                    serde_json::to_value(&response)?,
                )
                .map_err(runtime_state_error)?;
            release_session_operation(
                &mut state,
                &source_id,
                source_incarnation,
                SessionAdmission::Fork,
                &request_id,
            );
            advance_catalog_revision(&mut state, &sink);
            flush_runtime(&mut state, &sink);
            creation_finish.committed(incarnation)?;
            drop(state);
            let target_owner =
                rpc_owner(&epoch, Some((&session_id, incarnation)), &request_id, None);
            continue_execution(ingress.as_ref(), execution, &target_owner).await?;
            synchronize_attached_history(
                &connection,
                &options,
                &bridge_state,
                &sink,
                &cancellation,
                &session_id,
                incarnation,
                AttachmentHistoryFallback::Installed,
                ingress.as_ref(),
                execution,
                &request_id,
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
            let incarnation = {
                let mut state = state.lock().await;
                if command
                    .get("expectedIncarnation")
                    .and_then(Value::as_u64)
                    .is_some_and(|expected| {
                        state
                            .sessions
                            .active_session(&session_id)
                            .is_none_or(|session| session.state.incarnation != expected)
                    })
                {
                    if let Some(permit) = &auto_close {
                        permit.cancel();
                    }
                    return Ok(());
                }
                let incarnation = reserve_session_operation(
                    &mut state,
                    &session_id,
                    RuntimeSessionOperationKind::Close,
                    &request_id,
                )?;

                let epoch = state.sessions.epoch().to_string();
                if let Err(error) = state.sessions.start_operation(
                    &epoch,
                    &session_id,
                    incarnation,
                    request_id.clone(),
                    RuntimeSessionOperationKind::Close,
                    "closing",
                ) {
                    release_session_operation(
                        &mut state,
                        &session_id,
                        incarnation,
                        SessionAdmission::Close,
                        &request_id,
                    );
                    return Err(runtime_state_error(error));
                }
                if let Some(permit) = &auto_close {
                    if !permit.claim() {
                        state
                            .sessions
                            .fail_operation(
                                &epoch,
                                &session_id,
                                incarnation,
                                &request_id,
                                RuntimeSessionOperationKind::Close,
                                json!({"message":"observer returned"}),
                            )
                            .map_err(runtime_state_error)?;
                        release_session_operation(
                            &mut state,
                            &session_id,
                            incarnation,
                            SessionAdmission::Close,
                            &request_id,
                        );
                        return Ok(());
                    }
                }
                flush_runtime(&mut state, &sink);
                incarnation
            };
            sink.send(json!({
                "type": "bridge/session_operation_started",
                "requestId": request_id,
                "sessionId": session_id,
                "operation": "close",
            }));
            let owner = rpc_owner(&epoch, Some((&session_id, incarnation)), &request_id, None);
            let result = send_ordered(
                &connection,
                ingress.as_ref(),
                execution,
                owner.clone(),
                RequestClass::Control,
                CloseSessionRequest::new(session_id.clone()),
            )
            .await?;
            if let Err(error) = result {
                let mut state = state.lock().await;

                settle_runtime_operation_request_error(
                    &mut state,
                    &session_id,
                    incarnation,
                    &request_id,
                    RuntimeSessionOperationKind::Close,
                    &error,
                )?;
                release_session_operation(
                    &mut state,
                    &session_id,
                    incarnation,
                    SessionAdmission::Close,
                    &request_id,
                );
                return Err(error);
            }
            let terminal_releases = {
                let mut state = state.lock().await;

                let epoch = state.sessions.epoch().to_string();
                state
                    .sessions
                    .close_session(&epoch, &session_id, incarnation, &request_id)
                    .map_err(runtime_state_error)?;
                clear_ingest_resources(&mut state, &session_id, incarnation);
                cancel_interactions_locked(
                    &session_id,
                    incarnation,
                    "session_closed",
                    &mut state,
                    &sink,
                );
                flush_runtime(&mut state, &sink);
                drain_runtime_effects(&mut state, &sink)
            };
            drop(execution.take());
            release_runtime_terminals(terminals.as_ref(), terminal_releases).await;
            if let Some(terminals) = &terminals {
                terminals.release_session(&session_id).await;
            }
            continue_execution(ingress.as_ref(), execution, &owner).await?;
            {
                let mut state = state.lock().await;
                complete_session_retirement(
                    &mut state,
                    &sink,
                    &session_id,
                    incarnation,
                    SessionAdmission::Close,
                    &request_id,
                );
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
            let deletion = reserve_deletion(&mut *state.lock().await, &session_id, &request_id)?;
            let DeletionReservation::Session(incarnation) = deletion else {
                // An unopened history row is a catalog operation. Keep only the
                // ID reservation; its RPC and completion use the global lane.
                sink.send(json!({
                    "type": "bridge/session_operation_started", "requestId": request_id,
                    "sessionId": session_id, "operation": "delete",
                }));
                let result = send_ordered(
                    &connection,
                    ingress.as_ref(),
                    execution,
                    rpc_owner(&epoch, None, &request_id, None),
                    RequestClass::Control,
                    DeleteSessionRequest::new(session_id.clone()),
                )
                .await?;
                let mut locked = state.lock().await;
                release_deletion(&mut locked, &session_id, deletion, &request_id);
                result?;
                locked.listed_sessions.remove(&session_id);
                advance_catalog_revision(&mut locked, &sink);
                sink.send(json!({
                    "type": "acp/session_deleted", "requestId": request_id, "sessionId": session_id,
                }));
                return Ok(());
            };
            let (close_before_delete, runtime_incarnation) = {
                let mut state = state.lock().await;
                let close_before_delete = state.sessions.active_session(&session_id).is_some()
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
                        release_deletion(&mut state, &session_id, deletion, &request_id);
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
            let owner = rpc_owner(&epoch, Some((&session_id, incarnation)), &request_id, None);
            if close_before_delete {
                let mut close_owner = owner.clone();
                close_owner.attempt_id = Some(format!("delete-close-{}", Uuid::new_v4()));
                let close_result = send_ordered(
                    &connection,
                    ingress.as_ref(),
                    execution,
                    close_owner,
                    RequestClass::Control,
                    CloseSessionRequest::new(session_id.clone()),
                )
                .await?;
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
                        take_sync_control_candidate(&mut state, &session_id);
                    }
                    release_deletion(&mut state, &session_id, deletion, &request_id);
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

                    clear_ingest_resources(&mut state, &session_id, incarnation);
                }
                let terminal_releases = {
                    let mut state = state.lock().await;
                    cancel_interactions_locked(
                        &session_id,
                        incarnation,
                        "session_closed",
                        &mut state,
                        &sink,
                    );
                    flush_runtime(&mut state, &sink);
                    drain_runtime_effects(&mut state, &sink)
                };
                drop(execution.take());
                release_runtime_terminals(terminals.as_ref(), terminal_releases).await;
                if let Some(terminals) = &terminals {
                    terminals.release_session(&session_id).await;
                }
                continue_execution(ingress.as_ref(), execution, &owner).await?;
                retire_session_observation(
                    &mut *state.lock().await,
                    &sink,
                    &session_id,
                    incarnation,
                    "closed",
                );
                sink.send(json!({
                    "type": "acp/session_closed",
                    "requestId": request_id,
                    "sessionId": session_id,
                }));
            }
            let mut delete_owner = owner.clone();
            delete_owner.attempt_id = Some(format!("delete-{}", Uuid::new_v4()));
            let result = send_ordered(
                &connection,
                ingress.as_ref(),
                execution,
                delete_owner,
                RequestClass::Control,
                DeleteSessionRequest::new(session_id.clone()),
            )
            .await?;
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
                release_deletion(&mut state, &session_id, deletion, &request_id);
                return Err(error);
            }
            if let Some(incarnation) = runtime_incarnation {
                let mut state = state.lock().await;
                let epoch = state.sessions.epoch().to_string();
                state
                    .sessions
                    .complete_delete(&epoch, &session_id, incarnation, &request_id)
                    .map_err(runtime_state_error)?;
                take_sync_control_candidate(&mut state, &session_id);
            }
            if !close_before_delete {
                {
                    let mut state = state.lock().await;

                    clear_ingest_resources(&mut state, &session_id, incarnation);
                }
                let terminal_releases = {
                    let mut state = state.lock().await;
                    cancel_interactions_locked(
                        &session_id,
                        incarnation,
                        "session_closed",
                        &mut state,
                        &sink,
                    );
                    flush_runtime(&mut state, &sink);
                    drain_runtime_effects(&mut state, &sink)
                };
                drop(execution.take());
                release_runtime_terminals(terminals.as_ref(), terminal_releases).await;
                if let Some(terminals) = &terminals {
                    terminals.release_session(&session_id).await;
                }
                continue_execution(ingress.as_ref(), execution, &owner).await?;
            }
            {
                let mut state = state.lock().await;
                complete_session_retirement(
                    &mut state,
                    &sink,
                    &session_id,
                    incarnation,
                    SessionAdmission::Delete,
                    &request_id,
                );
                state.listed_sessions.remove(&session_id);
                advance_catalog_revision(&mut state, &sink);
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
            let (incarnation, mirror_operation_id) = {
                let mut state = state.lock().await;
                let incarnation = state
                    .sessions
                    .active_session(&session_id)
                    .map(|session| session.state.incarnation)
                    .ok_or_else(|| {
                        Error::invalid_params()
                            .data(format!("unknown or inactive session: {session_id}"))
                    })?;
                let epoch = state.sessions.epoch().to_string();
                let mirror_operation_id = match state
                    .sessions
                    .admit_session_turn(
                        &epoch,
                        &session_id,
                        incarnation,
                        expected_history_revision,
                        &client_intent_id,
                        runtime_prompt,
                    )
                    .map_err(|error| match error {
                        SessionTurnError::History(error) => mirror_error(error),
                        SessionTurnError::Live(error) => runtime_state_error(error),
                    })? {
                    TurnAdmission::Accepted { operation_id } => operation_id,
                    TurnAdmission::Duplicate { operation_id } => {
                        turn_responder.success(json!({
                            "operationId": operation_id,
                            "disposition": "duplicate",
                        }));
                        return Ok(());
                    }
                };
                flush_runtime(&mut state, &sink);
                let view = session_view_value(&mut state, &session_id, incarnation)?;
                sink.send(json!({
                    "type": "bridge/session_view", "sessionId": session_id, "view": view,
                }));
                sink.send(json!({
                    "type": "acp/prompt_started", "requestId": request_id,
                    "sessionId": session_id, "prompt": prompt,
                }));
                (incarnation, mirror_operation_id)
            };
            turn_responder.success(json!({
                "operationId": mirror_operation_id,
                "disposition": "accepted",
            }));
            let prompt_for_outcome = serde_json::to_value(&prompt)?;
            let owner = rpc_owner(&epoch, Some((&session_id, incarnation)), &request_id, None);
            let result = send_ordered_then(
                &connection,
                ingress.as_ref(),
                execution,
                owner,
                RequestClass::LongRunning,
                PromptRequest::new(session_id.clone(), prompt),
                || {
                    if let Some(prompt_start) = prompt_start.as_mut() {
                        prompt_start.finish();
                    }
                },
            )
            .await?;
            let completion = {
                let mut state = state.lock().await;
                let completion = commit_completed_turn_history(
                    &mut state,
                    &sink,
                    PromptCompletion {
                        session_id: &session_id,
                        incarnation,
                        runtime_operation_id: &runtime_operation_id,
                        operation_id: &mirror_operation_id,
                        client_intent_id: &client_intent_id,
                        prompt: prompt_for_outcome,
                        result,
                    },
                );
                finish_prompt_operation(&prompt_lifecycle);
                completion
            };
            drop(execution.take());
            if let Some(terminals) = &terminals {
                for (session_id, terminal_id) in completion.terminal_releases {
                    let _ = terminals
                        .release(ReleaseTerminalRequest::new(session_id, terminal_id))
                        .await;
                }
            }
            completion.result?;
        }
        "session/cancel" => {
            let session_id = string_field(&command, "sessionId")?;
            let expected_operation_id = command.get("expectedOperationId").and_then(Value::as_str);
            {
                let mut state = state.lock().await;
                let incarnation =
                    live_session_incarnation(&state, session_id).ok_or_else(|| {
                        Error::invalid_params()
                            .data(format!("unknown or inactive session: {session_id}"))
                    })?;
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
                    .sessions
                    .state(session_id)
                    .and_then(|session| session.active_turn.as_ref())
                    .and_then(|turn| turn.execution.as_ref())
                    .is_none()
                {
                    return Err(Error::invalid_request().data("session has no active prompt"));
                }
                connection.send_notification(CancelNotification::new(session_id.to_string()))?;
                commit_session_cancel(&mut state, &sink, session_id, incarnation)?;
            };
        }
        "session/set_mode" => {
            let request_id = string_field(&command, "requestId")?.to_string();
            let session_id = string_field(&command, "sessionId")?.to_string();
            let mode_id = string_field(&command, "modeId")?.to_string();
            let incarnation = {
                let mut state = state.lock().await;
                let session = state.sessions.active_session(&session_id).ok_or_else(|| {
                    Error::invalid_params()
                        .data(format!("unknown or inactive session: {session_id}"))
                })?;
                validate_session_mode_reference(session.live.modes(), &mode_id)
                    .map_err(semantic_error)?;
                let incarnation = session.state.incarnation;
                reserve_session_operation(
                    &mut state,
                    &session_id,
                    RuntimeSessionOperationKind::SetMode,
                    &request_id,
                )?;
                let epoch = state.sessions.epoch().to_string();
                if let Err(error) = state.sessions.start_operation(
                    &epoch,
                    &session_id,
                    incarnation,
                    request_id.clone(),
                    RuntimeSessionOperationKind::SetMode,
                    "setting_mode",
                ) {
                    release_session_operation(
                        &mut state,
                        &session_id,
                        incarnation,
                        SessionAdmission::Control,
                        &request_id,
                    );
                    return Err(runtime_state_error(error));
                }
                flush_runtime(&mut state, &sink);
                incarnation
            };
            sink.send(json!({
                "type": "bridge/session_operation_started",
                "requestId": request_id,
                "sessionId": session_id,
                "operation": "mode",
            }));
            let result = send_ordered(
                &connection,
                ingress.as_ref(),
                execution,
                rpc_owner(&epoch, Some((&session_id, incarnation)), &request_id, None),
                RequestClass::Control,
                SetSessionModeRequest::new(session_id.clone(), mode_id.clone()),
            )
            .await?;
            let response = match result {
                Ok(response) => response,
                Err(error) => {
                    let mut state = state.lock().await;

                    settle_runtime_operation_request_error(
                        &mut state,
                        &session_id,
                        incarnation,
                        &request_id,
                        RuntimeSessionOperationKind::SetMode,
                        &error,
                    )?;
                    release_session_operation(
                        &mut state,
                        &session_id,
                        incarnation,
                        SessionAdmission::Control,
                        &request_id,
                    );
                    return Err(error);
                }
            };
            if let Err(error) = relay_bytes(&response) {
                let mut state = state.lock().await;

                let epoch = state.sessions.epoch().to_string();
                state
                    .sessions
                    .fail_operation(
                        &epoch,
                        &session_id,
                        incarnation,
                        &request_id,
                        RuntimeSessionOperationKind::SetMode,
                        json!({ "message": error.message.clone(), "data": error.data.clone() }),
                    )
                    .map_err(runtime_state_error)?;
                release_session_operation(
                    &mut state,
                    &session_id,
                    incarnation,
                    SessionAdmission::Control,
                    &request_id,
                );
                return Err(error);
            }
            {
                let mut state = state.lock().await;

                let epoch = state.sessions.epoch().to_string();
                state
                    .sessions
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
                release_session_operation(
                    &mut state,
                    &session_id,
                    incarnation,
                    SessionAdmission::Control,
                    &request_id,
                );
                let business_delta = advance_session_delta(
                    &mut state,
                    &session_id,
                    incarnation,
                    json!({
                        "kind": "control_update",
                        "control": "mode",
                        "modeId": mode_id,
                    }),
                )?;
                sink.send(business_delta);
                sink.send(json!({
                    "type": "acp/mode_changed",
                    "requestId": request_id,
                    "sessionId": session_id,
                    "modeId": mode_id,
                }));
            }
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
            let incarnation = {
                let mut state = state.lock().await;
                let session = state.sessions.active_session(&session_id).ok_or_else(|| {
                    Error::invalid_params()
                        .data(format!("unknown or inactive session: {session_id}"))
                })?;
                validate_session_config_reference(
                    session.live.config_options(),
                    &config_id,
                    &value,
                )
                .map_err(semantic_error)?;
                let incarnation = session.state.incarnation;
                reserve_session_operation(
                    &mut state,
                    &session_id,
                    RuntimeSessionOperationKind::SetConfig,
                    &request_id,
                )?;
                let epoch = state.sessions.epoch().to_string();
                if let Err(error) = state.sessions.start_operation(
                    &epoch,
                    &session_id,
                    incarnation,
                    request_id.clone(),
                    RuntimeSessionOperationKind::SetConfig,
                    "setting_config",
                ) {
                    release_session_operation(
                        &mut state,
                        &session_id,
                        incarnation,
                        SessionAdmission::Control,
                        &request_id,
                    );
                    return Err(runtime_state_error(error));
                }
                flush_runtime(&mut state, &sink);
                incarnation
            };
            sink.send(json!({
                "type": "bridge/session_operation_started",
                "requestId": request_id,
                "sessionId": session_id,
                "operation": "config",
            }));
            let result = send_ordered(
                &connection,
                ingress.as_ref(),
                execution,
                rpc_owner(&epoch, Some((&session_id, incarnation)), &request_id, None),
                RequestClass::Control,
                request,
            )
            .await?;
            let response = match result {
                Ok(response) => response,
                Err(error) => {
                    let mut state = state.lock().await;

                    settle_runtime_operation_request_error(
                        &mut state,
                        &session_id,
                        incarnation,
                        &request_id,
                        RuntimeSessionOperationKind::SetConfig,
                        &error,
                    )?;
                    release_session_operation(
                        &mut state,
                        &session_id,
                        incarnation,
                        SessionAdmission::Control,
                        &request_id,
                    );
                    return Err(error);
                }
            };
            if let Err(error) = relay_bytes(&response) {
                let mut state = state.lock().await;

                let epoch = state.sessions.epoch().to_string();
                state
                    .sessions
                    .fail_operation(
                        &epoch,
                        &session_id,
                        incarnation,
                        &request_id,
                        RuntimeSessionOperationKind::SetConfig,
                        json!({ "message": error.message.clone(), "data": error.data.clone() }),
                    )
                    .map_err(runtime_state_error)?;
                release_session_operation(
                    &mut state,
                    &session_id,
                    incarnation,
                    SessionAdmission::Control,
                    &request_id,
                );
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

                let epoch = state.sessions.epoch().to_string();
                state
                    .sessions
                    .fail_operation(
                        &epoch,
                        &session_id,
                        incarnation,
                        &request_id,
                        RuntimeSessionOperationKind::SetConfig,
                        json!({ "message": error.message.clone(), "data": error.data.clone() }),
                    )
                    .map_err(runtime_state_error)?;
                release_session_operation(
                    &mut state,
                    &session_id,
                    incarnation,
                    SessionAdmission::Control,
                    &request_id,
                );
                return Err(error);
            }
            {
                let mut state = state.lock().await;

                let epoch = state.sessions.epoch().to_string();
                state
                    .sessions
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
                release_session_operation(
                    &mut state,
                    &session_id,
                    incarnation,
                    SessionAdmission::Control,
                    &request_id,
                );
                let business_delta = advance_session_delta(
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
                )?;
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
        }
        "permission/respond" => {
            let request_id = string_field(&command, "requestId")?.to_string();
            let permission_id = string_field(&command, "permissionId")?.to_string();
            let response: RequestPermissionResponse = serde_json::from_value(json!({
                "outcome": command.get("outcome").cloned().ok_or_else(|| Error::invalid_params().data("permission outcome is required"))?,
            }))?;
            let mut locked = state.lock().await;
            let (owner, pending) = locked.sessions.permission(&permission_id).ok_or_else(|| {
                Error::invalid_params().data("permission request is no longer pending")
            })?;
            if let Some(expected_session_id) = command.get("sessionId").and_then(Value::as_str)
                && expected_session_id != owner.session_id
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
            let owner = owner.clone();
            locked
                .sessions
                .begin_permission_response(
                    &owner.epoch,
                    &owner.session_id,
                    owner.incarnation,
                    &permission_id,
                    request_id.clone(),
                )
                .map_err(runtime_state_error)?;
            let pending = locked
                .sessions
                .take_permission(&permission_id, &owner)
                .expect("exact permission owner checked under the same lock");
            flush_runtime(&mut locked, &sink);
            // oneshot delivery is synchronous: keep owner retirement and canonical
            // completion ordered before another request can replace this incarnation.
            let delivered = pending.sender.send(response).is_ok();
            locked
                .sessions
                .complete_permission_response(
                    &owner.epoch,
                    &owner.session_id,
                    owner.incarnation,
                    &permission_id,
                    &request_id,
                )
                .map_err(runtime_state_error)?;
            let business_delta = advance_session_delta(
                &mut locked,
                &owner.session_id,
                owner.incarnation,
                json!({
                    "kind": "interaction_remove", "interactionId": permission_id,
                }),
            )?;
            flush_runtime(&mut locked, &sink);
            sink.send(business_delta);
            relay_interaction_resolution(
                &sink,
                json!({
                    "type": "acp/permission_resolved", "permissionId": permission_id, "requestId": request_id,
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
            let (owner, pending) = pending_elicitation(&locked, &elicitation_id)
                .ok_or_else(|| Error::invalid_params().data("elicitation is no longer pending"))?;
            if let Some(expected_session_id) = command.get("sessionId").and_then(Value::as_str)
                && owner.map(|owner| owner.session_id.as_str()) != Some(expected_session_id)
            {
                return Err(Error::invalid_params()
                    .data("elicitation does not belong to the requested session"));
            }
            validate_elicitation_response_value(&pending.request, &response_value)
                .map_err(semantic_error)?;
            let response: CreateElicitationResponse = serde_json::from_value(response_value)?;
            let accepted_url_id = matches!(&response.action, ElicitationAction::Accept(_))
                .then(|| pending.url_elicitation_id.clone())
                .flatten();
            let owner = owner.cloned();
            let runtime_scope = owner
                .as_ref()
                .map(|owner| (owner.session_id.as_str(), owner.incarnation));
            let epoch = locked.sessions.epoch().to_string();
            locked
                .sessions
                .begin_elicitation_response(
                    &epoch,
                    runtime_scope,
                    &elicitation_id,
                    request_id.clone(),
                )
                .map_err(runtime_state_error)?;
            let pending = take_pending_elicitation(&mut locked, &elicitation_id, owner.as_ref())
                .expect("exact elicitation owner checked under the same lock");
            flush_runtime(&mut locked, &sink);
            let delivered = pending.sender.send(response.clone()).is_ok();
            // Immediate Agent completion/reuse cannot overtake accepted URL registration.
            locked
                .sessions
                .complete_elicitation_response(
                    &epoch,
                    runtime_scope,
                    &elicitation_id,
                    &request_id,
                    delivered.then_some(accepted_url_id.as_deref()).flatten(),
                )
                .map_err(runtime_state_error)?;
            if delivered && let Some(url_id) = accepted_url_id {
                locked
                    .sessions
                    .register_url_route(&url_id, owner.clone())
                    .map_err(mirror_error)?;
            }
            let business_delta = if let Some(owner) = &owner {
                Some(advance_session_delta(
                    &mut locked,
                    &owner.session_id,
                    owner.incarnation,
                    json!({
                        "kind": "interaction_remove", "interactionId": elicitation_id,
                    }),
                )?)
            } else {
                None
            };
            flush_runtime(&mut locked, &sink);
            if let Some(delta) = business_delta {
                sink.send(delta);
            }
            relay_interaction_resolution(
                &sink,
                json!({
                    "type": "acp/elicitation_resolved", "elicitationId": elicitation_id,
                    "response": response, "requestId": request_id,
                }),
                delivered,
                "elicitation",
            )?;
        }
        "context/search" => {
            let request_id = string_field(&command, "requestId")?;
            let session_id = string_field(&command, "sessionId")?;
            let query = string_field_allow_empty(&command, "query")?;
            let filesystem = filesystem.ok_or_else(|| {
                Error::method_not_found()
                    .data("workspace context is unavailable for remote transports")
            })?;
            let (incarnation, filesystem) =
                require_session_workspace(session_id, &state, &filesystem).await?;
            let owner = rpc_owner(&epoch, Some((session_id, incarnation)), request_id, None);
            drop(execution.take());
            let result = filesystem.search_context(query).await;
            continue_execution(ingress.as_ref(), execution, &owner).await?;
            let matches = result?;
            if require_live_session_incarnation(session_id, &state).await? != incarnation {
                return Err(Error::invalid_params()
                    .data("session changed while searching workspace context"));
            }
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
            let path = nonempty_string_field(&command, "path")?;
            let filesystem = filesystem.ok_or_else(|| {
                Error::method_not_found()
                    .data("workspace context is unavailable for remote transports")
            })?;
            let (incarnation, filesystem) =
                require_session_workspace(session_id, &state, &filesystem).await?;
            let owner = rpc_owner(&epoch, Some((session_id, incarnation)), request_id, None);
            drop(execution.take());
            let result = filesystem.read_context(PathBuf::from(path).as_path()).await;
            continue_execution(ingress.as_ref(), execution, &owner).await?;
            let attachment = result?;
            if require_live_session_incarnation(session_id, &state).await? != incarnation {
                return Err(
                    Error::invalid_params().data("session changed while reading workspace context")
                );
            }
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
            let data = string_field_allow_empty(&command, "data")?;
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

struct PromptCompletion<'a> {
    session_id: &'a str,
    incarnation: u64,
    runtime_operation_id: &'a str,
    operation_id: &'a str,
    client_intent_id: &'a str,
    prompt: Value,
    result: Result<PromptResponse, Error>,
}

struct PromptCommit {
    result: Result<(), Error>,
    terminal_releases: Vec<(String, String)>,
}

/// Seal the exact RPC owner, commit history and enqueue its terminal projection
/// without releasing admission between Ready and the old operation's outcome.
fn commit_completed_turn_history(
    state: &mut BridgeState,
    sink: &EventSink,
    completion: PromptCompletion<'_>,
) -> PromptCommit {
    let mut terminal_releases = Vec::new();
    let result = (|| {
        let PromptCompletion {
            session_id,
            incarnation,
            runtime_operation_id,
            operation_id,
            client_intent_id,
            prompt,
            result,
        } = completion;
        let owns_turn = state
            .sessions
            .state(session_id)
            .filter(|owner| owner.incarnation == incarnation)
            .and_then(|owner| owner.active_turn.as_ref())
            .is_some_and(|turn| {
                turn.operation_id == operation_id
                    && turn
                        .execution
                        .as_ref()
                        .is_some_and(|execution| execution.rpc_operation_id == runtime_operation_id)
            });
        if !owns_turn {
            return Ok(());
        }
        let (response, error, invalid_response) = match result {
            Ok(response) => match validate_prompt_response(&response) {
                Ok(()) => (Some(serde_json::to_value(response)?), None, false),
                Err(message) => (None, Some(semantic_error(message)), true),
            },
            Err(error) => (None, Some(error), false),
        };
        let uncertain = error.as_ref().is_some_and(is_incoming_transport_closed);
        if let Some(error) = &error {
            settle_runtime_prompt_request_error(
                state,
                session_id,
                incarnation,
                runtime_operation_id,
                error,
            )?;
        } else {
            let epoch = state.sessions.epoch().to_string();
            state
                .sessions
                .complete_prompt(
                    &epoch,
                    session_id,
                    incarnation,
                    runtime_operation_id,
                    response.clone().expect("successful prompt has a response"),
                )
                .map_err(runtime_state_error)?;
        }
        // Detach old responder handles before the next turn can consume quota or
        // reuse a tool-call identity. Physical terminal I/O stays outside the lock.
        terminal_releases = drain_runtime_effects(state, sink);
        let sync_error = if uncertain {
            session_mirror(state)
                .block_turn(
                    session_id,
                    incarnation,
                    operation_id,
                    error.as_ref().unwrap().message.clone(),
                )
                .map_err(mirror_error)?;
            None
        } else {
            let terminal = if let Some(error) = &error {
                if invalid_response {
                    json!({ "invalidPromptResponse": error.message })
                } else {
                    json!({ "error": agent_error_value(error) })
                }
            } else {
                response.clone().unwrap()
            };
            session_mirror(state)
                .complete_turn(session_id, incarnation, operation_id, terminal)
                .map_err(mirror_error)?;
            flush_runtime(state, sink);
            let reconciling = session_view_value(state, session_id, incarnation)?;
            sink.send(json!({ "type": "bridge/session_view", "sessionId": session_id, "view": reconciling }));
            let commit = session_mirror(state)
                .commit_completed_turn_from_memory(session_id, incarnation, operation_id)
                .map(|_| ())
                .map_err(mirror_error);
            if let Err(error) = &commit {
                session_mirror(state)
                    .block_turn(session_id, incarnation, operation_id, error.message.clone())
                    .map_err(mirror_error)?;
            }
            commit.err()
        };
        if let Some(validation) = session_validation_mut(state, session_id) {
            validation.retire_turn();
        }
        flush_runtime(state, sink);
        let view = session_view_value(state, session_id, incarnation)?;
        let failure = error.as_ref().or(sync_error.as_ref());
        let outcome = session_turn_outcome_value(
            state,
            if failure.is_some() {
                "bridge/session_turn_failed"
            } else {
                "bridge/session_turn_complete"
            },
            session_id,
            incarnation,
            operation_id,
            if let Some(error) = failure {
                json!({ "clientIntentId": client_intent_id, "prompt": prompt, "error": agent_error_value(error) })
            } else {
                json!({ "clientIntentId": client_intent_id, "response": response })
            },
        )?;
        sink.send(json!({ "type": "bridge/session_view", "sessionId": session_id, "view": view }));
        if !uncertain {
            sink.send(json!({
                "type": "bridge/session_sync", "sessionId": session_id,
                "phase": if sync_error.is_some() { "blocked" } else { "ready" },
                "message": sync_error.as_ref().map(|error| &error.message), "source": "bridge_memory",
            }));
        }
        sink.send(outcome);
        if error.is_none() && sync_error.is_none() {
            sink.send(
                json!({ "type": "acp/prompt_complete", "requestId": runtime_operation_id,
                "sessionId": session_id, "response": response }),
            );
        }
        if let Some(error) = error.or(sync_error) {
            Err(error)
        } else {
            Ok(())
        }
    })();
    PromptCommit {
        result,
        terminal_releases,
    }
}

#[derive(Clone, Copy)]
enum AttachmentHistoryFallback<'a> {
    Received {
        updates: &'a [Value],
        notice: &'a str,
    },
    // The target accepted messages after its creation response; fallback must
    // preserve its current baseline, never reinstall the pre-response snapshot.
    Installed,
}

fn attachment_cache_notice<'a>(updates: &[Value], notice: &'a str) -> &'a str {
    if updates.is_empty() {
        "Earlier messages are unavailable from the Agent and memory cache. You can continue this session."
    } else {
        notice
    }
}

// Attachment success is independent of the Agent's ability to replay history.
async fn synchronize_attached_history(
    connection: &ConnectionTo<Agent>,
    options: &Options,
    state: &Arc<Mutex<BridgeState>>,
    sink: &EventSink,
    cancellation: &CancellationToken,
    session_id: &str,
    incarnation: u64,
    fallback: AttachmentHistoryFallback<'_>,
    ingress: Option<&IngressSender>,
    execution: &mut Option<ExecutionTurn>,
    operation_id: &str,
) -> Result<(), Error> {
    let epoch = state.lock().await.sessions.epoch().to_string();
    let owner = rpc_owner(&epoch, Some((session_id, incarnation)), operation_id, None);
    if ingress.is_some()
        && execution
            .as_ref()
            .is_none_or(|turn| !turn.matches_owner(&owner))
    {
        continue_execution(ingress, execution, &owner).await?;
    }
    if matches!(fallback, AttachmentHistoryFallback::Installed) {
        let state = state.lock().await;
        let session = state
            .sessions
            .state(session_id)
            .filter(|session| session.incarnation == incarnation)
            .ok_or_else(|| Error::request_cancelled().data("fork target owner changed"))?;
        // A browser may already have started using the published target. Optional
        // synchronization cannot seize that turn or another exclusive operation.
        if session.active_turn.is_some() || session.operation.is_some() {
            return Ok(());
        }
    }
    let result = synchronize_authoritative_history(
        connection,
        options,
        state,
        sink,
        cancellation,
        session_id,
        incarnation,
        ingress,
        execution,
        operation_id,
    )
    .await;
    if result.is_ok() {
        return Ok(());
    }
    if ingress.is_some() && execution.is_none() {
        return result;
    }
    let mut state = state.lock().await;
    // A concurrent close or runtime shutdown must never resurrect an attachment.
    if state
        .sessions
        .active_session(session_id)
        .is_none_or(|session| session.state.incarnation != incarnation)
    {
        return Err(Error::invalid_request().data("session closed while retrieving history"));
    }
    if matches!(fallback, AttachmentHistoryFallback::Installed)
        && state
            .sessions
            .state(session_id)
            .is_some_and(|session| session.active_turn.is_some() || session.operation.is_some())
    {
        return Ok(());
    }
    match fallback {
        AttachmentHistoryFallback::Received { updates, notice } => {
            session_mirror(&mut state)
                .use_cached_history(
                    session_id,
                    incarnation,
                    updates,
                    attachment_cache_notice(updates, notice).to_string(),
                )
                .map_err(mirror_error)?;
        }
        AttachmentHistoryFallback::Installed => {
            let current = session_mirror(&mut state)
                .view(session_id, incarnation)
                .map_err(mirror_error)?;
            if current.session.phase != MirrorPhase::Ready {
                // Failed optional replay never replaces the installed snapshot.
                // Retain any idle messages accepted between the response and load.
                let notice = current
                    .session
                    .history_notice
                    .as_deref()
                    .unwrap_or("Showing the branch context already received by the bridge.");
                session_mirror(&mut state)
                    .use_cached_history(
                        session_id,
                        incarnation,
                        current.baseline.updates(),
                        notice.to_string(),
                    )
                    .map_err(mirror_error)?;
            }
        }
    }
    let view = session_view_value(&mut state, session_id, incarnation)?;
    sink.send(json!({ "type": "bridge/session_view", "sessionId": session_id, "view": view }));
    sink.send(json!({ "type": "bridge/session_sync", "sessionId": session_id, "phase": "ready" }));
    Ok(())
}

async fn synchronize_authoritative_history(
    connection: &ConnectionTo<Agent>,
    options: &Options,
    state: &Arc<Mutex<BridgeState>>,
    sink: &EventSink,
    cancellation: &CancellationToken,
    session_id: &str,
    incarnation: u64,
    ingress: Option<&IngressSender>,
    execution: &mut Option<ExecutionTurn>,
    operation_id: &str,
) -> Result<(), Error> {
    require_agent_method("session/load", state).await?;
    let cwd = {
        let state = state.lock().await;
        state
            .sessions
            .active_session(session_id)
            .filter(|session| session.state.incarnation == incarnation)
            .map(|session| session.live.cwd.clone())
            .ok_or_else(|| {
                Error::invalid_params().data("session disappeared before reconciliation")
            })?
    };
    let epoch = state.lock().await.sessions.epoch().to_string();
    let mut backoff = RECONCILE_INITIAL_BACKOFF;
    loop {
        let attempt_id = Uuid::new_v4().to_string();
        let validation_owner = {
            let mut state = state.lock().await;
            session_mirror(&mut state)
                .begin_load(session_id, incarnation, attempt_id.clone())
                .map_err(mirror_error)?;
            let validation_owner = begin_replay_validation(&mut state, session_id);
            take_sync_control_candidate(&mut state, session_id);
            let phase = match session_mirror(&mut state)
                .state(session_id)
                .map(|session| session.phase)
            {
                Some(MirrorPhase::Loading) => "loading",
                _ => "reconciling",
            };
            sink.send(session_delta_value(
                &state,
                session_id,
                incarnation,
                json!({
                    "kind": "sync_state", "phase": phase, "attemptId": attempt_id,
                }),
            )?);
            sink.send(json!({
                "type": "bridge/session_sync", "sessionId": session_id,
                "phase": phase, "attemptId": attempt_id,
            }));
            validation_owner
        };

        let owner = rpc_owner(
            &epoch,
            Some((session_id, incarnation)),
            operation_id,
            Some(&attempt_id),
        );
        let result = send_history(
            connection,
            ingress,
            execution,
            state,
            owner.clone(),
            LoadSessionRequest::new(session_id.to_string(), &cwd)
                .additional_directories(local_additional_directories(options))
                .mcp_servers(options.mcp_servers.clone()),
        )
        .await?;

        let failure = {
            let mut state = state.lock().await;
            if !validation_owner.matches(&state, session_id) {
                return Err(
                    Error::invalid_request().data("session history synchronization owner changed")
                );
            }
            let failure = match result {
                Ok(response) => {
                    let response_value = serde_json::to_value(&response)?;
                    let validation_error = relay_bytes(&response)
                        .err()
                        .or_else(|| response_controls(&response_value).err())
                        .or_else(|| {
                            let modes =
                                response_value.get("modes").filter(|value| !value.is_null());
                            session_validation(&state, session_id).and_then(|validation| {
                                validate_replay_control_references(validation, modes).err()
                            })
                        })
                        .or_else(|| {
                            session_validation(&state, session_id)
                                .and_then(|validation| validation.invalid_reason.clone())
                                .map(semantic_error)
                        });
                    if let Some(error) = validation_error {
                        rollback_replay_validation(&mut state, session_id, &validation_owner);
                        take_sync_control_candidate(&mut state, session_id);
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
                                let controls =
                                    take_sync_control_candidate(&mut state, session_id).updates;
                                let epoch = state.sessions.epoch().to_string();
                                state
                                    .sessions
                                    .synchronize_loaded_session(
                                        &epoch,
                                        session_id,
                                        incarnation,
                                        response_value.clone(),
                                        controls,
                                    )
                                    .map_err(runtime_state_error)?;
                                let view = session_view_value(&mut state, session_id, incarnation)?;
                                commit_replay_validation(&mut state, session_id, &validation_owner);
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
                                rollback_replay_validation(
                                    &mut state,
                                    session_id,
                                    &validation_owner,
                                );
                                take_sync_control_candidate(&mut state, session_id);
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
                    rollback_replay_validation(&mut state, session_id, &validation_owner);
                    take_sync_control_candidate(&mut state, session_id);
                    let retryable = retryable_reconcile_error(&error);
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
            };
            failure.map(|(error, retryable, delta)| {
                sink.send(delta);
                let mut event = json!({
                    "type": "bridge/session_sync", "sessionId": session_id,
                    "phase": if retryable { "retrying" } else { "blocked" }, "message": error.message,
                });
                if retryable { event["retryAfterMs"] = json!(backoff.as_millis()); }
                sink.send(event);
                (error, retryable)
            })
        };

        let Some((error, retryable)) = failure else {
            unreachable!("successful reconciliation returns from the state transaction")
        };
        if !retryable {
            return Err(error);
        }
        drop(execution.take());
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = cancellation.cancelled() => return Err(Error::request_cancelled()),
        }
        continue_execution(ingress, execution, &owner).await?;
        backoff = backoff.saturating_mul(2).min(RECONCILE_MAX_BACKOFF);
    }
}

fn string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str, Error> {
    nonempty_string_field(value, field)
}

fn nonempty_string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str, Error> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::invalid_params().data(format!("{field} must contain non-empty")))
}

fn string_field_allow_empty<'a>(value: &'a Value, field: &str) -> Result<&'a str, Error> {
    value
        .get(field)
        .and_then(Value::as_str)
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

fn session_catalog_revision(state: &BridgeState) -> String {
    format!("{}:{}", state.sessions.epoch(), state.catalog_revision)
}

fn require_catalog_revision(state: &BridgeState, expected: &str) -> Result<(), Error> {
    let current = session_catalog_revision(state);
    if current != expected {
        return Err(Error::new(
            -32000,
            "Session catalog changed; refresh from the first page",
        )
        .data(json!({
            "kind": "session_catalog_changed",
            "catalogRevision": current,
        })));
    }
    Ok(())
}

fn advance_catalog_revision(state: &mut BridgeState, sink: &EventSink) -> u64 {
    state.catalog_revision = state
        .catalog_revision
        .checked_add(1)
        .expect("catalog revision exhausted");
    sink.send(json!({
        "type": "bridge/catalog_changed",
        "bridgeEpoch": state.sessions.epoch(),
        "revision": state.catalog_revision,
    }));
    state.catalog_revision
}

fn validate_session_list_page(
    response: &ListSessionsResponse,
    cursor: Option<&str>,
    _state: &BridgeState,
) -> Result<(), Error> {
    relay_bytes(response)?;

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
    }
    if let Some(next_cursor) = &response.next_cursor {
        if next_cursor.is_empty() {
            return Err(
                Error::invalid_request().data("Agent returned an invalid session/list cursor")
            );
        }
        if cursor == Some(next_cursor.as_str()) {
            return Err(Error::invalid_request()
                .data(format!("Agent reused session/list cursor: {next_cursor}")));
        }
    }
    Ok(())
}

fn valid_session_path(value: &str) -> bool {
    !value.is_empty() && !value.contains('\0') && is_portable_absolute_path(value)
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
    if !valid_session_path(&cwd.to_string_lossy())
        || (options.transport == Transport::Stdio && !cwd.is_absolute())
    {
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

#[cfg(test)]
async fn reserve_attachment(
    session_id: &str,
    kind: RuntimeSessionOperationKind,
    state: &Arc<Mutex<BridgeState>>,
) -> Result<AttachmentReservation, Error> {
    reserve_attachment_with_cwd(session_id, kind, None, "test-attachment", None, state).await
}

async fn reserve_attachment_with_cwd(
    session_id: &str,
    kind: RuntimeSessionOperationKind,
    requested_cwd: Option<&str>,
    operation_id: &str,
    materialization_id: Option<&str>,
    state: &Arc<Mutex<BridgeState>>,
) -> Result<AttachmentReservation, Error> {
    let mut state = state.lock().await;
    reserve_attachment_locked(
        &mut state,
        session_id,
        kind,
        requested_cwd,
        operation_id,
        materialization_id,
    )
}

fn reserve_attachment_locked(
    state: &mut BridgeState,
    session_id: &str,
    kind: RuntimeSessionOperationKind,
    requested_cwd: Option<&str>,
    operation_id: &str,
    materialization_id: Option<&str>,
) -> Result<AttachmentReservation, Error> {
    if session_materialization(state, session_id)
        .is_some_and(|owner| Some(owner.attempt_id.as_str()) != materialization_id)
    {
        return Err(Error::invalid_request()
            .data("session materialization is already owned by another request"));
    }
    if session_history_loading(state, session_id)
        || session_operation_pending(state, session_id, SessionAdmission::Delete)
    {
        return Err(
            Error::invalid_request().data("another prompt or session mutation is already running")
        );
    }
    if let Some(session) = state.sessions.state(session_id) {
        // Only the registered cold materialization may continue its own retry
        // interval. A new explicit load cannot steal that owner's backoff.
        let retrying = !state.sessions.active_session(session_id).is_some()
            && materialization_id.is_some_and(|id| {
                session_materialization(state, session_id)
                    .is_some_and(|current| current.attempt_id == id)
            })
            && session.can_retry_cold_attachment();
        if !retrying {
            let incarnation = session.incarnation;
            session_mirror(state)
                .can_begin(session_id, incarnation, SessionAdmission::Attachment)
                .map_err(mirror_error)?;
        }
    }
    let epoch = state.sessions.epoch().to_string();
    let tracked = state
        .sessions
        .active_session(session_id)
        .map(|session| (session.live.cwd.clone(), session.state.incarnation));
    let reservation = match tracked {
        Some((cwd, incarnation)) => {
            if kind != RuntimeSessionOperationKind::Load {
                return Err(Error::invalid_request()
                    .data("an active tracked session can only be reloaded with session/load"));
            }
            session_mirror(state)
                .begin_exclusive(
                    session_id,
                    incarnation,
                    RuntimeSessionOperationKind::Load,
                    operation_id,
                )
                .map_err(mirror_error)?;
            if let Err(error) =
                state
                    .sessions
                    .start_reload(&epoch, session_id, incarnation, operation_id, kind)
            {
                release_session_operation(
                    state,
                    session_id,
                    incarnation,
                    SessionAdmission::Attachment,
                    operation_id,
                );
                return Err(runtime_state_error(error));
            }
            AttachmentReservation::Reload { cwd, incarnation }
        }
        None => {
            let cwd = match state.listed_sessions.get(session_id) {
                Some(listed) => listed.cwd.clone(),
                None => PathBuf::from(requested_cwd.filter(|cwd| valid_session_path(cwd)).ok_or_else(|| {
                    Error::invalid_params().data(format!(
                        "session was not returned by session/list; an absolute cwd is required: {session_id}"
                    ))
                })?),
            };
            // A retry installs a fresh live incarnation. Move the same loading
            // transaction explicitly so retirement cancels every other old handle.
            let previous_incarnation = state
                .sessions
                .state(session_id)
                .map(|session| session.incarnation);
            let materialization = materialization_id
                .and_then(|id| take_session_materialization(state, session_id, id));
            let attached = state.sessions.start_attachment(
                &epoch,
                session_id,
                cwd.clone(),
                operation_id,
                kind,
            );
            let incarnation = match attached {
                Ok(incarnation) => {
                    if let Some(materialization) = materialization {
                        state
                            .sessions
                            .resources_mut(session_id, incarnation)
                            .expect("attachment installed its owner")
                            .materialization = Some(materialization);
                    }
                    incarnation
                }
                Err(error) => {
                    if let Some(materialization) = materialization {
                        if let Some(incarnation) = previous_incarnation
                            && let Ok(resources) =
                                state.sessions.resources_mut(session_id, incarnation)
                        {
                            resources.materialization = Some(materialization);
                        } else {
                            materialization.cancellation.cancel();
                            for waiter in materialization.waiters {
                                waiter.cancel("materialization owner was replaced");
                            }
                        }
                    }
                    return Err(runtime_state_error(error));
                }
            };
            let mirror = session_mirror(state);
            mirror.register_cold(session_id, incarnation);
            AttachmentReservation::Fresh { cwd, incarnation }
        }
    };
    begin_replay_validation(state, session_id);
    clear_attachment_delivery(state, session_id, reservation.incarnation());
    Ok(reservation)
}

fn settle_attachment(state: &mut BridgeState, session_id: &str) {
    let owner = state.sessions.state(session_id).and_then(|session| {
        session
            .operation
            .as_ref()
            .filter(|operation| operation.kind.admission() == SessionAdmission::Attachment)
            .map(|operation| (session.incarnation, operation.operation_id.clone()))
    });
    if let Some((incarnation, operation_id)) = owner {
        release_session_operation(
            state,
            session_id,
            incarnation,
            SessionAdmission::Attachment,
            &operation_id,
        );
    }
}

fn session_operation_pending(
    state: &BridgeState,
    session_id: &str,
    kind: SessionAdmission,
) -> bool {
    (kind == SessionAdmission::Delete && state.catalog_deletions.contains_key(session_id))
        || state
            .sessions
            .state(session_id)
            .and_then(|session| session.operation.as_ref())
            .is_some_and(|operation| operation.kind.admission() == kind)
}

fn session_prompt_pending(state: &BridgeState, session_id: &str) -> bool {
    state.sessions.state(session_id).is_some_and(|session| {
        matches!(
            session.phase,
            MirrorPhase::Running | MirrorPhase::Reconciling
        ) && session.active_turn.is_some()
    })
}

fn reserve_session_operation(
    state: &mut BridgeState,
    session_id: &str,
    operation: RuntimeSessionOperationKind,
    operation_id: &str,
) -> Result<u64, Error> {
    let incarnation = state
        .sessions
        .active_session(session_id)
        .map(|session| session.state.incarnation)
        .ok_or_else(|| {
            Error::invalid_params().data(format!("unknown or inactive session: {session_id}"))
        })?;
    session_mirror(state)
        .begin_exclusive(session_id, incarnation, operation, operation_id)
        .map_err(mirror_error)?;
    Ok(incarnation)
}

fn complete_session_retirement(
    state: &mut BridgeState,
    sink: &EventSink,
    session_id: &str,
    incarnation: u64,
    operation: SessionAdmission,
    operation_id: &str,
) {
    if session_mirror(state)
        .finish_session_cleanup(session_id, incarnation, operation, operation_id)
        .is_ok()
    {
        retire_session_observation(
            state,
            sink,
            session_id,
            incarnation,
            if operation == SessionAdmission::Delete {
                "deleted"
            } else {
                "closed"
            },
        );
        state
            .sessions
            .cancel_resources(session_id, incarnation, "session retired");
        state.sessions.remove(session_id, incarnation);
    }
}

fn release_session_operation(
    state: &mut BridgeState,
    session_id: &str,
    incarnation: u64,
    operation: SessionAdmission,
    operation_id: &str,
) {
    if session_operation_uncertain(state, session_id, incarnation, operation_id) {
        return;
    }
    // A late completion cannot release a newer operation or a reopened session.
    let _ =
        session_mirror(state).settle_exclusive(session_id, incarnation, operation, operation_id);
}

fn session_operation_uncertain(
    state: &BridgeState,
    session_id: &str,
    incarnation: u64,
    operation_id: &str,
) -> bool {
    state
        .sessions
        .state(session_id)
        .filter(|session| session.incarnation == incarnation)
        .and_then(|session| session.operation.as_ref())
        .is_some_and(|operation| {
            operation.operation_id == operation_id && operation.stage == "uncertain"
        })
}

fn finish_prompt_operation(lifecycle: &PromptLifecycle) {
    // The caller holds the owner lock until terminal history, responders and
    // outcome publication have committed. Shutdown can now observe completion.
    lifecycle.prompt_finished();
}

struct CatalogDeletion {
    request_id: String,
    allocation: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeletionReservation {
    Catalog(Uuid),
    Session(u64),
}

fn reserve_deletion(
    state: &mut BridgeState,
    session_id: &str,
    operation_id: &str,
) -> Result<DeletionReservation, Error> {
    if session_history_loading(state, session_id)
        || state.catalog_deletions.contains_key(session_id)
    {
        return Err(
            Error::invalid_request().data("another prompt or session mutation is already running")
        );
    }
    if let Some(session) = state.sessions.state(session_id) {
        let incarnation = session.incarnation;
        session
            .can_begin(SessionAdmission::Delete)
            .map_err(mirror_error)?;
        if incarnation != 0 {
            state
                .sessions
                .begin_exclusive(
                    session_id,
                    incarnation,
                    RuntimeSessionOperationKind::Delete,
                    operation_id,
                )
                .map_err(mirror_error)?;
            return Ok(DeletionReservation::Session(incarnation));
        }
    }
    let allocation = Uuid::new_v4();
    state.catalog_deletions.insert(
        session_id.to_owned(),
        CatalogDeletion {
            request_id: operation_id.to_owned(),
            allocation,
        },
    );
    Ok(DeletionReservation::Catalog(allocation))
}

fn release_deletion(
    state: &mut BridgeState,
    session_id: &str,
    reservation: DeletionReservation,
    operation_id: &str,
) {
    match reservation {
        DeletionReservation::Catalog(allocation) => {
            if state
                .catalog_deletions
                .get(session_id)
                .is_some_and(|pending| {
                    pending.allocation == allocation && pending.request_id == operation_id
                })
            {
                state.catalog_deletions.remove(session_id);
            }
        }
        DeletionReservation::Session(incarnation) => {
            release_session_operation(
                state,
                session_id,
                incarnation,
                SessionAdmission::Delete,
                operation_id,
            );
        }
    }
}

fn session_history_loading(state: &BridgeState, session_id: &str) -> bool {
    state.sessions.active_session(session_id).is_some()
        && state.sessions.state(session_id).is_some_and(|session| {
            matches!(
                session.phase,
                MirrorPhase::Cold | MirrorPhase::Loading | MirrorPhase::Reconciling
            )
        })
}

fn begin_runtime_delete(
    state: &mut BridgeState,
    session_id: &str,
    operation_id: &str,
    close_before_delete: bool,
) -> Result<Option<u64>, Error> {
    let active_incarnation = state
        .sessions
        .active_session(session_id)
        .map(|session| session.state.incarnation);
    let closed_incarnation = state.sessions.state(session_id).and_then(|session| {
        session
            .live
            .as_ref()
            .is_some_and(|live| live.lifecycle == SessionLifecycle::Closed)
            .then_some(session.incarnation)
    });
    let Some(incarnation) = active_incarnation.or(closed_incarnation) else {
        return Ok(None);
    };
    let epoch = state.sessions.epoch().to_string();
    let result = if close_before_delete || closed_incarnation.is_some() {
        state
            .sessions
            .start_delete(&epoch, session_id, incarnation, operation_id.to_string())
    } else {
        state.sessions.start_direct_delete(
            &epoch,
            session_id,
            incarnation,
            operation_id.to_string(),
        )
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
    let epoch = state.sessions.epoch().to_string();
    state
        .sessions
        .delete_close_succeeded(&epoch, session_id, incarnation, operation_id)
        .map_err(runtime_state_error)?;
    flush_runtime(state, sink);
    Ok(())
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

async fn require_session_workspace(
    session_id: &str,
    state: &Arc<Mutex<BridgeState>>,
    filesystem: &WorkspaceFileSystem,
) -> Result<(u64, WorkspaceFileSystem), Error> {
    let (incarnation, cwd) = {
        let state = state.lock().await;
        let session = state.sessions.resource_session(session_id).ok_or_else(|| {
            Error::invalid_params().data(format!("unknown or inactive session: {session_id}"))
        })?;
        (session.state.incarnation, session.live.cwd.clone())
    };
    Ok((incarnation, filesystem.for_workspace(&cwd)?))
}

async fn cancel_interactions(
    session_id: &str,
    incarnation: u64,
    reason: &str,
    state: &Arc<Mutex<BridgeState>>,
    sink: &EventSink,
) {
    let mut state = state.lock().await;
    cancel_interactions_locked(session_id, incarnation, reason, &mut state, sink);
}

fn commit_session_cancel(
    state: &mut BridgeState,
    sink: &EventSink,
    session_id: &str,
    incarnation: u64,
) -> Result<(), Error> {
    let epoch = state.sessions.epoch().to_string();
    state
        .sessions
        .request_cancel(&epoch, session_id, incarnation)
        .map_err(runtime_state_error)?;
    // Select and retire this turn's current interactions before completion can
    // admit a new turn in the same incarnation.
    cancel_interactions_locked(session_id, incarnation, "session_cancelled", state, sink);
    Ok(())
}

fn cancel_interactions_locked(
    session_id: &str,
    incarnation: u64,
    reason: &str,
    state: &mut BridgeState,
    sink: &EventSink,
) {
    let owner = SessionResourceOwner::new(state.sessions.epoch(), session_id, incarnation);
    if !state.sessions.owns_resources(&owner) {
        return;
    }
    let resolve_canonical = state.sessions.live(session_id).is_some_and(|live| {
        matches!(
            live.lifecycle,
            SessionLifecycle::Active
                | SessionLifecycle::Attaching
                | SessionLifecycle::Closing
                | SessionLifecycle::ClosingForDelete
        )
    });
    let permission_ids = state
        .sessions
        .permission_owners
        .iter()
        .filter(|(_, route)| *route == &owner)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    for id in permission_ids {
        if let Some(pending) = state.sessions.take_permission(&id, &owner) {
            if resolve_canonical
                && let Err(error) =
                    state
                        .sessions
                        .resolve_permission(&owner.epoch, session_id, incarnation, &id)
            {
                sink.acp_error(runtime_state_error(error), None, Some("permission/cancel"));
            }
            let _ = pending.sender.send(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ));
            sink.send(json!({ "type": "acp/permission_resolved", "permissionId": id }));
        }
    }
    let elicitation_ids = state
        .sessions
        .elicitation_owners
        .iter()
        .filter(|(_, route)| *route == &owner)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    for id in elicitation_ids {
        if let Some(pending) = state.sessions.take_elicitation(&id, &owner) {
            if resolve_canonical
                && let Err(error) = state.sessions.resolve_elicitation(
                    &owner.epoch,
                    Some((session_id, incarnation)),
                    &id,
                    None,
                )
            {
                sink.acp_error(runtime_state_error(error), None, Some("elicitation/cancel"));
            }
            let response = CreateElicitationResponse::new(ElicitationAction::Cancel);
            let _ = pending.sender.send(response.clone());
            sink.send(json!({ "type": "acp/elicitation_resolved", "elicitationId": id, "response": response }));
        }
    }
    let url_ids = state
        .sessions
        .url_owners
        .iter()
        .filter(|(_, route)| route.owner.as_ref() == Some(&owner))
        .map(|(id, route)| (id.clone(), route.clone()))
        .collect::<Vec<_>>();
    for (elicitation_id, registration) in url_ids {
        if resolve_canonical {
            match state.sessions.settle_url_registration(
                &owner.epoch,
                Some((session_id, incarnation)),
                &elicitation_id,
                &registration.registration_id,
                UrlFlowStatus::Cancelled,
            ) {
                Ok(UrlFlowResolution::StaleRegistration) => continue,
                Ok(_) => {}
                Err(error) => {
                    sink.acp_error(runtime_state_error(error), None, Some("elicitation/abort"));
                }
            }
        }
        if state
            .sessions
            .take_url_route(&elicitation_id, &registration)
            .is_some()
        {
            sink.send(json!({ "type": "acp/elicitation_aborted", "elicitationId": elicitation_id, "sessionId": session_id, "reason": reason }));
        }
    }
    flush_runtime(state, sink);
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

fn validate_auth_methods(methods: &[AuthMethod], terminal_supported: bool) -> Result<(), Error> {
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
        if name.is_empty() {
            return Err(semantic_error(format!(
                "Agent authentication method name must not be empty"
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
    const FORMER_BRIDGE_MESSAGE_BYTES: usize = 5 * 1024 * 1024;
    const FORMER_BRIDGE_IDENTIFIER_LENGTH: usize = 1_024;
    const FORMER_BRIDGE_PATH_LENGTH: usize = 16_384;
    const FORMER_AGENT_RELAY_BYTES: usize = 4_000_000;
    const FORMER_TRACKED_SESSIONS: usize = 32;
    const FORMER_BRIDGE_ERROR_DATA_BYTES: usize = 256 * 1024;

    use clap::Parser;

    fn register_test_session(state: &mut BridgeState, session_id: &str, cwd: &str) -> u64 {
        let epoch = state.sessions.epoch().to_string();
        let incarnation = state
            .sessions
            .open_new(&epoch, session_id, cwd, json!({ "sessionId": session_id }))
            .unwrap();

        session_mirror(state).register_new(session_id, incarnation);
        incarnation
    }

    #[test]
    fn retirement_publishes_its_business_fact_before_ending_observers() {
        for admission in [SessionAdmission::Close, SessionAdmission::Delete] {
            let mut state = BridgeState::default();
            let incarnation = register_test_session(&mut state, "session", "/workspace");
            let epoch = state.sessions.epoch().to_string();
            let lease = ObservationLease::new();
            assert!(
                state
                    .sessions
                    .resources_mut("session", incarnation)
                    .unwrap()
                    .observers
                    .observe(7, lease.clone())
            );
            if admission == SessionAdmission::Close {
                state
                    .sessions
                    .start_operation(
                        &epoch,
                        "session",
                        incarnation,
                        "retire",
                        RuntimeSessionOperationKind::Close,
                        "closing",
                    )
                    .unwrap();
                state
                    .sessions
                    .close_session(&epoch, "session", incarnation, "retire")
                    .unwrap();
            } else {
                state
                    .sessions
                    .start_direct_delete(&epoch, "session", incarnation, "retire")
                    .unwrap();
                state
                    .sessions
                    .complete_delete(&epoch, "session", incarnation, "retire")
                    .unwrap();
            }
            let (tx, mut events) = mpsc::unbounded_channel();
            let sink = EventSink { tx: tx.into() };
            complete_session_retirement(
                &mut state,
                &sink,
                "session",
                incarnation,
                admission,
                "retire",
            );
            let retired: Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
            let ended: Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
            assert_eq!(
                retired,
                json!({
                    "type": "bridge/session_retired", "sessionId": "session",
                    "bridgeEpoch": epoch, "sessionIncarnation": incarnation,
                    "reason": if admission == SessionAdmission::Delete { "deleted" } else { "closed" },
                })
            );
            assert_eq!(ended["type"], "bridge/internal_observer_end");
            assert_eq!(ended["observerId"], 7);
            assert!(state.sessions.state("session").is_none());
            assert!(!lease.is_cancelled());
            complete_session_retirement(
                &mut state,
                &sink,
                "session",
                incarnation,
                admission,
                "retire",
            );
            assert!(
                events.try_recv().is_err(),
                "late cleanup cannot publish another retirement"
            );
        }
    }

    #[test]
    fn observation_recovery_distinguishes_connection_replacement_from_same_epoch_retirement() {
        let mut state = BridgeState::default();
        let incarnation = register_test_session(&mut state, "session", "/workspace");
        let current_epoch = state.sessions.epoch().to_string();
        let old = SessionResourceOwner::new("previous-connection", "session", incarnation);
        let (tx, _events) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        let (response, mut view) = oneshot::channel();
        assert!(
            prepare_session_view_request_for_owner(
                &mut state,
                &sink,
                "session".into(),
                Some("/workspace".into()),
                Some(old.clone()),
                SessionViewWaiter::View(response)
            )
            .is_none()
        );
        assert!(
            matches!(view.try_recv().unwrap(), Err(SessionViewError::ConnectionReplaced {
            owner, current_epoch: replacement,
        }) if owner == old && replacement == current_epoch)
        );
        let lease = ObservationLease::new();
        let (reply, mut observation) = oneshot::channel();
        assert!(
            prepare_session_view_request_for_owner(
                &mut state,
                &sink,
                "session".into(),
                None,
                Some(old.clone()),
                SessionViewWaiter::Observe {
                    observer_id: 5,
                    lease: lease.clone(),
                    reply,
                }
            )
            .is_none()
        );
        assert!(
            matches!(observation.try_recv().unwrap(), Err(SessionViewError::ConnectionReplaced {
            owner, current_epoch: replacement,
        }) if owner == old && replacement == current_epoch)
        );
        assert!(lease.is_cancelled());
        assert_eq!(
            state.sessions.state("session").unwrap().incarnation,
            incarnation
        );
        assert_eq!(state.sessions.allocated_session_count(), 1);
        assert!(session_materialization(&state, "session").is_none());

        let missing = SessionResourceOwner::new(&current_epoch, "closed-session", incarnation);
        let (response, mut view) = oneshot::channel();
        assert!(
            prepare_session_view_request_for_owner(
                &mut state,
                &sink,
                "closed-session".into(),
                Some("/workspace".into()),
                Some(missing.clone()),
                SessionViewWaiter::View(response)
            )
            .is_none()
        );
        assert!(
            matches!(view.try_recv().unwrap(), Err(SessionViewError::Retired(owner)) if owner == missing)
        );
        assert!(state.sessions.state("closed-session").is_none());
    }

    #[test]
    fn recovering_views_and_observers_cannot_materialize_a_retired_owner() {
        let mut state = BridgeState::default();
        state.agent_capabilities = Some(AgentCapabilities::new().load_session(true));
        state
            .listed_sessions
            .insert("session".into(), session_info("session", "/workspace"));
        let original = SessionResourceOwner::new(state.sessions.epoch(), "session", 99);
        let (tx, _events) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        for replacement in [false, true] {
            if replacement {
                register_test_session(&mut state, "session", "/workspace");
            }
            let before = state
                .sessions
                .state("session")
                .map(|state| state.incarnation);
            let (response, mut view) = oneshot::channel();
            assert!(
                prepare_session_view_request_for_owner(
                    &mut state,
                    &sink,
                    "session".into(),
                    Some("/workspace".into()),
                    Some(original.clone()),
                    SessionViewWaiter::View(response)
                )
                .is_none()
            );
            assert!(
                matches!(view.try_recv().unwrap(), Err(SessionViewError::Retired(owner)) if owner == original)
            );
            let lease = ObservationLease::new();
            let (reply, mut observed) = oneshot::channel();
            assert!(
                prepare_session_view_request_for_owner(
                    &mut state,
                    &sink,
                    "session".into(),
                    Some("/workspace".into()),
                    Some(original.clone()),
                    SessionViewWaiter::Observe {
                        observer_id: 1,
                        lease: lease.clone(),
                        reply,
                    }
                )
                .is_none()
            );
            assert!(
                matches!(observed.try_recv().unwrap(), Err(SessionViewError::Retired(owner)) if owner == original)
            );
            assert!(lease.is_cancelled());
            assert_eq!(
                state
                    .sessions
                    .state("session")
                    .map(|state| state.incarnation),
                before
            );
            assert!(session_materialization(&state, "session").is_none());
        }
        let current = state.sessions.state("session").unwrap().incarnation;
        let current_owner = SessionResourceOwner::new(state.sessions.epoch(), "session", current);
        let (response, mut view) = oneshot::channel();
        assert!(
            prepare_session_view_request_for_owner(
                &mut state,
                &sink,
                "session".into(),
                None,
                Some(current_owner),
                SessionViewWaiter::View(response)
            )
            .is_none()
        );
        assert_eq!(
            view.try_recv().unwrap().unwrap()["session"]["incarnation"],
            current
        );

        let (response, _view) = oneshot::channel();
        assert!(
            prepare_session_view_request_for_owner(
                &mut state,
                &sink,
                "explicit-open".into(),
                Some("/workspace".into()),
                None,
                SessionViewWaiter::View(response)
            )
            .is_some(),
            "an explicit first open must still be allowed to materialize"
        );
    }

    #[test]
    fn delete_close_retires_observation_even_when_the_following_delete_fails() {
        let mut state = BridgeState::default();
        let incarnation = register_test_session(&mut state, "session", "/workspace");
        let epoch = state.sessions.epoch().to_string();
        let owner = SessionResourceOwner::new(&epoch, "session", incarnation);
        state
            .sessions
            .start_delete(&epoch, "session", incarnation, "delete")
            .unwrap();
        assert!(
            !state
                .sessions
                .state("session")
                .unwrap()
                .observation_retired()
        );
        // A failed close preserves the original observable runtime.
        state
            .sessions
            .fail_operation(
                &epoch,
                "session",
                incarnation,
                "delete",
                RuntimeSessionOperationKind::Delete,
                json!({"message":"close failed"}),
            )
            .unwrap();
        assert!(
            !state
                .sessions
                .state("session")
                .unwrap()
                .observation_retired()
        );
        state
            .sessions
            .start_delete(&epoch, "session", incarnation, "retry")
            .unwrap();
        state
            .sessions
            .delete_close_succeeded(&epoch, "session", incarnation, "retry")
            .unwrap();
        let (tx, mut events) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        retire_session_observation(&mut state, &sink, "session", incarnation, "closed");
        let event: Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
        assert_eq!(event["reason"], "closed");
        assert!(
            state.sessions.state("session").unwrap().operation.is_some(),
            "the Delete owner must survive observation retirement"
        );
        for failed in [false, true] {
            if failed {
                state
                    .sessions
                    .fail_operation(
                        &epoch,
                        "session",
                        incarnation,
                        "retry",
                        RuntimeSessionOperationKind::Delete,
                        json!({"message":"delete denied"}),
                    )
                    .unwrap();
            }
            let (response, mut view) = oneshot::channel();
            assert!(
                prepare_session_view_request_for_owner(
                    &mut state,
                    &sink,
                    "session".into(),
                    None,
                    Some(owner.clone()),
                    SessionViewWaiter::View(response)
                )
                .is_none()
            );
            assert!(matches!(
                view.try_recv().unwrap(),
                Err(SessionViewError::Retired(_))
            ));
            assert!(
                state
                    .sessions
                    .state("session")
                    .unwrap()
                    .observation_retired()
            );
        }
        assert_eq!(
            state.sessions.live("session").unwrap().lifecycle,
            SessionLifecycle::Closed
        );
        assert!(state.sessions.state("session").unwrap().operation.is_none());

        let direct = register_test_session(&mut state, "direct", "/workspace");
        state
            .sessions
            .start_direct_delete(&epoch, "direct", direct, "direct-delete")
            .unwrap();
        assert!(
            !state
                .sessions
                .state("direct")
                .unwrap()
                .observation_retired(),
            "an Agent without close support has not confirmed retirement yet"
        );
    }

    fn start_test_turn(
        state: &mut BridgeState,
        session_id: &str,
        intent: &str,
    ) -> Result<String, MirrorError> {
        let incarnation = state.sessions.state(session_id).unwrap().incarnation;
        let mirror = session_mirror(state);
        let revision = mirror
            .state(session_id)
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        let prompt = vec![json!({ "type": "text", "text": intent })];
        let epoch = mirror.epoch().to_string();
        let admission = mirror
            .admit_session_turn(&epoch, session_id, incarnation, &revision, intent, prompt)
            .map_err(|error| match error {
                SessionTurnError::History(error) => error,
                SessionTurnError::Live(error) => panic!("test live admission failed: {error:?}"),
            })?;
        let TurnAdmission::Accepted { operation_id } = admission else {
            panic!("test intent must be newly admitted");
        };
        Ok(operation_id)
    }

    fn complete_test_turn(state: &mut BridgeState, session_id: &str, operation_id: &str) {
        let incarnation = state.sessions.state(session_id).unwrap().incarnation;
        let epoch = state.sessions.epoch().to_string();
        let terminal = json!({ "stopReason": "end_turn" });
        let runtime_operation = state
            .sessions
            .session(session_id)
            .unwrap()
            .active_turn
            .as_ref()
            .unwrap()
            .operation_id
            .clone();
        state
            .sessions
            .complete_prompt(
                &epoch,
                session_id,
                incarnation,
                &runtime_operation,
                terminal.clone(),
            )
            .unwrap();
        let mirror = session_mirror(state);
        mirror
            .complete_turn(session_id, incarnation, operation_id, terminal)
            .unwrap();
        mirror
            .commit_completed_turn_from_memory(session_id, incarnation, operation_id)
            .unwrap();
    }

    #[tokio::test]
    async fn prompt_completion_publishes_old_outcome_before_waiting_next_turn_can_admit() {
        let state = Arc::new(Mutex::new(BridgeState::default()));
        let (tx, mut events) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        let mut locked = state.lock().await;
        let incarnation = register_test_session(&mut locked, "session", "/workspace");
        let operation = start_test_turn(&mut locked, "session", "old-intent").unwrap();
        let update = json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "last old update" } });
        locked
            .sessions
            .append_turn_update("session", incarnation, &operation, update.clone())
            .unwrap();
        sink.send(
            session_delta_value(
                &locked,
                "session",
                incarnation,
                json!({ "kind": "turn_update", "update": update }),
            )
            .unwrap(),
        );
        let (attempting, attempted) = oneshot::channel();
        let competing_state = state.clone();
        let competing_sink = sink.clone();
        let next_turn = tokio::spawn(async move {
            attempting.send(()).unwrap();
            let mut state = competing_state.lock().await;
            let next = start_test_turn(&mut state, "session", "next-intent").unwrap();
            let view = session_view_value(&mut state, "session", incarnation).unwrap();
            competing_sink.send(
                json!({ "type": "bridge/session_view", "sessionId": "session", "view": view }),
            );
            (next, view)
        });
        attempted.await.unwrap();
        // The competing turn is already queued for this owner. Every completion
        // event must enter the outbox before releasing this critical section.
        let completed = commit_completed_turn_history(
            &mut locked,
            &sink,
            PromptCompletion {
                session_id: "session",
                incarnation,
                runtime_operation_id: "old-intent",
                operation_id: &operation,
                client_intent_id: "old-intent",
                prompt: json!([]),
                result: Ok(serde_json::from_value(json!({ "stopReason": "end_turn" })).unwrap()),
            },
        );
        completed.result.unwrap();
        assert!(completed.terminal_releases.is_empty());
        let ready = session_view_value(&mut locked, "session", incarnation).unwrap();
        assert_eq!(ready["session"]["phase"], "ready");
        drop(locked);
        let (next_operation, next_view) = next_turn.await.unwrap();
        let published = std::iter::from_fn(|| events.try_recv().ok())
            .map(|event| serde_json::from_str::<Value>(&event).unwrap())
            .collect::<Vec<_>>();
        let last_update = published
            .iter()
            .position(|event| event["type"] == "bridge/session_delta")
            .unwrap();
        let old_outcome = published
            .iter()
            .position(|event| event["type"] == "bridge/session_turn_complete")
            .unwrap();
        let new_running = published
            .iter()
            .position(|event| {
                event["type"] == "bridge/session_view"
                    && event["view"]["session"]["activeTurn"]["operationId"] == next_operation
            })
            .unwrap();
        assert!(last_update < old_outcome && old_outcome < new_running);
        let outcome = &published[old_outcome];
        assert_eq!(outcome["operationId"], operation);
        assert_eq!(outcome["phase"], "ready");
        assert_eq!(outcome["viewRevision"], ready["session"]["viewRevision"]);
        assert_eq!(
            outcome["historyRevision"],
            ready["session"]["historyRevision"]
        );
        assert!(
            outcome["viewRevision"].as_u64().unwrap()
                < next_view["session"]["viewRevision"].as_u64().unwrap()
        );
        // A late duplicate result from the old RPC must not seal the new turn.
        let mut locked = state.lock().await;
        let stale = commit_completed_turn_history(
            &mut locked,
            &sink,
            PromptCompletion {
                session_id: "session",
                incarnation,
                runtime_operation_id: "old-intent",
                operation_id: &operation,
                client_intent_id: "old-intent",
                prompt: json!([]),
                result: Ok(serde_json::from_value(json!({ "stopReason": "end_turn" })).unwrap()),
            },
        );
        stale.result.unwrap();
        assert_eq!(
            session_view_value(&mut locked, "session", incarnation).unwrap(),
            next_view
        );
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn prompt_failure_outcome_keeps_its_terminal_revision_and_transport_uncertainty() {
        for uncertain in [false, true] {
            let mut state = BridgeState::default();
            let incarnation = register_test_session(&mut state, "session", "/workspace");
            let operation = start_test_turn(&mut state, "session", "failed-intent").unwrap();
            let error = if uncertain {
                Error::internal_error().data(
                    json!({ "reason": "incoming_transport_closed", "method": "session/prompt" }),
                )
            } else {
                Error::invalid_request().data("Agent rejected the prompt")
            };
            let (tx, mut events) = mpsc::unbounded_channel();
            let sink = EventSink { tx: tx.into() };
            let completed = commit_completed_turn_history(
                &mut state,
                &sink,
                PromptCompletion {
                    session_id: "session",
                    incarnation,
                    runtime_operation_id: "failed-intent",
                    operation_id: &operation,
                    client_intent_id: "failed-intent",
                    prompt: json!([]),
                    result: Err(error),
                },
            );
            assert!(completed.result.is_err());
            let view = session_view_value(&mut state, "session", incarnation).unwrap();
            let published = std::iter::from_fn(|| events.try_recv().ok())
                .map(|event| serde_json::from_str::<Value>(&event).unwrap())
                .collect::<Vec<_>>();
            let outcome = published
                .iter()
                .find(|event| event["type"] == "bridge/session_turn_failed")
                .unwrap();
            assert_eq!(outcome["operationId"], operation);
            assert_eq!(outcome["viewRevision"], view["session"]["viewRevision"]);
            assert_eq!(
                outcome["historyRevision"],
                view["session"]["historyRevision"]
            );
            assert_eq!(
                outcome["phase"],
                if uncertain { "blocked" } else { "ready" }
            );
            assert_eq!(
                view["live"]["lifecycle"],
                if uncertain { "uncertain" } else { "active" }
            );
            assert_eq!(
                start_test_turn(&mut state, "session", "after-failure").is_err(),
                uncertain
            );
        }
    }

    #[test]
    fn empty_auth_methods_are_valid() {
        validate_auth_methods(&[], true).unwrap();
    }

    #[test]
    fn remote_new_session_uses_agent_absolute_path_syntax() {
        let options = options(&[
            "attyd",
            "--transport",
            "ws",
            "--",
            "ws://127.0.0.1:9999/acp",
        ]);
        for cwd in [r"C:\repo", r"\\server\workspace", "/remote/repo"] {
            assert_eq!(
                new_session_cwd(&json!({ "cwd": cwd }), &options).unwrap(),
                PathBuf::from(cwd)
            );
        }
        for cwd in ["relative", "", "C:relative"] {
            assert!(new_session_cwd(&json!({ "cwd": cwd }), &options).is_err());
        }
    }

    fn options(arguments: &[&str]) -> Options {
        Options::try_parse_from(arguments)
            .unwrap()
            .normalized()
            .unwrap()
    }

    async fn request(
        commands: &mpsc::UnboundedSender<BridgeInput>,
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
                        expected_owner: None,
                        session_id: session_id.clone(),
                        cwd: None,
                        response,
                    })
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
                .unwrap();
            result.await.unwrap()
        } else {
            let (response, result) = oneshot::channel();
            commands
                .send(BridgeInput::BusinessRequest { command, response })
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
    async fn stdio_clean_eof_stops_an_idle_bridge() {
        verify_stdio_clean_eof("exit-idle", false).await;
    }

    #[tokio::test]
    async fn stdio_clean_eof_fails_a_pending_prompt_and_stops() {
        verify_stdio_clean_eof("exit-pending", false).await;
    }

    #[tokio::test]
    async fn stdio_clean_eof_drains_a_completed_burst_before_stopping() {
        verify_stdio_clean_eof("exit-completed", false).await;
    }

    #[tokio::test]
    async fn stdio_clean_eof_drains_a_large_message_before_stopping() {
        verify_stdio_clean_eof("exit-completed", true).await;
    }

    async fn verify_stdio_clean_eof(exit_mode: &str, large: bool) {
        let cwd = env!("CARGO_MANIFEST_DIR");
        let fixture = format!("{cwd}/tests/fixtures/burst-prompt-agent.mjs");
        let options = options(&["attyd", "--cwd", cwd, "--", "node", &fixture, exit_mode]);
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (event_tx, mut events) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let mut bridge = tokio::spawn(run_with_cancellation(
            Arc::new(options),
            command_rx,
            event_tx.into(),
            cancellation.clone(),
        ));
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let raw = events.recv().await.expect("bridge stopped before initialization");
                let event: Value = serde_json::from_str(&raw).unwrap();
                assert_ne!(event["type"], "bridge/error", "{event}");
                if event["type"] == "bridge/phase" && event["phase"] == "ready" {
                    break;
                }
            }
            if exit_mode != "exit-idle" {
                let (response, result) = oneshot::channel();
                commands.send(BridgeInput::SessionViewRequest {
                    session_id: "burst".into(),
                    cwd: Some(cwd.into()),
                    expected_owner: None,
                    response,
                }).unwrap();
                let view = result.await.unwrap().unwrap();
                request(&commands, json!({
                    "type": "session/prompt", "requestId": "eof-prompt", "sessionId": "burst",
                    "historyRevision": view["session"]["historyRevision"],
                    "prompt": [{"type": "text", "text": if large { "answer-large" } else { "answer" }}],
                })).await.unwrap();
            }

            // Keep the consumer idle until the process and bridge both finish.
            // A clean EOF must terminate without closing the browser command
            // channel, and every accepted output must remain available afterward.
            (&mut bridge).await.unwrap();
            let mut stopped = false;
            let mut text = String::new();
            let mut outcomes = Vec::new();
            let mut errors = Vec::new();
            while let Some(raw) = events.recv().await {
                let event: Value = serde_json::from_str(&raw).unwrap();
                assert!(!stopped, "event published after bridge stopped: {}", event["type"]);
                if matches!(event["type"].as_str(), Some("acp/error" | "bridge/error")) {
                    errors.push(event.clone());
                }
                if event["type"] == "acp/session_update"
                    && event["notification"]["update"]["sessionUpdate"] == "agent_message_chunk"
                {
                    text.push_str(event["notification"]["update"]["content"]["text"].as_str().unwrap());
                }
                if matches!(event["type"].as_str(), Some("bridge/session_turn_complete" | "bridge/session_turn_failed")) {
                    outcomes.push(event.clone());
                }
                if event["type"] == "bridge/phase" {
                    assert_eq!(event["phase"], "stopped", "{event}; errors: {errors:?}");
                    stopped = true;
                }
            }
            assert!(stopped, "clean EOF did not publish a stopped phase");
            match exit_mode {
                "exit-idle" => assert!(outcomes.is_empty()),
                "exit-pending" => {
                    assert_eq!(outcomes.len(), 1);
                    assert_eq!(outcomes[0]["type"], "bridge/session_turn_failed");
                }
                "exit-completed" => {
                    assert_eq!(outcomes.len(), 1);
                    assert_eq!(outcomes[0]["type"], "bridge/session_turn_complete");
                    assert_eq!(outcomes[0]["response"]["stopReason"], "end_turn");
                    let expected = if large {
                        "完整🙂".repeat(900_000)
                    } else {
                        (0..2_048).map(|index| format!("片段{index}🙂\n")).collect::<String>()
                    };
                    assert_eq!(text, expected, "output preceding EOF was truncated");
                }
                _ => unreachable!(),
            }
        }).await;
        cancellation.cancel();
        if !bridge.is_finished() {
            tokio::time::timeout(Duration::from_secs(5), bridge)
                .await
                .unwrap()
                .unwrap();
        }
        result.expect("clean EOF left the bridge running");
    }

    #[tokio::test]
    async fn cold_load_burst_preserves_all_history_and_keeps_catalog_available() {
        let cwd = env!("CARGO_MANIFEST_DIR");
        let fixture = format!("{cwd}/tests/fixtures/burst-load-agent.mjs");
        let options = options(&["attyd", "--cwd", cwd, "--", "node", &fixture]);
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (event_tx, mut events) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let bridge = tokio::spawn(run_with_cancellation(
            Arc::new(options),
            command_rx,
            event_tx.into(),
            cancellation.clone(),
        ));
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let raw = events
                    .recv()
                    .await
                    .expect("bridge stopped before initialization");
                let event: Value = serde_json::from_str(&raw).unwrap();
                assert_ne!(event["type"], "bridge/error", "{event}");
                if event["type"] == "bridge/phase" && event["phase"] == "ready" {
                    break;
                }
            }
            let (response, mut view_result) = oneshot::channel();
            commands
                .send(BridgeInput::SessionViewRequest {
                    session_id: "burst-history".to_string(),
                    cwd: Some(cwd.to_string()),
                    expected_owner: None,
                    response,
                })
                .unwrap();
            // The fixture withholds the load response until catalog operations
            // have completed. Listing cannot depend on the history transaction.
            loop {
                let listed = request(
                    &commands,
                    json!({"type": "session/list", "requestId": "during-replay"}),
                )
                .await
                .unwrap();
                if listed["sessions"]
                    .as_array()
                    .is_some_and(|sessions| !sessions.is_empty())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(matches!(
                view_result.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            request(
                &commands,
                json!({"type": "session/list", "requestId": "finish-replay"}),
            )
            .await
            .unwrap();
            let view = loop {
                tokio::select! {
                    biased;
                    event = events.recv() => {
                        let raw = event.expect("bridge stopped during history replay");
                        let event: Value = serde_json::from_str(&raw).unwrap();
                        assert_ne!(event["type"], "bridge/error", "history replay failed: {event}");
                    }
                    result = &mut view_result => break result.unwrap().unwrap(),
                }
            };
            let updates = view["baseline"]["updates"].as_array().unwrap();
            assert_eq!(updates.len(), 10_050, "history replay was truncated");
            assert!(serialized_value_len(&view["baseline"]["updates"]) > 1_000_000);
            for (index, update) in updates.iter().enumerate() {
                assert_eq!(update["messageId"], format!("history-{index}"));
            }
            request(
                &commands,
                json!({"type": "session/list", "requestId": "after-burst"}),
            )
            .await
            .unwrap();
            assert!(
                !cancellation.is_cancelled(),
                "history replay stopped the connection"
            );
        })
        .await;
        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(5), bridge)
            .await
            .unwrap()
            .unwrap();
        result.expect("history replay did not complete");
    }

    #[tokio::test]
    async fn fork_response_batch_keeps_following_target_update_without_load() {
        async fn next_event(events: &mut mpsc::UnboundedReceiver<String>) -> Value {
            let raw = tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .expect("timed out waiting for bridge event")
                .expect("bridge stopped unexpectedly");
            let event: Value = serde_json::from_str(&raw).unwrap();
            assert_ne!(
                event["type"], "bridge/error",
                "unexpected bridge error: {event}"
            );
            event
        }
        let cwd = env!("CARGO_MANIFEST_DIR");
        let fixture = format!("{cwd}/tests/fixtures/fork-response-batch-agent.mjs");
        let options = options(&["attyd", "--cwd", cwd, "--", "node", &fixture]);
        let (commands, command_rx) = mpsc::unbounded_channel();
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
            json!({"type":"session/new", "requestId":"new", "cwd":cwd}),
        )
        .await
        .unwrap();
        let source = loop {
            let event = next_event(&mut events).await;
            if event["type"] == "bridge/session_view"
                && event["sessionId"] == "source"
                && event["view"]["baseline"]["updates"]
                    .as_array()
                    .is_some_and(|updates| {
                        updates
                            .iter()
                            .any(|update| update["messageId"] == "source-before-fork")
                    })
            {
                break event["view"].clone();
            }
        };
        let fork = request(
            &commands,
            json!({"type":"session/fork", "requestId":"fork", "sessionId":"source"}),
        )
        .await
        .unwrap();
        assert_eq!(fork["sessionId"], "target");
        assert_eq!(fork["view"]["session"]["phase"], "ready");
        let updates = fork["view"]["baseline"]["updates"].as_array().unwrap();
        assert!(
            updates
                .iter()
                .any(|update| update["messageId"] == "source-before-fork"),
            "the admission snapshot is the initial target baseline"
        );
        assert!(
            updates
                .iter()
                .any(|update| update["messageId"] == "target-after-response"),
            "post-response target traffic must append before optional history fallback"
        );
        assert_ne!(
            fork["view"]["baseline"]["updates"],
            source["baseline"]["updates"]
        );
        let (response, received) = oneshot::channel();
        commands
            .send(BridgeInput::SessionViewRequest {
                expected_owner: None,
                session_id: "target".into(),
                cwd: None,
                response,
            })
            .unwrap();
        let recovered = received.await.unwrap().unwrap();
        assert_eq!(recovered["baseline"], fork["view"]["baseline"]);
        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(5), bridge)
            .await
            .expect("bridge did not stop")
            .unwrap();
    }

    #[tokio::test]
    async fn fork_fallback_uses_the_source_baseline_captured_at_admission() {
        async fn next_event(events: &mut mpsc::UnboundedReceiver<String>) -> Value {
            let raw = tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .expect("timed out waiting for bridge event")
                .expect("bridge stopped unexpectedly");
            let event: Value = serde_json::from_str(&raw).unwrap();
            assert_ne!(
                event["type"], "bridge/error",
                "unexpected bridge error: {event}"
            );
            event
        }

        let cwd = env!("CARGO_MANIFEST_DIR");
        let fixture = format!("{cwd}/tests/fixtures/session-capabilities-agent.ts");
        let options = options(&[
            "attyd",
            "--cwd",
            cwd,
            "--",
            "node",
            "--import",
            "tsx",
            &fixture,
            "--fork-source-update-before-response",
        ]);
        let (commands, command_rx) = mpsc::unbounded_channel();
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
        request(&commands, json!({
            "type": "session/load", "requestId": "load-source", "sessionId": "saved", "cwd": cwd,
        })).await.unwrap();
        let source_before = loop {
            let event = next_event(&mut events).await;
            if event["type"] == "bridge/session_view"
                && event["sessionId"] == "saved"
                && event["view"]["session"]["phase"] == "ready"
            {
                break event["view"].clone();
            }
        };
        assert!(
            !source_before["baseline"]["updates"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        let fork_commands = commands.clone();
        let fork = tokio::spawn(async move {
            request(
                &fork_commands,
                json!({
                    "type": "session/fork", "requestId": "fork", "sessionId": "saved",
                }),
            )
            .await
        });
        let source_after = loop {
            let event = next_event(&mut events).await;
            if event["type"] == "bridge/session_view"
                && event["sessionId"] == "saved"
                && event["view"]["baseline"]["updates"]
                    .as_array()
                    .is_some_and(|updates| {
                        updates
                            .iter()
                            .any(|update| update["messageId"] == "source-update-during-fork")
                    })
            {
                break event["view"].clone();
            }
        };
        // The fixture sends a real idle update only after receiving Fork, and
        // holds its response until this test explicitly releases the barrier.
        assert!(
            !fork.is_finished(),
            "the Fork response must still be held by the fixture"
        );
        assert_eq!(source_after["session"]["phase"], "ready");
        assert_ne!(
            source_before["baseline"]["revision"],
            source_after["baseline"]["revision"]
        );
        assert_ne!(
            source_before["baseline"]["updates"],
            source_after["baseline"]["updates"]
        );

        let (release_reply, release_result) = oneshot::channel();
        commands
            .send(BridgeInput::BusinessRequest {
                command: json!({
                    "type": "session/list", "requestId": "release-fork-response", "cursor": "release-fork",
                }),
                response: release_reply,
            })
            .unwrap();
        // Releasing Fork can change the catalog before this list response is
        // reduced. That obsolete page is correctly rejected, but no other error is.
        if let Err(error) = release_result.await.unwrap() {
            assert_eq!(error.data.unwrap()["kind"], "session_catalog_changed");
        }
        let response = tokio::time::timeout(Duration::from_secs(10), fork)
            .await
            .expect("fork did not complete after releasing its response")
            .unwrap()
            .unwrap();
        assert_eq!(response["sessionId"], "forked");
        let target = &response["view"];
        assert_eq!(target["session"]["phase"], "ready");
        assert_eq!(
            target["baseline"]["updates"],
            source_before["baseline"]["updates"]
        );
        assert_ne!(
            target["baseline"]["updates"],
            source_after["baseline"]["updates"]
        );
        assert!(
            target["session"]["historyNotice"]
                .as_str()
                .unwrap()
                .contains("snapshot of the source")
        );
        let listed = request(
            &commands,
            json!({ "type": "session/list", "requestId": "after-fork" }),
        )
        .await
        .unwrap();
        assert_eq!(listed["_meta"]["forks"], 1);

        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(5), bridge)
            .await
            .expect("bridge did not stop")
            .unwrap();
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
        let (commands, command_rx) = mpsc::unbounded_channel();
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
        let (commands, command_rx) = mpsc::unbounded_channel();
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
        let (commands, command_rx) = mpsc::unbounded_channel();
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
                expected_owner: None,
                session_id: "test-session".to_string(),
                cwd: None,
                response: response_tx,
            })
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
        let (commands, command_rx) = mpsc::unbounded_channel();
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
        assert_eq!(
            state.lock().await.creation_staging["new-session"]
                .early_notifications
                .len(),
            1
        );
        assert!(rx.try_recv().is_err());

        state.lock().await.pending_creations = 0;
        handle_session_update(notification.clone(), &state, &sink, None).await;
        assert!(
            rx.try_recv().is_err(),
            "late unknown updates must stay hidden"
        );

        {
            let mut state = state.lock().await;
            let response = json!({ "sessionId": "new-session" });
            let replay = prepare_session_response(
                &mut state,
                "new-session",
                PathBuf::from("/workspace"),
                &response,
                None,
            )
            .unwrap();
            assert_eq!((state.early_update_count, state.early_update_bytes), (0, 0));
            let epoch = state.sessions.epoch().to_string();
            let incarnation = state
                .sessions
                .open_new_with_replay(
                    &epoch,
                    "new-session",
                    "/workspace",
                    response,
                    notification_values(&replay),
                )
                .unwrap();
            promote_creation_validation(&mut state, "new-session", incarnation).unwrap();
            assert!(state.creation_staging.is_empty());

            state.sessions.register_new("new-session", incarnation);
            state.published_runtime_seq = state.sessions.seq();
        }
        handle_session_update(notification, &state, &sink, None).await;
        let relayed = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|event| serde_json::from_str::<Value>(&event).unwrap())
            .find(|event| event["type"] == "acp/session_update")
            .unwrap();
        assert_eq!(relayed["type"], "acp/session_update");
        assert_eq!(relayed["notification"]["sessionId"], "new-session");
    }

    #[tokio::test]
    async fn rejects_invalid_active_updates_transactionally_then_relays_recovery() {
        let mut bridge = BridgeState::default();
        register_test_session(&mut bridge, "session", "/workspace");
        start_test_turn(&mut bridge, "session", "prompt").unwrap();
        bridge.published_runtime_seq = bridge.sessions.seq();
        let state = Arc::new(Mutex::new(bridge));
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
            session_validation(&*state.lock().await, "session")
                .unwrap()
                .update_count,
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
        let event = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|event| serde_json::from_str::<Value>(&event).unwrap())
            .find(|event| event["type"] == "acp/session_update")
            .unwrap();
        assert_eq!(event["type"], "acp/session_update");
        assert_eq!(
            session_validation(&*state.lock().await, "session")
                .unwrap()
                .update_count,
            1
        );
    }

    #[tokio::test]
    async fn canonical_runtime_routes_conversation_and_control_updates_separately() {
        let mut bridge_state = BridgeState::default();
        register_test_session(&mut bridge_state, "session", "/workspace");
        start_test_turn(&mut bridge_state, "session", "prompt").unwrap();
        bridge_state
            .sessions
            .sessions
            .get_mut("session")
            .unwrap()
            .state
            .live
            .as_mut()
            .unwrap()
            .session["modes"] = json!({ "currentModeId": "build" });
        bridge_state.published_runtime_seq = bridge_state.sessions.seq();
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
        let runtime = state.sessions.session("session").unwrap();
        assert_eq!(runtime.active_turn.as_ref().unwrap().updates.len(), 1);
        let owner = state.sessions.state("session").unwrap();
        assert!(Arc::ptr_eq(
            &runtime.active_turn.as_ref().unwrap().updates,
            &owner.active_turn.as_ref().unwrap().updates,
        ));
        assert_eq!(
            runtime.control_state["current_mode_update"]["currentModeId"],
            "plan",
        );
        assert_eq!(
            state
                .sessions
                .live("session")
                .unwrap()
                .current_mode_id()
                .unwrap(),
            "plan",
        );
        drop(state);
        let types = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|event| serde_json::from_str::<Value>(&event).unwrap()["type"].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            vec![
                json!("bridge/session_delta"),
                json!("bridge/internal_runtime_delta"),
                json!("acp/session_update"),
                json!("bridge/internal_runtime_delta"),
                json!("acp/session_update"),
            ],
        );
    }

    #[tokio::test]
    async fn idle_session_updates_advance_the_memory_view_without_a_local_prompt() {
        let mut bridge_state = BridgeState::default();
        let epoch = bridge_state.sessions.epoch().to_string();
        let incarnation = bridge_state
            .sessions
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        bridge_state
            .sessions
            .start_prompt(&epoch, "session", incarnation, "finished", Vec::new())
            .unwrap();
        bridge_state
            .sessions
            .complete_prompt(
                &epoch,
                "session",
                incarnation,
                "finished",
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();

        bridge_state.published_runtime_seq = bridge_state.sessions.seq();
        session_mirror(&mut bridge_state).register_new("session", incarnation);
        let previous_revision = session_mirror(&mut bridge_state)
            .view("session", incarnation)
            .unwrap()
            .baseline
            .revision()
            .to_string();
        let before = bridge_state.sessions.snapshot();
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

        let mut state = state.lock().await;
        assert_eq!(state.sessions.snapshot(), before);
        let view = session_mirror(&mut state)
            .view("session", incarnation)
            .unwrap();
        assert_eq!(view.baseline.updates().len(), 1);
        assert_eq!(view.baseline.updates()[0]["content"]["text"], "too late");
        assert_ne!(view.baseline.revision(), previous_revision);
        assert!(view.session.active_turn.is_none());
        drop(state);
        let events = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|raw| serde_json::from_str::<Value>(&raw).unwrap())
            .collect::<Vec<_>>();
        assert!(
            events
                .iter()
                .any(|event| event["type"] == "bridge/session_view")
        );
        assert!(
            events
                .iter()
                .any(|event| event["type"] == "acp/session_update")
        );
    }

    #[tokio::test]
    async fn terminal_reference_reduction_does_not_wait_for_the_physical_manager() {
        for valid in [true, false] {
            let workspace = tempfile::tempdir().unwrap();
            let filesystem =
                Arc::new(WorkspaceFileSystem::new(workspace.path(), false, &[]).unwrap());
            let (terminal_events, _terminal_rx) = mpsc::unbounded_channel();
            let terminals = TerminalManager::new_with_snapshots(filesystem, terminal_events, None);
            let mut bridge = BridgeState::default();
            let incarnation = register_test_session(&mut bridge, "session", "/workspace");
            let owner = bridge
                .sessions
                .resource_owner("session", incarnation)
                .unwrap();
            let (tx, mut rx) = mpsc::unbounded_channel();
            let sink = EventSink { tx: tx.into() };
            if valid {
                register_created_terminal_locked(&mut bridge, &sink, &owner, "terminal").unwrap();
            }
            while rx.try_recv().is_ok() {}
            let state = Arc::new(Mutex::new(bridge));
            let notification = serde_json::from_value(json!({
                "sessionId": "session",
                "update": {
                    "sessionUpdate": "tool_call", "toolCallId": "tool", "title": "Run",
                    "content": [{ "type": "terminal", "terminalId": "terminal" }]
                }
            }))
            .unwrap();
            let _physical_busy = terminals.pause_references_for_test().await;
            let update = handle_session_update(notification, &state, &sink, Some(&terminals));
            tokio::pin!(update);
            assert!(
                futures::poll!(&mut update).is_ready(),
                "local reduction cannot wait on terminal I/O"
            );
            let mut locked = state.lock().await;
            let view = locked.sessions.view("session", incarnation).unwrap();
            assert_eq!(view.baseline.updates().len(), usize::from(valid));
            let events = std::iter::from_fn(|| rx.try_recv().ok())
                .map(|raw| serde_json::from_str::<Value>(&raw).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                events
                    .iter()
                    .any(|event| event["type"] == "acp/session_update"),
                valid
            );
            assert_eq!(
                events.iter().any(|event| event["type"] == "bridge/error"),
                !valid
            );
        }
    }

    #[test]
    fn terminal_creation_identity_preserves_output_and_rejects_retired_handles() {
        let mut state = BridgeState::default();
        let old = register_test_session(&mut state, "session", "/workspace");
        let owner = state.sessions.resource_owner("session", old).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        publish_terminal_snapshot(
            &mut state,
            &sink,
            TerminalSnapshot {
                incarnation: old,
                value: json!({
                    "sessionId": "session", "terminalId": "terminal", "output": "already arrived",
                    "outputAppend": true, "retainedBytes": 15, "truncated": false,
                    "exitStatus": { "exitCode": 0 }, "released": false
                }),
            },
        );
        let before = state.sessions.live("session").unwrap().terminals["terminal"].clone();
        register_created_terminal_locked(&mut state, &sink, &owner, "terminal").unwrap();
        assert_eq!(
            state.sessions.live("session").unwrap().terminals["terminal"],
            before
        );
        publish_terminal_snapshot(
            &mut state,
            &sink,
            TerminalSnapshot {
                incarnation: old,
                value: json!({ "sessionId": "session", "terminalId": "terminal", "released": true }),
            },
        );
        assert!(register_created_terminal_locked(&mut state, &sink, &owner, "terminal").is_err());
        assert!(state.sessions.live("session").unwrap().terminals.is_empty());
        state
            .sessions
            .start_operation(
                &owner.epoch,
                "session",
                old,
                "close",
                RuntimeSessionOperationKind::Close,
                "closing",
            )
            .unwrap();
        state
            .sessions
            .close_session(&owner.epoch, "session", old, "close")
            .unwrap();
        complete_session_retirement(
            &mut state,
            &sink,
            "session",
            old,
            SessionAdmission::Close,
            "close",
        );
        let new = register_test_session(&mut state, "session", "/workspace");
        assert_ne!(new, old);
        assert!(
            register_created_terminal_locked(&mut state, &sink, &owner, "late-terminal").is_err()
        );
        assert!(state.sessions.live("session").unwrap().terminals.is_empty());
    }

    #[tokio::test]
    async fn reload_validation_failure_preserves_the_session_compaction_index() {
        let mut bridge_state = BridgeState::default();
        let epoch = bridge_state.sessions.epoch().to_string();
        let incarnation = bridge_state
            .sessions
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        session_mirror(&mut bridge_state).register_new("session", incarnation);

        let validation =
            ensure_session_validation(&mut bridge_state, "session", incarnation).unwrap();
        validate_and_track_session_update(validation, &json!({ "sessionUpdate": "compaction_update", "compactionId": "original", "status": "in_progress" })).unwrap();
        let state = Arc::new(Mutex::new(bridge_state));
        reserve_attachment("session", RuntimeSessionOperationKind::Load, &state)
            .await
            .unwrap();
        let mut state = state.lock().await;
        validate_and_track_session_update(session_validation_mut(&mut state, "session").unwrap(), &json!({ "sessionUpdate": "compaction_update", "compactionId": "candidate", "status": "in_progress" })).unwrap();
        let owner = replay_validation_backup(&state, "session")
            .unwrap()
            .owner
            .clone();
        clear_attachment_tracking(&mut state, "session", &owner, true);
        let validation = session_validation_mut(&mut state, "session")
            .expect("failed replay keeps prior validation");
        validate_and_track_session_update(validation, &json!({ "sessionUpdate": "compaction_summary_chunk", "compactionId": "original", "content": { "type": "text", "text": "continued" } })).unwrap();
        assert!(validate_and_track_session_update(validation, &json!({ "sessionUpdate": "compaction_summary_chunk", "compactionId": "candidate", "content": { "type": "text", "text": "must not survive" } })).is_err());
    }

    #[tokio::test]
    async fn automatic_load_blocks_close_delete_and_another_reload_until_its_attempt_finishes() {
        let mut state = BridgeState::default();
        let epoch = state.sessions.epoch().to_string();
        let incarnation = state
            .sessions
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();

        session_mirror(&mut state).register_cold("session", incarnation);
        assert!(
            reserve_session_operation(
                &mut state,
                "session",
                RuntimeSessionOperationKind::Close,
                "close"
            )
            .is_err(),
            "the cold target is already reserved for materialization"
        );
        assert!(reserve_deletion(&mut state, "session", "delete").is_err());
        session_mirror(&mut state)
            .begin_load("session", incarnation, "automatic")
            .unwrap();
        begin_replay_validation(&mut state, "session");
        assert!(
            reserve_session_operation(
                &mut state,
                "session",
                RuntimeSessionOperationKind::Close,
                "close"
            )
            .is_err()
        );
        assert!(reserve_deletion(&mut state, "session", "delete").is_err());
        let state = Arc::new(Mutex::new(state));
        assert!(
            reserve_attachment("session", RuntimeSessionOperationKind::Load, &state)
                .await
                .is_err()
        );
        let mut state = state.lock().await;
        session_mirror(&mut state)
            .fail_load("session", incarnation, "automatic", "retrying", true)
            .unwrap();
        assert!(
            reserve_session_operation(
                &mut state,
                "session",
                RuntimeSessionOperationKind::Close,
                "close"
            )
            .is_err(),
            "retry backoff retains the load scope"
        );
    }

    #[test]
    fn replay_validation_rollback_cannot_replace_a_new_attempt_or_incarnation() {
        let mut state = BridgeState::default();
        let epoch = state.sessions.epoch().to_string();
        let first = state
            .sessions
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();

        let old = begin_replay_validation(&mut state, "session");
        let newer = begin_replay_validation(&mut state, "session");
        rollback_replay_validation(&mut state, "session", &old);
        assert!(newer.matches(&state, "session"));
        assert!(replay_validation_backup(&state, "session").is_some());
        state
            .sessions
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
            .sessions
            .close_session(&epoch, "session", first, "close")
            .unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        complete_session_retirement(
            &mut state,
            &sink,
            "session",
            first,
            SessionAdmission::Close,
            "close",
        );
        let second = state
            .sessions
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        let fresh = ensure_session_validation(&mut state, "session", second)
            .unwrap()
            .allocation
            .clone();
        rollback_replay_validation(&mut state, "session", &newer);
        assert!(Arc::ptr_eq(
            &fresh,
            &session_validation(&state, "session").unwrap().allocation
        ));
        assert!(replay_validation_backup(&state, "session").is_none());
    }

    #[test]
    fn an_early_allocation_owner_survives_materialization_but_not_reallocation() {
        let mut state = BridgeState {
            pending_creations: 1,
            ..BridgeState::default()
        };
        let staging = state
            .creation_staging
            .entry("session".to_string())
            .or_default();
        validate_and_track_session_update(&mut staging.validation, &json!({
            "sessionUpdate": "compaction_update", "compactionId": "early", "status": "in_progress"
        })).unwrap();
        let owner = SessionUpdateOwner {
            incarnation: None,
            allocation: staging.validation.allocation.clone(),
        };
        let response = json!({ "sessionId": "session" });
        prepare_session_response(
            &mut state,
            "session",
            PathBuf::from("/workspace"),
            &response,
            None,
        )
        .unwrap();
        state.pending_creations = 0;
        assert!(state.creation_staging.contains_key("session"));
        assert!(
            state.sessions.state("session").is_none(),
            "response validation does not publish an inc=0 owner"
        );
        let epoch = state.sessions.epoch().to_string();
        let incarnation = state
            .sessions
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        promote_creation_validation(&mut state, "session", incarnation).unwrap();

        state.sessions.register_new("session", incarnation);
        assert!(owner.matches(&state, "session"));
        assert!(state.creation_staging.is_empty());
        validate_and_track_session_update(session_validation_mut(&mut state, "session").unwrap(), &json!({
            "sessionUpdate": "compaction_summary_chunk", "compactionId": "early", "content": { "type": "text", "text": "continued after creation" }
        })).unwrap();
        begin_replay_validation(&mut state, "session");
        assert!(!owner.matches(&state, "session"));
    }

    #[tokio::test]
    async fn creation_staging_keeps_cold_resources_separate_and_counts_every_unknown_id() {
        let mut bridge = BridgeState {
            pending_creations: 1,
            ..BridgeState::default()
        };
        bridge.sessions.register_cold("cold", 0);
        bridge.sessions.register_cold("another-cold", 0);
        let cold_allocation = ensure_session_validation(&mut bridge, "cold", 0)
            .unwrap()
            .allocation
            .clone();
        let cancellation = CancellationToken::new();
        bridge
            .sessions
            .resources_mut("cold", 0)
            .unwrap()
            .materialization = Some(crate::session_resources::MaterializationResources {
            attempt_id: "cold-load".to_string(),
            waiters: Vec::new(),
            cancellation: cancellation.clone(),
        });
        let state = Arc::new(Mutex::new(bridge));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        for session_id in ["cold", "another-cold"] {
            handle_session_update(
                serde_json::from_value(json!({
                    "sessionId": session_id, "update": {
                        "sessionUpdate": "agent_message_chunk", "messageId": "early",
                        "content": { "type": "text", "text": "creation replay" }
                    }
                }))
                .unwrap(),
                &state,
                &sink,
                None,
            )
            .await;
        }
        let mut state = state.lock().await;
        assert_eq!(state.creation_staging.len(), 1);
        assert!(
            !state.creation_staging.contains_key("another-cold"),
            "a preexisting cold entry cannot bypass the creation ID limit"
        );
        assert_eq!(state.early_update_count, 1);
        assert_eq!(
            state.early_update_bytes,
            state.creation_staging["cold"].bytes
        );
        assert!(state.early_update_bytes > 0);
        let staged_allocation = state.creation_staging["cold"].validation.allocation.clone();
        assert!(!Arc::ptr_eq(&cold_allocation, &staged_allocation));
        assert!(Arc::ptr_eq(
            &cold_allocation,
            &session_validation(&state, "cold").unwrap().allocation
        ));
        assert_eq!(session_validation(&state, "cold").unwrap().update_count, 0);
        assert!(!cancellation.is_cancelled());
        assert!(
            promote_creation_validation(&mut state, "cold", 0).is_err(),
            "promotion cannot overwrite resources of an existing owner"
        );
        assert_eq!(state.creation_staging.len(), 1);
        let owner = SessionUpdateOwner {
            incarnation: None,
            allocation: staged_allocation,
        };
        assert!(owner.matches(&state, "cold"));
        state.pending_creations = 0;
        clear_pending_creation_replays(&mut state);
        assert_eq!((state.early_update_count, state.early_update_bytes), (0, 0));
        assert!(!owner.matches(&state, "cold"));
        assert!(Arc::ptr_eq(
            &cold_allocation,
            &session_validation(&state, "cold").unwrap().allocation
        ));
        assert!(!cancellation.is_cancelled());
        drop(state);
        let error: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert!(error["data"].as_str().unwrap().contains("more session IDs"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_known_attachment_does_not_consume_creation_staging_or_accept_its_old_owner() {
        let mut bridge = BridgeState {
            pending_creations: 1,
            ..BridgeState::default()
        };
        bridge.sessions.register_cold("session", 0);
        let staging = bridge
            .creation_staging
            .entry("session".to_string())
            .or_default();
        let early_owner = SessionUpdateOwner {
            incarnation: None,
            allocation: staging.validation.allocation.clone(),
        };
        let state = Arc::new(Mutex::new(bridge));
        let reservation = reserve_attachment_with_cwd(
            "session",
            RuntimeSessionOperationKind::Load,
            Some("/workspace"),
            "load",
            None,
            &state,
        )
        .await
        .unwrap();
        let incarnation = reservation.incarnation();
        let owner = {
            let mut state = state.lock().await;
            state
                .sessions
                .begin_load("session", incarnation, "load")
                .unwrap();
            assert!(
                !early_owner.matches(&state, "session"),
                "an earlier unknown creation cannot follow a different attachment allocation"
            );
            replay_validation_backup(&state, "session")
                .unwrap()
                .owner
                .clone()
        };
        assert_eq!(owner.incarnation, Some(incarnation));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        handle_session_update(serde_json::from_value(json!({
            "sessionId": "session", "update": { "sessionUpdate": "agent_message_chunk", "messageId": "loaded",
                "content": { "type": "text", "text": "attachment replay" } }
        })).unwrap(), &state, &sink, None).await;
        let mut state = state.lock().await;
        assert!(owner.matches(&state, "session"));
        assert_eq!(
            state.creation_staging["session"].early_notifications.len(),
            0
        );
        assert_eq!((state.early_update_count, state.early_update_bytes), (0, 0));
        assert_eq!(
            state
                .sessions
                .load_candidate("session", incarnation, "load")
                .unwrap()
                .lock()
                .updates()
                .unwrap()
                .len(),
            1
        );
        let validation = state
            .sessions
            .resources("session", incarnation)
            .unwrap()
            .validation
            .as_ref()
            .unwrap()
            .allocation
            .clone();
        state.pending_creations = 0;
        clear_pending_creation_replays(&mut state);
        assert!(Arc::ptr_eq(
            &validation,
            &session_validation(&state, "session").unwrap().allocation
        ));
        assert!(owner.matches(&state, "session"));
        drop(state);
        assert!(std::iter::from_fn(|| rx.try_recv().ok()).all(|event| {
            serde_json::from_str::<Value>(&event).unwrap()["type"] != "bridge/error"
        }));
    }

    #[tokio::test]
    async fn attachment_updates_stay_candidate_only_until_agent_response() {
        let mut bridge_state = BridgeState::default();
        let epoch = bridge_state.sessions.epoch().to_string();
        let incarnation = bridge_state
            .sessions
            .start_attachment(
                &epoch,
                "session",
                "/workspace",
                "load",
                RuntimeSessionOperationKind::Load,
            )
            .unwrap();
        session_mirror(&mut bridge_state).register_cold("session", incarnation);
        let operation = bridge_state
            .sessions
            .state("session")
            .unwrap()
            .operation
            .as_ref()
            .unwrap();
        assert_eq!(operation.operation_id, "load");
        assert_eq!(operation.kind, RuntimeSessionOperationKind::Load);
        assert_eq!(operation.stage, "attaching");
        bridge_state
            .sessions
            .begin_load("session", incarnation, "load")
            .unwrap();
        clear_attachment_delivery(&mut bridge_state, "session", incarnation);
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
            !serde_json::to_string(&state.sessions.snapshot())
                .unwrap()
                .contains("candidate")
        );
        let baseline = state
            .sessions
            .commit_load("session", incarnation, "load")
            .unwrap();
        assert_eq!(baseline.updates()[0]["content"]["text"], "candidate");
        state
            .sessions
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
            !serde_json::to_string(&state.sessions.snapshot())
                .unwrap()
                .contains("candidate"),
            "history payload belongs to the shared cache, not runtime snapshots",
        );
        assert!(
            state
                .sessions
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
        let incarnation = register_test_session(&mut bridge_state, "session", "/workspace");
        let state = Arc::new(Mutex::new(bridge_state));
        reserve_attachment("session", RuntimeSessionOperationKind::Load, &state)
            .await
            .unwrap();
        {
            let mut state = state.lock().await;
            state
                .sessions
                .begin_load("session", incarnation, "test-attachment")
                .unwrap();
            state
                .sessions
                .resources_mut("session", incarnation)
                .unwrap()
                .attachment
                .subscriber = Some(42);
        }
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
            session_validation(&state, "session")
                .unwrap()
                .invalid_reason
                .is_some(),
            "invalid replacement input must force the later load response to roll back"
        );
        assert_eq!(
            state.sessions.session("session").unwrap().incarnation,
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
        let error = prepare_session_response(
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
        assert!(!state.sessions.active_session("new-session").is_some());
        state.pending_creations = 0;
        clear_pending_creation_replays(&mut state);
        assert!(!state.creation_staging.contains_key("new-session"));
        assert!(state.sessions.state("new-session").is_none());
    }

    #[test]
    fn relays_complete_structured_acp_errors() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        sink.acp_error(
            Error::internal_error()
                .data(json!({ "payload": "x".repeat(FORMER_BRIDGE_ERROR_DATA_BYTES + 1) })),
            Some("request"),
            Some("session/prompt"),
        );
        let event: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(event["type"], "bridge/error");
        assert!(event.get("dataTruncated").is_none());
        assert_eq!(
            event["data"]["payload"].as_str().unwrap().len(),
            FORMER_BRIDGE_ERROR_DATA_BYTES + 1
        );
        assert_eq!(event["requestId"], "request");
    }

    #[test]
    fn relays_large_values_and_browser_events_without_replacement() {
        assert!(relay_bytes(&json!({ "ok": true })).is_ok());
        assert!(relay_bytes(&json!({ "padding": "x".repeat(FORMER_AGENT_RELAY_BYTES) })).is_ok());

        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        sink.send(json!({ "padding": "x".repeat(FORMER_BRIDGE_MESSAGE_BYTES) }));
        let event: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(
            event["padding"].as_str().unwrap().len(),
            FORMER_BRIDGE_MESSAGE_BYTES
        );
    }

    #[test]
    fn validates_browser_field_types_and_terminal_dimensions() {
        let command = json!({
            "requestId": "request",
            "empty": "",
            "tooLong": "x".repeat(FORMER_BRIDGE_IDENTIFIER_LENGTH + 1),
            "wideIdentifier": "😀".repeat(FORMER_BRIDGE_IDENTIFIER_LENGTH / 2),
            "tooWideIdentifier": "😀".repeat(FORMER_BRIDGE_IDENTIFIER_LENGTH / 2 + 1),
            "path": "x".repeat(FORMER_BRIDGE_PATH_LENGTH),
            "cols": 80,
            "zero": 0,
        });
        assert_eq!(string_field(&command, "requestId").unwrap(), "request");
        assert!(string_field(&command, "empty").is_err());
        assert!(string_field(&command, "tooLong").is_ok());
        assert!(string_field(&command, "wideIdentifier").is_ok());
        assert!(string_field(&command, "tooWideIdentifier").is_ok());
        assert!(nonempty_string_field(&command, "path").is_ok());
        assert_eq!(string_field_allow_empty(&command, "empty").unwrap(), "");
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
    async fn attaching_workspace_uses_its_canonical_path_before_response() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("session.txt");
        std::fs::write(&path, "attachment workspace").unwrap();
        let filesystem = WorkspaceFileSystem::new(root.path(), false, &[]).unwrap();
        let mut bridge = BridgeState::default();
        let epoch = bridge.sessions.epoch().to_string();
        let incarnation = bridge
            .sessions
            .start_attachment(
                &epoch,
                "session",
                workspace.path(),
                "load",
                RuntimeSessionOperationKind::Load,
            )
            .unwrap();
        let state = Arc::new(Mutex::new(bridge));
        let (owner, filesystem) = require_session_workspace("session", &state, &filesystem)
            .await
            .unwrap();
        assert_eq!(owner, incarnation);
        assert_eq!(
            filesystem
                .read(ReadTextFileRequest::new("session", path))
                .await
                .unwrap()
                .content,
            "attachment workspace"
        );
        let mut locked = state.lock().await;
        locked
            .sessions
            .complete_attachment(
                &epoch,
                "session",
                incarnation,
                "load",
                RuntimeSessionOperationKind::Load,
                json!({}),
            )
            .unwrap();
        locked.sessions.register_new("session", incarnation);
        locked
            .sessions
            .start_operation(
                &epoch,
                "session",
                incarnation,
                "close",
                RuntimeSessionOperationKind::Close,
                "closing",
            )
            .unwrap();
        locked
            .sessions
            .close_session(&epoch, "session", incarnation, "close")
            .unwrap();
        drop(locked);
        assert!(
            require_session_workspace("session", &state, &filesystem)
                .await
                .is_err()
        );
    }

    #[test]
    fn reload_does_not_consume_an_extra_creation_slot() {
        let mut state = BridgeState::default();
        for index in 0..FORMER_TRACKED_SESSIONS - 1 {
            register_test_session(&mut state, &format!("session-{index}"), "/workspace");
        }
        reserve_attachment_locked(
            &mut state,
            "session-0",
            RuntimeSessionOperationKind::Load,
            None,
            "reload",
            None,
        )
        .unwrap();
        assert_eq!(
            state.sessions.allocated_session_count(),
            FORMER_TRACKED_SESSIONS - 1
        );
        reserve_attachment_locked(
            &mut state,
            "new-session",
            RuntimeSessionOperationKind::Load,
            Some("/workspace"),
            "fresh-load",
            None,
        )
        .unwrap();
        assert_eq!(
            state.sessions.allocated_session_count(),
            FORMER_TRACKED_SESSIONS
        );
        assert!(
            reserve_attachment_locked(
                &mut state,
                "overflow",
                RuntimeSessionOperationKind::Load,
                Some("/workspace"),
                "overflow-load",
                None
            )
            .is_ok()
        );
        // A confirmed close followed by failed deletion leaves a Closed live
        // owner. Reattaching replaces that allocation instead of requiring a new slot.
        let incarnation = state.sessions.state("session-1").unwrap().incarnation;
        let epoch = state.sessions.epoch().to_string();
        state
            .sessions
            .start_delete(&epoch, "session-1", incarnation, "delete")
            .unwrap();
        state
            .sessions
            .delete_close_succeeded(&epoch, "session-1", incarnation, "delete")
            .unwrap();
        state
            .sessions
            .fail_operation(
                &epoch,
                "session-1",
                incarnation,
                "delete",
                RuntimeSessionOperationKind::Delete,
                json!({}),
            )
            .unwrap();
        let replacement = reserve_attachment_locked(
            &mut state,
            "session-1",
            RuntimeSessionOperationKind::Load,
            Some("/workspace"),
            "replace-closed",
            None,
        )
        .unwrap();
        assert_ne!(replacement.incarnation(), incarnation);
        assert_eq!(
            state.sessions.allocated_session_count(),
            FORMER_TRACKED_SESSIONS + 1
        );
    }

    #[tokio::test]
    async fn reserves_listed_sessions_once_and_rejects_busy_attachments() {
        let state = Arc::new(Mutex::new(BridgeState::default()));
        state.lock().await.listed_sessions.insert(
            "saved".to_string(),
            session_info("saved", "/agent/workspace"),
        );
        let reservation = reserve_attachment("saved", RuntimeSessionOperationKind::Load, &state)
            .await
            .unwrap();
        let AttachmentReservation::Fresh { cwd, incarnation } = reservation else {
            panic!("listed session must begin a fresh attachment");
        };
        assert_eq!(cwd, PathBuf::from("/agent/workspace"));
        assert!(
            reserve_attachment("saved", RuntimeSessionOperationKind::Load, &state)
                .await
                .is_err()
        );
        {
            let mut state = state.lock().await;
            let epoch = state.sessions.epoch().to_string();
            session_mirror(&mut state)
                .begin_load("saved", incarnation, "test-attachment")
                .unwrap();
            state
                .sessions
                .complete_attachment(
                    &epoch,
                    "saved",
                    incarnation,
                    "test-attachment",
                    RuntimeSessionOperationKind::Load,
                    json!({ "sessionId": "saved" }),
                )
                .unwrap();
            session_mirror(&mut state)
                .commit_load("saved", incarnation, "test-attachment")
                .unwrap();
            settle_attachment(&mut state, "saved");

            start_test_turn(&mut state, "saved", "busy-prompt").unwrap();
        }
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
        let incarnation = register_test_session(&mut bridge, "session", "/agent/workspace");
        let state = Arc::new(Mutex::new(bridge));
        let reservation = reserve_attachment("session", RuntimeSessionOperationKind::Load, &state)
            .await
            .expect("an idle tracked session must reach the Agent's session/load implementation");
        assert_eq!(
            reservation,
            AttachmentReservation::Reload {
                cwd: PathBuf::from("/agent/workspace"),
                incarnation
            }
        );
        assert!(session_operation_pending(
            &*state.lock().await,
            "session",
            SessionAdmission::Attachment
        ));
        {
            let mut state = state.lock().await;
            let epoch = state.sessions.epoch().to_string();
            state
                .sessions
                .complete_reload(
                    &epoch,
                    "session",
                    incarnation,
                    "test-attachment",
                    json!({ "sessionId": "session" }),
                )
                .unwrap();
            let owner = replay_validation_backup(&state, "session")
                .unwrap()
                .owner
                .clone();
            clear_attachment_tracking(&mut state, "session", &owner, true);
            let operation = start_test_turn(&mut state, "session", "prompt").unwrap();
            // The live projection can retire its turn before the owner commits
            // history. Reconciling must still exclude another load in that window.
            let terminal = json!({ "stopReason": "end_turn" });
            state
                .sessions
                .complete_prompt(&epoch, "session", incarnation, "prompt", terminal.clone())
                .unwrap();
            session_mirror(&mut state)
                .complete_turn("session", incarnation, &operation, terminal)
                .unwrap();
            assert!(
                state
                    .sessions
                    .session("session")
                    .unwrap()
                    .active_turn
                    .is_none()
            );
            assert_eq!(
                session_mirror(&mut state).state("session").unwrap().phase,
                MirrorPhase::Reconciling
            );
        }
        assert!(
            reserve_attachment("session", RuntimeSessionOperationKind::Load, &state)
                .await
                .is_err()
        );
        assert!(!session_operation_pending(
            &*state.lock().await,
            "session",
            SessionAdmission::Attachment
        ));
        {
            let mut state = state.lock().await;
            let mirror = session_mirror(&mut state);
            let operation = mirror
                .state("session")
                .unwrap()
                .active_turn
                .as_ref()
                .unwrap()
                .operation_id
                .clone();
            mirror
                .commit_completed_turn_from_memory("session", incarnation, &operation)
                .unwrap();
        }
        assert!(
            reserve_attachment("session", RuntimeSessionOperationKind::Load, &state)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn observe_registration_and_reset_precede_the_next_owner_delta() {
        let mut state = BridgeState::default();
        let incarnation = register_test_session(&mut state, "session", "/workspace");
        let operation = start_test_turn(&mut state, "session", "turn").unwrap();
        let revision = state.sessions.state("session").unwrap().view_revision;
        let (tx, mut events) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        let lease = ObservationLease::new();
        let (reply, result) = oneshot::channel();
        assert!(
            prepare_session_view_request(
                &mut state,
                &sink,
                "session".into(),
                None,
                SessionViewWaiter::Observe {
                    observer_id: 9,
                    lease: lease.clone(),
                    reply
                },
            )
            .is_none()
        );
        assert!(result.await.unwrap().is_ok());

        let update = json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "after observe" } });
        state
            .sessions
            .append_turn_update("session", incarnation, &operation, update.clone())
            .unwrap();
        sink.send(
            session_delta_value(
                &state,
                "session",
                incarnation,
                json!({ "kind": "turn_update", "update": update }),
            )
            .unwrap(),
        );
        let ready: Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
        let delta: Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
        assert_eq!(ready["type"], "bridge/internal_observer_ready");
        assert_eq!(ready["reset"]["viewRevision"], revision);
        assert_eq!(delta["fromRevision"], revision);
        assert_eq!(delta["viewRevision"], revision + 1);
        assert_eq!(ready["reset"]["sessionIncarnation"], incarnation);
        assert_eq!(ready["reset"]["bridgeEpoch"], delta["bridgeEpoch"]);
        assert!(events.try_recv().is_err());
        let resources = state
            .sessions
            .resources_mut("session", incarnation)
            .unwrap();
        assert!(
            resources
                .observers
                .refresh(
                    Some(&SessionLifecycle::Active),
                    0,
                    tokio::time::Instant::now()
                )
                .is_none()
        );
        assert!(resources.observers.unobserve(9, &lease));
    }

    #[tokio::test(start_paused = true)]
    async fn owner_cancelled_observe_restarts_absence_and_fences_an_old_timer() {
        let mut state = BridgeState::default();
        let incarnation = register_test_session(&mut state, "session", "/workspace");
        let (input, mut commands) = mpsc::unbounded_channel();
        state.observer_events = Some(input);
        state.observer_timeout = Some(30);
        refresh_observer_timers(&mut state);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        let (tx, _events) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        let lease = ObservationLease::new();
        let (reply, accepted) = oneshot::channel();
        prepare_session_view_request(
            &mut state,
            &sink,
            "session".into(),
            None,
            SessionViewWaiter::Observe {
                observer_id: 1,
                lease: lease.clone(),
                reply,
            },
        );
        assert!(accepted.await.unwrap().is_ok());
        lease.cancel();
        tokio::task::yield_now().await;
        let BridgeInput::UnobserveSession {
            observer_id, lease, ..
        } = commands.try_recv().unwrap()
        else {
            panic!("lease cancellation must notify the owner");
        };
        state
            .sessions
            .resources_mut("session", incarnation)
            .unwrap()
            .observers
            .unobserve(observer_id, &lease);
        refresh_observer_timers(&mut state);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(29)).await;
        tokio::task::yield_now().await;
        assert!(commands.try_recv().is_err());
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        let BridgeInput::RetireUnobservedSession {
            absence_id,
            permit,
            incarnation: timer_incarnation,
            ..
        } = commands.try_recv().unwrap()
        else {
            panic!("full absence interval must expire");
        };
        assert_eq!(timer_incarnation, incarnation);
        assert!(
            state
                .sessions
                .resources("session", incarnation)
                .unwrap()
                .observers
                .matches_absence(absence_id, &permit)
        );
        state.observers_stopped = true;
        refresh_observer_timers(&mut state);
        assert!(!permit.claim());
    }

    #[tokio::test]
    async fn cold_materialization_moves_waiters_to_the_installed_incarnation_once() {
        let mut bridge = BridgeState::default();
        let (tx, _events) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        let (first, mut first_result) = oneshot::channel();
        let command = prepare_session_view_request(
            &mut bridge,
            &sink,
            "saved".into(),
            Some("/workspace".into()),
            SessionViewWaiter::View(first),
        )
        .unwrap();
        let owner = command["bridgeMaterializationId"]
            .as_str()
            .unwrap()
            .to_string();
        let (second, mut second_result) = oneshot::channel();
        assert!(
            prepare_session_view_request(
                &mut bridge,
                &sink,
                "saved".into(),
                Some("/workspace".into()),
                SessionViewWaiter::View(second)
            )
            .is_none()
        );
        let token = session_materialization(&bridge, "saved")
            .unwrap()
            .cancellation
            .clone();
        let state = Arc::new(Mutex::new(bridge));
        assert!(
            reserve_attachment_with_cwd(
                "saved",
                RuntimeSessionOperationKind::Load,
                Some("/workspace"),
                "explicit-load",
                None,
                &state
            )
            .await
            .is_err()
        );
        let reservation = reserve_attachment_with_cwd(
            "saved",
            RuntimeSessionOperationKind::Load,
            Some("/workspace"),
            "attempt",
            Some(&owner),
            &state,
        )
        .await
        .unwrap();
        assert!(reservation.incarnation() > 0);
        assert!(!token.is_cancelled());
        assert!(matches!(
            first_result.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            second_result.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        {
            let state = state.lock().await;
            let resources = state
                .sessions
                .resources("saved", reservation.incarnation())
                .unwrap();
            assert_eq!(resources.materialization.as_ref().unwrap().waiters.len(), 2);
            assert_eq!(
                resources.materialization.as_ref().unwrap().attempt_id,
                owner
            );
            assert_eq!(
                state
                    .sessions
                    .iter_states()
                    .filter(|session| session.session_id == "saved")
                    .count(),
                1
            );
        }
        let not_found = Error::resource_not_found(None);
        {
            let mut state = state.lock().await;
            fail_runtime_attachment(
                &mut state,
                "saved",
                reservation.incarnation(),
                "attempt",
                RuntimeSessionOperationKind::Load,
                false,
                &not_found,
            )
            .unwrap();
            state.sessions.remove("saved", reservation.incarnation());
            assert!(
                !token.is_cancelled(),
                "history removal must leave delivery to the load result owner"
            );
            assert!(session_materialization(&state, "saved").is_some());
        }
        resolve_session_view_waiters(&state, &sink, "saved", &owner, &Err(not_found)).await;
        assert!(matches!(
            first_result.await.unwrap(),
            Err(SessionViewError::NotFound)
        ));
        assert!(matches!(
            second_result.await.unwrap(),
            Err(SessionViewError::NotFound)
        ));
        assert!(token.is_cancelled());
        assert!(state.lock().await.sessions.state("saved").is_none());
    }

    #[tokio::test]
    async fn only_the_owner_can_settle_materialization_waiters() {
        let state = Arc::new(Mutex::new(BridgeState::default()));
        let (events, _events_rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx: events.into() };
        let (sender, mut receiver) = oneshot::channel();
        {
            let mut state = state.lock().await;
            state.sessions.register_cold("session", 0);
            state
                .sessions
                .resources_mut("session", 0)
                .unwrap()
                .materialization = Some(crate::session_resources::MaterializationResources {
                attempt_id: "owner".into(),
                waiters: vec![SessionViewWaiter::View(sender)],
                cancellation: CancellationToken::new(),
            });
        }

        let failed = Err(Error::internal_error().data("unrelated load"));
        resolve_session_view_waiters(&state, &sink, "session", "not-owner", &failed).await;
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            session_materialization(&*state.lock().await, "session")
                .map(|owner| owner.attempt_id.as_str()),
            Some("owner")
        );

        resolve_session_view_waiters(&state, &sink, "session", "owner", &failed).await;
        assert!(receiver.await.unwrap().is_err());
        assert!(session_materialization(&*state.lock().await, "session").is_none());
    }

    #[test]
    fn serializes_prompts_lifecycle_controls_and_deletion_attachment_races() {
        let mut state = BridgeState::default();
        let active = register_test_session(&mut state, "active", "/agent/workspace");
        register_test_session(&mut state, "other", "/agent/other-workspace");
        let active_turn = start_test_turn(&mut state, "active", "active-prompt").unwrap();
        let other_turn = start_test_turn(&mut state, "other", "other-prompt").unwrap();
        assert!(session_prompt_pending(&state, "active"));
        assert!(session_prompt_pending(&state, "other"));
        assert!(start_test_turn(&mut state, "active", "competing-prompt").is_err());
        assert!(
            reserve_session_operation(
                &mut state,
                "active",
                RuntimeSessionOperationKind::Fork,
                "fork"
            )
            .is_err()
        );
        reserve_session_operation(
            &mut state,
            "active",
            RuntimeSessionOperationKind::SetMode,
            "control",
        )
        .unwrap();
        assert!(session_prompt_pending(&state, "active"));
        assert!(
            reserve_session_operation(
                &mut state,
                "active",
                RuntimeSessionOperationKind::Close,
                "close"
            )
            .is_err()
        );
        release_session_operation(
            &mut state,
            "active",
            active,
            SessionAdmission::Control,
            "not-owner",
        );
        assert!(session_operation_pending(
            &state,
            "active",
            SessionAdmission::Control
        ));
        release_session_operation(
            &mut state,
            "active",
            active,
            SessionAdmission::Control,
            "control",
        );
        reserve_session_operation(
            &mut state,
            "active",
            RuntimeSessionOperationKind::Close,
            "close",
        )
        .unwrap();
        assert!(session_prompt_pending(&state, "active"));
        release_session_operation(
            &mut state,
            "active",
            active,
            SessionAdmission::Close,
            "close",
        );
        complete_test_turn(&mut state, "other", &other_turn);
        complete_test_turn(&mut state, "active", &active_turn);
        reserve_session_operation(
            &mut state,
            "active",
            RuntimeSessionOperationKind::SetMode,
            "idle-control",
        )
        .unwrap();
        assert!(
            reserve_session_operation(
                &mut state,
                "active",
                RuntimeSessionOperationKind::Fork,
                "fork"
            )
            .is_err()
        );
        release_session_operation(
            &mut state,
            "active",
            active,
            SessionAdmission::Control,
            "idle-control",
        );
        reserve_session_operation(
            &mut state,
            "active",
            RuntimeSessionOperationKind::Close,
            "idle-close",
        )
        .unwrap();
        release_session_operation(
            &mut state,
            "active",
            active,
            SessionAdmission::Close,
            "idle-close",
        );

        state.listed_sessions.insert(
            "saved".to_string(),
            session_info("saved", "/agent/workspace"),
        );
        session_mirror(&mut state).register_cold("saved", 0);
        session_mirror(&mut state)
            .begin_exclusive("saved", 0, RuntimeSessionOperationKind::Load, "attachment")
            .unwrap();
        assert!(reserve_deletion(&mut state, "saved", "delete").is_err());
        release_session_operation(
            &mut state,
            "saved",
            0,
            SessionAdmission::Attachment,
            "attachment",
        );
        let saved = reserve_deletion(&mut state, "saved", "delete").unwrap();
        assert!(reserve_deletion(&mut state, "saved", "other-delete").is_err());
        let turn = start_test_turn(&mut state, "active", "independent-prompt").unwrap();
        complete_test_turn(&mut state, "active", &turn);
        release_deletion(&mut state, "saved", saved, "not-owner");
        assert!(session_operation_pending(
            &state,
            "saved",
            SessionAdmission::Delete
        ));
        release_deletion(&mut state, "saved", saved, "delete");
        let deleting = reserve_deletion(&mut state, "active", "delete-active").unwrap();
        assert!(start_test_turn(&mut state, "active", "prompt-during-delete").is_err());
        release_deletion(&mut state, "active", deleting, "delete-active");

        // Agent-owned identity need not exist in a local listing to be deleted.
        let remote = reserve_deletion(&mut state, "agent-owned-session", "delete-remote").unwrap();
        assert!(
            !state
                .sessions
                .active_session("agent-owned-session")
                .is_some()
        );
        release_deletion(&mut state, "agent-owned-session", remote, "delete-remote");
        assert!(
            session_mirror(&mut state)
                .state("agent-owned-session")
                .is_none()
        );
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

        register_test_session(&mut state, "source-session", "/agent/workspace");
        assert!(!can_close_rejected_chat_session(&state, "source-session"));

        state.listed_sessions.insert(
            "listed-session".to_string(),
            session_info("listed-session", "/agent/workspace"),
        );
        assert!(!can_close_rejected_chat_session(&state, "listed-session"));

        reserve_deletion(&mut state, "deleting-session", "delete").unwrap();
        assert!(!can_close_rejected_chat_session(&state, "deleting-session"));

        state.agent_capabilities = Some(AgentCapabilities::new());
        assert!(!can_close_rejected_chat_session(&state, "new-allocation"));
    }

    #[test]
    fn catalog_deletion_reservations_do_not_allocate_sessions_and_reject_stale_cleanup() {
        for placeholder in [false, true] {
            let mut state = BridgeState::default();
            if placeholder {
                state.sessions.register_cold("remote", 0);
            }
            let first = reserve_deletion(&mut state, "remote", "first").unwrap();
            assert!(matches!(first, DeletionReservation::Catalog(_)));
            assert!(state.sessions.session_ref("remote").is_none());
            assert_eq!(state.sessions.state("remote").is_some(), placeholder);
            assert!(reserve_deletion(&mut state, "remote", "competing").is_err());
            assert!(
                reserve_attachment_locked(
                    &mut state,
                    "remote",
                    RuntimeSessionOperationKind::Load,
                    Some("/workspace"),
                    "load-during-delete",
                    None,
                )
                .is_err()
            );
            assert!(
                prepare_session_response(
                    &mut state,
                    "remote",
                    PathBuf::from("/workspace"),
                    &json!({ "sessionId": "remote" }),
                    None,
                )
                .is_err()
            );
            release_deletion(&mut state, "remote", first, "first");
            assert!(state.catalog_deletions.is_empty());

            let retry = reserve_deletion(&mut state, "remote", "first").unwrap();
            assert_ne!(retry, first);
            release_deletion(&mut state, "remote", first, "first");
            assert!(session_operation_pending(
                &state,
                "remote",
                SessionAdmission::Delete
            ));
            assert_eq!(state.sessions.state("remote").is_some(), placeholder);
            release_deletion(&mut state, "remote", retry, "first");
            assert!(state.catalog_deletions.is_empty());
        }
    }

    #[test]
    fn catalog_deletion_reservations_have_no_global_count_limit() {
        let mut state = BridgeState::default();
        let first = reserve_deletion(&mut state, "first", "first").unwrap();
        for index in 1..FORMER_TRACKED_SESSIONS {
            let id = format!("remote-{index}");
            reserve_deletion(&mut state, &id, &id).unwrap();
        }
        assert!(reserve_deletion(&mut state, "excess", "excess").is_ok());
        assert_eq!(state.catalog_deletions.len(), FORMER_TRACKED_SESSIONS + 1);
        assert_eq!(state.sessions.iter_states().count(), 0);
        release_deletion(&mut state, "first", first, "first");
        assert!(reserve_deletion(&mut state, "replacement", "replacement").is_ok());
    }

    #[tokio::test]
    async fn prompt_completion_releases_exclusion_before_notifying_shutdown() {
        let state = Arc::new(Mutex::new(BridgeState::default()));
        let (_incarnation, operation) = {
            let mut state = state.lock().await;
            let incarnation = register_test_session(&mut state, "session", "/workspace");
            let operation = start_test_turn(&mut state, "session", "prompt").unwrap();
            (incarnation, operation)
        };
        let lifecycle = Arc::new(PromptLifecycle::default());
        let waiter = {
            let state = state.clone();
            let lifecycle = lifecycle.clone();
            tokio::spawn(async move {
                loop {
                    let changed = lifecycle.changed.notified();
                    if !session_prompt_pending(&*state.lock().await, "session") {
                        return;
                    }
                    changed.await;
                }
            })
        };
        tokio::task::yield_now().await;
        {
            let mut state = state.lock().await;
            complete_test_turn(&mut state, "session", &operation);
            finish_prompt_operation(&lifecycle);
        }
        tokio::time::timeout(Duration::from_millis(100), waiter)
            .await
            .expect("shutdown waiter missed the prompt terminal edge")
            .unwrap();
    }

    #[test]
    fn rejects_cyclic_cursors_and_accepts_large_session_metadata() {
        let mut state = BridgeState::default();
        state.listed_sessions.insert(
            "saved".to_string(),
            session_info("saved", "/agent/workspace"),
        );

        let cyclic: ListSessionsResponse = serde_json::from_value(json!({
            "sessions": [{
                "sessionId": "saved",
                "cwd": "/agent/workspace",
                "title": "Duplicate is a safe upsert"
            }],
            "nextCursor": "cursor-a"
        }))
        .unwrap();
        let error = validate_session_list_page(&cyclic, Some("cursor-a"), &state).unwrap_err();
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
        assert!(validate_session_list_page(&too_many_roots, None, &state).is_ok());

        let invalid_metadata: ListSessionsResponse = serde_json::from_value(json!({
            "sessions": [{
                "sessionId": "metadata",
                "cwd": "/agent/workspace",
                "title": "x".repeat(16_385)
            }]
        }))
        .unwrap();
        assert!(validate_session_list_page(&invalid_metadata, None, &state).is_ok());

        let invalid_cursor: ListSessionsResponse = serde_json::from_value(json!({
            "sessions": [], "nextCursor": "x".repeat(4_097)
        }))
        .unwrap();
        assert!(validate_session_list_page(&invalid_cursor, None, &state).is_ok());
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
        assert!(validate_auth_methods(std::slice::from_ref(&agent_method), true,).is_ok());
        assert!(validate_auth_methods(&[agent_method.clone(), agent_method], true,).is_err());
        assert!(
            validate_auth_methods(
                &[AuthMethod::Terminal(AuthMethodTerminal::new(
                    "terminal", "Terminal",
                ))],
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

    fn register_test_url(
        state: &mut BridgeState,
        owner: Option<SessionResourceOwner>,
        interaction: &str,
    ) -> UrlRegistration {
        let epoch = state.sessions.epoch().to_string();
        let scope = owner
            .as_ref()
            .map(|owner| (owner.session_id.as_str(), owner.incarnation));
        state
            .sessions
            .upsert_elicitation(&epoch, scope, interaction, json!({ "mode": "url" }))
            .unwrap();
        state
            .sessions
            .resolve_elicitation(&epoch, scope, interaction, Some("reusable-url"))
            .unwrap();
        state
            .sessions
            .register_url_route("reusable-url", owner)
            .unwrap();
        state.sessions.url_owners["reusable-url"].clone()
    }

    #[test]
    fn delayed_url_abort_preserves_reused_session_and_request_registrations() {
        for request_scoped in [false, true] {
            let mut state = BridgeState::default();
            let incarnation = register_test_session(&mut state, "session", "/workspace");
            let owner = (!request_scoped).then(|| {
                state
                    .sessions
                    .resource_owner("session", incarnation)
                    .unwrap()
            });
            let old = register_test_url(&mut state, owner.clone(), "old-registration");
            let epoch = state.sessions.epoch().to_string();
            let scope = owner
                .as_ref()
                .map(|owner| (owner.session_id.as_str(), owner.incarnation));
            state
                .sessions
                .settle_url_registration(
                    &epoch,
                    scope,
                    "reusable-url",
                    &old.registration_id,
                    UrlFlowStatus::Completed,
                )
                .unwrap();
            state.sessions.take_url_route("reusable-url", &old).unwrap();
            let current = register_test_url(&mut state, owner.clone(), "new-registration");
            let abort = |registration: &UrlRegistration| RuntimeEffect::AbortUrlFlow {
                session_id: registration
                    .owner
                    .as_ref()
                    .map(|owner| owner.session_id.clone()),
                incarnation: registration.owner.as_ref().map(|owner| owner.incarnation),
                elicitation_id: "reusable-url".to_string(),
                registration_id: registration.registration_id.clone(),
            };
            state.sessions.effects.push_back(abort(&old));
            let (tx, mut events) = mpsc::unbounded_channel();
            let sink = EventSink { tx: tx.into() };
            assert!(drain_runtime_effects(&mut state, &sink).is_empty());
            assert_eq!(
                state.sessions.url_owners.get("reusable-url"),
                Some(&current)
            );
            assert!(events.try_recv().is_err());
            // Once this precise registration retires, its duplicated local effects
            // remove the lightweight route and publish only one terminal event.
            state
                .sessions
                .settle_url_registration(
                    &epoch,
                    scope,
                    "reusable-url",
                    &current.registration_id,
                    UrlFlowStatus::Cancelled,
                )
                .unwrap();
            state
                .sessions
                .effects
                .extend([abort(&current), abort(&current)]);
            assert!(drain_runtime_effects(&mut state, &sink).is_empty());
            assert!(!state.sessions.url_owners.contains_key("reusable-url"));
            let event: Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
            assert_eq!(event["type"], "acp/elicitation_aborted");
            assert!(events.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn cancel_commit_retires_interactions_before_next_turn_reuses_url() {
        fn pending(
            state: &mut BridgeState,
            owner: &SessionResourceOwner,
            suffix: &str,
        ) -> (
            oneshot::Receiver<RequestPermissionResponse>,
            oneshot::Receiver<CreateElicitationResponse>,
        ) {
            let permission_id = format!("permission-{suffix}");
            let elicitation_id = format!("elicitation-{suffix}");
            let (permission_sender, permission) = oneshot::channel();
            let (elicitation_sender, elicitation) = oneshot::channel();
            state
                .sessions
                .upsert_permission(
                    &owner.epoch,
                    &owner.session_id,
                    owner.incarnation,
                    &permission_id,
                    json!({}),
                )
                .unwrap();
            state
                .sessions
                .insert_permission(
                    owner.clone(),
                    permission_id,
                    PermissionResponder {
                        tool_call_id: "reusable-tool".to_string(),
                        option_ids: HashSet::new(),
                        sender: permission_sender,
                    },
                )
                .unwrap();
            state
                .sessions
                .upsert_elicitation(
                    &owner.epoch,
                    Some((&owner.session_id, owner.incarnation)),
                    &elicitation_id,
                    json!({ "mode": "form" }),
                )
                .unwrap();
            state
                .sessions
                .insert_elicitation(
                    owner.clone(),
                    elicitation_id,
                    ElicitationResponder {
                        url_elicitation_id: None,
                        request: json!({ "mode": "form" }),
                        sender: elicitation_sender,
                    },
                )
                .unwrap();
            (permission, elicitation)
        }
        let state = Arc::new(Mutex::new(BridgeState::default()));
        let (tx, _events) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        let mut locked = state.lock().await;
        let incarnation = register_test_session(&mut locked, "session", "/workspace");
        let operation = start_test_turn(&mut locked, "session", "old-intent").unwrap();
        let owner = locked
            .sessions
            .resource_owner("session", incarnation)
            .unwrap();
        let (mut old_permission, mut old_elicitation) = pending(&mut locked, &owner, "old");
        register_test_url(&mut locked, Some(owner.clone()), "old-url");
        let competing_state = state.clone();
        let next_owner = owner.clone();
        let (queued, ready) = oneshot::channel();
        let next = tokio::spawn(async move {
            queued.send(()).unwrap();
            let mut state = competing_state.lock().await;
            start_test_turn(&mut state, "session", "new-intent").unwrap();
            let receivers = pending(&mut state, &next_owner, "new");
            let registration = register_test_url(&mut state, Some(next_owner), "new-url");
            (receivers, registration)
        });
        ready.await.unwrap();
        commit_session_cancel(&mut locked, &sink, "session", incarnation).unwrap();
        assert!(matches!(
            old_permission.try_recv().unwrap().outcome,
            RequestPermissionOutcome::Cancelled
        ));
        assert!(matches!(
            old_elicitation.try_recv().unwrap().action,
            ElicitationAction::Cancel
        ));
        assert!(locked.sessions.url_owners.is_empty());
        assert!(
            locked
                .sessions
                .live("session")
                .unwrap()
                .url_flows
                .is_empty()
        );
        complete_test_turn(&mut locked, "session", &operation);
        drop(locked);
        let ((mut permission, mut elicitation), registration) = next.await.unwrap();
        // The old command's eventual effect drain has no authority over the new turn.
        apply_runtime_effects(&state, &sink, None).await;
        assert!(matches!(
            permission.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            elicitation.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        let locked = state.lock().await;
        assert_eq!(
            locked.sessions.url_owners.get("reusable-url"),
            Some(&registration)
        );
        assert_eq!(
            locked.sessions.live("session").unwrap().url_flows["reusable-url"].registration_id,
            "new-url"
        );
    }

    #[tokio::test]
    async fn retired_incarnation_effects_cannot_cancel_reused_interaction_or_url_routes() {
        let state = Arc::new(Mutex::new(BridgeState::default()));
        let (old_permission_sender, old_permission) = oneshot::channel();
        let (old_elicitation_sender, old_elicitation) = oneshot::channel();
        let (new_permission_sender, mut new_permission) = oneshot::channel();
        let (new_elicitation_sender, mut new_elicitation) = oneshot::channel();
        let (old_incarnation, current_owner) = {
            let mut state = state.lock().await;
            let epoch = state.sessions.epoch().to_string();
            let incarnation = state
                .sessions
                .start_attachment(
                    &epoch,
                    "session",
                    "/workspace",
                    "load",
                    RuntimeSessionOperationKind::Load,
                )
                .unwrap();
            let owner = state
                .sessions
                .resource_owner("session", incarnation)
                .unwrap();
            state
                .sessions
                .upsert_permission(&epoch, "session", incarnation, "permission", json!({}))
                .unwrap();
            state
                .sessions
                .insert_permission(
                    owner.clone(),
                    "permission".to_string(),
                    PermissionResponder {
                        tool_call_id: "tool".to_string(),
                        option_ids: HashSet::new(),
                        sender: old_permission_sender,
                    },
                )
                .unwrap();
            state
                .sessions
                .upsert_elicitation(
                    &epoch,
                    Some(("session", incarnation)),
                    "elicitation",
                    json!({}),
                )
                .unwrap();
            state
                .sessions
                .insert_elicitation(
                    owner.clone(),
                    "elicitation".to_string(),
                    ElicitationResponder {
                        url_elicitation_id: None,
                        request: json!({}),
                        sender: old_elicitation_sender,
                    },
                )
                .unwrap();
            state
                .sessions
                .upsert_elicitation(
                    &epoch,
                    Some(("session", incarnation)),
                    "accepted-url",
                    json!({}),
                )
                .unwrap();
            state
                .sessions
                .resolve_elicitation(
                    &epoch,
                    Some(("session", incarnation)),
                    "accepted-url",
                    Some("url"),
                )
                .unwrap();
            state
                .sessions
                .register_url_route("url", Some(owner))
                .unwrap();
            // Failed attachment queues incarnation-scoped canonical effects; physical
            // retirement also answers real resources before those effects are drained.
            state
                .sessions
                .fail_attachment(
                    &epoch,
                    "session",
                    incarnation,
                    "load",
                    RuntimeSessionOperationKind::Load,
                    json!({}),
                )
                .unwrap();
            state.sessions.remove("session", incarnation);
            assert!(state.sessions.permission_owners.is_empty());
            assert!(state.sessions.elicitation_owners.is_empty());
            assert!(state.sessions.url_owners.is_empty());
            let next = state
                .sessions
                .open_new(&epoch, "session", "/new", json!({}))
                .unwrap();
            let current = state.sessions.resource_owner("session", next).unwrap();
            state
                .sessions
                .insert_permission(
                    current.clone(),
                    "permission".to_string(),
                    PermissionResponder {
                        tool_call_id: "tool".to_string(),
                        option_ids: HashSet::new(),
                        sender: new_permission_sender,
                    },
                )
                .unwrap();
            state
                .sessions
                .insert_elicitation(
                    current.clone(),
                    "elicitation".to_string(),
                    ElicitationResponder {
                        url_elicitation_id: None,
                        request: json!({}),
                        sender: new_elicitation_sender,
                    },
                )
                .unwrap();
            state
                .sessions
                .upsert_elicitation(&epoch, Some(("session", next)), "new-url", json!({}))
                .unwrap();
            state
                .sessions
                .resolve_elicitation(&epoch, Some(("session", next)), "new-url", Some("url"))
                .unwrap();
            state
                .sessions
                .register_url_route("url", Some(current.clone()))
                .unwrap();
            (incarnation, current)
        };
        assert!(matches!(
            old_permission.await.unwrap().outcome,
            RequestPermissionOutcome::Cancelled
        ));
        assert!(matches!(
            old_elicitation.await.unwrap().action,
            ElicitationAction::Cancel
        ));
        let (tx, mut events) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        apply_runtime_effects(&state, &sink, None).await;
        cancel_interactions("session", old_incarnation, "stale cleanup", &state, &sink).await;
        assert!(matches!(
            new_permission.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            new_elicitation.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        let state = state.lock().await;
        assert_eq!(
            state.sessions.permission_owners.get("permission"),
            Some(&current_owner)
        );
        assert_eq!(
            state.sessions.elicitation_owners.get("elicitation"),
            Some(&current_owner)
        );
        assert_eq!(
            state
                .sessions
                .url_owners
                .get("url")
                .map(|route| &route.owner),
            Some(&Some(current_owner))
        );
        assert!(
            events.try_recv().is_err(),
            "old effects must not publish terminal events for reused IDs"
        );
    }

    #[tokio::test]
    async fn prompt_terminal_effects_cancel_owned_responders_in_the_adapter() {
        let (permission_sender, permission_receiver) = oneshot::channel();
        let (elicitation_sender, elicitation_receiver) = oneshot::channel();
        let state = Arc::new(Mutex::new(BridgeState::default()));
        {
            let mut state = state.lock().await;
            let epoch = state.sessions.epoch().to_string();
            let incarnation = state
                .sessions
                .open_new(
                    &epoch,
                    "session",
                    "/workspace",
                    json!({ "sessionId": "session" }),
                )
                .unwrap();
            state
                .sessions
                .start_prompt(&epoch, "session", incarnation, "prompt", Vec::new())
                .unwrap();
            state
                .sessions
                .upsert_permission(
                    &epoch,
                    "session",
                    incarnation,
                    "permission",
                    json!({ "sessionId": "session" }),
                )
                .unwrap();
            state
                .sessions
                .upsert_elicitation(
                    &epoch,
                    Some(("session", incarnation)),
                    "elicitation",
                    json!({ "sessionId": "session", "mode": "form" }),
                )
                .unwrap();
            let owner = state
                .sessions
                .resource_owner("session", incarnation)
                .unwrap();
            state
                .sessions
                .insert_permission(
                    owner.clone(),
                    "permission".to_string(),
                    PermissionResponder {
                        tool_call_id: "tool".to_string(),
                        option_ids: HashSet::new(),
                        sender: permission_sender,
                    },
                )
                .unwrap();
            state
                .sessions
                .insert_elicitation(
                    owner.clone(),
                    "elicitation".to_string(),
                    ElicitationResponder {
                        url_elicitation_id: None,
                        request: json!({ "sessionId": "session", "mode": "form" }),
                        sender: elicitation_sender,
                    },
                )
                .unwrap();
            state
                .sessions
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
        assert!(state.sessions.permission_owners.is_empty());
        assert!(state.sessions.elicitation_owners.is_empty());
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
        let epoch = state.sessions.epoch().to_string();
        let incarnation = state
            .sessions
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        state
            .sessions
            .start_delete(&epoch, "session", incarnation, "first-delete")
            .unwrap();
        state
            .sessions
            .delete_close_succeeded(&epoch, "session", incarnation, "first-delete")
            .unwrap();
        state
            .sessions
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
            .sessions
            .complete_delete(&epoch, "session", incarnation, "retry-delete")
            .unwrap();
        assert!(state.sessions.session("session").is_none());
    }

    #[test]
    fn close_before_delete_publishes_deleting_before_the_delete_rpc_wait() {
        let mut state = BridgeState::default();
        let epoch = state.sessions.epoch().to_string();
        let incarnation = state
            .sessions
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        state
            .sessions
            .start_delete(&epoch, "session", incarnation, "delete")
            .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        flush_runtime(&mut state, &sink);
        while rx.try_recv().is_ok() {}

        commit_runtime_delete_close_success(&mut state, "session", incarnation, "delete", &sink)
            .unwrap();

        assert_eq!(state.published_runtime_seq, state.sessions.seq());
        let event = serde_json::from_str::<Value>(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(event["type"], "bridge/internal_runtime_delta");
        assert_eq!(event["value"]["change"]["session"]["lifecycle"], "deleting");
    }

    #[tokio::test]
    async fn close_cleanup_tombstone_blocks_same_id_reattachment() {
        let state = Arc::new(Mutex::new(BridgeState::default()));
        let (tx, _events) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        let incarnation = {
            let mut state = state.lock().await;
            let incarnation = register_test_session(&mut state, "session", "/workspace");
            let epoch = state.sessions.epoch().to_string();
            reserve_session_operation(
                &mut state,
                "session",
                RuntimeSessionOperationKind::Close,
                "close",
            )
            .unwrap();
            state
                .sessions
                .start_operation(
                    &epoch,
                    "session",
                    incarnation,
                    "close",
                    RuntimeSessionOperationKind::Close,
                    "closing",
                )
                .unwrap();
            state
                .sessions
                .close_session(&epoch, "session", incarnation, "close")
                .unwrap();

            state
                .listed_sessions
                .insert("session".to_string(), session_info("session", "/workspace"));
            incarnation
        };
        let error = reserve_attachment("session", RuntimeSessionOperationKind::Load, &state)
            .await
            .unwrap_err();
        assert_eq!(error.code, Error::invalid_request().code);
        assert!(!session_operation_pending(
            &*state.lock().await,
            "session",
            SessionAdmission::Attachment
        ));
        {
            let mut state = state.lock().await;
            complete_session_retirement(
                &mut state,
                &sink,
                "session",
                incarnation,
                SessionAdmission::Close,
                "not-owner",
            );
            assert!(session_operation_pending(
                &state,
                "session",
                SessionAdmission::Close
            ));
            complete_session_retirement(
                &mut state,
                &sink,
                "session",
                incarnation,
                SessionAdmission::Close,
                "close",
            );
        }
        assert!(
            reserve_attachment("session", RuntimeSessionOperationKind::Load, &state)
                .await
                .is_ok()
        );
    }

    #[test]
    fn terminal_business_deltas_and_replacement_view_share_the_same_output() {
        let mut state = BridgeState::default();
        let epoch = state.sessions.epoch().to_string();
        let incarnation = state
            .sessions
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();

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
                .sessions
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
        let epoch = state.sessions.epoch().to_string();
        let first = state
            .sessions
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();

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
            .sessions
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
            .sessions
            .close_session(&epoch, "session", first, "close")
            .unwrap();

        assert_eq!(
            state
                .sessions
                .state("session")
                .unwrap()
                .operation
                .as_ref()
                .unwrap()
                .stage,
            "cleanup"
        );
        let (tx, _rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        complete_session_retirement(
            &mut state,
            &sink,
            "session",
            first,
            SessionAdmission::Close,
            "close",
        );
        assert!(state.sessions.state("session").is_none());
        let second = state
            .sessions
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();

        assert_ne!(first, second);
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
                .sessions
                .session("session")
                .unwrap()
                .terminals
                .is_empty()
        );
    }

    #[test]
    fn transport_eof_is_uncertain_but_an_agent_error_is_definite_failure() {
        let mut uncertain = BridgeState::default();
        let epoch = uncertain.sessions.epoch().to_string();
        let incarnation = uncertain
            .sessions
            .open_new(
                &epoch,
                "uncertain",
                "/workspace",
                json!({ "sessionId": "uncertain" }),
            )
            .unwrap();
        uncertain
            .sessions
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
            uncertain.sessions.session("uncertain").unwrap().lifecycle,
            SessionLifecycle::Uncertain
        );
        let uncertain_session = uncertain.sessions.session("uncertain").unwrap();
        assert!(uncertain_session.active_turn.is_none());

        let mut failed = BridgeState::default();
        let epoch = failed.sessions.epoch().to_string();
        let incarnation = failed
            .sessions
            .open_new(
                &epoch,
                "failed",
                "/workspace",
                json!({ "sessionId": "failed" }),
            )
            .unwrap();
        failed
            .sessions
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
            failed.sessions.session("failed").unwrap().lifecycle,
            SessionLifecycle::Active
        );
        let failed_session = failed.sessions.session("failed").unwrap();
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
            let epoch = state.sessions.epoch().to_string();
            let incarnation = state
                .sessions
                .open_new(
                    &epoch,
                    "session",
                    "/workspace",
                    json!({ "sessionId": "session" }),
                )
                .unwrap();

            state
                .sessions
                .upsert_permission(
                    &epoch,
                    "session",
                    incarnation,
                    "permission",
                    json!({ "sessionId": "session" }),
                )
                .unwrap();
            state
                .sessions
                .upsert_elicitation(
                    &epoch,
                    Some(("session", incarnation)),
                    "session-elicitation",
                    json!({ "sessionId": "session", "mode": "form" }),
                )
                .unwrap();
            state
                .sessions
                .upsert_elicitation(
                    &epoch,
                    Some(("session", incarnation)),
                    "accepted-session-url",
                    json!({ "sessionId": "session", "mode": "url" }),
                )
                .unwrap();
            state
                .sessions
                .resolve_elicitation(
                    &epoch,
                    Some(("session", incarnation)),
                    "accepted-session-url",
                    Some("session-url"),
                )
                .unwrap();
            state
                .sessions
                .upsert_elicitation(
                    &epoch,
                    None,
                    "request-elicitation",
                    json!({ "requestId": 1, "mode": "form" }),
                )
                .unwrap();
            state
                .sessions
                .upsert_elicitation(
                    &epoch,
                    None,
                    "accepted-request-url",
                    json!({ "requestId": 2, "mode": "url" }),
                )
                .unwrap();
            state
                .sessions
                .resolve_elicitation(&epoch, None, "accepted-request-url", Some("request-url"))
                .unwrap();
            let owner = state
                .sessions
                .resource_owner("session", incarnation)
                .unwrap();
            state
                .sessions
                .insert_permission(
                    owner.clone(),
                    "permission".to_string(),
                    PermissionResponder {
                        tool_call_id: "tool".to_string(),
                        option_ids: HashSet::new(),
                        sender: permission_sender,
                    },
                )
                .unwrap();
            state
                .sessions
                .insert_elicitation(
                    owner.clone(),
                    "session-elicitation".to_string(),
                    ElicitationResponder {
                        url_elicitation_id: None,
                        request: json!({
                            "sessionId": "session", "mode": "form", "message": "test",
                            "requestedSchema": { "type": "object", "properties": {} }
                        }),
                        sender: session_sender,
                    },
                )
                .unwrap();
            state.request_elicitations.insert(
                "request-elicitation".to_string(),
                ElicitationResponder {
                    url_elicitation_id: None,
                    request: json!({
                        "requestId": 1, "mode": "form", "message": "test",
                        "requestedSchema": { "type": "object", "properties": {} }
                    }),
                    sender: request_sender,
                },
            );
            state
                .sessions
                .register_url_route("session-url", Some(owner))
                .unwrap();
            state
                .sessions
                .register_url_route("request-url", None)
                .unwrap();
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        let incarnation = state
            .lock()
            .await
            .sessions
            .state("session")
            .unwrap()
            .incarnation;
        cancel_interactions("session", incarnation, "session_cancelled", &state, &sink).await;

        assert!(matches!(
            permission_receiver.await.unwrap().outcome,
            RequestPermissionOutcome::Cancelled
        ));
        assert!(matches!(
            session_receiver.await.unwrap().action,
            ElicitationAction::Cancel
        ));
        let state = state.lock().await;
        assert!(state.sessions.permission_owners.is_empty());
        assert!(
            state
                .request_elicitations
                .contains_key("request-elicitation")
        );
        assert!(!state.sessions.url_owners.contains_key("session-url"));
        assert!(state.sessions.url_owners.contains_key("request-url"));
        let runtime = state.sessions.snapshot();
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
    async fn preserves_early_update_bursts_until_creation_completes() {
        let state = Arc::new(Mutex::new(BridgeState {
            pending_creations: 1,
            ..BridgeState::default()
        }));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx: tx.into() };
        for index in 0..10_016 {
            let notification: SessionNotification = serde_json::from_value(json!({
                "sessionId": "new-session",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "early-answer",
                    "content": { "type": "text", "text": format!("{index}:{}", "x".repeat(128)) }
                }
            }))
            .unwrap();
            handle_session_update(notification, &state, &sink, None).await;
        }
        let retained = state
            .lock()
            .await
            .creation_staging
            .values()
            .map(|staging| staging.early_notifications.len())
            .sum::<usize>();
        let state = state.lock().await;
        assert_eq!(retained, 10_016);
        assert!(state.early_update_bytes > 1_000_000);
        assert!(
            state.creation_staging["new-session"]
                .validation
                .invalid_reason
                .is_none()
        );
        drop(state);
        assert!(rx.try_recv().is_err());
    }
}

#[cfg(test)]
mod catalog_command_tests {
    use super::*;
    use agent_client_protocol::Lines;
    use clap::Parser;
    use futures::{SinkExt, StreamExt};
    use std::collections::VecDeque;
    use std::io;

    struct CatalogWire {
        commands: mpsc::UnboundedSender<BridgeInput>,
        requests: futures::channel::mpsc::Receiver<String>,
        responses: futures::channel::mpsc::Sender<io::Result<String>>,
        buffered: VecDeque<Value>,
        _events: mpsc::UnboundedReceiver<String>,
        cancellation: CancellationToken,
        task: tokio::task::JoinHandle<Result<(), Error>>,
    }

    impl CatalogWire {
        async fn start() -> Self {
            let (outgoing, requests) = futures::channel::mpsc::channel::<String>(64);
            let (responses, incoming) = futures::channel::mpsc::channel::<io::Result<String>>(64);
            let transport = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
            let options = Options::try_parse_from([
                "attyd",
                "--transport",
                "ws",
                "--",
                "ws://127.0.0.1:1/acp",
            ])
            .unwrap()
            .normalized()
            .unwrap();
            let (commands, command_rx) = mpsc::unbounded_channel();
            let (events, event_rx) = mpsc::unbounded_channel();
            let cancellation = CancellationToken::new();
            let task = tokio::spawn(run_connection(
                transport,
                Arc::new(options),
                command_rx,
                EventSink { tx: events.into() },
                cancellation.clone(),
            ));
            let mut wire = Self {
                commands,
                requests,
                responses,
                buffered: VecDeque::new(),
                _events: event_rx,
                cancellation,
                task,
            };
            let initialize = wire.next_request().await;
            assert_eq!(initialize["method"], "initialize");
            wire.reply(
                &initialize,
                json!({
                    "protocolVersion": 1,
                    "agentCapabilities": {
                        "sessionCapabilities": {"list": {}, "delete": {}, "fork": {}}
                    },
                    "authMethods": []
                }),
            )
            .await;
            wire
        }

        async fn submit(
            &self,
            command: Value,
        ) -> oneshot::Receiver<Result<Value, BridgeRequestError>> {
            let (response, result) = oneshot::channel();
            self.commands
                .send(BridgeInput::BusinessRequest { command, response })
                .unwrap();
            result
        }

        async fn next_request(&mut self) -> Value {
            loop {
                if let Some(request) = self.buffered.pop_front() {
                    return request;
                }
                let raw = tokio::time::timeout(Duration::from_secs(3), self.requests.next())
                    .await
                    .expect("catalog request did not reach the Agent")
                    .expect("catalog connection ended unexpectedly");
                match serde_json::from_str(&raw).unwrap() {
                    Value::Array(batch) => self.buffered.extend(batch),
                    request => self.buffered.push_back(request),
                }
            }
        }

        async fn reply(&mut self, request: &Value, result: Value) {
            self.responses
                .send(Ok(json!({
                    "jsonrpc": "2.0", "id": request["id"], "result": result,
                })
                .to_string()))
                .await
                .unwrap();
        }

        async fn refuse(&mut self, request: &Value) {
            self.responses
                .send(Ok(json!({
                    "jsonrpc": "2.0", "id": request["id"],
                    "error": {"code": -32600, "message": "Mutation refused"},
                })
                .to_string()))
                .await
                .unwrap();
        }

        async fn result(
            result: oneshot::Receiver<Result<Value, BridgeRequestError>>,
        ) -> Result<Value, BridgeRequestError> {
            tokio::time::timeout(Duration::from_secs(3), result)
                .await
                .expect("catalog command did not settle")
                .unwrap()
        }

        async fn stop(self) {
            self.cancellation.cancel();
            tokio::time::timeout(Duration::from_secs(3), self.task)
                .await
                .expect("catalog connection did not stop")
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn session_list_waiters_survive_backlog_without_blocking_catalog_deletion() {
        let mut wire = CatalogWire::start().await;
        let first = wire
            .submit(json!({
                "type": "session/list", "requestId": "held-list",
            }))
            .await;
        let held = wire.next_request().await;
        assert_eq!(held["method"], "session/list");
        let mut waiting = Vec::new();
        for index in 1..64 {
            let result = wire
                .submit(json!({
                    "type": "session/list", "requestId": format!("waiting-{index}"),
                }))
                .await;
            if index == 1 {
                // A disconnected HTTP caller drops only its receiver; accepted
                // work still settles in order.
                drop(result);
                waiting.push(None);
            } else {
                waiting.push(Some(result));
            }
        }
        let excess = wire
            .submit(json!({
                "type": "session/list", "requestId": "excess",
            }))
            .await;
        waiting.push(Some(excess));

        // List mutex waiters must neither borrow every Control RPC slot nor
        // retain the global execution ticket while the Agent is parked.
        let deleting = wire
            .submit(json!({
                "type": "session/delete", "requestId": "delete", "sessionId": "saved",
            }))
            .await;
        let delete = wire.next_request().await;
        assert_eq!(delete["method"], "session/delete");
        wire.reply(&delete, json!({})).await;
        CatalogWire::result(deleting).await.unwrap();
        wire.reply(&held, json!({"sessions": []})).await;
        assert_eq!(
            CatalogWire::result(first).await.unwrap_err().data.unwrap()["kind"],
            "session_catalog_changed",
        );
        for result in waiting {
            let request = wire.next_request().await;
            assert_eq!(request["method"], "session/list");
            wire.reply(&request, json!({"sessions": []})).await;
            if let Some(result) = result {
                CatalogWire::result(result).await.unwrap();
            }
        }
        let retry = wire
            .submit(json!({
                "type": "session/list", "requestId": "excess",
            }))
            .await;
        let request = wire.next_request().await;
        wire.reply(&request, json!({"sessions": []})).await;
        CatalogWire::result(retry).await.unwrap();
        wire.stop().await;
    }

    #[tokio::test]
    async fn session_list_rejects_inflight_snapshots_and_old_pages_after_catalog_mutations() {
        for mutation in ["session/delete", "session/new", "session/fork"] {
            let mut wire = CatalogWire::start().await;
            if mutation == "session/fork" {
                let creating = wire
                    .submit(json!({
                        "type": "session/new", "requestId": "source", "cwd": "/repo",
                    }))
                    .await;
                let request = wire.next_request().await;
                wire.reply(&request, json!({"sessionId": "source"})).await;
                CatalogWire::result(creating).await.unwrap();
            }
            let first = wire
                .submit(json!({
                    "type": "session/list", "requestId": "first",
                }))
                .await;
            let request = wire.next_request().await;
            let page = json!({
                "sessions": [{"sessionId": "saved", "cwd": "/repo"}],
                "nextCursor": "agent-cursor",
            });
            wire.reply(&request, page.clone()).await;
            let first = CatalogWire::result(first).await.unwrap();
            let revision = first["catalogRevision"].as_str().unwrap().to_string();
            let stale = wire
                .submit(json!({
                    "type": "session/list", "requestId": "stale-snapshot",
                }))
                .await;
            let held = wire.next_request().await;
            let changing = wire
                .submit(json!({
                    "type": mutation, "requestId": "mutation", "cwd": "/repo",
                    "sessionId": if mutation == "session/fork" { "source" } else { "saved" },
                }))
                .await;
            let request = wire.next_request().await;
            assert_eq!(request["method"], mutation);
            let result = if mutation == "session/delete" {
                json!({})
            } else {
                json!({"sessionId": "created"})
            };
            wire.reply(&request, result).await;
            CatalogWire::result(changing).await.unwrap();
            wire.reply(&held, page).await;
            let error = CatalogWire::result(stale).await.unwrap_err();
            assert_eq!(
                error.data.unwrap()["kind"],
                "session_catalog_changed",
                "{mutation}"
            );

            let stale_page = wire
                .submit(json!({
                    "type": "session/list", "requestId": "old-page", "cursor": "agent-cursor",
                    "expectedCatalogRevision": revision,
                }))
                .await;
            assert_eq!(
                CatalogWire::result(stale_page)
                    .await
                    .unwrap_err()
                    .data
                    .unwrap()["kind"],
                "session_catalog_changed",
            );
            let refresh = wire
                .submit(json!({
                    "type": "session/list", "requestId": "refresh",
                }))
                .await;
            let request = wire.next_request().await;
            assert!(
                request["params"]["cursor"].is_null(),
                "old page must fail before Agent I/O"
            );
            wire.reply(
                &request,
                json!({"sessions": [], "nextCursor": "fresh-cursor"}),
            )
            .await;
            let refresh = CatalogWire::result(refresh).await.unwrap();
            assert_ne!(refresh["catalogRevision"], revision);
            let fresh_page = wire
                .submit(json!({
                    "type": "session/list", "requestId": "fresh-page", "cursor": "fresh-cursor",
                    "expectedCatalogRevision": refresh["catalogRevision"],
                }))
                .await;
            let request = wire.next_request().await;
            assert_eq!(request["params"]["cursor"], "fresh-cursor");
            assert!(request["params"].get("expectedCatalogRevision").is_none());
            wire.reply(&request, json!({"sessions": []})).await;
            CatalogWire::result(fresh_page).await.unwrap();
            let mut changes = Vec::new();
            while let Ok(raw) = wire._events.try_recv() {
                let event: Value = serde_json::from_str(&raw).unwrap();
                if event["type"] == "bridge/catalog_changed" {
                    changes.push(event);
                } else if event["type"] == "acp/sessions_listed" {
                    assert_ne!(event["requestId"], "stale-snapshot");
                    assert_ne!(event["requestId"], "old-page");
                }
            }
            assert_eq!(
                changes.len(),
                if mutation == "session/fork" { 2 } else { 1 }
            );
            let last = changes.last().unwrap();
            assert_eq!(
                refresh["catalogRevision"],
                format!(
                    "{}:{}",
                    last["bridgeEpoch"].as_str().unwrap(),
                    last["revision"],
                )
            );
            wire.stop().await;
        }
    }

    #[tokio::test]
    async fn refused_catalog_deletion_preserves_the_current_list_and_revision() {
        let mut wire = CatalogWire::start().await;
        let first = wire
            .submit(json!({
                "type": "session/list", "requestId": "first",
            }))
            .await;
        let request = wire.next_request().await;
        wire.reply(&request, json!({"sessions": [], "nextCursor": "page"}))
            .await;
        let first = CatalogWire::result(first).await.unwrap();
        let revision = first["catalogRevision"].as_str().unwrap().to_string();
        let pending = wire
            .submit(json!({
                "type": "session/list", "requestId": "pending", "cursor": "page",
                "expectedCatalogRevision": revision,
            }))
            .await;
        let list = wire.next_request().await;
        let deletion = wire
            .submit(json!({
                "type": "session/delete", "requestId": "refused", "sessionId": "saved",
            }))
            .await;
        let delete = wire.next_request().await;
        wire.refuse(&delete).await;
        CatalogWire::result(deletion).await.unwrap_err();
        wire.reply(
            &list,
            json!({"sessions": [{"sessionId": "saved", "cwd": "/repo"}]}),
        )
        .await;
        let response = CatalogWire::result(pending).await.unwrap();
        assert_eq!(response["catalogRevision"], revision);
        assert_eq!(response["sessions"][0]["sessionId"], "saved");
        wire.stop().await;
    }

    #[test]
    fn catalog_revision_rejects_a_previous_bridge_epoch() {
        let first = BridgeState::default();
        let second = BridgeState::default();
        assert_eq!(first.catalog_revision, second.catalog_revision);
        let error =
            require_catalog_revision(&second, &session_catalog_revision(&first)).unwrap_err();
        assert_eq!(error.data.unwrap()["kind"], "session_catalog_changed");
    }
}

#[cfg(test)]
mod ordered_command_tests {
    use super::*;
    use std::io;

    use agent_client_protocol::{Dispatch, Lines, UntypedMessage};
    use futures::{SinkExt, StreamExt};

    use super::scheduling::{BridgeIngress, ScheduledInput, Scheduling};
    use crate::session_dispatch::TrafficClass;

    #[tokio::test]
    async fn ordered_send_survives_many_registrations_and_yields_only_during_rpc_wait() {
        let (events, _events) = mpsc::unbounded_channel();
        let (mut scheduling, ingress) = Scheduling::new(
            "epoch".into(),
            EventSink { tx: events.into() },
            CancellationToken::new(),
        )
        .unwrap();
        let handle = scheduling.register_session("session", 1).unwrap();
        for _ in 0..2 {
            ingress
                .try_browser(BridgeInput::RuntimeSnapshotRequest, TrafficClass::Ordinary)
                .unwrap();
            let item = scheduling.ingress_rx.try_recv().unwrap();
            scheduling
                .route_session(&handle, item)
                .unwrap_or_else(|_| panic!("route failed"));
        }
        let mut execution = Some(scheduling.pump().unwrap().unwrap().turn);
        let mut reservations = Vec::new();
        for index in 0..128 {
            reservations.push(
                ingress
                    .prepare_rpc(
                        RequestClass::LongRunning,
                        rpc_owner(
                            "epoch",
                            Some(("session", 1)),
                            &format!("occupied-{index}"),
                            None,
                        ),
                        Some(handle.clone()),
                    )
                    .unwrap(),
            );
        }
        let (outgoing, mut requests) = futures::channel::mpsc::channel::<String>(4);
        let (mut responses, incoming) = futures::channel::mpsc::channel::<io::Result<String>>(4);
        let transport = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
        let (finished, finish) = oneshot::channel();
        let connection = Client
            .builder()
            .on_receive_dispatch(
                {
                    let ingress = ingress.clone();
                    async move |dispatch: Dispatch, _connection| {
                        ingress.receive_dispatch(dispatch).await
                    }
                },
                agent_client_protocol::on_receive_dispatch!(),
            )
            .connect_with(transport, async move |connection| {
                let owner = rpc_owner("epoch", Some(("session", 1)), "prompt", None);
                let mut waiting = Box::pin(send_ordered(
                    &connection,
                    Some(&ingress),
                    &mut execution,
                    owner,
                    RequestClass::LongRunning,
                    UntypedMessage::new("test/prompt", json!({})).unwrap(),
                ));
                assert!(futures::poll!(&mut waiting).is_pending());
                drop(
                    scheduling
                        .pump()
                        .unwrap()
                        .expect("RPC wait released its session ticket"),
                );
                // A global read can execute even while this session awaits its response.
                ingress
                    .try_browser(BridgeInput::RuntimeSnapshotRequest, TrafficClass::Ordinary)
                    .unwrap();
                let global = scheduling.ingress_rx.try_recv().unwrap();
                scheduling
                    .route_global(global)
                    .unwrap_or_else(|_| panic!("global route failed"));
                assert!(scheduling.pump().unwrap().unwrap().turn.handle().is_none());
                for _ in 0..3 {
                    let item = tokio::select! {
                        item = scheduling.ingress_rx.recv() => item.unwrap(),
                        _ = &mut waiting => panic!("response escaped before its ordered ticket"),
                    };
                    scheduling
                        .route_session(&handle, item)
                        .unwrap_or_else(|_| panic!("response route failed"));
                }
                let ScheduledInput { event, turn, .. } = scheduling.pump().unwrap().unwrap();
                assert!(matches!(
                    event,
                    BridgeIngress::Acp(Dispatch::Notification(_))
                ));
                drop(turn);
                let ScheduledInput { event, turn, .. } = scheduling.pump().unwrap().unwrap();
                let BridgeIngress::RpcCompleted(completion) = event else {
                    panic!("expected completion")
                };
                scheduling.handoff_completion(completion, turn).unwrap();
                assert_eq!(waiting.await.unwrap().unwrap()["answer"], 42);
                assert!(execution.is_some());
                assert!(
                    scheduling.pump().unwrap().is_none(),
                    "next update cannot pass response reduction"
                );
                drop(execution.take());
                assert!(scheduling.pump().unwrap().is_some());
                drop(reservations);
                finished.send(()).unwrap();
                Ok(())
            });
        let peer = async move {
            let request: Value = serde_json::from_str(&requests.next().await.unwrap()).unwrap();
            responses
                .send(Ok(json!([
                    {"jsonrpc":"2.0","method":"test/update","params":{"order":"before"}},
                    {"jsonrpc":"2.0","id":request["id"],"result":{"answer":42}},
                    {"jsonrpc":"2.0","method":"test/update","params":{"order":"after"}}
                ])
                .to_string()))
                .await
                .unwrap();
            finish.await.unwrap();
        };
        let (result, ()) = tokio::time::timeout(
            Duration::from_secs(3),
            futures::future::join(connection, peer),
        )
        .await
        .expect("ordered command hung");
        result.unwrap();
    }
    #[tokio::test]
    async fn creation_replay_rejection_releases_the_raw_response_target_claim() {
        let (events, _events) = mpsc::unbounded_channel();
        let (mut scheduling, ingress) = Scheduling::new(
            "epoch".into(),
            EventSink { tx: events.into() },
            CancellationToken::new(),
        )
        .unwrap();
        ingress
            .try_browser(BridgeInput::RuntimeSnapshotRequest, TrafficClass::Ordinary)
            .unwrap();
        let item = scheduling.ingress_rx.try_recv().unwrap();
        scheduling
            .route_global(item)
            .unwrap_or_else(|_| panic!("global route failed"));
        let mut execution = Some(scheduling.pump().unwrap().unwrap().turn);
        let (outgoing, mut requests) = futures::channel::mpsc::channel::<String>(4);
        let (mut responses, incoming) = futures::channel::mpsc::channel::<io::Result<String>>(4);
        let transport = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
        let (finished, finish) = oneshot::channel();
        let connection = Client
            .builder()
            .on_receive_dispatch(
                {
                    let ingress = ingress.clone();
                    async move |dispatch: Dispatch, _connection| {
                        ingress.receive_dispatch(dispatch).await
                    }
                },
                agent_client_protocol::on_receive_dispatch!(),
            )
            .connect_with(transport, async move |connection| {
                let owner = rpc_owner("epoch", None, "create", None);
                let mut waiting = Box::pin(send_ordered(
                    &connection,
                    Some(&ingress),
                    &mut execution,
                    owner.clone(),
                    RequestClass::Control,
                    NewSessionRequest::new("/tmp"),
                ));
                let item = tokio::select! {
                    item = scheduling.ingress_rx.recv() => item.unwrap(),
                    _ = &mut waiting => panic!("response bypassed its ordered completion"),
                };
                scheduling
                    .route_global(item)
                    .unwrap_or_else(|_| panic!("completion route failed"));
                let ScheduledInput { event, turn, .. } = scheduling.pump().unwrap().unwrap();
                let BridgeIngress::RpcCompleted(completion) = event else {
                    panic!("expected completion")
                };
                assert_eq!(completion.result.as_ref().unwrap()["sessionId"], "target");
                scheduling.handoff_completion(completion, turn).unwrap();
                let response = waiting.await.unwrap().unwrap();
                let mut state = BridgeState::default();
                state
                    .creation_staging
                    .entry("target".into())
                    .or_default()
                    .validation
                    .invalid_reason = Some("early replay was invalid".into());
                let admitted = (|| {
                    let _creation_finish =
                        CreationFinish::new(Some(&ingress), &execution, owner.clone());
                    prepare_session_response(
                        &mut state,
                        "target",
                        PathBuf::from("/tmp"),
                        &serde_json::to_value(response).unwrap(),
                        None,
                    )
                })();
                assert!(
                    admitted.is_err(),
                    "invalid early replay rejects the returned target"
                );
                let item = scheduling.ingress_rx.try_recv().unwrap();
                let BridgeIngress::CreationFinished {
                    owner: finished_owner,
                    target_id,
                    incarnation,
                } = item.event
                else {
                    panic!("raw target claim was not released")
                };
                assert_eq!(finished_owner, owner);
                assert_eq!(target_id, "target");
                assert_eq!(incarnation, None);
                assert!(
                    scheduling.ingress_rx.try_recv().is_err(),
                    "claim resolves once"
                );
                drop(execution.take());
                finished.send(()).unwrap();
                Ok(())
            });
        let peer = async move {
            let request: Value = serde_json::from_str(&requests.next().await.unwrap()).unwrap();
            responses
                .send(Ok(json!({
                    "jsonrpc":"2.0", "id":request["id"],
                    "result":{"sessionId":"target"}
                })
                .to_string()))
                .await
                .unwrap();
            finish.await.unwrap();
        };
        let (result, ()) = tokio::time::timeout(
            Duration::from_secs(3),
            futures::future::join(connection, peer),
        )
        .await
        .expect("creation replay rejection hung");
        result.unwrap();
    }
}
