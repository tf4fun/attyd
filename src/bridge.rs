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

use crate::protocol::controls::with_notification_meta;

use self::publication::{BridgeLock, ProtocolState, agent_launch, operation_error};
use self::scheduling::{ExecutionTurn, IngressSender};
use crate::agent_process::StdioAcpAgent;
use crate::auth_terminal::{AuthTerminalManager, validate_method as validate_terminal_auth_method};
use crate::bridge_event::{BridgeEvent, EventSender};
use crate::elicitation_validation::validate_elicitation_request;
use crate::filesystem::WorkspaceFileSystem;
use crate::mcp::McpManager;
use crate::mcp_config::{server_name, server_type};
use crate::options::{Options, Transport};
use crate::ordered_ingress::{RequestClass, RequestOwner};
use crate::protocol::documents::{OperationKind as OpKind, Scope as OpScope};
use crate::protocol::wire;
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
use crate::session_mirror::{MirrorError, MirrorPhase};
use crate::session_observers::SessionWork;
use crate::session_registry::{HistorySyncStatus, SessionRegistry};
use crate::session_resources::{
    AttachmentDelivery, ElicitationResponder, HistorySyncOwner, PermissionResponder,
    ReplayValidationBackup, SessionResourceOwner, SessionResources, SyncControlCandidate,
    UrlRegistration,
};
use crate::session_state::SessionAdmission;
use crate::terminal::{TerminalManager, TerminalSnapshot};

mod agent_dispatch;
#[cfg(test)]
mod control_owner_tests;
mod coordinator;
#[cfg(test)]
mod coordinator_tests;
mod inbound_requests;
#[cfg(test)]
mod memory_retention_tests;
pub(crate) mod controls;
pub(crate) mod operations;
#[cfg(test)]
mod protocol_gate_tests;
pub(crate) mod publication;
pub(crate) mod resources;
mod scheduling;
mod streams;

const SHUTDOWN_CANCEL_GRACE_PERIOD: Duration = Duration::from_secs(2);
const RECONCILE_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const RECONCILE_MAX_BACKOFF: Duration = Duration::from_secs(8);

/// What the bridge tells its host, and where it records its diagnostics.
#[derive(Clone)]
struct EventSink {
    tx: EventSender,
}

impl EventSink {
    /// The Agent connection initialized (see `BridgeEvent`).
    fn initialized(&self) {
        let _ = self.tx.send(BridgeEvent::Initialized);
    }

    /// Records an ACP error. Request- and operation-scoped failures are also
    /// answered through their own response or operation.
    fn acp_error(&self, error: Error, request_id: Option<&str>, operation: Option<&str>) {
        tracing::warn!(
            request_id = ?request_id,
            operation = ?operation,
            code = i32::from(error.code),
            data = ?error.data,
            "{}",
            error.message
        );
    }
}

fn ensure_interaction_delivered(delivered: bool, interaction: &str) -> Result<(), Error> {
    if !delivered {
        return Err(Error::request_cancelled().data(format!(
            "Agent cancelled the {interaction} request before delivery"
        )));
    }
    Ok(())
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
    Fresh { cwd: PathBuf },
    Reload { cwd: PathBuf },
}

impl AttachmentReservation {
    fn cwd(&self) -> &PathBuf {
        match self {
            Self::Fresh { cwd, .. } | Self::Reload { cwd, .. } => cwd,
        }
    }

    fn is_reload(&self) -> bool {
        matches!(self, Self::Reload { .. })
    }
}

#[derive(Default)]
struct CreationStaging {
    validation: SessionUpdateSemanticState,
    early_notifications: Vec<SessionNotification>,
    bytes: usize,
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
    /// Where the end of a control action's command is answered, by its request ID.
    action_replies: HashMap<String, oneshot::Sender<Result<(), crate::protocol::documents::ProtocolError>>>,
    /// The session a `close` action closes, by its request ID: when the Agent closed it, the
    /// scope ends with `closed` (protocol §7).
    action_closes: HashMap<String, String>,
    sessions: SessionRegistry,
    observer_events: Option<mpsc::UnboundedSender<BridgeInput>>,
    observer_timeout: Option<i64>,
    observers_stopped: bool,
    /// Protocol v2: the published documents and the Agent's state.
    protocol: ProtocolState,
    /// Protocol v2: the sessions the previous connection held for a
    /// subscription, which this one loads again once the Agent is ready.
    stream_reloads: std::collections::VecDeque<String>,
    /// The connection's ingress, for tests that wait on its receipt.
    #[cfg(test)]
    ingress: Option<IngressSender>,
}

fn session_ingest<'a>(state: &'a BridgeState, session_id: &str) -> Option<&'a SessionResources> {
    state.sessions.resources(session_id).ok()
}

fn session_ingest_mut<'a>(
    state: &'a mut BridgeState,
    session_id: &str,
) -> Option<&'a mut SessionResources> {
    state.sessions.resources_mut(session_id).ok()
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
) -> Result<&'a mut SessionUpdateSemanticState, Error> {
    Ok(state
        .sessions
        .resources_mut(session_id)
        .map_err(mirror_error)?
        .validation
        .get_or_insert_with(SessionUpdateSemanticState::default))
}

/// The validation an Agent update folds into: the installed session's, or the
/// pending creation staging of an ID no session holds yet.
fn update_validation_mut<'a>(
    state: &'a mut BridgeState,
    session_id: &str,
    staged: bool,
) -> Option<&'a mut SessionUpdateSemanticState> {
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

fn clear_attachment_delivery(state: &mut BridgeState, session_id: &str) {
    if let Ok(resources) = state.sessions.resources_mut(session_id) {
        resources.attachment = AttachmentDelivery::default();
    }
}

fn clear_ingest_resources(state: &mut BridgeState, session_id: &str) {
    if let Ok(resources) = state.sessions.resources_mut(session_id) {
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

#[cfg(test)]
fn pending_elicitation_count(state: &BridgeState) -> usize {
    state.request_elicitations.len() + state.sessions.elicitation_owners.len()
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

/// Delivers `response` to the pending permission `permission_id` of `owner`
/// and retires it; `resolve` retires the runtime interaction.
/// Returns whether the Agent still waited for it.
fn deliver_permission(
    locked: &mut BridgeState,
    owner: &SessionResourceOwner,
    permission_id: &str,
    response: RequestPermissionResponse,
    resolve: impl FnOnce(&mut BridgeState) -> Result<(), Error>,
) -> Result<bool, Error> {
    let pending = locked
        .sessions
        .take_permission(permission_id, owner)
        .expect("exact permission owner checked under the same lock");
    // oneshot delivery is synchronous: keep owner retirement and canonical
    // completion ordered before another request can replace this session.
    let delivered = pending.sender.send(response).is_ok();
    resolve(locked)?;
    refresh_observer_timers(locked);
    Ok(delivered)
}

/// Delivers `response` to the pending elicitation `elicitation_id` (of
/// `owner`, or request-scoped) and retires it; `resolve` retires
/// the runtime interaction, and a delivered accept of a URL elicitation
/// becomes its flow with its route in the same step. Returns whether the
/// Agent still waited for it.
fn deliver_elicitation(
    locked: &mut BridgeState,
    elicitation_id: &str,
    owner: Option<&SessionResourceOwner>,
    response: CreateElicitationResponse,
    accepted_url_id: Option<String>,
    resolve: impl FnOnce(&mut BridgeState, Option<&str>) -> Result<(), Error>,
) -> Result<bool, Error> {
    let pending = take_pending_elicitation(locked, elicitation_id, owner)
        .expect("exact elicitation owner checked under the same lock");
    let delivered = pending.sender.send(response).is_ok();
    // Immediate Agent completion/reuse cannot overtake accepted URL registration.
    let accepted_url_id = accepted_url_id.filter(|_| delivered);
    resolve(locked, accepted_url_id.as_deref())?;
    if let Some(url_id) = accepted_url_id {
        locked
            .sessions
            .register_url_route(&url_id, owner.cloned())
            .map_err(mirror_error)?;
        if owner.is_none() {
            locked.sessions.accept_joined_flow(elicitation_id);
        }
    }
    refresh_observer_timers(locked);
    Ok(delivered)
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

fn mirror_error(error: MirrorError) -> Error {
    match error {
        MirrorError::WrongPhase => {
            Error::invalid_request().data("session is not ready to accept this operation")
        }
        MirrorError::UnknownSession => Error::invalid_params().data("unknown session"),
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
    allowed_attachment: Option<&str>,
) -> Result<Vec<SessionNotification>, Error> {
    validate_agent_session_id(session_id).map_err(semantic_error)?;
    let attaching = allowed_attachment.is_some_and(|operation_id| {
        state.sessions.state(session_id).is_some_and(|session| {
            session
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
        ensure_session_validation(state, session_id)?
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

fn promote_creation_validation(state: &mut BridgeState, session_id: &str) -> Result<(), Error> {
    if state.sessions.active_session(session_id).is_none() {
        return Err(Error::invalid_request().data("creation requires its installed active session"));
    }
    let resources = state.sessions.resources(session_id).map_err(mirror_error)?;
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
        .resources_mut(session_id)
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

/// The session timeline reduced from notifications as the Agent sent them,
/// each with its own `_meta` (bridge memory model §3).
fn timeline_from_notifications(
    notifications: &[SessionNotification],
) -> crate::protocol::timeline::Timeline {
    let mut timeline = crate::protocol::timeline::Timeline::default();
    for notification in notifications {
        let Ok(update) = serde_json::to_value(&notification.update) else {
            continue;
        };
        if is_conversation_update(&update) {
            let envelope = notification
                .meta
                .as_ref()
                .map(|meta| Value::Object(meta.clone()));
            timeline.reduce(&update, envelope.as_ref());
        }
    }
    timeline
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
    operation_id: &str,
    kind: RuntimeSessionOperationKind,
    error: &Error,
) -> Result<(), Error> {
    let epoch = state.sessions.epoch().to_string();
    if is_incoming_transport_closed(error) {
        state
            .sessions
            .mark_operation_uncertain(&epoch, session_id, operation_id, error.message.clone())
            .map_err(runtime_state_error)
    } else {
        state
            .sessions
            .fail_operation(
                &epoch,
                session_id,
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
    operation_id: &str,
    error: &Error,
) -> Result<(), Error> {
    let epoch = state.sessions.epoch().to_string();
    if is_incoming_transport_closed(error) {
        state
            .sessions
            .mark_prompt_uncertain(&epoch, session_id, operation_id, error.message.clone())
            .map_err(runtime_state_error)
    } else {
        state
            .sessions
            .fail_prompt(&epoch, session_id, operation_id, agent_error_value(error))
            .map_err(runtime_state_error)
    }
}

fn settle_runtime_attachment_request_error(
    state: &mut BridgeState,
    session_id: &str,
    operation_id: &str,
    kind: RuntimeSessionOperationKind,
    reload: bool,
    error: &Error,
) -> Result<(), Error> {
    let epoch = state.sessions.epoch().to_string();
    if is_incoming_transport_closed(error) {
        state
            .sessions
            .mark_operation_uncertain(&epoch, session_id, operation_id, error.message.clone())
            .map_err(runtime_state_error)
    } else if reload {
        state
            .sessions
            .fail_reload(&epoch, session_id, operation_id, agent_error_value(error))
            .map_err(runtime_state_error)
    } else {
        state
            .sessions
            .fail_attachment(
                &epoch,
                session_id,
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
                operation_id,
                kind,
                json!({ "message": error.message.clone(), "data": error.data.clone() }),
            )
            .map_err(runtime_state_error)
    }
}

fn clear_attachment_tracking(state: &mut BridgeState, session_id: &str, attempt_id: &str) {
    if !replay_attempt_current(state, session_id, attempt_id) {
        return;
    }
    settle_attachment(state, session_id);
    clear_attachment_delivery(state, session_id);
    rollback_replay_validation(state, session_id, attempt_id);
}

/// A load's replay validation stays current until that attempt settles it.
fn replay_attempt_current(state: &BridgeState, session_id: &str, attempt_id: &str) -> bool {
    replay_validation_backup(state, session_id)
        .is_some_and(|backup| backup.attempt_id == attempt_id)
}

fn begin_replay_validation(state: &mut BridgeState, session_id: &str, attempt_id: &str) {
    let resources = state
        .sessions
        .resources_mut(session_id)
        .expect("validated replay owner");
    let previous = resources
        .validation
        .replace(SessionUpdateSemanticState::default());
    resources.replay_validation_backup = Some(ReplayValidationBackup {
        attempt_id: attempt_id.to_string(),
        previous,
    });
}

fn take_replay_validation_backup(
    state: &mut BridgeState,
    session_id: &str,
    attempt_id: &str,
) -> Option<ReplayValidationBackup> {
    if !replay_attempt_current(state, session_id, attempt_id) {
        return None;
    }
    session_ingest_mut(state, session_id)?
        .replay_validation_backup
        .take()
}

fn commit_replay_validation(state: &mut BridgeState, session_id: &str, attempt_id: &str) {
    if take_replay_validation_backup(state, session_id, attempt_id).is_some()
        && let Some(validation) = session_validation_mut(state, session_id)
    {
        validation.retire_turn();
    }
}

fn rollback_replay_validation(state: &mut BridgeState, session_id: &str, attempt_id: &str) {
    if let Some(backup) = take_replay_validation_backup(state, session_id, attempt_id) {
        session_ingest_mut(state, session_id)
            .expect("validated replay owner")
            .validation = backup.previous;
    }
}

fn fail_mirror_attachment(
    state: &mut BridgeState,
    session_id: &str,
    attempt_id: &str,
    reload: bool,
    retain_cold: bool,
    retryable: bool,
    message: &str,
) {
    if reload || retain_cold {
        let _ = session_mirror(state).fail_load(session_id, attempt_id, message, retryable);
    } else {
        session_mirror(state).remove(session_id);
    }
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
    // Only a session the Agent can bring back is released, and only once it
    // is not needed (S6); a needed one counts as running work.
    let releasable = streams::releasable(state);
    let followed = streams::followed(state);
    let owners = state
        .sessions
        .iter_entries()
        .map(|entry| {
            let session_id = entry.state.session_id.clone();
            let work = if streams::needed(state, &followed, &session_id) {
                SessionWork::Running
            } else {
                SessionWork::of(entry)
            };
            (
                followed.contains(&session_id),
                session_id,
                entry.state.live.as_ref().map(|live| live.lifecycle.clone()),
                work,
            )
        })
        .collect::<Vec<_>>();
    for (is_followed, session_id, lifecycle, work) in owners {
        let Ok(resources) = state.sessions.resources_mut(&session_id) else {
            continue;
        };
        if state.observers_stopped || !releasable {
            resources.observers.stop_absence();
            continue;
        }
        if is_followed {
            // A subscription follows the session again: its next absence is
            // a new cycle, which may close it even after a refused close.
            resources.observers.follow();
            continue;
        }
        let Some(timer) = resources.observers.refresh(
            lifecycle.as_ref(),
            work,
            timeout,
            tokio::time::Instant::now(),
        ) else {
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

/// An unobserved session outside the Idle work state must not be retired:
/// in-flight work would be cut off and a pending interaction would be
/// cancelled Agent-side.
fn session_blocks_unobserved_retire(state: &BridgeState, session_id: &str) -> bool {
    state
        .sessions
        .work(session_id)
        .is_some_and(|work| work != SessionWork::Idle)
}

/// Starts the load a v2 stream waits for (protocol §5.1); a session that
/// needs none settles the wait at once.
fn prepare_session_load(
    state: &mut BridgeState,
    session_id: String,
    cwd: Option<String>,
) -> Option<Value> {
    if state.catalog_deletions.contains_key(&session_id)
        || state
            .sessions
            .state(&session_id)
            .is_some_and(|session| session.operation.is_some() && session.observation_retired())
    {
        streams::finish_load(
            state,
            &session_id,
            Err(SessionViewError::unavailable("session is being deleted")),
        );
        return None;
    }
    let held = state
        .sessions
        .session_ref(&session_id)
        .is_some_and(|session| {
            !matches!(
                session.live.lifecycle,
                SessionLifecycle::Attaching | SessionLifecycle::Closed
            )
        })
        || state
            .sessions
            .state(&session_id)
            .is_some_and(|session| session.phase == MirrorPhase::Blocked);
    if held {
        refresh_observer_timers(state);
        return None;
    }
    if !state.listed_sessions.contains_key(&session_id) && cwd.is_none() {
        streams::finish_load(state, &session_id, Err(SessionViewError::NotFound));
        return None;
    }
    if session_materialization(state, &session_id).is_some() {
        return None;
    }
    if state.sessions.state(&session_id).is_none() {
        state.sessions.register_cold(&session_id);
    }
    let materialization_id = Uuid::new_v4().to_string();
    state
        .sessions
        .resources_mut(&session_id)
        .expect("registered materialization owner")
        .materialization = Some(crate::session_resources::MaterializationResources {
        attempt_id: materialization_id.clone(),
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
    state
        .sessions
        .resources(session_id)
        .ok()?
        .materialization
        .as_ref()
}

fn take_session_materialization(
    state: &mut BridgeState,
    session_id: &str,
    attempt_id: &str,
) -> Option<crate::session_resources::MaterializationResources> {
    if session_materialization(state, session_id)?.attempt_id != attempt_id {
        return None;
    }
    state
        .sessions
        .resources_mut(session_id)
        .ok()?
        .materialization
        .take()
}

fn validate_terminal_references(
    state: &BridgeState,
    session_id: &str,
    tool_call: &Value,
) -> Result<(), Error> {
    let references = terminal_references(tool_call);
    if references.is_empty() {
        return Ok(());
    }
    let live = state
        .sessions
        .resource_session(session_id)
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
    owner: &SessionResourceOwner,
    terminal_id: &str,
) -> Result<(), Error> {
    if !state.sessions.owns_resources(owner)
        || state.sessions.resource_session(&owner.session_id).is_none()
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
        TerminalSnapshot {
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
    if state.sessions.resource_session(&session_id).is_none() {
        return false;
    }
    let epoch = state.sessions.epoch().to_string();
    state
        .sessions
        .upsert_terminal(&epoch, &session_id, terminal_id, snapshot.value)
        .is_ok_and(|result| {
            matches!(
                result,
                crate::runtime_state::TerminalUpsert::Inserted
                    | crate::runtime_state::TerminalUpsert::Updated
            )
        })
}

fn publish_terminal_snapshot(state: &mut BridgeState, snapshot: TerminalSnapshot) {
    if apply_terminal_snapshot(state, snapshot) {
        refresh_observer_timers(state);
    }
}

fn live_session(state: &BridgeState, session_id: &str) -> bool {
    state.sessions.resource_session(session_id).is_some()
}

fn attached_session(state: &BridgeState, session_id: &str) -> bool {
    state
        .sessions
        .resource_session(session_id)
        .is_some_and(|session| session.live.lifecycle != SessionLifecycle::Attaching)
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
    ingress: Option<scheduling::IngressSender>,
    options: Arc<Options>,
    state: Arc<BridgeLock>,
    sink: EventSink,
    terminals: Option<TerminalManager>,
    auth_terminal: AuthTerminalManager,
    prompt_lifecycle: Arc<PromptLifecycle>,
    cancellation: CancellationToken,
    history_flow: Arc<HistoryFlowGuard>,
}

/// The command-level owner of one registered history workflow. The command's
/// finalizer takes the owner back out; an unexpectedly dropped task instead
/// posts an identified cleanup so the session never leaks a busy slot.
struct HistoryFlowGuard {
    owner: std::sync::Mutex<Option<HistorySyncOwner>>,
    events: mpsc::UnboundedSender<BridgeInput>,
}

impl HistoryFlowGuard {
    fn register(&self, owner: HistorySyncOwner) {
        *self.owner.lock().expect("history flow guard poisoned") = Some(owner);
    }

    fn take(&self) -> Option<HistorySyncOwner> {
        self.owner
            .lock()
            .expect("history flow guard poisoned")
            .take()
    }
}

impl Drop for HistoryFlowGuard {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.owner.lock()
            && let Some(owner) = slot.take()
        {
            let _ = self.events.send(BridgeInput::FinishHistorySync { owner });
        }
    }
}

pub(crate) enum BridgeInput {
    RetireUnobservedSession {
        session_id: String,
        absence_id: u64,
        permit: crate::auto_close::AutoClosePermit,
    },
    /// A dropped command task releases its history workflow through the
    /// coordinator instead of mutating canonical state inside `Drop`.
    FinishHistorySync { owner: HistorySyncOwner },
    /// A v2 stream (protocol §5.1): registers the subscription, holds and
    /// loads its unheld sessions, and joins distribution under one bridge
    /// guard.
    Subscribe {
        scopes: Vec<crate::protocol::subscription::Subscribed>,
        reply: oneshot::Sender<crate::protocol::feed::FeedSubscription>,
    },
    /// Starts the next load v2 streams wait for.
    ServeStreams,
    /// An operation write (protocol §6): admitted and installed under one
    /// bridge guard, then run as the command of its kind.
    Operation {
        scope: OpScope,
        write: crate::protocol::admission::OpWrite,
        reply: oneshot::Sender<
            Result<operations::OperationAdmission, crate::protocol::documents::ProtocolError>,
        >,
    },
    /// A resource action (protocol §7): found and applied under one bridge
    /// guard; it installs no operation.
    Resource {
        scope: OpScope,
        resource: String,
        action: resources::ResourceAction,
        reply: oneshot::Sender<Result<(), crate::protocol::documents::ProtocolError>>,
    },
    /// A control action (protocol §7): checked under one bridge guard, then sent
    /// to the Agent; the answer arrives when the Agent has answered.
    Control {
        session_id: String,
        control: controls::Control,
        reply: oneshot::Sender<
            Result<controls::ControlAnswer, crate::protocol::documents::ProtocolError>,
        >,
    },
    /// The workspace of a held session, for its context reads (protocol §8);
    /// it holds, loads and renews nothing.
    Workspace {
        session_id: String,
        reply:
            oneshot::Sender<Result<WorkspaceFileSystem, crate::protocol::documents::ProtocolError>>,
    },
}

impl BridgeInput {
    /// The session whose order the input runs in, if any.
    fn session_id(&self) -> Option<&str> {
        match self {
            Self::Subscribe { .. }
            | Self::ServeStreams
            | Self::Resource { .. }
            | Self::Control { .. }
            | Self::Workspace { .. } => None,
            Self::RetireUnobservedSession { session_id, .. } => Some(session_id),
            Self::FinishHistorySync { owner } => Some(&owner.session.session_id),
            Self::Operation { scope, write, .. } => operations::write_session(scope, write),
        }
    }

    /// Charge retained payload plus bounded envelope/handle overhead without
    /// constructing a second serialized copy of a potentially large prompt.
    fn payload_bytes(&self) -> usize {
        let payload = match self {
            Self::ServeStreams => 0,
            Self::Subscribe { scopes, .. } => scopes.len().saturating_mul(256),
            Self::RetireUnobservedSession { session_id, .. } => serialized_value_len(session_id),
            Self::FinishHistorySync { owner } => serialized_value_len(&(
                &owner.session.epoch,
                &owner.session.session_id,
                &owner.flow_id,
            )),
            Self::Operation { write, .. } => serialized_value_len(&write.args),
            Self::Resource {
                resource, action, ..
            } => serialized_value_len(resource).saturating_add(match action {
                resources::ResourceAction::Input { data } => data.len(),
                _ => 0,
            }),
            Self::Control { control, .. } => serialized_value_len(&format!("{control:?}")),
            Self::Workspace { session_id, .. } => serialized_value_len(session_id),
        };
        payload.saturating_add(256)
    }

    /// Capacity and stale-owner rejection must finish the original HTTP wait;
    /// dropping a sender alone loses the cause.
    fn reject(self, error: Error) {
        match self {
            Self::Subscribe { .. } | Self::ServeStreams => {}
            Self::RetireUnobservedSession { permit, .. } => {
                permit.cancel();
            }
            Self::FinishHistorySync { owner } => {
                owner.cancellation.cancel();
            }
            Self::Operation { reply, .. } => {
                let _ = reply.send(Err(crate::protocol::documents::ProtocolError::new(
                    crate::protocol::documents::ErrorCode::Unavailable,
                    error_message(error),
                )));
            }
            Self::Resource { reply, .. } => {
                let _ = reply.send(Err(crate::protocol::documents::ProtocolError::new(
                    crate::protocol::documents::ErrorCode::Unavailable,
                    error_message(error),
                )));
            }
            Self::Control { reply, .. } => {
                let _ = reply.send(Err(crate::protocol::documents::ProtocolError::new(
                    crate::protocol::documents::ErrorCode::Unavailable,
                    error_message(error),
                )));
            }
            Self::Workspace { reply, .. } => {
                let _ = reply.send(Err(crate::protocol::documents::ProtocolError::new(
                    crate::protocol::documents::ErrorCode::Unavailable,
                    error_message(error),
                )));
            }
        }
    }
}

fn browser_traffic_class(input: &BridgeInput) -> crate::session_dispatch::TrafficClass {
    use crate::session_dispatch::TrafficClass;
    match input {
        BridgeInput::Operation { write, .. } if operations::ordinary(write) => {
            TrafficClass::Ordinary
        }
        _ => TrafficClass::Reserved,
    }
}

/// Why a load a v2 stream waited for did not run.
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
        if error.code == agent_client_protocol::ErrorCode::ResourceNotFound {
            Self::NotFound
        } else {
            Self::Unavailable(error_message(error))
        }
    }
}

#[cfg(test)]
pub async fn run_with_cancellation(
    options: Arc<Options>,
    commands: mpsc::UnboundedReceiver<BridgeInput>,
    events: EventSender,
    cancellation: CancellationToken,
) {
    run_publishing(
        options,
        commands,
        events,
        cancellation,
        crate::protocol::publication::Publication::default(),
    )
    .await
}

/// One generation of the bridge, publishing into `publication`, which the
/// host shares across its generations (protocol §5.4).
pub(crate) async fn run_publishing(
    options: Arc<Options>,
    commands: mpsc::UnboundedReceiver<BridgeInput>,
    events: EventSender,
    cancellation: CancellationToken,
    publication: crate::protocol::publication::Publication,
) {
    let sink = EventSink { tx: events };
    let result = match options.transport {
        Transport::Stdio => {
            let config = AcpAgentConfig::new(&options.command[0])
                .args(options.command.iter().skip(1).cloned());
            let (fatal_tx, mut fatal_rx) = mpsc::unbounded_channel();
            let agent = StdioAcpAgent::new(config)
                .on_stderr(move |chunk| {
                    tracing::debug!(target: "attyd::agent_stderr", "{chunk}");
                })
                .on_fatal(move |error| {
                    let _ = fatal_tx.send(error);
                });
            let connection = run_generation(
                Ok(agent),
                options.clone(),
                commands,
                sink.clone(),
                cancellation.clone(),
                publication,
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
            run_generation(
                Ok(crate::websocket_agent::WebSocketAgent::new(
                    options.command[0].clone(),
                )),
                options.clone(),
                commands,
                sink.clone(),
                cancellation.clone(),
                publication,
            )
            .await
        }
        Transport::Http => {
            // A transport that cannot be built still ends this generation
            // through its final commit, so the documents show why.
            let client = remote_http_client(&options.command[0])
                .map_err(|error| Error::invalid_params().data(error.to_string()));
            run_generation(
                client,
                options.clone(),
                commands,
                sink.clone(),
                cancellation.clone(),
                publication,
            )
            .await
        }
    };

    if let Err(error) = result {
        tracing::error!(error = ?error, "ACP bridge failed");
    }
}

fn remote_http_client(
    endpoint: &str,
) -> Result<HttpClient, agent_client_protocol_http::HttpClientError> {
    // Keep long-lived SSE streams out of the pool used by later ACP requests.
    // Diagnostics captured stale dispatch readiness returning an active SSE
    // connection to the pool, where a later RPC reused it and stalled. Disabling
    // reuse avoids this path, at the cost of a fresh connection per RPC; see
    // docs/testing.md for the evidence and scope of this workaround.
    let http = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()?;
    HttpClient::with_endpoint_and_client(endpoint, http)
}

#[cfg(test)]
async fn run_connection<T>(
    transport: T,
    options: Arc<Options>,
    commands: mpsc::UnboundedReceiver<BridgeInput>,
    sink: EventSink,
    cancellation: CancellationToken,
) -> Result<(), Error>
where
    T: ConnectTo<Client>,
{
    run_generation(
        Ok(transport),
        options,
        commands,
        sink,
        cancellation,
        crate::protocol::publication::Publication::default(),
    )
    .await
}

/// One generation of the bridge actor: one Agent connection, publishing into
/// `publication`, which the generations before and after it share. A
/// transport that could not be built connects nothing; the generation still
/// publishes and ends with its error.
async fn run_generation<T>(
    transport: Result<T, Error>,
    options: Arc<Options>,
    commands: mpsc::UnboundedReceiver<BridgeInput>,
    sink: EventSink,
    cancellation: CancellationToken,
    publication: crate::protocol::publication::Publication,
) -> Result<(), Error>
where
    T: ConnectTo<Client>,
{
    let mut state = BridgeState::default();
    state.protocol.publication = publication;
    state.seed_from_previous_connection();
    let state = Arc::new(BridgeLock::new(state));
    state.lock().await.protocol.agent.launch = agent_launch(&options);
    #[cfg(test)]
    memory_retention_tests::capture_coordinator_state(&state);
    let result = match transport {
        Ok(transport) => {
            run_connection_with(
                state.clone(),
                transport,
                options,
                commands,
                sink,
                cancellation,
            )
            .await
        }
        Err(error) => Err(error),
    };
    // The last commit, on every return once the state exists: running
    // operations fail and the final agent shows.
    state.lock().await.end_connection(result.as_ref().err());
    result
}

async fn run_connection_with<T>(
    state: Arc<BridgeLock>,
    transport: T,
    options: Arc<Options>,
    mut commands: mpsc::UnboundedReceiver<BridgeInput>,
    sink: EventSink,
    cancellation: CancellationToken,
) -> Result<(), Error>
where
    T: ConnectTo<Client>,
{
    let epoch = state.lock().await.sessions.epoch().to_string();
    let (mut scheduling, ingress) =
        scheduling::Scheduling::new(epoch.clone(), sink.clone(), cancellation.clone())?;
    let mut coordinator = coordinator::Coordinator::default();
    let (observer_events, mut observer_rx) = mpsc::unbounded_channel();
    {
        let mut state = state.lock().await;
        #[cfg(test)]
        {
            state.ingress = Some(ingress.clone());
        }
        state.observer_events = Some(observer_events.clone());
        state.observer_timeout = Some(options.session_unobserved_timeout);
    }
    // A closed v2 subscription may leave a session unneeded (S6).
    let mut feed_closes = {
        let state = state.lock().await;
        let published = state.protocol.publication.lock();
        published.feed.closes()
    };
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
        TerminalManager::new_with_snapshots(
            filesystem,
            cancellation.clone(),
            Some(terminal_snapshot_tx),
        )
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
    let mcp = McpManager::new(options.cwd.clone(), options.acp_mcp_providers.clone());
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
            operation_id: "connection".to_string(),
            attempt_id: None,
        },
        url_registration: None,
        request_lease: None,
        request_scope: None,
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
            state.lock().await.protocol.agent.phase =
                crate::protocol::documents::AgentPhase::Initializing;
            let initialize = InitializeRequest::new(ProtocolVersion::V1)
                .client_capabilities(client_capabilities(&options))
                .client_info(
                    Implementation::new("attyd", crate::VERSION).title("attyd web client"),
                );
            // While initialize is unanswered, a stream subscribes from the
            // published cut; the other inputs wait in ingress, in order.
            let request = connection.send_request(initialize).block_task();
            tokio::pin!(request);
            let mut commands_open = true;
            let response = loop {
                tokio::select! {
                    biased;
                    // Cancellation ends the wait: the final commit below
                    // shows the Agent stopped before the streams close.
                    _ = cancellation.cancelled() => return Ok(()),
                    response = &mut request => break response?,
                    input = commands.recv(), if commands_open => match input {
                        Some(BridgeInput::Subscribe { scopes, reply }) => {
                            let state = state.lock().await;
                            let _ = reply.send(streams::register(&state, &scopes));
                        }
                        Some(input) => {
                            let class = browser_traffic_class(&input);
                            let _ = ingress.try_browser(input, class);
                        }
                        None => {
                            commands_open = false;
                            cancellation.cancel();
                        }
                    },
                }
            };
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
                state.protocol.agent.phase = crate::protocol::documents::AgentPhase::Ready;
                state.protocol.agent.initialize = Some(serde_json::to_value(&response)?);
                state.agent_capabilities = Some(response.agent_capabilities.clone());
                state.auth_methods = response
                    .auth_methods
                    .iter()
                    .map(|method| (method.id().0.to_string(), method.clone()))
                    .collect();
                // The streams wait for the Agent: load what they follow.
                streams::wake(&state);
            }
            sink.initialized();

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
                        Ok(()) = feed_closes.changed() => {
                            refresh_observer_timers(&mut *state.lock().await);
                        }
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
                let command = match input {
                    Some(BridgeInput::Subscribe { scopes, reply }) => {
                        let mut state = state.lock().await;
                        let (subscription, command) =
                            streams::subscribe(&mut state, &scopes);
                        let _ = reply.send(subscription);
                        let Some(command) = command else {
                            continue;
                        };
                        command
                    }
                    Some(BridgeInput::ServeStreams) => {
                        let mut state = state.lock().await;
                        let command = streams::serve(&mut state);
                        refresh_observer_timers(&mut state);
                        let Some(command) = command else {
                            continue;
                        };
                        command
                    }
                    Some(BridgeInput::FinishHistorySync { owner }) => {
                        // A dropped task may not release its flow through the
                        // command finalizer; only the exact identity may do so.
                        let mut state = state.lock().await;
                        state.sessions.finish_history_sync(&owner);
                        refresh_observer_timers(&mut state);
                        continue;
                    }
                    Some(BridgeInput::RetireUnobservedSession {
                        session_id,
                        absence_id,
                        permit,
                    }) => {
                        if !permit.pending() {
                            continue;
                        }
                        let mut state = state.lock().await;
                        if !state
                            .sessions
                            .resources(&session_id)
                            .is_ok_and(|resources| {
                                resources.observers.matches_absence(absence_id, &permit)
                            })
                            || state.sessions.active_session(&session_id).is_none()
                        {
                            permit.cancel();
                            continue;
                        }
                        // A running turn, an in-flight operation, or a pending
                        // interaction owns the session's next step: cancel this
                        // countdown. Settling back to idle starts a fresh full
                        // interval rather than resuming a stale deadline.
                        if session_blocks_unobserved_retire(&state, &session_id)
                            || !streams::releasable(&state)
                            || streams::needed(&state, &streams::followed(&state), &session_id)
                        {
                            if let Ok(resources) =
                                state.sessions.resources_mut(&session_id)
                            {
                                resources.observers.stop_absence();
                            }
                            continue;
                        }
                        // The Agent owns the session: close it when supported,
                        // otherwise drop our local copy and let the Agent keep
                        // whatever it retains (the Zed model).
                        let supports_close = state
                            .agent_capabilities
                            .as_ref()
                            .is_some_and(|caps| caps.session_capabilities.close.is_some());
                        drop(state);
                        auto_close = Some(permit);
                        let request_id =
                            format!("bridge-unobserved-retire-{}", Uuid::new_v4());
                        json!({
                            "type": if supports_close { "session/close" } else { "bridge/retire_session" },
                            "requestId": request_id,
                            "sessionId": session_id,
                        })
                    }
                    Some(BridgeInput::Operation { scope, write, reply }) => {
                        let mut state = state.lock().await;
                        match operations::admit(&mut state, &scope, &write) {
                            Err(error) => {
                                let _ = reply.send(Err(error));
                                continue;
                            }
                            Ok(operations::Write::Running(admitted)) => {
                                let applied = operations::apply(
                                    &mut state,
                                    &sink,
                                    &connection,
                                    &auth_terminal,
                                    &scope,
                                    admitted,
                                );
                                let _ = reply.send(applied);
                                continue;
                            }
                            Ok(operations::Write::Repeat(admission)) => {
                                let _ = reply.send(Ok(admission));
                                continue;
                            }
                            Ok(operations::Write::Start(admission, command)) => {
                                let _ = reply.send(Ok(admission));
                                command
                            }
                        }
                    }
                    Some(BridgeInput::Resource {
                        scope,
                        resource,
                        action,
                        reply,
                    }) => {
                        let acted = {
                            let mut state = state.lock().await;
                            resources::act(&mut state, &sink, &scope, &resource, action)
                        };
                        match acted {
                            Ok(Some(io)) => resources::perform(&auth_terminal, io, reply),
                            acted => {
                                let _ = reply.send(acted.map(drop));
                            }
                        }
                        continue;
                    }
                    Some(BridgeInput::Control { session_id, control, reply }) => {
                        let mut state = state.lock().await;
                        match controls::admit(&mut state, &sink, &session_id, &control) {
                            Err(error) => {
                                let _ = reply.send(Err(error));
                                continue;
                            }
                            Ok((command, answer)) => {
                                let _ = reply.send(Ok(answer));
                                command
                            }
                        }
                    }
                    Some(BridgeInput::Workspace { session_id, reply }) => {
                        let workspace = {
                            let state = state.lock().await;
                            resources::workspace(&state, filesystem.as_deref(), &session_id)
                        };
                        let _ = reply.send(workspace);
                        continue;
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
                    ingress: Some(ingress.clone()),
                    options: options.clone(),
                    state: state.clone(),
                    sink: sink.clone(),
                    terminals: terminals.clone(),
                                        auth_terminal: auth_terminal.clone(),
                    prompt_lifecycle: prompt_lifecycle.clone(),
                    cancellation: cancellation.clone(),
                    history_flow: Arc::new(HistoryFlowGuard {
                        owner: std::sync::Mutex::new(None),
                        events: observer_events.clone(),
                    }),
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
                    let protocol_operation = command
                        .get(operations::OPERATION_FIELD)
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
                        if let Err(error) = reserve_command_request_id(&mut state, request_id) {
                            if let Some(op) = &protocol_operation {
                                state.sessions.fail_protocol_operation(op, operation_error(&error, false));
                                if operation == "session/delete" {
                                    streams::wake(&state);
                                }
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
                            let result = tokio::select! {
                                _ = materialization_cancel.cancelled() => break Err(Error::request_cancelled()),
                                result = handle_command(
                                    command.clone(), task_connection.clone(), context.clone(), None, execution.take(),
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
                                        if locked.sessions.state(session_id).is_none() {
                                            break Err(Error::request_cancelled().data("materialization owner retired"));
                                        }
                                        RequestOwner {
                                            epoch: locked.sessions.epoch().to_string(), session_id: Some(session_id.to_string()),
                                            operation_id: command["requestId"].as_str().unwrap().to_string(),
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
                            execution.take(),
                        )
                        .await
                    };
                    if let Err(error) = &result {
                        // The Agent connection ending, by a cancel or by the
                        // transport closing, is not the Agent's error.
                        let closing = context.cancellation.is_cancelled()
                            || is_incoming_transport_closed(error);
                        // A write's command settles its own operation only.
                        if let Some(op) = &protocol_operation {
                            let mut state = context.state.lock().await;
                            state.sessions.fail_protocol_operation(op, operation_error(error, closing));
                            refresh_observer_timers(&mut state);
                            if operation == "session/delete" {
                                // The failed delete frees what it occupied:
                                // the streams that waited for it load the
                                // session.
                                streams::wake(&state);
                            }
                        }
                    }
                    if let Some(request_id) = request_id.as_deref() {
                        let mut state = context.state.lock().await;
                        state.in_flight_request_ids.remove(request_id);
                        // A closed session's scope ends with `closed`; a release ends nothing.
                        if let Some(session_id) = state.action_closes.remove(request_id)
                            && result.is_ok()
                        {
                            state.sessions.scopes.ended.push((
                                session_id,
                                crate::protocol::documents::End {
                                    reason: crate::protocol::documents::EndReason::Closed,
                                    error: None,
                                },
                            ));
                        }
                        // A control action is answered with the end of its command.
                        if let Some(reply) = state.action_replies.remove(request_id) {
                            let _ = reply.send(match &result {
                                Ok(_) => Ok(()),
                                Err(error) => Err(operation_error(
                                    error,
                                    context.cancellation.is_cancelled()
                                        || is_incoming_transport_closed(error),
                                )),
                            });
                        }
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
            Ok(())
        })
        .await
}

fn error_message(error: Error) -> String {
    error.data.map_or(error.message, |data| data.to_string())
}

/// Complete materialization and its observation cut in the result owner's
/// local transition, before that session's next delivery is allowed to run.
fn finish_materialization(
    state: &mut BridgeState,
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
        Ok(()) if state.sessions.active_session(session_id).is_none() => {
            Err(SessionViewError::unavailable(
                "session/load completed without materializing the session",
            ))
        }
        Ok(()) => Ok(()),
        Err(error) => Err(SessionViewError::from(error.clone())),
    };
    streams::finish_load(state, session_id, result);
    if state.sessions.state(session_id).is_some_and(|session| {
        session.phase == MirrorPhase::Cold
            && session.live.is_none()
            && session.operation.is_none()
            && session.history.is_none()
    }) {
        state.sessions.remove(session_id);
    }
    refresh_observer_timers(state);
}

fn drain_runtime_effects(state: &mut BridgeState) -> Vec<(String, String)> {
    let effects = state.sessions.take_effects();
    let mut terminal_releases = Vec::new();
    for effect in effects {
        match effect {
            RuntimeEffect::CancelPermissionResponder {
                session_id,
                interaction_id,
            } => {
                let owner = SessionResourceOwner::new(state.sessions.epoch(), &session_id);
                if let Some(pending) = state.sessions.take_permission(&interaction_id, &owner) {
                    let _ = pending.sender.send(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ));
                }
            }
            RuntimeEffect::CancelElicitationResponder {
                session_id,
                interaction_id,
            } => {
                let owner = session_id.map(|session_id| {
                    SessionResourceOwner::new(state.sessions.epoch(), session_id)
                });
                if let Some(pending) =
                    take_pending_elicitation(state, &interaction_id, owner.as_ref())
                {
                    let _ = pending
                        .sender
                        .send(CreateElicitationResponse::new(ElicitationAction::Cancel));
                }
            }
            RuntimeEffect::AbortUrlFlow {
                session_id,
                elicitation_id,
                registration_id,
            } => {
                let owner = session_id.map(|session_id| {
                    SessionResourceOwner::new(state.sessions.epoch(), session_id)
                });
                let registration = UrlRegistration {
                    owner,
                    registration_id,
                };
                state
                    .sessions
                    .take_url_route(&elicitation_id, &registration);
            }
            RuntimeEffect::ReleaseTerminal {
                session_id,
                terminal_id,
            } => terminal_releases.push((session_id, terminal_id)),
        }
    }
    terminal_releases
}

#[cfg(test)]
async fn apply_runtime_effects(state: &Arc<BridgeLock>, terminals: Option<&TerminalManager>) {
    let terminal_releases = {
        let mut locked = state.lock().await;
        drain_runtime_effects(&mut locked)
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
    state: &Arc<BridgeLock>,
    sink: &EventSink,
    _terminals: Option<&TerminalManager>,
) {
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
    // The notification's own `_meta`, kept beside the update (memory model §3.5).
    let envelope = notification
        .meta
        .as_ref()
        .map(|meta| Value::Object(meta.clone()));
    let tool_update = matches!(
        update.get("sessionUpdate").and_then(Value::as_str),
        Some("tool_call" | "tool_call_update")
    );
    {
        let mut state = state.lock().await;
        let staged = if live_session(&state, &session_id) {
            if let Err(error) = ensure_session_validation(&mut state, &session_id) {
                sink.acp_error(error, None, Some("session/update"));
                return;
            }
            false
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
            state
                .creation_staging
                .entry(session_id.clone())
                .or_default();
            true
        } else {
            // Late updates for closed or failed sessions must not leak into the visible thread.
            return;
        };
        let active = attached_session(&state, &session_id);
        let attachment =
            session_operation_pending(&state, &session_id, SessionAdmission::Attachment);
        let early_creation = !active && !attachment && state.pending_creations > 0;
        if !active && !attachment && !early_creation {
            return;
        }
        let mirror_phase = active
            .then(|| {
                state
                    .sessions
                    .state(&session_id)
                    .map(|session| session.phase)
            })
            .flatten();
        let mirror_replay_attempt = active
            .then(|| state.sessions.load_attempt(&session_id).map(str::to_string))
            .flatten();
        let reference_error = if tool_update && !attachment && mirror_replay_attempt.is_none() {
            validate_terminal_references(&state, &session_id, &update).err()
        } else {
            None
        };
        let validation = update_validation_mut(&mut state, &session_id, staged)
            .expect("update validation was selected");
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
            if state.sessions.state(&session_id).is_none() {
                Err(runtime_state_error(
                    "missing canonical attachment transaction",
                ))
            } else if is_conversation_update(&update) {
                state
                    .sessions
                    .load_attempt(&session_id)
                    .map(str::to_string)
                    .ok_or_else(|| runtime_state_error("attachment has no load attempt"))
                    .and_then(|attempt| {
                        state
                            .sessions
                            .append_load_update_with_meta(
                                &session_id,
                                &attempt,
                                update.clone(),
                                envelope.as_ref(),
                            )
                            .map_err(mirror_error)
                    })
            } else {
                let epoch = state.sessions.epoch().to_string();
                state
                    .sessions
                    .append_attachment_candidate(
                        &epoch,
                        &session_id,
                        with_notification_meta(&update, envelope.as_ref()),
                    )
                    .map_err(runtime_state_error)
            }
        } else if active
            && !attachment
            && matches!(
                mirror_phase,
                Some(MirrorPhase::Loading | MirrorPhase::Reconciling)
            )
            && mirror_replay_attempt.is_some()
        {
            let mirrored = if is_conversation_update(&update) {
                session_mirror(&mut state)
                    .append_load_update_with_meta(
                        &session_id,
                        mirror_replay_attempt
                            .as_deref()
                            .expect("reconciliation has a load attempt"),
                        update.clone(),
                        envelope.as_ref(),
                    )
                    .map_err(mirror_error)
            } else {
                let bytes = serialized_value_len(&update);
                let candidate = &mut session_ingest_mut(&mut state, &session_id)
                    .expect("replay has an owner")
                    .attachment
                    .control_candidate;
                candidate.bytes = candidate.bytes.saturating_add(bytes);
                candidate.updates.push(with_notification_meta(&update, envelope.as_ref()));
                Ok(())
            };
            mirrored
        } else if early_creation {
            let notification_bytes = serde_json::to_vec(&notification)
                .expect("ACP notification serializes")
                .len();
            let staging = state
                .creation_staging
                .get_mut(&session_id)
                .expect("creation staging was selected");
            staging.early_notifications.push(notification.clone());
            staging.bytes += notification_bytes;
            state.early_update_count += 1;
            state.early_update_bytes += notification_bytes;
            Ok(())
        } else {
            if active {
                let epoch = state.sessions.epoch().to_string();
                if is_conversation_update(&update) {
                    if mirror_phase == Some(MirrorPhase::Running)
                        && let Some(operation_id) = session_mirror(&mut state)
                            .active_operation_id(&session_id)
                            .map(str::to_string)
                    {
                        session_mirror(&mut state)
                            .append_turn_update_with_meta(
                                &session_id,
                                &operation_id,
                                update.clone(),
                                envelope.as_ref(),
                            )
                            .map_err(mirror_error)
                    } else if matches!(
                        mirror_phase,
                        Some(MirrorPhase::Ready | MirrorPhase::Reconciling)
                    ) {
                        session_mirror(&mut state)
                            .append_session_update_with_meta(
                                &session_id,
                                update.clone(),
                                envelope.as_ref(),
                            )
                            .map_err(mirror_error)
                    } else {
                        Ok(())
                    }
                } else {
                    state
                        .sessions
                        .update_control_state(
                            &epoch,
                            &session_id,
                            with_notification_meta(&update, envelope.as_ref()),
                        )
                        .map_err(runtime_state_error)
                }
            } else {
                Ok(())
            }
        };
        if let Err(error) = &result
            && (attachment || mirror_replay_attempt.is_some())
            && let Some(validation) = update_validation_mut(&mut state, &session_id, staged)
        {
            validation.invalid_reason.get_or_insert_with(|| {
                error
                    .data
                    .as_ref()
                    .map_or_else(|| error.message.clone(), Value::to_string)
            });
        }
        if result.is_err()
            && let Some(attempt_id) = state.sessions.load_attempt(&session_id).map(str::to_string)
        {
            let _ = session_mirror(&mut state).invalidate_load(&session_id, &attempt_id);
        }
        refresh_observer_timers(&mut state);
        // A failed replay attempt is reported through its load outcome.
        if let Err(error) = result
            && mirror_replay_attempt.is_none()
        {
            sink.acp_error(error, None, Some("session/update"));
        }
    }
}

async fn cancel_prompts_on_shutdown(
    connection: &ConnectionTo<Agent>,
    state: &Arc<BridgeLock>,
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
            .map(|session| session.session_id.clone())
            .collect::<Vec<_>>()
    };
    if session_ids.is_empty() {
        return;
    }

    for session_id in &session_ids {
        // Incoming EOF already failed pending RPCs. Writing a cancellation to
        // an exited Agent can break the transport before their results drain.
        if !connection.is_incoming_closed() {
            let _ = connection.send_notification(CancelNotification::new(session_id.clone()));
        }
        cancel_interactions(session_id, state, sink).await;
    }

    // ACP cancellation completes through the original session/prompt response.
    // Keep the connection alive briefly so the notification reaches the Agent
    // and that response can be consumed before the transport is released.
    let wait_for_completion = async {
        loop {
            let changed = lifecycle.changed.notified();
            let all_finished = {
                let state = state.lock().await;
                session_ids.iter().all(|session_id| {
                    state.sessions.state(session_id).is_none()
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

fn reserve_command_request_id(state: &mut BridgeState, request_id: &str) -> Result<(), Error> {
    if state.in_flight_request_ids.insert(request_id.to_string()) {
        return Ok(());
    }
    Err(Error::invalid_request().data("requestId is already in flight on this bridge"))
}

fn rpc_owner(
    epoch: &str,
    session_id: Option<&str>,
    operation_id: &str,
    attempt_id: Option<&str>,
) -> RequestOwner {
    RequestOwner {
        epoch: epoch.to_string(),
        session_id: session_id.map(ToOwned::to_owned),
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
    state: &Arc<BridgeLock>,
    owner: RequestOwner,
    request: Req,
) -> Result<Result<Req::Response, Error>, Error> {
    let session_id = owner
        .session_id
        .as_deref()
        .ok_or_else(|| runtime_state_error("load has no session"))?;
    let attempt_id = owner
        .attempt_id
        .as_deref()
        .ok_or_else(|| runtime_state_error("load has no attempt"))?;
    let replay = state
        .lock()
        .await
        .sessions
        .load_candidate(session_id, attempt_id)
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
            .load_candidate(session_id, attempt_id)
            .map_err(mirror_error)?;
        let (validation, controls) = replay.take_controls();
        let current = session_validation_mut(&mut state, session_id)
            .ok_or_else(|| runtime_state_error("load lost its validation state"))?;
        *current = validation;
        if session_operation_pending(&state, session_id, SessionAdmission::Attachment) {
            let epoch = state.sessions.epoch().to_owned();
            for update in controls {
                state
                    .sessions
                    .append_attachment_candidate(&epoch, session_id, update)
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

    fn committed(&mut self) -> Result<(), Error> {
        if let (Some(ingress), Some(target_id)) = (&self.ingress, self.target_id.take()) {
            ingress.finish_creation(self.owner.clone(), target_id, true)?;
        }
        Ok(())
    }
}

impl Drop for CreationFinish {
    fn drop(&mut self) {
        if let (Some(ingress), Some(target_id)) = (&self.ingress, self.target_id.take()) {
            let _ = ingress.finish_creation(self.owner.clone(), target_id, false);
        }
    }
}

async fn handle_command(
    command: Value,
    connection: ConnectionTo<Agent>,
    context: CommandContext,
    prompt_start: Option<PromptStartGuard>,
    mut execution: Option<ExecutionTurn>,
) -> Result<(), Error> {
    let result = handle_command_inner(
        command.clone(),
        connection,
        context.clone(),
        prompt_start,
        &mut execution,
    )
    .await;
    // A registered history flow ends inside the same boundary as its business
    // outcome: the response publishes first, then the flow releases and the
    // idle interval restarts — even when the ordered epilogue was skipped.
    if let Some(flow) = context.history_flow.take() {
        let mut state = context.state.lock().await;
        state.sessions.finish_history_sync(&flow);
        refresh_observer_timers(&mut state);
        drop(state);
    }
    // Result publication, interaction retirement and the Observe materialization
    // cut are part of this local turn, including every early error path above.
    let terminal_releases = if context.ingress.is_none() || execution.is_some() {
        let mut state = context.state.lock().await;
        refresh_observer_timers(&mut state);
        let releases = drain_runtime_effects(&mut state);
        let final_attempt = command
            .get("bridgeMaterializationFinalAttempt")
            .and_then(Value::as_bool)
            .unwrap_or(false);
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
            finish_materialization(&mut state, session_id, materialization_id, &result);
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
    execution: &mut Option<ExecutionTurn>,
) -> Result<(), Error> {
    if context.ingress.is_some() && execution.is_none() {
        return Err(Error::internal_error().data("ordered command has no execution ticket"));
    }
    let CommandContext {
        auto_close,
        options,
        ingress,
        state,
        sink,
        terminals,
        auth_terminal,
        prompt_lifecycle,
        cancellation,
        history_flow,
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
            {
                let mut state = state.lock().await;
                state.protocol.agent.authenticated_method_id = Some(method_id.to_string());
                state.sessions.complete_scope_operation(
                    &OpScope::Global,
                    OpKind::Authenticate,
                    wire::value(&wire::Empty {}),
                );
            }
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
            {
                let mut state = state.lock().await;
                state.protocol.agent.authenticated_method_id = None;
                state.sessions.complete_scope_operation(
                    &OpScope::Global,
                    OpKind::Logout,
                    wire::value(&wire::Empty {}),
                );
            }
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
                    if let Err(error) = runtime_result {
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
                    promote_creation_validation(&mut state, &session_id)?;
                    if state.pending_creations == 0 {
                        clear_pending_creation_replays(&mut state);
                    }

                    session_mirror(&mut state).register_new(session_id.clone());
                    // The Agent's messages before the response are the
                    // session's first history (#8).
                    session_mirror(&mut state).replace_session_timeline(
                        &session_id,
                        timeline_from_notifications(&early_updates),
                    );
                    state.sessions.complete_scope_operation(
                        &OpScope::Global,
                        OpKind::Create,
                        wire::value(&wire::SessionCreated {
                            session_id: session_id.clone(),
                        }),
                    );
                    advance_catalog_revision(&mut state);
                    refresh_observer_timers(&mut state);
                    creation_finish.committed()?;
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
            let result = wire::value(&wire::CatalogPage {
                revision: state.catalog_revision.to_string(),
                sessions: response
                    .sessions
                    .iter()
                    .map(serde_json::to_value)
                    .collect::<Result<_, _>>()?,
                next_cursor: response.next_cursor.clone(),
                meta: response.meta.clone().map(Value::Object),
            });
            state
                .sessions
                .complete_scope_operation(&OpScope::Catalog, OpKind::List, result);
            drop(state);
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
            let reservation = reserve_attachment_with_cwd(
                &session_id,
                attachment_kind,
                requested_cwd,
                &request_id,
                command
                    .get("bridgeMaterializationId")
                    .and_then(Value::as_str),
                &state,
            )
            .await?;
            let cwd = reservation.cwd().clone();
            let reload = reservation.is_reload();
            {
                let mut state = state.lock().await;
                {
                    let mirror = session_mirror(&mut state);
                    if let Err(error) = mirror.begin_load(&session_id, &request_id) {
                        let error = mirror_error(error);
                        fail_runtime_attachment(
                            &mut state,
                            &session_id,
                            &request_id,
                            attachment_kind,
                            reload,
                            &error,
                        )?;
                        if !reload {
                            session_mirror(&mut state).remove(&session_id);
                        }
                        clear_attachment_tracking(&mut state, &session_id, &request_id);
                        return Err(error);
                    }
                }
                refresh_observer_timers(&mut state);
            }
            let owner = rpc_owner(&epoch, Some(&session_id), &request_id, Some(&request_id));
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
            if !replay_attempt_current(&state, &session_id, &request_id) {
                return Err(Error::invalid_request().data("session attachment owner changed"));
            }
            let response = match result {
                Ok(response) => response,
                Err(error) => {
                    settle_runtime_attachment_request_error(
                        &mut state,
                        &session_id,
                        &request_id,
                        attachment_kind,
                        reload,
                        &error,
                    )?;
                    {
                        fail_mirror_attachment(
                            &mut state,
                            &session_id,
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
                            session_mirror(&mut state).remove(&session_id);
                            state.listed_sessions.remove(&session_id);
                        }
                    }
                    clear_attachment_tracking(&mut state, &session_id, &request_id);
                    return Err(error);
                }
            };
            if let Err(error) = relay_bytes(&response) {
                fail_runtime_attachment(
                    &mut state,
                    &session_id,
                    &request_id,
                    attachment_kind,
                    reload,
                    &error,
                )?;
                {
                    fail_mirror_attachment(
                        &mut state,
                        &session_id,
                        &request_id,
                        reload,
                        bridge_managed_materialization,
                        false,
                        &error.message,
                    );
                }
                clear_attachment_tracking(&mut state, &session_id, &request_id);
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
                    state.sessions.active_session(&session_id).ok_or_else(|| {
                        runtime_state_error("same-session reload lost its active session")
                    })?;
                    Ok(())
                });
                result
            } else {
                prepare_session_response(
                    &mut state,
                    &session_id,
                    cwd.clone(),
                    &response,
                    Some(request_id.as_str()),
                )
                .map(|_| ())
            };
            match tracked {
                Ok(()) => {}
                Err(error) => {
                    fail_runtime_attachment(
                        &mut state,
                        &session_id,
                        &request_id,
                        attachment_kind,
                        reload,
                        &error,
                    )?;
                    {
                        fail_mirror_attachment(
                            &mut state,
                            &session_id,
                            &request_id,
                            reload,
                            bridge_managed_materialization,
                            false,
                            &error.message,
                        );
                    }
                    clear_attachment_tracking(&mut state, &session_id, &request_id);
                    return Err(error);
                }
            };
            let epoch = state.sessions.epoch().to_string();
            if operation == "session/load" {
                session_mirror(&mut state)
                    .commit_load(&session_id, &request_id)
                    .map_err(mirror_error)?;
            } else {
                session_mirror(&mut state)
                    .commit_attachment_cache(&session_id, &request_id)
                    .map_err(mirror_error)?;
            }
            let completed = if reload {
                state
                    .sessions
                    .complete_reload(&epoch, &session_id, &request_id, response.clone())
            } else {
                state.sessions.complete_attachment(
                    &epoch,
                    &session_id,
                    &request_id,
                    attachment_kind,
                    response.clone(),
                )
            };
            if let Err(error) = completed {
                clear_attachment_tracking(&mut state, &session_id, &request_id);
                return Err(runtime_state_error(error));
            }
            commit_replay_validation(&mut state, &session_id, &request_id);
            // The optional sync is registered before the attachment operation
            // releases: retirement must see continuous work across the handoff.
            let history_owner = if operation == "session/resume" {
                let owner = HistorySyncOwner {
                    session: SessionResourceOwner::new(epoch.clone(), session_id.clone()),
                    flow_id: Uuid::new_v4().to_string(),
                    cancellation: CancellationToken::new(),
                };
                state
                    .sessions
                    .begin_history_sync(&owner)
                    .map_err(mirror_error)?;
                history_flow.register(owner.clone());
                Some(owner)
            } else {
                None
            };
            release_session_operation(
                &mut state,
                &session_id,
                SessionAdmission::Attachment,
                &request_id,
            );
            refresh_observer_timers(&mut state);
            clear_attachment_delivery(&mut state, &session_id);
            drop(state);
            if operation == "session/resume" {
                synchronize_attached_history(
                    &connection,
                    &options,
                    &bridge_state,
                    &cancellation,
                    &session_id,
                    AttachmentHistoryFallback::Received,
                    history_owner
                        .as_ref()
                        .expect("resume registers its history flow"),
                    ingress.as_ref(),
                    execution,
                    &request_id,
                )
                .await?;
            }
        }
        "session/fork" => {
            let request_id = string_field(&command, "requestId")?.to_string();
            let source_id = string_field(&command, "sessionId")?.to_string();
            require_agent_method("session/fork", &state).await?;
            let cwd = {
                let mut state = state.lock().await;
                let source = state.sessions.active_session(&source_id).ok_or_else(|| {
                    Error::invalid_params()
                        .data(format!("unknown or inactive session: {source_id}"))
                })?;
                // A fork's explicit `cwd` overrides the source's (protocol §6.2).
                let cwd = command
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or_else(|| source.live.cwd.clone());
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
                    request_id.clone(),
                    RuntimeSessionOperationKind::Fork,
                    "forking",
                ) {
                    release_session_operation(
                        &mut state,
                        &source_id,
                        SessionAdmission::Fork,
                        &request_id,
                    );
                    return Err(runtime_state_error(error));
                }
                // The accepted fork owns the source's projection for its read;
                // a parked optional history flow yields instead of replaying
                // over the branched session later.
                state.sessions.supersede_history_sync(&source_id);
                refresh_observer_timers(&mut state);
                state.pending_creations += 1;
                cwd
            };
            let owner = rpc_owner(&epoch, Some(&source_id), &request_id, None);
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
                        &request_id,
                        RuntimeSessionOperationKind::Fork,
                        &error,
                    )?;
                    release_session_operation(
                        &mut state,
                        &source_id,
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
            let target_replay = Some(notification_values(&early_updates));
            let epoch = state.sessions.epoch().to_string();
            let runtime_result = state.sessions.open_forked(
                &epoch,
                session_id.clone(),
                cwd.clone(),
                response_value,
                &source_id,
                target_replay,
            );
            if let Err(error) = runtime_result {
                discard_creation_staging(&mut state, &session_id);
                state
                    .sessions
                    .fail_operation(
                        &epoch,
                        &source_id,
                        &request_id,
                        RuntimeSessionOperationKind::Fork,
                        json!({ "message": format!("{error:?}") }),
                    )
                    .map_err(runtime_state_error)?;
                release_session_operation(
                    &mut state,
                    &source_id,
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
            promote_creation_validation(&mut state, &session_id)?;
            if state.pending_creations == 0 {
                clear_pending_creation_replays(&mut state);
            }

            session_mirror(&mut state).register_cold(session_id.clone());
            // The target never inherits the source's entries: tool, plan and
            // compaction IDs are unique only within their own session, and the
            // Agent promises no transcript. It holds what the Agent sent for it
            // (partial); a load of the target, when the Agent supports one,
            // replaces that with the replay (bridge memory model §6.3).
            session_mirror(&mut state).adopt_session_timeline(
                &session_id,
                timeline_from_notifications(&early_updates),
            );
            // The response claim is about to release subsequent target messages.
            // Give the target its log before making it routable.
            session_mirror(&mut state)
                .use_cached_history(&session_id)
                .map_err(mirror_error)?;
            state
                .sessions
                .complete_operation(
                    &epoch,
                    &source_id,
                    &request_id,
                    RuntimeSessionOperationKind::Fork,
                    serde_json::to_value(&response)?,
                )
                .map_err(runtime_state_error)?;
            release_session_operation(&mut state, &source_id, SessionAdmission::Fork, &request_id);
            // The optional-history workflow is live from the handoff onward:
            // registering before publication and the runtime flush keeps the
            // unobserved target working through every retry wait.
            let history_owner = HistorySyncOwner {
                session: SessionResourceOwner::new(epoch.clone(), session_id.clone()),
                flow_id: Uuid::new_v4().to_string(),
                cancellation: CancellationToken::new(),
            };
            state
                .sessions
                .begin_history_sync(&history_owner)
                .map_err(mirror_error)?;
            history_flow.register(history_owner.clone());
            advance_catalog_revision(&mut state);
            refresh_observer_timers(&mut state);
            creation_finish.committed()?;
            drop(state);
            let target_owner = rpc_owner(&epoch, Some(&session_id), &request_id, None);
            continue_execution(ingress.as_ref(), execution, &target_owner).await?;
            synchronize_attached_history(
                &connection,
                &options,
                &bridge_state,
                &cancellation,
                &session_id,
                AttachmentHistoryFallback::Installed,
                &history_owner,
                ingress.as_ref(),
                execution,
                &request_id,
            )
            .await?;
        }
        "session/close" => {
            let request_id = string_field(&command, "requestId")?.to_string();
            let session_id = string_field(&command, "sessionId")?.to_string();
            require_agent_method("session/close", &state).await?;
            {
                let mut state = state.lock().await;
                if session_blocks_unobserved_retire(&state, &session_id)
                    && let Some(permit) = &auto_close
                {
                    // Live work superseded this attempt; a fresh interval
                    // starts when the session settles back to idle.
                    permit.cancel();
                    return Ok(());
                }
                // Retiring a replaced session cancelled its absence permit.
                if let Some(permit) = &auto_close
                    && (!permit.pending() || state.sessions.active_session(&session_id).is_none())
                {
                    permit.cancel();
                    return Ok(());
                }
                reserve_session_operation(
                    &mut state,
                    &session_id,
                    RuntimeSessionOperationKind::Close,
                    &request_id,
                )?;

                let epoch = state.sessions.epoch().to_string();
                if let Err(error) = state.sessions.start_operation(
                    &epoch,
                    &session_id,
                    request_id.clone(),
                    RuntimeSessionOperationKind::Close,
                    "closing",
                ) {
                    release_session_operation(
                        &mut state,
                        &session_id,
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
                                &request_id,
                                RuntimeSessionOperationKind::Close,
                                json!({"message":"observer returned"}),
                            )
                            .map_err(runtime_state_error)?;
                        release_session_operation(
                            &mut state,
                            &session_id,
                            SessionAdmission::Close,
                            &request_id,
                        );
                        return Ok(());
                    }
                    // A refused or failed automatic close stays settled: its own
                    // Running-to-Idle transition must not rearm the countdown.
                    if let Ok(resources) = state.sessions.resources_mut(&session_id) {
                        resources.observers.mark_close_attempted();
                    }
                }
                refresh_observer_timers(&mut state);
            }
            let owner = rpc_owner(&epoch, Some(&session_id), &request_id, None);
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
                    &request_id,
                    RuntimeSessionOperationKind::Close,
                    &error,
                )?;
                release_session_operation(
                    &mut state,
                    &session_id,
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
                    .close_session(&epoch, &session_id, &request_id)
                    .map_err(runtime_state_error)?;
                clear_ingest_resources(&mut state, &session_id);
                cancel_interactions_locked(&session_id, &mut state, &sink);
                refresh_observer_timers(&mut state);
                drain_runtime_effects(&mut state)
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
                    &session_id,
                    SessionAdmission::Close,
                    &request_id,
                );
            }
        }
        // Local retirement for Agents without session/close: the Agent keeps
        // whatever it retains; we drop our copy and the session may still be
        // listed or re-materialized on the next open.
        "bridge/retire_session" => {
            let request_id = string_field(&command, "requestId")?.to_string();
            let session_id = string_field(&command, "sessionId")?.to_string();
            {
                let mut state = state.lock().await;
                if session_blocks_unobserved_retire(&state, &session_id)
                    && let Some(permit) = &auto_close
                {
                    permit.cancel();
                    return Ok(());
                }
                // Retiring a replaced session cancelled its absence permit.
                if let Some(permit) = &auto_close
                    && (!permit.pending() || state.sessions.active_session(&session_id).is_none())
                {
                    permit.cancel();
                    return Ok(());
                }
                reserve_session_operation(
                    &mut state,
                    &session_id,
                    RuntimeSessionOperationKind::Close,
                    &request_id,
                )?;

                let epoch = state.sessions.epoch().to_string();
                if let Err(error) = state.sessions.start_operation(
                    &epoch,
                    &session_id,
                    request_id.clone(),
                    RuntimeSessionOperationKind::Close,
                    "closing",
                ) {
                    release_session_operation(
                        &mut state,
                        &session_id,
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
                                &request_id,
                                RuntimeSessionOperationKind::Close,
                                json!({"message":"observer returned"}),
                            )
                            .map_err(runtime_state_error)?;
                        release_session_operation(
                            &mut state,
                            &session_id,
                            SessionAdmission::Close,
                            &request_id,
                        );
                        return Ok(());
                    }
                    if let Ok(resources) = state.sessions.resources_mut(&session_id) {
                        resources.observers.mark_close_attempted();
                    }
                }
                refresh_observer_timers(&mut state);
            }
            let terminal_releases = {
                let mut state = state.lock().await;

                let epoch = state.sessions.epoch().to_string();
                state
                    .sessions
                    .close_session(&epoch, &session_id, &request_id)
                    .map_err(runtime_state_error)?;
                clear_ingest_resources(&mut state, &session_id);
                cancel_interactions_locked(&session_id, &mut state, &sink);
                refresh_observer_timers(&mut state);
                drain_runtime_effects(&mut state)
            };
            drop(execution.take());
            release_runtime_terminals(terminals.as_ref(), terminal_releases).await;
            if let Some(terminals) = &terminals {
                terminals.release_session(&session_id).await;
            }
            let owner = rpc_owner(&epoch, Some(&session_id), &request_id, None);
            continue_execution(ingress.as_ref(), execution, &owner).await?;
            {
                let mut state = state.lock().await;
                complete_session_retirement(
                    &mut state,
                    &session_id,
                    SessionAdmission::Close,
                    &request_id,
                );
            }
        }
        "session/delete" => {
            let request_id = string_field(&command, "requestId")?.to_string();
            let session_id = string_field(&command, "sessionId")?.to_string();
            require_agent_method("session/delete", &state).await?;
            let deletion = reserve_deletion(&mut *state.lock().await, &session_id, &request_id)?;
            let DeletionReservation::Session = deletion else {
                // An unopened history row is a catalog operation. Keep only the
                // ID reservation; its RPC and completion use the global lane.
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
                locked.sessions.complete_catalog_delete(&session_id);
                streams::end_waiting(
                    &locked,
                    &session_id,
                    crate::protocol::documents::EndReason::Deleted,
                    None,
                );
                advance_catalog_revision(&mut locked);
                return Ok(());
            };
            let (close_before_delete, runtime_delete) = {
                let mut state = state.lock().await;
                let close_before_delete = state.sessions.active_session(&session_id).is_some()
                    && state
                        .agent_capabilities
                        .as_ref()
                        .is_some_and(|capabilities| {
                            capabilities.session_capabilities.close.is_some()
                        });
                let runtime_delete = match begin_runtime_delete(
                    &mut state,
                    &session_id,
                    &request_id,
                    close_before_delete,
                ) {
                    Ok(runtime_delete) => runtime_delete,
                    Err(error) => {
                        release_deletion(&mut state, &session_id, deletion, &request_id);
                        return Err(error);
                    }
                };
                if runtime_delete {
                    refresh_observer_timers(&mut state);
                }
                (close_before_delete, runtime_delete)
            };
            let owner = rpc_owner(&epoch, Some(&session_id), &request_id, None);
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
                    if runtime_delete {
                        settle_runtime_operation_request_error(
                            &mut state,
                            &session_id,
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
                    if runtime_delete {
                        commit_runtime_delete_close_success(&mut state, &session_id, &request_id)?;
                    }

                    clear_ingest_resources(&mut state, &session_id);
                }
                let terminal_releases = {
                    let mut state = state.lock().await;
                    cancel_interactions_locked(&session_id, &mut state, &sink);
                    refresh_observer_timers(&mut state);
                    drain_runtime_effects(&mut state)
                };
                drop(execution.take());
                release_runtime_terminals(terminals.as_ref(), terminal_releases).await;
                if let Some(terminals) = &terminals {
                    terminals.release_session(&session_id).await;
                }
                continue_execution(ingress.as_ref(), execution, &owner).await?;
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
                if runtime_delete {
                    settle_runtime_operation_request_error(
                        &mut state,
                        &session_id,
                        &request_id,
                        RuntimeSessionOperationKind::Delete,
                        &error,
                    )?;
                }
                release_deletion(&mut state, &session_id, deletion, &request_id);
                return Err(error);
            }
            if runtime_delete {
                let mut state = state.lock().await;
                let epoch = state.sessions.epoch().to_string();
                state
                    .sessions
                    .complete_delete(&epoch, &session_id, &request_id)
                    .map_err(runtime_state_error)?;
                take_sync_control_candidate(&mut state, &session_id);
            }
            if !close_before_delete {
                {
                    let mut state = state.lock().await;

                    clear_ingest_resources(&mut state, &session_id);
                }
                let terminal_releases = {
                    let mut state = state.lock().await;
                    cancel_interactions_locked(&session_id, &mut state, &sink);
                    refresh_observer_timers(&mut state);
                    drain_runtime_effects(&mut state)
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
                    &session_id,
                    SessionAdmission::Delete,
                    &request_id,
                );
                state.listed_sessions.remove(&session_id);
                advance_catalog_revision(&mut state);
            }
        }
        "session/prompt" => {
            let request_id = string_field(&command, "requestId")?.to_string();
            let runtime_operation_id = request_id.clone();
            let client_intent_id = string_field(&command, "clientIntentId")?.to_string();
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
            let (mirror_operation_id, cancelled) = {
                let mut state = state.lock().await;
                if state.sessions.active_session(&session_id).is_none() {
                    return Err(Error::invalid_params()
                        .data(format!("unknown or inactive session: {session_id}")));
                }
                let epoch = state.sessions.epoch().to_string();
                let mirror_operation_id = state
                    .sessions
                    .admit_session_turn(&epoch, &session_id, &client_intent_id, runtime_prompt)
                    .map_err(|error| match error {
                        SessionTurnError::History(error) => mirror_error(error),
                        SessionTurnError::Live(error) => runtime_state_error(error),
                    })?;
                // The accepted turn owns the projection now: an optional
                // history workflow still parked in backoff yields instead of
                // retrying over this turn's outcome.
                state.sessions.supersede_history_sync(&session_id);
                // A v2 prompt cancelled before its turn started is cancelled
                // as soon as it is sent.
                let cancelled = state.sessions.operation_cancel_requested(
                    &OpScope::Session(session_id.clone()),
                    &client_intent_id,
                );
                if cancelled {
                    commit_session_cancel(&mut state, &sink, &session_id)?;
                }
                refresh_observer_timers(&mut state);
                (mirror_operation_id, cancelled)
            };
            let owner = rpc_owner(&epoch, Some(&session_id), &request_id, None);
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
                    if cancelled {
                        let _ = connection
                            .send_notification(CancelNotification::new(session_id.clone()));
                    }
                },
            )
            .await?;
            let completion = {
                let mut state = state.lock().await;
                let completion = commit_completed_turn_history(
                    &mut state,
                    PromptCompletion {
                        session_id: &session_id,
                        runtime_operation_id: &runtime_operation_id,
                        operation_id: &mirror_operation_id,
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
        "session/set_mode" => {
            let request_id = string_field(&command, "requestId")?.to_string();
            let session_id = string_field(&command, "sessionId")?.to_string();
            let mode_id = string_field(&command, "modeId")?.to_string();
            {
                let mut state = state.lock().await;
                let session = state.sessions.active_session(&session_id).ok_or_else(|| {
                    Error::invalid_params()
                        .data(format!("unknown or inactive session: {session_id}"))
                })?;
                let modes = serde_json::to_value(&session.live.controls.modes)?;
                validate_session_mode_reference((!modes.is_null()).then_some(&modes), &mode_id)
                    .map_err(semantic_error)?;
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
                    request_id.clone(),
                    RuntimeSessionOperationKind::SetMode,
                    "setting_mode",
                ) {
                    release_session_operation(
                        &mut state,
                        &session_id,
                        SessionAdmission::Control,
                        &request_id,
                    );
                    return Err(runtime_state_error(error));
                }
                // The admitted control owns the projection now; an optional
                // history flow parked in backoff yields instead of replaying
                // over this control's outcome.
                state.sessions.supersede_history_sync(&session_id);
                refresh_observer_timers(&mut state);
            }
            let result = send_ordered(
                &connection,
                ingress.as_ref(),
                execution,
                rpc_owner(&epoch, Some(&session_id), &request_id, None),
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
                        &request_id,
                        RuntimeSessionOperationKind::SetMode,
                        &error,
                    )?;
                    release_session_operation(
                        &mut state,
                        &session_id,
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
                        &request_id,
                        RuntimeSessionOperationKind::SetMode,
                        json!({ "message": error.message.clone(), "data": error.data.clone() }),
                    )
                    .map_err(runtime_state_error)?;
                release_session_operation(
                    &mut state,
                    &session_id,
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
                        &request_id,
                        RuntimeSessionOperationKind::SetMode,
                        json!({
                            "sessionUpdate": "current_mode_update",
                            "currentModeId": mode_id,
                        }),
                    )
                    .map_err(runtime_state_error)?;
                release_session_operation(
                    &mut state,
                    &session_id,
                    SessionAdmission::Control,
                    &request_id,
                );
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
            {
                let mut state = state.lock().await;
                let session = state.sessions.active_session(&session_id).ok_or_else(|| {
                    Error::invalid_params()
                        .data(format!("unknown or inactive session: {session_id}"))
                })?;
                validate_session_config_reference(
                    &Value::from(session.live.controls.config_options.clone()),
                    &config_id,
                    &value,
                )
                .map_err(semantic_error)?;
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
                    request_id.clone(),
                    RuntimeSessionOperationKind::SetConfig,
                    "setting_config",
                ) {
                    release_session_operation(
                        &mut state,
                        &session_id,
                        SessionAdmission::Control,
                        &request_id,
                    );
                    return Err(runtime_state_error(error));
                }
                state.sessions.supersede_history_sync(&session_id);
                refresh_observer_timers(&mut state);
            }
            let result = send_ordered(
                &connection,
                ingress.as_ref(),
                execution,
                rpc_owner(&epoch, Some(&session_id), &request_id, None),
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
                        &request_id,
                        RuntimeSessionOperationKind::SetConfig,
                        &error,
                    )?;
                    release_session_operation(
                        &mut state,
                        &session_id,
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
                        &request_id,
                        RuntimeSessionOperationKind::SetConfig,
                        json!({ "message": error.message.clone(), "data": error.data.clone() }),
                    )
                    .map_err(runtime_state_error)?;
                release_session_operation(
                    &mut state,
                    &session_id,
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
                        &request_id,
                        RuntimeSessionOperationKind::SetConfig,
                        json!({ "message": error.message.clone(), "data": error.data.clone() }),
                    )
                    .map_err(runtime_state_error)?;
                release_session_operation(
                    &mut state,
                    &session_id,
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
                        &request_id,
                        RuntimeSessionOperationKind::SetConfig,
                        json!({
                            "sessionUpdate": "config_option_update",
                            "configOptions": config_options,
                        }),
                    )
                    .map_err(runtime_state_error)?;
                release_session_operation(
                    &mut state,
                    &session_id,
                    SessionAdmission::Control,
                    &request_id,
                );
            }
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
            // The running `terminal_login` owns the terminal, by its ID, from
            // before the process can print (protocol §4.6).
            // It starts under the guard a cancel takes, so a cancel either
            // finds the process to stop or keeps it from starting.
            let mut locked = state.lock().await;
            let global = &locked.sessions.scopes.global;
            let login = global
                .is_running_kind(OpKind::TerminalLogin)
                .then(|| global.id.clone());
            if let Some(op) = &login {
                if global.cancel_requested {
                    locked.sessions.login_exited(
                        op,
                        crate::protocol::registry::LoginExit {
                            exit_code: None,
                            signal: None,
                            cancelled: true,
                            failure: None,
                        },
                    );
                    return Ok(());
                }
                locked.sessions.start_login_terminal(op, method_id);
            }
            let observer = login
                .clone()
                .map(|op| publication::login_observer(Arc::downgrade(&state), op));
            if let Err(error) = auth_terminal.start_observed(
                request_id,
                method,
                &options.command,
                &options.cwd,
                cols,
                rows,
                observer,
            ) {
                if let Some(op) = &login {
                    locked.sessions.discard_login_terminal(op);
                }
                return Err(error);
            }
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
    runtime_operation_id: &'a str,
    operation_id: &'a str,
    result: Result<PromptResponse, Error>,
}

struct PromptCommit {
    result: Result<(), Error>,
    terminal_releases: Vec<(String, String)>,
}

/// Seal the exact RPC owner and commit its history and terminal releases
/// without releasing admission between Ready and the old operation's outcome.
fn commit_completed_turn_history(
    state: &mut BridgeState,
    completion: PromptCompletion<'_>,
) -> PromptCommit {
    let mut terminal_releases = Vec::new();
    let result = (|| {
        let PromptCompletion {
            session_id,
            runtime_operation_id,
            operation_id,
            result,
        } = completion;
        let owns_turn = state
            .sessions
            .state(session_id)
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
            settle_runtime_prompt_request_error(state, session_id, runtime_operation_id, error)?;
        } else {
            let epoch = state.sessions.epoch().to_string();
            state
                .sessions
                .complete_prompt(
                    &epoch,
                    session_id,
                    runtime_operation_id,
                    response.clone().expect("successful prompt has a response"),
                )
                .map_err(runtime_state_error)?;
        }
        // Detach old responder handles before the next turn can consume quota or
        // reuse a tool-call identity. Physical terminal I/O stays outside the lock.
        terminal_releases = drain_runtime_effects(state);
        let sync_error = if uncertain {
            session_mirror(state)
                .block_turn(
                    session_id,
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
                .complete_turn(session_id, operation_id, terminal)
                .map_err(mirror_error)?;
            let commit = session_mirror(state)
                .commit_completed_turn(session_id, operation_id)
                .map_err(mirror_error);
            if let Err(error) = &commit {
                session_mirror(state)
                    .block_turn(session_id, operation_id, error.message.clone())
                    .map_err(mirror_error)?;
            }
            commit.err()
        };
        if let Some(validation) = session_validation_mut(state, session_id) {
            validation.retire_turn();
        }
        refresh_observer_timers(state);
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

/// Without a replay the session goes on with the log it has.
#[derive(Clone, Copy)]
enum AttachmentHistoryFallback {
    /// A resume gave its context.
    Received,
    // The target accepted messages after its creation response; fallback must
    // not take a turn or an operation that started since.
    Installed,
}

/// The terminal state of one optional history workflow. Only `Failed` may
/// enter the cached fallback: a superseded flow must not overwrite the
/// successor's projection, and a retired owner must not publish at all.
enum HistorySyncOutcome {
    Loaded,
    Failed(Error),
    Superseded,
    Stopped(Error),
}

// Attachment success is independent of the Agent's ability to replay history.
async fn synchronize_attached_history(
    connection: &ConnectionTo<Agent>,
    options: &Options,
    state: &Arc<BridgeLock>,
    cancellation: &CancellationToken,
    session_id: &str,
    fallback: AttachmentHistoryFallback,
    flow: &HistorySyncOwner,
    ingress: Option<&IngressSender>,
    execution: &mut Option<ExecutionTurn>,
    operation_id: &str,
) -> Result<(), Error> {
    let epoch = state.lock().await.sessions.epoch().to_string();
    let owner = rpc_owner(&epoch, Some(session_id), operation_id, None);
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
            .ok_or_else(|| Error::request_cancelled().data("fork target owner changed"))?;
        // A browser may already have started using the published target. Optional
        // synchronization cannot seize that turn or another exclusive operation.
        if session.active_turn.is_some() || session.operation.is_some() {
            return Ok(());
        }
    }
    let outcome = synchronize_authoritative_history(
        connection,
        options,
        state,
        cancellation,
        session_id,
        flow,
        ingress,
        execution,
        operation_id,
    )
    .await;
    match outcome {
        HistorySyncOutcome::Loaded => return Ok(()),
        HistorySyncOutcome::Stopped(error) => return Err(error),
        HistorySyncOutcome::Superseded => {
            // An admitted business operation owns the projection now. If the
            // original target is still its live owner the command completes
            // with the current view; a lifecycle handoff settles as a conflict.
            let state = state.lock().await;
            let alive = state.sessions.active_session(session_id).is_some();
            return if alive {
                Ok(())
            } else {
                Err(Error::invalid_request().data("session closed while retrieving history"))
            };
        }
        HistorySyncOutcome::Failed(error) => {
            if ingress.is_some() && execution.is_none() {
                return Err(error);
            }
        }
    }
    let mut state = state.lock().await;
    // A concurrent close or runtime shutdown must never resurrect an attachment.
    if state.sessions.active_session(session_id).is_none() {
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
    // A failed optional replay never replaces the log the session has.
    let ready = state
        .sessions
        .state(session_id)
        .is_some_and(|session| session.phase == MirrorPhase::Ready);
    if matches!(fallback, AttachmentHistoryFallback::Received) || !ready {
        session_mirror(&mut state)
            .use_cached_history(session_id)
            .map_err(mirror_error)?;
    }
    Ok(())
}

async fn synchronize_authoritative_history(
    connection: &ConnectionTo<Agent>,
    options: &Options,
    state: &Arc<BridgeLock>,
    cancellation: &CancellationToken,
    session_id: &str,
    flow: &HistorySyncOwner,
    ingress: Option<&IngressSender>,
    execution: &mut Option<ExecutionTurn>,
    operation_id: &str,
) -> HistorySyncOutcome {
    if let Err(error) = require_agent_method("session/load", state).await {
        return HistorySyncOutcome::Failed(error);
    }
    let cwd = {
        let state = state.lock().await;
        match state
            .sessions
            .active_session(session_id)
            .map(|session| session.live.cwd.clone())
        {
            Some(cwd) => cwd,
            None => {
                return HistorySyncOutcome::Stopped(
                    Error::invalid_params().data("session disappeared before reconciliation"),
                );
            }
        }
    };
    let epoch = state.lock().await.sessions.epoch().to_string();
    let mut backoff = RECONCILE_INITIAL_BACKOFF;
    loop {
        let attempt_id = Uuid::new_v4().to_string();
        {
            let mut state = state.lock().await;
            // Every attempt belongs to the registered flow: a superseded or
            // retired owner never dispatches another load or replays into a
            // successor's session.
            match state.sessions.history_sync_status(flow) {
                HistorySyncStatus::Owned if !flow.cancellation.is_cancelled() => {}
                HistorySyncStatus::Owned | HistorySyncStatus::Superseded => {
                    return HistorySyncOutcome::Superseded;
                }
                HistorySyncStatus::Gone => {
                    return HistorySyncOutcome::Stopped(
                        Error::request_cancelled().data("history workflow owner was retired"),
                    );
                }
            }
            if let Err(error) =
                session_mirror(&mut state).begin_load(session_id, attempt_id.clone())
            {
                return HistorySyncOutcome::Failed(mirror_error(error));
            }
            begin_replay_validation(&mut state, session_id, &attempt_id);
            take_sync_control_candidate(&mut state, session_id);
        }

        let owner = rpc_owner(&epoch, Some(session_id), operation_id, Some(&attempt_id));
        let result = match send_history(
            connection,
            ingress,
            execution,
            state,
            owner.clone(),
            LoadSessionRequest::new(session_id.to_string(), &cwd)
                .additional_directories(local_additional_directories(options))
                .mcp_servers(options.mcp_servers.clone()),
        )
        .await
        {
            Ok(result) => result,
            Err(error) => return HistorySyncOutcome::Stopped(error),
        };

        let failure = {
            let mut state = state.lock().await;
            if !replay_attempt_current(&state, session_id, &attempt_id) {
                return HistorySyncOutcome::Stopped(
                    Error::invalid_request().data("session history synchronization owner changed"),
                );
            }
            let failure = match result {
                Ok(response) => {
                    let response_value = match serde_json::to_value(&response) {
                        Ok(value) => value,
                        Err(error) => return HistorySyncOutcome::Failed(error.into()),
                    };
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
                        rollback_replay_validation(&mut state, session_id, &attempt_id);
                        take_sync_control_candidate(&mut state, session_id);
                        if let Err(mirror) = session_mirror(&mut state).fail_load(
                            session_id,
                            &attempt_id,
                            error.message.clone(),
                            false,
                        ) {
                            return HistorySyncOutcome::Failed(mirror_error(mirror));
                        }
                        Some((error, false))
                    } else {
                        match session_mirror(&mut state).commit_load(session_id, &attempt_id) {
                            Ok(_) => {
                                let controls =
                                    take_sync_control_candidate(&mut state, session_id).updates;
                                let epoch = state.sessions.epoch().to_string();
                                if let Err(error) = state
                                    .sessions
                                    .synchronize_loaded_session(
                                        &epoch,
                                        session_id,
                                        response_value.clone(),
                                        controls,
                                    )
                                    .map_err(runtime_state_error)
                                {
                                    return HistorySyncOutcome::Failed(error);
                                }
                                commit_replay_validation(&mut state, session_id, &attempt_id);
                                return HistorySyncOutcome::Loaded;
                            }
                            Err(error) => {
                                rollback_replay_validation(&mut state, session_id, &attempt_id);
                                take_sync_control_candidate(&mut state, session_id);
                                let error = mirror_error(error);
                                if let Err(mirror) = session_mirror(&mut state).fail_load(
                                    session_id,
                                    &attempt_id,
                                    error.message.clone(),
                                    false,
                                ) {
                                    return HistorySyncOutcome::Failed(mirror_error(mirror));
                                }
                                Some((error, false))
                            }
                        }
                    }
                }
                Err(error) => {
                    rollback_replay_validation(&mut state, session_id, &attempt_id);
                    take_sync_control_candidate(&mut state, session_id);
                    let retryable = retryable_reconcile_error(&error);
                    if let Err(mirror) = session_mirror(&mut state).fail_load(
                        session_id,
                        &attempt_id,
                        error.message.clone(),
                        retryable,
                    ) {
                        return HistorySyncOutcome::Failed(mirror_error(mirror));
                    }
                    Some((error, retryable))
                }
            };
            failure
        };

        let Some((error, retryable)) = failure else {
            unreachable!("successful reconciliation returns from the state transaction")
        };
        if !retryable {
            return HistorySyncOutcome::Failed(error);
        }
        // The wait is part of the workflow, not of the Agent RPC: the ticket and
        // the state lock are released so other sessions progress. A superseding
        // admission or a retired owner wakes the continuation through the flow
        // token; the next loop's identity check decides whether to proceed.
        drop(execution.take());
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = flow.cancellation.cancelled() => {}
            _ = cancellation.cancelled() => {
                return HistorySyncOutcome::Stopped(Error::request_cancelled());
            }
        }
        if let Err(error) = continue_execution(ingress, execution, &owner).await {
            return HistorySyncOutcome::Stopped(error);
        }
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

fn advance_catalog_revision(state: &mut BridgeState) -> u64 {
    state.catalog_revision = state
        .catalog_revision
        .checked_add(1)
        .expect("catalog revision exhausted");
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

pub(crate) fn valid_session_path(value: &str) -> bool {
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
    state: &Arc<BridgeLock>,
) -> Result<AttachmentReservation, Error> {
    reserve_attachment_with_cwd(session_id, kind, None, "test-attachment", None, state).await
}

async fn reserve_attachment_with_cwd(
    session_id: &str,
    kind: RuntimeSessionOperationKind,
    requested_cwd: Option<&str>,
    operation_id: &str,
    materialization_id: Option<&str>,
    state: &Arc<BridgeLock>,
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
            session_mirror(state)
                .can_begin(session_id, SessionAdmission::Attachment)
                .map_err(mirror_error)?;
        }
    }
    let epoch = state.sessions.epoch().to_string();
    let tracked = state
        .sessions
        .active_session(session_id)
        .map(|session| session.live.cwd.clone());
    let reservation = match tracked {
        Some(cwd) => {
            // A session the previous connection left waiting may also come
            // back with a resume, which replays nothing (protocol §5.4).
            if kind != RuntimeSessionOperationKind::Load
                && !state.sessions.waits_for_load(session_id)
            {
                return Err(Error::invalid_request()
                    .data("an active tracked session can only be reloaded with session/load"));
            }
            session_mirror(state)
                .begin_exclusive(session_id, kind, operation_id)
                .map_err(mirror_error)?;
            if let Err(error) = state
                .sessions
                .start_reload(&epoch, session_id, operation_id, kind)
            {
                release_session_operation(
                    state,
                    session_id,
                    SessionAdmission::Attachment,
                    operation_id,
                );
                return Err(runtime_state_error(error));
            }
            // The accepted reload owns the session's projection; a parked
            // optional history flow yields instead of replaying over it.
            state.sessions.supersede_history_sync(session_id);
            AttachmentReservation::Reload { cwd }
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
            // A retry installs a fresh live entry. Move the same loading
            // transaction explicitly so retirement cancels every other old handle.
            let had_entry = state.sessions.state(session_id).is_some();
            let materialization = materialization_id
                .and_then(|id| take_session_materialization(state, session_id, id));
            let attached = state.sessions.start_attachment(
                &epoch,
                session_id,
                cwd.clone(),
                operation_id,
                kind,
            );
            match attached {
                Ok(()) => {
                    if let Some(materialization) = materialization {
                        state
                            .sessions
                            .resources_mut(session_id)
                            .expect("attachment installed its owner")
                            .materialization = Some(materialization);
                    }
                }
                Err(error) => {
                    if let Some(materialization) = materialization {
                        if had_entry && let Ok(resources) = state.sessions.resources_mut(session_id)
                        {
                            resources.materialization = Some(materialization);
                        } else {
                            materialization.cancellation.cancel();
                        }
                    }
                    return Err(runtime_state_error(error));
                }
            }
            session_mirror(state).register_cold(session_id);
            AttachmentReservation::Fresh { cwd }
        }
    };
    begin_replay_validation(state, session_id, operation_id);
    clear_attachment_delivery(state, session_id);
    Ok(reservation)
}

fn settle_attachment(state: &mut BridgeState, session_id: &str) {
    let owner = state.sessions.state(session_id).and_then(|session| {
        session
            .operation
            .as_ref()
            .filter(|operation| operation.kind.admission() == SessionAdmission::Attachment)
            .map(|operation| operation.operation_id.clone())
    });
    if let Some(operation_id) = owner {
        release_session_operation(
            state,
            session_id,
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
) -> Result<(), Error> {
    if state.sessions.active_session(session_id).is_none() {
        return Err(
            Error::invalid_params().data(format!("unknown or inactive session: {session_id}"))
        );
    }
    session_mirror(state)
        .begin_exclusive(session_id, operation, operation_id)
        .map_err(mirror_error)
}

fn complete_session_retirement(
    state: &mut BridgeState,
    session_id: &str,
    operation: SessionAdmission,
    operation_id: &str,
) {
    if session_mirror(state)
        .finish_session_cleanup(session_id, operation, operation_id)
        .is_ok()
    {
        state.sessions.cancel_resources(session_id);
        state.sessions.remove(session_id);
    }
}

fn release_session_operation(
    state: &mut BridgeState,
    session_id: &str,
    operation: SessionAdmission,
    operation_id: &str,
) {
    if session_operation_uncertain(state, session_id, operation_id) {
        return;
    }
    // A late completion cannot release a newer operation or a reopened session.
    let _ = session_mirror(state).settle_exclusive(session_id, operation, operation_id);
}

fn session_operation_uncertain(state: &BridgeState, session_id: &str, operation_id: &str) -> bool {
    state
        .sessions
        .state(session_id)
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
    Session,
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
        session
            .can_begin(SessionAdmission::Delete)
            .map_err(mirror_error)?;
        if session.live.is_some() {
            state
                .sessions
                .begin_exclusive(
                    session_id,
                    RuntimeSessionOperationKind::Delete,
                    operation_id,
                )
                .map_err(mirror_error)?;
            return Ok(DeletionReservation::Session);
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
        DeletionReservation::Session => {
            release_session_operation(state, session_id, SessionAdmission::Delete, operation_id);
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
) -> Result<bool, Error> {
    let active = state.sessions.active_session(session_id).is_some();
    let closed = state
        .sessions
        .live(session_id)
        .is_some_and(|live| live.lifecycle == SessionLifecycle::Closed);
    if !active && !closed {
        return Ok(false);
    }
    let epoch = state.sessions.epoch().to_string();
    let result = if close_before_delete || closed {
        state
            .sessions
            .start_delete(&epoch, session_id, operation_id.to_string())
    } else {
        state
            .sessions
            .start_direct_delete(&epoch, session_id, operation_id.to_string())
    };
    result.map_err(runtime_state_error)?;
    Ok(true)
}

fn commit_runtime_delete_close_success(
    state: &mut BridgeState,
    session_id: &str,
    operation_id: &str,
) -> Result<(), Error> {
    let epoch = state.sessions.epoch().to_string();
    state
        .sessions
        .delete_close_succeeded(&epoch, session_id, operation_id)
        .map_err(runtime_state_error)?;
    refresh_observer_timers(state);
    Ok(())
}

async fn require_session_workspace(
    session_id: &str,
    state: &Arc<BridgeLock>,
    filesystem: &WorkspaceFileSystem,
) -> Result<WorkspaceFileSystem, Error> {
    let cwd = {
        let state = state.lock().await;
        let session = state.sessions.resource_session(session_id).ok_or_else(|| {
            Error::invalid_params().data(format!("unknown or inactive session: {session_id}"))
        })?;
        session.live.cwd.clone()
    };
    filesystem.for_workspace(&cwd)
}

async fn cancel_interactions(session_id: &str, state: &Arc<BridgeLock>, sink: &EventSink) {
    let mut state = state.lock().await;
    cancel_interactions_locked(session_id, &mut state, sink);
}

fn commit_session_cancel(
    state: &mut BridgeState,
    sink: &EventSink,
    session_id: &str,
) -> Result<(), Error> {
    let epoch = state.sessions.epoch().to_string();
    state
        .sessions
        .request_cancel(&epoch, session_id)
        .map_err(runtime_state_error)?;
    // Select and retire this turn's current interactions before completion can
    // admit a new turn. Accepted URL flows belong to the session scope and
    // survive the cancel (S7).
    if let Some(owner) = owned_resources(state, session_id) {
        cancel_pending_interactions(&owner, state, sink);
        refresh_observer_timers(state);
    }
    Ok(())
}

fn owned_resources(state: &BridgeState, session_id: &str) -> Option<SessionResourceOwner> {
    let owner = SessionResourceOwner::new(state.sessions.epoch(), session_id);
    state.sessions.owns_resources(&owner).then_some(owner)
}

/// Cancels the session's pending interactions and releases its scope's
/// resources: the session ended or the connection is ending.
fn cancel_interactions_locked(session_id: &str, state: &mut BridgeState, sink: &EventSink) {
    let Some(owner) = owned_resources(state, session_id) else {
        return;
    };
    cancel_pending_interactions(&owner, state, sink);
    release_url_flows(&owner, state, sink);
    refresh_observer_timers(state);
}

fn resolves_canonically(state: &BridgeState, session_id: &str) -> bool {
    state.sessions.live(session_id).is_some_and(|live| {
        matches!(
            live.lifecycle,
            SessionLifecycle::Active
                | SessionLifecycle::Attaching
                | SessionLifecycle::Closing
                | SessionLifecycle::ClosingForDelete
        )
    })
}

/// Answers the session's pending permissions and elicitations with cancel.
fn cancel_pending_interactions(
    owner: &SessionResourceOwner,
    state: &mut BridgeState,
    sink: &EventSink,
) {
    let session_id = owner.session_id.as_str();
    let resolve_canonical = resolves_canonically(state, session_id);
    let permission_ids = state
        .sessions
        .permission_owners
        .iter()
        .filter(|(_, route)| *route == owner)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    for id in permission_ids {
        if let Some(pending) = state.sessions.take_permission(&id, owner) {
            if resolve_canonical
                && let Err(error) = state
                    .sessions
                    .resolve_permission(&owner.epoch, session_id, &id)
            {
                sink.acp_error(runtime_state_error(error), None, Some("permission/cancel"));
            }
            let _ = pending.sender.send(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ));
        }
    }
    let elicitation_ids = state
        .sessions
        .elicitation_owners
        .iter()
        .filter(|(_, route)| *route == owner)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    for id in elicitation_ids {
        if let Some(pending) = state.sessions.take_elicitation(&id, owner) {
            if resolve_canonical
                && let Err(error) =
                    state
                        .sessions
                        .resolve_elicitation(&owner.epoch, Some(session_id), &id, None)
            {
                sink.acp_error(runtime_state_error(error), None, Some("elicitation/cancel"));
            }
            let _ = pending
                .sender
                .send(CreateElicitationResponse::new(ElicitationAction::Cancel));
        }
    }
}

/// Settles the URL flows accepted in the session as cancelled: its scope
/// ended (protocol §4.6), which also frees the Agent's elicitation IDs.
fn release_url_flows(owner: &SessionResourceOwner, state: &mut BridgeState, sink: &EventSink) {
    let session_id = owner.session_id.as_str();
    let resolve_canonical = resolves_canonically(state, session_id);
    let url_ids = state
        .sessions
        .url_owners
        .iter()
        .filter(|(_, route)| route.owner.as_ref() == Some(owner))
        .map(|(id, route)| (id.clone(), route.clone()))
        .collect::<Vec<_>>();
    for (elicitation_id, registration) in url_ids {
        if resolve_canonical {
            match state.sessions.settle_url_registration(
                &owner.epoch,
                Some(session_id),
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
        state
            .sessions
            .take_url_route(&elicitation_id, &registration);
    }
    // A request-scoped URL flow accepted in this session ends with it too.
    let scope = OpScope::Session(session_id.to_string());
    let request_flows = state
        .sessions
        .url_owners
        .iter()
        .filter(|(_, route)| {
            route.owner.is_none()
                && state
                    .sessions
                    .scopes
                    .flow_scopes
                    .get(&route.registration_id)
                    == Some(&scope)
        })
        .map(|(id, route)| (id.clone(), route.clone()))
        .collect::<Vec<_>>();
    for (elicitation_id, registration) in request_flows {
        match state.sessions.settle_url_registration(
            &owner.epoch,
            None,
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
        state
            .sessions
            .take_url_route(&elicitation_id, &registration);
        state
            .sessions
            .scopes
            .flow_scopes
            .remove(&registration.registration_id);
    }
    // A finished flow has no route; it ends with its scope too.
    let finished: Vec<String> = state
        .sessions
        .request_url_flows
        .iter()
        .filter(|(_, flow)| {
            flow.status != UrlFlowStatus::Waiting
                && state.sessions.scopes.flow_scopes.get(&flow.registration_id) == Some(&scope)
        })
        .map(|(elicitation_id, _)| elicitation_id.clone())
        .collect();
    for elicitation_id in finished {
        if let Some(flow) = state.sessions.request_url_flows.remove(&elicitation_id) {
            state.sessions.scopes.flow_scopes.remove(&flow.registration_id);
        }
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

async fn require_agent_method(operation: &str, state: &Arc<BridgeLock>) -> Result<(), Error> {
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
    const FORMER_BRIDGE_IDENTIFIER_LENGTH: usize = 1_024;
    const FORMER_BRIDGE_PATH_LENGTH: usize = 16_384;
    const FORMER_AGENT_RELAY_BYTES: usize = 4_000_000;
    const FORMER_TRACKED_SESSIONS: usize = 32;

    use clap::Parser;

    fn register_test_session(state: &mut BridgeState, session_id: &str, cwd: &str) {
        let epoch = state.sessions.epoch().to_string();
        state
            .sessions
            .open_new(&epoch, session_id, cwd, json!({ "sessionId": session_id }))
            .unwrap();

        session_mirror(state).register_new(session_id);
    }

    /// The turn a session runs, as the session state holds it.
    fn running_turn(
        state: &BridgeState,
        session_id: &str,
    ) -> Option<crate::session_state::TurnOverlay> {
        state.sessions.state(session_id)?.active_turn.clone()
    }

    fn cancel_requested(state: &BridgeState, session_id: &str) -> bool {
        running_turn(state, session_id)
            .and_then(|turn| turn.execution)
            .unwrap()
            .cancel_requested
    }

    #[test]
    fn a_cancel_intent_holds_until_completion_and_keeps_late_updates() {
        let mut state = BridgeState::default();
        register_test_session(&mut state, "session", "/workspace");
        register_test_session(&mut state, "other", "/workspace");
        let operation = start_test_turn(&mut state, "session", "intent").unwrap();
        start_test_turn(&mut state, "other", "other-intent").unwrap();
        state.sessions.append_turn_update("session", &operation, json!({
            "sessionUpdate": "tool_call", "toolCallId": "tool", "title": "Run", "status": "in_progress"
        })).unwrap();
        assert!(!cancel_requested(&state, "session"));

        let epoch = state.sessions.epoch().to_string();
        state.sessions.request_cancel(&epoch, "session").unwrap();
        assert_eq!(
            state.sessions.state("session").unwrap().phase,
            MirrorPhase::Running
        );
        assert!(cancel_requested(&state, "session"));
        assert!(running_turn(&state, "session").unwrap().terminal.is_none());
        assert!(!cancel_requested(&state, "other"));

        state
            .sessions
            .append_turn_update(
                "session",
                &operation,
                json!({
                    "sessionUpdate": "tool_call_update", "toolCallId": "tool", "status": "completed"
                }),
            )
            .unwrap();
        assert!(cancel_requested(&state, "session"));
        assert_eq!(
            running_turn(&state, "session").unwrap().updates[0]["status"],
            "completed"
        );

        complete_test_turn(&mut state, "session", &operation);
        assert!(running_turn(&state, "session").is_none());
        start_test_turn(&mut state, "session", "next-intent").unwrap();
        assert!(!cancel_requested(&state, "session"));
    }

    fn start_test_turn(
        state: &mut BridgeState,
        session_id: &str,
        intent: &str,
    ) -> Result<String, MirrorError> {
        let mirror = session_mirror(state);
        let prompt = vec![json!({ "type": "text", "text": intent })];
        let epoch = mirror.epoch().to_string();
        mirror
            .admit_session_turn(&epoch, session_id, intent, prompt)
            .map_err(|error| match error {
                SessionTurnError::History(error) => error,
                SessionTurnError::Live(error) => panic!("test live admission failed: {error:?}"),
            })
    }

    fn complete_test_turn(state: &mut BridgeState, session_id: &str, operation_id: &str) {
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
            .complete_prompt(&epoch, session_id, &runtime_operation, terminal.clone())
            .unwrap();
        let mirror = session_mirror(state);
        mirror
            .complete_turn(session_id, operation_id, terminal)
            .unwrap();
        mirror
            .commit_completed_turn(session_id, operation_id)
            .unwrap();
    }

    #[tokio::test]
    async fn a_waiting_turn_admits_after_completion_and_a_late_old_result_cannot_seal_it() {
        let state = Arc::new(BridgeLock::new(BridgeState::default()));
        let mut locked = state.lock().await;
        register_test_session(&mut locked, "session", "/workspace");
        let operation = start_test_turn(&mut locked, "session", "old-intent").unwrap();
        let update = json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "last old update" } });
        locked
            .sessions
            .append_turn_update("session", &operation, update)
            .unwrap();
        let (attempting, attempted) = oneshot::channel();
        let competing_state = state.clone();
        let next_turn = tokio::spawn(async move {
            attempting.send(()).unwrap();
            let mut state = competing_state.lock().await;
            start_test_turn(&mut state, "session", "next-intent").unwrap()
        });
        attempted.await.unwrap();
        // The competing turn is already queued for this owner: the old turn
        // completes before releasing this critical section.
        let completion = |locked: &mut BridgeState| {
            commit_completed_turn_history(
                locked,
                PromptCompletion {
                    session_id: "session",
                    runtime_operation_id: "old-intent",
                    operation_id: &operation,
                    result: Ok(serde_json::from_value(json!({ "stopReason": "end_turn" })).unwrap()),
                },
            )
        };
        let completed = completion(&mut *locked);
        completed.result.unwrap();
        assert!(completed.terminal_releases.is_empty());
        assert_eq!(
            locked.sessions.state("session").unwrap().phase,
            MirrorPhase::Ready
        );
        assert!(running_turn(&locked, "session").is_none());
        drop(locked);
        let next_operation = next_turn.await.unwrap();
        // A late duplicate result from the old RPC must not seal the new turn.
        let mut locked = state.lock().await;
        let running = running_turn(&locked, "session").unwrap();
        assert_eq!(running.operation_id, next_operation);
        completion(&mut *locked).result.unwrap();
        assert_eq!(running_turn(&locked, "session"), Some(running));
        assert_eq!(
            locked.sessions.state("session").unwrap().phase,
            MirrorPhase::Running
        );
    }

    #[test]
    fn a_prompt_failure_keeps_its_transport_uncertainty() {
        for uncertain in [false, true] {
            let mut state = BridgeState::default();
            register_test_session(&mut state, "session", "/workspace");
            let operation = start_test_turn(&mut state, "session", "failed-intent").unwrap();
            let error = if uncertain {
                Error::internal_error().data(
                    json!({ "reason": "incoming_transport_closed", "method": "session/prompt" }),
                )
            } else {
                Error::invalid_request().data("Agent rejected the prompt")
            };
            let completed = commit_completed_turn_history(
                &mut state,
                PromptCompletion {
                    session_id: "session",
                    runtime_operation_id: "failed-intent",
                    operation_id: &operation,
                    result: Err(error),
                },
            );
            assert!(completed.result.is_err());
            assert_eq!(
                state.sessions.state("session").unwrap().phase,
                if uncertain {
                    MirrorPhase::Blocked
                } else {
                    MirrorPhase::Ready
                }
            );
            assert_eq!(
                state.sessions.live("session").unwrap().lifecycle,
                if uncertain {
                    SessionLifecycle::Uncertain
                } else {
                    SessionLifecycle::Active
                }
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

    /// The updates the session's running turn accepted.
    async fn accepted_updates(state: &BridgeLock, session_id: &str) -> Vec<Value> {
        let state = state.lock().await;
        running_turn(&state, session_id)
            .map(|turn| {
                turn.updates
                    .iter()
                    .map(|update| (**update).clone())
                    .collect()
            })
            .unwrap_or_default()
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
        use crate::protocol::replica::Replica;
        let cwd = env!("CARGO_MANIFEST_DIR");
        let fixture = format!("{cwd}/tests/fixtures/burst-prompt-agent.mjs");
        let options = options(&["attyd", "--cwd", cwd, "--", "node", &fixture, exit_mode]);
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (event_tx, mut events) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let mut bridge = tokio::spawn(run_with_cancellation(
            Arc::new(options),
            command_rx,
            event_tx,
            cancellation.clone(),
        ));
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            assert_eq!(
                events.recv().await,
                Some(BridgeEvent::Initialized),
                "bridge stopped before initialization"
            );
            // The session's v2 stream, as a client reads it.
            fn apply(
                replica: &mut Replica,
                ops: &mut Vec<Value>,
                event: crate::protocol::engine::StreamEvent,
            ) {
                let event = serde_json::to_value(event).unwrap();
                assert!(event.get("end").is_none(), "the stream ended: {event}");
                replica.apply(&event).unwrap();
                ops.push(replica.live()["op"].clone());
            }
            let mut replica = Replica::default();
            let mut ops = Vec::new();
            let mut subscription = None;
            if exit_mode != "exit-idle" {
                let scope = OpScope::Session("burst".into());
                let (reply, subscribed) = oneshot::channel();
                commands
                    .send(BridgeInput::Subscribe {
                        scopes: vec![crate::protocol::subscription::Subscribed {
                            scope: scope.clone(),
                            have: None,
                            cwd: Some(cwd.into()),
                        }],
                        reply,
                    })
                    .unwrap();
                let stream = subscribed.await.unwrap();
                while !(replica.live()["op"]["kind"] == "load"
                    && replica.live()["op"]["status"] == "completed")
                {
                    let event = stream.next().await.expect("the stream closed");
                    apply(&mut replica, &mut ops, event);
                }
                let expect = replica.live()["op"]["id"].as_str().unwrap().to_string();
                let (reply, admitted) = oneshot::channel();
                commands
                    .send(BridgeInput::Operation {
                        scope,
                        write: crate::protocol::admission::OpWrite {
                            id: None,
                            expect,
                            kind: "prompt".into(),
                            args: json!({"prompt": [{
                                "type": "text",
                                "text": if large { "answer-large" } else { "answer" },
                            }]}),
                        },
                        reply,
                    })
                    .unwrap();
                admitted.await.unwrap().unwrap();
                subscription = Some(stream);
            }

            // Read the stream until the process and bridge both finish. A
            // clean EOF must terminate without closing the browser command
            // channel, and every accepted output must remain published.
            if let Some(stream) = &subscription {
                loop {
                    tokio::select! {
                        biased;
                        finished = &mut bridge => {
                            finished.unwrap();
                            break;
                        }
                        event = stream.next() => match event {
                            Some(event) => apply(&mut replica, &mut ops, event),
                            None => break,
                        },
                    }
                }
                while let Some(event) = stream.try_next() {
                    apply(&mut replica, &mut ops, event);
                }
            }
            if !bridge.is_finished() {
                (&mut bridge).await.unwrap();
            }
            assert_eq!(
                events.recv().await,
                None,
                "a clean EOF tells the host nothing more"
            );
            let prompts = ops
                .iter()
                .filter(|op| op["kind"] == "prompt")
                .map(|op| op["status"].clone())
                .collect::<Vec<_>>();
            match exit_mode {
                "exit-idle" => assert!(prompts.is_empty()),
                "exit-pending" => {
                    assert_eq!(prompts.last(), Some(&json!("failed")), "{ops:?}");
                    assert!(
                        replica.entries().iter().all(|entry| entry["role"] != "agent"),
                        "a failed turn keeps no reply it never received"
                    );
                }
                "exit-completed" => {
                    assert_eq!(prompts.last(), Some(&json!("completed")), "{ops:?}");
                    let expected = if large {
                        "完整🙂".repeat(900_000)
                    } else {
                        (0..2_048)
                            .map(|index| format!("片段{index}🙂\n"))
                            .collect::<String>()
                    };
                    let text = replica
                        .entries()
                        .iter()
                        .filter(|entry| entry["role"] == "agent")
                        .flat_map(|entry| entry["chunks"].as_array().cloned().unwrap_or_default())
                        .filter_map(|chunk| chunk["content"]["text"].as_str().map(str::to_owned))
                        .collect::<String>();
                    assert_eq!(text, expected, "output preceding EOF was truncated");
                }
                _ => unreachable!(),
            }
        })
        .await;
        cancellation.cancel();
        if !bridge.is_finished() {
            tokio::time::timeout(Duration::from_secs(5), bridge)
                .await
                .unwrap()
                .unwrap();
        }
        result.expect("clean EOF left the bridge running");
    }

    /// One v2 stream of a real bridge connection, read as a browser reads it.
    pub(super) struct V2Stream {
        stream: crate::protocol::feed::FeedSubscription,
        replicas: HashMap<String, crate::protocol::replica::Replica>,
    }

    impl V2Stream {
        pub(super) async fn open(
            commands: &mpsc::UnboundedSender<BridgeInput>,
            scopes: &[(&str, Option<&str>)],
        ) -> Self {
            let (reply, subscribed) = oneshot::channel();
            commands
                .send(BridgeInput::Subscribe {
                    scopes: scopes
                        .iter()
                        .map(|(scope, cwd)| crate::protocol::subscription::Subscribed {
                            scope: OpScope::parse(scope).unwrap(),
                            have: None,
                            cwd: cwd.map(String::from),
                        })
                        .collect(),
                    reply,
                })
                .unwrap();
            Self {
                stream: subscribed.await.unwrap(),
                replicas: HashMap::new(),
            }
        }

        pub(super) async fn next(&mut self) {
            let event = tokio::time::timeout(Duration::from_secs(10), self.stream.next())
                .await
                .expect("timed out waiting for a stream event")
                .expect("the stream closed");
            let event = serde_json::to_value(event).unwrap();
            assert!(event.get("end").is_none(), "the stream ended: {event}");
            self.replicas
                .entry(event["scope"].as_str().unwrap().to_string())
                .or_default()
                .apply(&event)
                .unwrap();
        }

        pub(super) async fn until(&mut self, condition: impl Fn(&Self) -> bool) {
            while !condition(self) {
                self.next().await;
            }
        }

        pub(super) fn live(&self, scope: &str) -> Value {
            self.replicas
                .get(scope)
                .map_or(Value::Null, |replica| replica.live())
        }

        /// Writes an operation of `kind` in `scope` after the one it shows.

        pub(super) async fn write(
            &self,
            commands: &mpsc::UnboundedSender<BridgeInput>,
            scope: &str,
            kind: &str,
            args: Value,
        ) -> operations::OperationAdmission {
            let (reply, admitted) = oneshot::channel();
            commands
                .send(BridgeInput::Operation {
                    scope: OpScope::parse(scope).unwrap(),
                    write: crate::protocol::admission::OpWrite {
                        id: None,
                        expect: self.live(scope)["op"]["id"].as_str().unwrap().to_string(),
                        kind: kind.into(),
                        args,
                    },
                    reply,
                })
                .unwrap();
            admitted.await.unwrap().unwrap()
        }

        /// Lists the catalog: the page its held response ends with.
        pub(super) async fn list(
            &mut self,
            commands: &mpsc::UnboundedSender<BridgeInput>,
        ) -> Value {
            let admission = self.write(commands, "catalog", "list", json!({})).await;
            let page = tokio::time::timeout(Duration::from_secs(10), admission.end.unwrap())
                .await
                .expect("the list did not end")
                .unwrap()
                .unwrap();
            self.until(|stream| stream.live("catalog")["op"]["id"] == admission.op)
                .await;
            page
        }
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
            event_tx,
            cancellation.clone(),
        ));
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            assert_eq!(
                events.recv().await,
                Some(BridgeEvent::Initialized),
                "bridge stopped before initialization"
            );
            let scope = "session/burst-history";
            let mut stream =
                V2Stream::open(&commands, &[("catalog", None), (scope, Some(cwd))]).await;
            stream
                .until(|stream| stream.live("catalog").is_object())
                .await;
            // The fixture withholds the load response until catalog operations
            // have completed. Listing cannot depend on the history transaction.
            loop {
                let page = stream.list(&commands).await;
                if page["sessions"]
                    .as_array()
                    .is_some_and(|sessions| !sessions.is_empty())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert_ne!(
                stream.live(scope)["op"]["status"],
                "completed",
                "the load waits for the fixture"
            );
            stream.list(&commands).await;
            stream
                .until(|stream| {
                    stream.live(scope)["op"]["kind"] == "load"
                        && stream.live(scope)["op"]["status"] == "completed"
                })
                .await;
            // Every replayed message reaches the session's log, in order.
            fn texts<'a>(value: &'a Value, found: &mut Vec<&'a str>) {
                match value {
                    Value::String(text) if text.starts_with("Historical message ") => {
                        found.push(text)
                    }
                    Value::Array(values) => values.iter().for_each(|value| texts(value, found)),
                    Value::Object(values) => values.values().for_each(|value| texts(value, found)),
                    _ => {}
                }
            }
            let log = Value::Array(stream.replicas[scope].entries());
            let mut found = Vec::new();
            texts(&log, &mut found);
            let replayed = found.concat();
            let expected = (0..10_050)
                .map(|index| format!("Historical message {index}. {}", "x".repeat(128)))
                .collect::<String>();
            assert!(replayed.len() > 1_000_000);
            assert_eq!(replayed, expected, "history replay was truncated");
            stream.list(&commands).await;
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
            event_tx,
            cancellation.clone(),
        ));

        tokio::time::timeout(Duration::from_secs(10), async {
            assert_eq!(events.recv().await, Some(BridgeEvent::Initialized));
        })
        .await
        .expect("bridge did not initialize");
        let mut global = V2Stream::open(&commands, &[("global", None)]).await;
        global
            .until(|stream| stream.live("global").is_object())
            .await;
        let created = global
            .write(&commands, "global", "create", json!({"cwd": cwd}))
            .await;
        let created = tokio::time::timeout(Duration::from_secs(10), created.end.unwrap())
            .await
            .expect("the create did not end")
            .unwrap()
            .unwrap();
        assert_eq!(created["sessionId"], "test-session");
        let scope = "session/test-session";
        let mut session = V2Stream::open(&commands, &[(scope, None)]).await;
        session.until(|stream| stream.live(scope).is_object()).await;
        assert!(session.replicas[scope].entries().is_empty());
        let initial_rev = session.replicas[scope].rev;

        let prompt = session
            .write(
                &commands,
                scope,
                "prompt",
                json!({"prompt": [{ "type": "text", "text": "message-actions-flow" }]}),
            )
            .await
            .op;
        // From its admission on, the session shows only this prompt: the
        // committed turn neither reloads the session nor runs the prompt again.
        let mut ops = Vec::new();
        while !(session.live(scope)["op"]["id"] == prompt
            && session.live(scope)["op"]["status"] == "completed")
        {
            session.next().await;
            ops.push(session.live(scope)["op"].clone());
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        while let Some(event) = session.stream.try_next() {
            let event = serde_json::to_value(event).unwrap();
            session
                .replicas
                .get_mut(scope)
                .unwrap()
                .apply(&event)
                .unwrap();
            ops.push(session.live(scope)["op"].clone());
        }
        assert!(
            ops.iter().all(|op| op["id"] == prompt),
            "a completed turn must stay in memory instead of issuing session/load: {ops:?}"
        );
        let entries = session.replicas[scope].entries();
        let prompts: Vec<_> = entries.iter().filter(|entry| entry["role"] == "user").collect();
        assert_eq!(prompts.len(), 1, "history commit redispatched the prompt");
        assert_eq!(
            prompts[0]["chunks"][0]["content"],
            json!({ "type": "text", "text": "message-actions-flow" })
        );
        assert_ne!(session.replicas[scope].rev, initial_rev);

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
        let error = ensure_interaction_delivered(false, "elicitation").unwrap_err();
        assert_eq!(error.code, ErrorCode::RequestCancelled);
        ensure_interaction_delivered(true, "elicitation").unwrap();
    }

    #[tokio::test]
    async fn buffers_pre_response_updates_and_drops_late_unknown_sessions() {
        // Zed's ACP regression suite requires load replay notifications arriving before
        // the RPC response to survive. New/fork need the same ordering guarantee here.
        let state = Arc::new(BridgeLock::new(BridgeState {
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
            state
                .sessions
                .open_new_with_replay(
                    &epoch,
                    "new-session",
                    "/workspace",
                    response,
                    notification_values(&replay),
                )
                .unwrap();
            promote_creation_validation(&mut state, "new-session").unwrap();
            assert!(state.creation_staging.is_empty());

            state.sessions.register_new("new-session");
        }
        let accepted = |state: &BridgeState| {
            session_validation(state, "new-session")
                .unwrap()
                .update_count
        };
        let before = accepted(&*state.lock().await);
        handle_session_update(notification, &state, &sink, None).await;
        assert_eq!(accepted(&*state.lock().await), before + 1);
    }

    #[tokio::test]
    async fn rejects_invalid_active_updates_transactionally_then_relays_recovery() {
        let mut bridge = BridgeState::default();
        register_test_session(&mut bridge, "session", "/workspace");
        start_test_turn(&mut bridge, "session", "prompt").unwrap();
        let state = Arc::new(BridgeLock::new(bridge));
        let (tx, _rx) = mpsc::unbounded_channel();
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
        let before = accepted_updates(&state, "session").await;
        handle_session_update(invalid, &state, &sink, None).await;
        assert_eq!(
            accepted_updates(&state, "session").await,
            before,
            "an invalid update must not be accepted"
        );
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
        assert_ne!(accepted_updates(&state, "session").await, before);
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
            .controls
            .fold_update(
                &json!({ "sessionUpdate": "current_mode_update", "currentModeId": "build" }),
            );
        let state = Arc::new(BridgeLock::new(bridge_state));
        let (tx, _rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx };
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
            runtime.controls.modes.as_ref().unwrap().current_mode_id,
            "plan",
        );
    }

    #[tokio::test]
    async fn terminal_reference_reduction_does_not_wait_for_the_physical_manager() {
        for valid in [true, false] {
            let workspace = tempfile::tempdir().unwrap();
            let filesystem =
                Arc::new(WorkspaceFileSystem::new(workspace.path(), false, &[]).unwrap());
            let terminals =
                TerminalManager::new_with_snapshots(filesystem, CancellationToken::new(), None);
            let mut bridge = BridgeState::default();
            register_test_session(&mut bridge, "session", "/workspace");
            let owner = bridge.sessions.resource_owner("session").unwrap();
            let (tx, _rx) = mpsc::unbounded_channel();
            let sink = EventSink { tx };
            if valid {
                register_created_terminal_locked(&mut bridge, &owner, "terminal").unwrap();
            }
            let state = Arc::new(BridgeLock::new(bridge));
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
            let locked = state.lock().await;
            assert_eq!(
                session_validation(&locked, "session").unwrap().update_count,
                usize::from(valid)
            );
        }
    }

    #[test]
    fn terminal_creation_identity_preserves_output_and_rejects_released_handles() {
        let mut state = BridgeState::default();
        register_test_session(&mut state, "session", "/workspace");
        let owner = state.sessions.resource_owner("session").unwrap();
        publish_terminal_snapshot(
            &mut state,
            TerminalSnapshot {
                value: json!({
                    "sessionId": "session", "terminalId": "terminal", "output": "already arrived",
                    "outputAppend": true, "retainedBytes": 15, "truncated": false,
                    "exitStatus": { "exitCode": 0 }, "released": false
                }),
            },
        );
        let before = state.sessions.live("session").unwrap().terminals["terminal"].clone();
        register_created_terminal_locked(&mut state, &owner, "terminal").unwrap();
        assert_eq!(
            state.sessions.live("session").unwrap().terminals["terminal"],
            before
        );
        publish_terminal_snapshot(
            &mut state,
            TerminalSnapshot {
                value: json!({ "sessionId": "session", "terminalId": "terminal", "released": true }),
            },
        );
        assert!(register_created_terminal_locked(&mut state, &owner, "terminal").is_err());
        assert!(state.sessions.live("session").unwrap().terminals.is_empty());
    }

    #[tokio::test]
    async fn reload_validation_failure_preserves_the_session_compaction_index() {
        let mut bridge_state = BridgeState::default();
        let epoch = bridge_state.sessions.epoch().to_string();
        bridge_state
            .sessions
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        session_mirror(&mut bridge_state).register_new("session");

        let validation = ensure_session_validation(&mut bridge_state, "session").unwrap();
        validate_and_track_session_update(validation, &json!({ "sessionUpdate": "compaction_update", "compactionId": "original", "status": "in_progress" })).unwrap();
        let state = Arc::new(BridgeLock::new(bridge_state));
        reserve_attachment("session", RuntimeSessionOperationKind::Load, &state)
            .await
            .unwrap();
        let mut state = state.lock().await;
        validate_and_track_session_update(session_validation_mut(&mut state, "session").unwrap(), &json!({ "sessionUpdate": "compaction_update", "compactionId": "candidate", "status": "in_progress" })).unwrap();
        clear_attachment_tracking(&mut state, "session", "test-attachment");
        let validation = session_validation_mut(&mut state, "session")
            .expect("failed replay keeps prior validation");
        validate_and_track_session_update(validation, &json!({ "sessionUpdate": "compaction_summary_chunk", "compactionId": "original", "content": { "type": "text", "text": "continued" } })).unwrap();
        assert!(validate_and_track_session_update(validation, &json!({ "sessionUpdate": "compaction_summary_chunk", "compactionId": "candidate", "content": { "type": "text", "text": "must not survive" } })).is_err());
    }

    #[tokio::test]
    async fn automatic_load_blocks_close_delete_and_another_reload_until_its_attempt_finishes() {
        let mut state = BridgeState::default();
        let epoch = state.sessions.epoch().to_string();
        state
            .sessions
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();

        session_mirror(&mut state).register_cold("session");
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
            .begin_load("session", "automatic")
            .unwrap();
        begin_replay_validation(&mut state, "session", "automatic");
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
        let state = Arc::new(BridgeLock::new(state));
        assert!(
            reserve_attachment("session", RuntimeSessionOperationKind::Load, &state)
                .await
                .is_err()
        );
        let mut state = state.lock().await;
        session_mirror(&mut state)
            .fail_load("session", "automatic", "retrying", true)
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
    fn replay_validation_rollback_settles_only_its_own_attempt() {
        let mut state = BridgeState::default();
        let epoch = state.sessions.epoch().to_string();
        state
            .sessions
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();

        begin_replay_validation(&mut state, "session", "old");
        begin_replay_validation(&mut state, "session", "newer");
        rollback_replay_validation(&mut state, "session", "old");
        assert!(replay_attempt_current(&state, "session", "newer"));
        assert!(!replay_attempt_current(&state, "session", "old"));
        state
            .sessions
            .start_operation(
                &epoch,
                "session",
                "close",
                RuntimeSessionOperationKind::Close,
                "closing",
            )
            .unwrap();
        state
            .sessions
            .close_session(&epoch, "session", "close")
            .unwrap();
        complete_session_retirement(&mut state, "session", SessionAdmission::Close, "close");
        state
            .sessions
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        ensure_session_validation(&mut state, "session")
            .unwrap()
            .update_count = 7;
        rollback_replay_validation(&mut state, "session", "newer");
        assert_eq!(
            session_validation(&state, "session").unwrap().update_count,
            7
        );
        assert!(replay_validation_backup(&state, "session").is_none());
    }

    #[test]
    fn an_early_compaction_continues_its_summary_after_creation() {
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
            "response validation does not publish a session owner"
        );
        let epoch = state.sessions.epoch().to_string();
        state
            .sessions
            .open_new(
                &epoch,
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        promote_creation_validation(&mut state, "session").unwrap();

        state.sessions.register_new("session");
        assert!(state.creation_staging.is_empty());
        validate_and_track_session_update(session_validation_mut(&mut state, "session").unwrap(), &json!({
            "sessionUpdate": "compaction_summary_chunk", "compactionId": "early", "content": { "type": "text", "text": "continued after creation" }
        })).unwrap();
    }

    #[tokio::test]
    async fn creation_staging_keeps_cold_resources_separate_and_counts_every_unknown_id() {
        let mut bridge = BridgeState {
            pending_creations: 1,
            ..BridgeState::default()
        };
        bridge.sessions.register_cold("cold");
        bridge.sessions.register_cold("another-cold");
        ensure_session_validation(&mut bridge, "cold").unwrap();
        let cancellation = CancellationToken::new();
        bridge
            .sessions
            .resources_mut("cold")
            .unwrap()
            .materialization = Some(crate::session_resources::MaterializationResources {
            attempt_id: "cold-load".to_string(),
            cancellation: cancellation.clone(),
        });
        let state = Arc::new(BridgeLock::new(bridge));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx };
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
        assert_eq!(state.creation_staging["cold"].validation.update_count, 1);
        assert_eq!(session_validation(&state, "cold").unwrap().update_count, 0);
        assert!(!cancellation.is_cancelled());
        assert!(
            promote_creation_validation(&mut state, "cold").is_err(),
            "promotion cannot overwrite resources of an existing owner"
        );
        assert_eq!(state.creation_staging.len(), 1);
        state.pending_creations = 0;
        clear_pending_creation_replays(&mut state);
        assert_eq!((state.early_update_count, state.early_update_bytes), (0, 0));
        assert!(state.creation_staging.is_empty());
        assert_eq!(session_validation(&state, "cold").unwrap().update_count, 0);
        assert!(!cancellation.is_cancelled());
        drop(state);
        // The rejected extra ID is an operation-scoped failure: logged, not published.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_known_attachment_does_not_consume_creation_staging_or_accept_its_old_owner() {
        let mut bridge = BridgeState {
            pending_creations: 1,
            ..BridgeState::default()
        };
        bridge.sessions.register_cold("session");
        bridge
            .creation_staging
            .entry("session".to_string())
            .or_default();
        let state = Arc::new(BridgeLock::new(bridge));
        reserve_attachment_with_cwd(
            "session",
            RuntimeSessionOperationKind::Load,
            Some("/workspace"),
            "load",
            None,
            &state,
        )
        .await
        .unwrap();
        {
            let mut state = state.lock().await;
            state.sessions.begin_load("session", "load").unwrap();
            assert!(replay_attempt_current(&state, "session", "load"));
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx };
        handle_session_update(serde_json::from_value(json!({
            "sessionId": "session", "update": { "sessionUpdate": "agent_message_chunk", "messageId": "loaded",
                "content": { "type": "text", "text": "attachment replay" } }
        })).unwrap(), &state, &sink, None).await;
        let mut state = state.lock().await;
        assert!(replay_attempt_current(&state, "session", "load"));
        assert_eq!(
            state.creation_staging["session"].early_notifications.len(),
            0
        );
        assert_eq!((state.early_update_count, state.early_update_bytes), (0, 0));
        assert_eq!(
            state
                .sessions
                .load_candidate("session", "load")
                .unwrap()
                .lock()
                .updates()
                .unwrap()
                .len(),
            1
        );
        state.pending_creations = 0;
        clear_pending_creation_replays(&mut state);
        assert!(session_validation(&state, "session").is_some());
        assert!(replay_attempt_current(&state, "session", "load"));
        drop(state);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn attachment_updates_stay_candidate_only_until_agent_response() {
        let mut bridge_state = BridgeState::default();
        let epoch = bridge_state.sessions.epoch().to_string();
        bridge_state
            .sessions
            .start_attachment(
                &epoch,
                "session",
                "/workspace",
                "load",
                RuntimeSessionOperationKind::Load,
            )
            .unwrap();
        session_mirror(&mut bridge_state).register_cold("session");
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
        bridge_state.sessions.begin_load("session", "load").unwrap();
        clear_attachment_delivery(&mut bridge_state, "session");
        let state = Arc::new(BridgeLock::new(bridge_state));
        let (tx, _rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx };
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
        assert!(live_session(&state, "session"));
        assert!(apply_terminal_snapshot(
            &mut state,
            TerminalSnapshot {
                value: json!({
                    "sessionId": "session",
                    "terminalId": "pre-response-terminal",
                    "released": false,
                }),
            },
        ));
        assert!(
            !serde_json::to_string(&state.sessions.session("session"))
                .unwrap()
                .contains("candidate")
        );
        state.sessions.commit_load("session", "load").unwrap();
        assert_eq!(
            state.sessions.state("session").unwrap().history,
            Some(crate::protocol::timeline::History::Complete)
        );
        state
            .sessions
            .complete_attachment(
                &epoch,
                "session",
                "load",
                RuntimeSessionOperationKind::Load,
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        assert!(
            !serde_json::to_string(&state.sessions.session("session"))
                .unwrap()
                .contains("candidate"),
            "history payload belongs to the shared cache, not the live state",
        );
        assert!(
            state
                .sessions
                .session("session")
                .unwrap()
                .terminals
                .contains_key("pre-response-terminal")
        );
    }

    #[tokio::test]
    async fn invalid_same_session_reload_update_is_private_and_poisoned_for_rollback() {
        let mut bridge_state = BridgeState::default();
        register_test_session(&mut bridge_state, "session", "/workspace");
        let state = Arc::new(BridgeLock::new(bridge_state));
        reserve_attachment("session", RuntimeSessionOperationKind::Load, &state)
            .await
            .unwrap();
        {
            let mut state = state.lock().await;
            state
                .sessions
                .begin_load("session", "test-attachment")
                .unwrap();
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx };
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
        assert!(state.sessions.session("session").is_some());
        drop(state);
        assert!(
            rx.try_recv().is_err(),
            "invalid reload input is reported through the load outcome"
        );
    }

    #[tokio::test]
    async fn invalid_early_updates_prevent_session_commit_and_clear_cleanly() {
        let state = Arc::new(BridgeLock::new(BridgeState {
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
        assert!(
            rx.try_recv().is_err(),
            "invalid early replay is reported by the creation outcome, not published"
        );

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
    fn relays_large_values_without_replacement() {
        assert!(relay_bytes(&json!({ "ok": true })).is_ok());
        assert!(relay_bytes(&json!({ "padding": "x".repeat(FORMER_AGENT_RELAY_BYTES) })).is_ok());
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
        bridge
            .sessions
            .start_attachment(
                &epoch,
                "session",
                workspace.path(),
                "load",
                RuntimeSessionOperationKind::Load,
            )
            .unwrap();
        let state = Arc::new(BridgeLock::new(bridge));
        let filesystem = require_session_workspace("session", &state, &filesystem)
            .await
            .unwrap();
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
                "load",
                RuntimeSessionOperationKind::Load,
                json!({}),
            )
            .unwrap();
        locked.sessions.register_new("session");
        locked
            .sessions
            .start_operation(
                &epoch,
                "session",
                "close",
                RuntimeSessionOperationKind::Close,
                "closing",
            )
            .unwrap();
        locked
            .sessions
            .close_session(&epoch, "session", "close")
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
        let epoch = state.sessions.epoch().to_string();
        state
            .sessions
            .start_delete(&epoch, "session-1", "delete")
            .unwrap();
        state
            .sessions
            .delete_close_succeeded(&epoch, "session-1", "delete")
            .unwrap();
        state
            .sessions
            .fail_operation(
                &epoch,
                "session-1",
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
        assert!(matches!(replacement, AttachmentReservation::Fresh { .. }));
        assert_eq!(
            state.sessions.allocated_session_count(),
            FORMER_TRACKED_SESSIONS + 1
        );
    }

    #[tokio::test]
    async fn reserves_listed_sessions_once_and_rejects_busy_attachments() {
        let state = Arc::new(BridgeLock::new(BridgeState::default()));
        state.lock().await.listed_sessions.insert(
            "saved".to_string(),
            session_info("saved", "/agent/workspace"),
        );
        let reservation = reserve_attachment("saved", RuntimeSessionOperationKind::Load, &state)
            .await
            .unwrap();
        let AttachmentReservation::Fresh { cwd } = reservation else {
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
                .begin_load("saved", "test-attachment")
                .unwrap();
            state
                .sessions
                .complete_attachment(
                    &epoch,
                    "saved",
                    "test-attachment",
                    RuntimeSessionOperationKind::Load,
                    json!({ "sessionId": "saved" }),
                )
                .unwrap();
            session_mirror(&mut state)
                .commit_load("saved", "test-attachment")
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
        register_test_session(&mut bridge, "session", "/agent/workspace");
        let state = Arc::new(BridgeLock::new(bridge));
        let reservation = reserve_attachment("session", RuntimeSessionOperationKind::Load, &state)
            .await
            .expect("an idle tracked session must reach the Agent's session/load implementation");
        assert_eq!(
            reservation,
            AttachmentReservation::Reload {
                cwd: PathBuf::from("/agent/workspace"),
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
                    "test-attachment",
                    json!({ "sessionId": "session" }),
                )
                .unwrap();
            clear_attachment_tracking(&mut state, "session", "test-attachment");
            let operation = start_test_turn(&mut state, "session", "prompt").unwrap();
            // The live projection can retire its turn before the owner commits
            // history. Reconciling must still exclude another load in that window.
            let terminal = json!({ "stopReason": "end_turn" });
            state
                .sessions
                .complete_prompt(&epoch, "session", "prompt", terminal.clone())
                .unwrap();
            session_mirror(&mut state)
                .complete_turn("session", &operation, terminal)
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
            mirror.commit_completed_turn("session", &operation).unwrap();
        }
        assert!(
            reserve_attachment("session", RuntimeSessionOperationKind::Load, &state)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn cold_materialization_moves_to_the_installed_session_once() {
        let mut bridge = BridgeState::default();
        let command =
            prepare_session_load(&mut bridge, "saved".into(), Some("/workspace".into())).unwrap();
        let owner = command["bridgeMaterializationId"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            prepare_session_load(&mut bridge, "saved".into(), Some("/workspace".into())).is_none()
        );
        let token = session_materialization(&bridge, "saved")
            .unwrap()
            .cancellation
            .clone();
        let state = Arc::new(BridgeLock::new(bridge));
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
        assert!(matches!(reservation, AttachmentReservation::Fresh { .. }));
        assert!(!token.is_cancelled());
        {
            let state = state.lock().await;
            let resources = state.sessions.resources("saved").unwrap();
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
                "attempt",
                RuntimeSessionOperationKind::Load,
                false,
                &not_found,
            )
            .unwrap();
            state.sessions.remove("saved");
            assert!(
                !token.is_cancelled(),
                "history removal must leave delivery to the load result owner"
            );
            assert!(session_materialization(&state, "saved").is_some());
        }
        finish_materialization(&mut *state.lock().await, "saved", &owner, &Err(not_found));
        assert!(token.is_cancelled());
        assert!(state.lock().await.sessions.state("saved").is_none());
    }

    #[tokio::test]
    async fn only_the_owner_can_settle_a_materialization() {
        let state = Arc::new(BridgeLock::new(BridgeState::default()));
        {
            let mut state = state.lock().await;
            state.sessions.register_cold("session");
            state
                .sessions
                .resources_mut("session")
                .unwrap()
                .materialization = Some(crate::session_resources::MaterializationResources {
                attempt_id: "owner".into(),
                cancellation: CancellationToken::new(),
            });
        }

        let failed = Err(Error::internal_error().data("unrelated load"));
        finish_materialization(&mut *state.lock().await, "session", "not-owner", &failed);
        assert_eq!(
            session_materialization(&*state.lock().await, "session")
                .map(|owner| owner.attempt_id.as_str()),
            Some("owner")
        );

        finish_materialization(&mut *state.lock().await, "session", "owner", &failed);
        assert!(session_materialization(&*state.lock().await, "session").is_none());
    }

    #[test]
    fn serializes_prompts_lifecycle_controls_and_deletion_attachment_races() {
        let mut state = BridgeState::default();
        register_test_session(&mut state, "active", "/agent/workspace");
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
        release_session_operation(&mut state, "active", SessionAdmission::Control, "not-owner");
        assert!(session_operation_pending(
            &state,
            "active",
            SessionAdmission::Control
        ));
        release_session_operation(&mut state, "active", SessionAdmission::Control, "control");
        reserve_session_operation(
            &mut state,
            "active",
            RuntimeSessionOperationKind::Close,
            "close",
        )
        .unwrap();
        assert!(session_prompt_pending(&state, "active"));
        release_session_operation(&mut state, "active", SessionAdmission::Close, "close");
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
        release_session_operation(&mut state, "active", SessionAdmission::Close, "idle-close");

        state.listed_sessions.insert(
            "saved".to_string(),
            session_info("saved", "/agent/workspace"),
        );
        session_mirror(&mut state).register_cold("saved");
        session_mirror(&mut state)
            .begin_exclusive("saved", RuntimeSessionOperationKind::Load, "attachment")
            .unwrap();
        assert!(reserve_deletion(&mut state, "saved", "delete").is_err());
        release_session_operation(
            &mut state,
            "saved",
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
                state.sessions.register_cold("remote");
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
        let state = Arc::new(BridgeLock::new(BridgeState::default()));
        let operation = {
            let mut state = state.lock().await;
            register_test_session(&mut state, "session", "/workspace");
            start_test_turn(&mut state, "session", "prompt").unwrap()
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
        let state = Arc::new(BridgeLock::new(BridgeState {
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
        let scope = owner.as_ref().map(|owner| owner.session_id.as_str());
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
            register_test_session(&mut state, "session", "/workspace");
            let owner =
                (!request_scoped).then(|| state.sessions.resource_owner("session").unwrap());
            let old = register_test_url(&mut state, owner.clone(), "old-registration");
            let epoch = state.sessions.epoch().to_string();
            let scope = owner.as_ref().map(|owner| owner.session_id.as_str());
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
                elicitation_id: "reusable-url".to_string(),
                registration_id: registration.registration_id.clone(),
            };
            state.sessions.effects.push_back(abort(&old));
            assert!(drain_runtime_effects(&mut state).is_empty());
            assert_eq!(
                state.sessions.url_owners.get("reusable-url"),
                Some(&current)
            );
            // Once this precise registration retires, its duplicated local effects
            // remove the lightweight route exactly once.
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
            assert!(drain_runtime_effects(&mut state).is_empty());
            assert!(!state.sessions.url_owners.contains_key("reusable-url"));
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
                .upsert_permission(&owner.epoch, &owner.session_id, &permission_id, json!({}))
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
                    Some(&owner.session_id),
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
        let state = Arc::new(BridgeLock::new(BridgeState::default()));
        let (tx, _events) = mpsc::unbounded_channel();
        let sink = EventSink { tx };
        let mut locked = state.lock().await;
        register_test_session(&mut locked, "session", "/workspace");
        let operation = start_test_turn(&mut locked, "session", "old-intent").unwrap();
        let owner = locked.sessions.resource_owner("session").unwrap();
        let (mut old_permission, mut old_elicitation) = pending(&mut locked, &owner, "old");
        let old_url = register_test_url(&mut locked, Some(owner.clone()), "old-url");
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
        commit_session_cancel(&mut locked, &sink, "session").unwrap();
        assert!(matches!(
            old_permission.try_recv().unwrap().outcome,
            RequestPermissionOutcome::Cancelled
        ));
        assert!(matches!(
            old_elicitation.try_recv().unwrap().action,
            ElicitationAction::Cancel
        ));
        // The accepted flow belongs to the session scope, not to the turn, so
        // the cancel keeps it (S7). Only its completion frees the ID.
        assert_eq!(
            locked.sessions.url_owners.get("reusable-url"),
            Some(&old_url)
        );
        assert_eq!(
            locked.sessions.live("session").unwrap().url_flows["reusable-url"].registration_id,
            "old-url"
        );
        let epoch = locked.sessions.epoch().to_string();
        locked
            .sessions
            .settle_url_registration(
                &epoch,
                Some("session"),
                "reusable-url",
                &old_url.registration_id,
                UrlFlowStatus::Completed,
            )
            .unwrap();
        locked
            .sessions
            .take_url_route("reusable-url", &old_url)
            .unwrap();
        complete_test_turn(&mut locked, "session", &operation);
        drop(locked);
        let ((mut permission, mut elicitation), registration) = next.await.unwrap();
        // The old command's eventual effect drain has no authority over the new turn.
        apply_runtime_effects(&state, None).await;
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
    async fn a_prompts_end_leaves_the_owned_responders_to_their_own_lifetime() {
        let (permission_sender, permission_receiver) = oneshot::channel();
        let (elicitation_sender, elicitation_receiver) = oneshot::channel();
        let state = Arc::new(BridgeLock::new(BridgeState::default()));
        {
            let mut state = state.lock().await;
            let epoch = state.sessions.epoch().to_string();
            state
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
                .start_prompt(&epoch, "session", "prompt", Vec::new())
                .unwrap();
            state
                .sessions
                .upsert_permission(
                    &epoch,
                    "session",
                    "permission",
                    json!({ "sessionId": "session" }),
                )
                .unwrap();
            state
                .sessions
                .upsert_elicitation(
                    &epoch,
                    Some("session"),
                    "elicitation",
                    json!({ "sessionId": "session", "mode": "form" }),
                )
                .unwrap();
            let owner = state.sessions.resource_owner("session").unwrap();
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
                    "prompt",
                    json!({ "stopReason": "end_turn" }),
                )
                .unwrap();
        }
        apply_runtime_effects(&state, None).await;

        // Nothing answered them: they are still the session's, and answered by its close.
        let mut permission_receiver = permission_receiver;
        let mut elicitation_receiver = elicitation_receiver;
        assert!(permission_receiver.try_recv().is_err());
        assert!(elicitation_receiver.try_recv().is_err());
        {
            let state = state.lock().await;
            assert_eq!(state.sessions.permission_owners.len(), 1);
            assert_eq!(state.sessions.elicitation_owners.len(), 1);
        }
        let sink = EventSink { tx: mpsc::unbounded_channel().0 };
        cancel_interactions("session", &state, &sink).await;
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
    }

    #[test]
    fn retrying_a_partially_failed_delete_reuses_the_closed_runtime_session() {
        let mut state = BridgeState::default();
        let epoch = state.sessions.epoch().to_string();
        state
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
            .start_delete(&epoch, "session", "first-delete")
            .unwrap();
        state
            .sessions
            .delete_close_succeeded(&epoch, "session", "first-delete")
            .unwrap();
        state
            .sessions
            .fail_operation(
                &epoch,
                "session",
                "first-delete",
                RuntimeSessionOperationKind::Delete,
                json!("delete failed"),
            )
            .unwrap();

        assert!(begin_runtime_delete(&mut state, "session", "retry-delete", false).unwrap());
        state
            .sessions
            .complete_delete(&epoch, "session", "retry-delete")
            .unwrap();
        assert!(state.sessions.session("session").is_none());
    }

    #[test]
    fn close_before_delete_marks_the_session_deleting_before_the_delete_rpc_wait() {
        let mut state = BridgeState::default();
        let epoch = state.sessions.epoch().to_string();
        state
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
            .start_delete(&epoch, "session", "delete")
            .unwrap();

        commit_runtime_delete_close_success(&mut state, "session", "delete").unwrap();

        assert_eq!(
            state.sessions.live("session").unwrap().lifecycle,
            SessionLifecycle::Deleting
        );
    }

    #[tokio::test]
    async fn close_cleanup_tombstone_blocks_same_id_reattachment() {
        let state = Arc::new(BridgeLock::new(BridgeState::default()));
        {
            let mut state = state.lock().await;
            register_test_session(&mut state, "session", "/workspace");
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
                    "close",
                    RuntimeSessionOperationKind::Close,
                    "closing",
                )
                .unwrap();
            state
                .sessions
                .close_session(&epoch, "session", "close")
                .unwrap();

            state
                .listed_sessions
                .insert("session".to_string(), session_info("session", "/workspace"));
        }
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
                "session",
                SessionAdmission::Close,
                "not-owner",
            );
            assert!(session_operation_pending(
                &state,
                "session",
                SessionAdmission::Close
            ));
            complete_session_retirement(&mut state, "session", SessionAdmission::Close, "close");
        }
        assert!(
            reserve_attachment("session", RuntimeSessionOperationKind::Load, &state)
                .await
                .is_ok()
        );
    }

    #[test]
    fn transport_eof_is_uncertain_but_an_agent_error_is_definite_failure() {
        let mut uncertain = BridgeState::default();
        let epoch = uncertain.sessions.epoch().to_string();
        uncertain
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
            .start_prompt(&epoch, "uncertain", "prompt", Vec::new())
            .unwrap();
        let transport_error = Error::internal_error().data(json!({
            "reason": "incoming_transport_closed",
            "method": "session/prompt",
        }));
        settle_runtime_prompt_request_error(
            &mut uncertain,
            "uncertain",
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
        failed
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
            .start_prompt(&epoch, "failed", "prompt", Vec::new())
            .unwrap();
        settle_runtime_prompt_request_error(
            &mut failed,
            "failed",
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
        let state = Arc::new(BridgeLock::new(BridgeState::default()));
        {
            let mut state = state.lock().await;
            let epoch = state.sessions.epoch().to_string();
            state
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
                    "permission",
                    json!({ "sessionId": "session" }),
                )
                .unwrap();
            state
                .sessions
                .upsert_elicitation(
                    &epoch,
                    Some("session"),
                    "session-elicitation",
                    json!({ "sessionId": "session", "mode": "form" }),
                )
                .unwrap();
            state
                .sessions
                .upsert_elicitation(
                    &epoch,
                    Some("session"),
                    "accepted-session-url",
                    json!({ "sessionId": "session", "mode": "url" }),
                )
                .unwrap();
            state
                .sessions
                .resolve_elicitation(
                    &epoch,
                    Some("session"),
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
            let owner = state.sessions.resource_owner("session").unwrap();
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
        let sink = EventSink { tx };
        cancel_interactions("session", &state, &sink).await;

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
        let runtime_session = state.sessions.live("session").unwrap();
        assert!(runtime_session.permissions.is_empty());
        assert!(runtime_session.elicitations.is_empty());
        assert!(
            !runtime_session.url_flows.contains_key("session-url"),
            "a terminal URL flow must be removed instead of retained as completed state"
        );
        assert!(
            state
                .sessions
                .request_elicitations
                .contains_key("request-elicitation")
        );
        assert_eq!(
            state.sessions.request_url_flows["request-url"].status,
            UrlFlowStatus::Waiting
        );
        drop(state);
        let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(
            events.is_empty(),
            "cancellation is reflected in canonical state, not events: {events:?}"
        );
    }

    #[tokio::test]
    async fn preserves_early_update_bursts_until_creation_completes() {
        let state = Arc::new(BridgeLock::new(BridgeState {
            pending_creations: 1,
            ..BridgeState::default()
        }));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink { tx };
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
        _events: mpsc::UnboundedReceiver<BridgeEvent>,
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
                EventSink { tx: events },
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

        async fn stop(self) {
            self.cancellation.cancel();
            tokio::time::timeout(Duration::from_secs(3), self.task)
                .await
                .expect("catalog connection did not stop")
                .unwrap()
                .unwrap();
        }
    }

    /// A list waits for the Agent's page on the catalog revision it was
    /// admitted at: a create that commits meanwhile makes that page stale.
    #[tokio::test]
    async fn a_list_in_flight_returns_no_page_from_before_a_catalog_change() {
        let mut wire = CatalogWire::start().await;
        let mut stream =
            super::tests::V2Stream::open(&wire.commands, &[("catalog", None), ("global", None)])
                .await;
        stream
            .until(|stream| stream.live("catalog").is_object() && stream.live("global").is_object())
            .await;
        let revision = stream.live("catalog")["revision"].clone();
        let listing = stream
            .write(
                &wire.commands,
                "catalog",
                "list",
                json!({"revision": revision}),
            )
            .await;
        let list = wire.next_request().await;
        assert_eq!(list["method"], "session/list");
        let creating = stream
            .write(
                &wire.commands,
                "global",
                "create",
                json!({"cwd": "/workspace"}),
            )
            .await;
        let new = wire.next_request().await;
        assert_eq!(new["method"], "session/new");
        wire.reply(&new, json!({"sessionId": "created"})).await;
        tokio::time::timeout(Duration::from_secs(3), creating.end.unwrap())
            .await
            .expect("the create did not end")
            .unwrap()
            .unwrap();
        wire.reply(&list, json!({"sessions": [], "nextCursor": null}))
            .await;
        let stale = tokio::time::timeout(Duration::from_secs(3), listing.end.unwrap())
            .await
            .expect("the list did not end")
            .unwrap();
        assert!(stale.is_err(), "a stale page was returned: {stale:?}");
        stream
            .until(|stream| {
                stream.live("catalog")["op"]["id"] == listing.op
                    && stream.live("catalog")["op"]["status"] == "failed"
            })
            .await;
        assert_ne!(stream.live("catalog")["revision"], revision);
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
            EventSink { tx: events },
            CancellationToken::new(),
        )
        .unwrap();
        let handle = scheduling.register_session("session").unwrap();
        for _ in 0..2 {
            ingress
                .try_browser(BridgeInput::ServeStreams, TrafficClass::Ordinary)
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
                        rpc_owner("epoch", Some("session"), &format!("occupied-{index}"), None),
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
                let owner = rpc_owner("epoch", Some("session"), "prompt", None);
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
                    .try_browser(BridgeInput::ServeStreams, TrafficClass::Ordinary)
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
            EventSink { tx: events },
            CancellationToken::new(),
        )
        .unwrap();
        ingress
            .try_browser(BridgeInput::ServeStreams, TrafficClass::Ordinary)
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
                    committed,
                } = item.event
                else {
                    panic!("raw target claim was not released")
                };
                assert_eq!(finished_owner, owner);
                assert_eq!(target_id, "target");
                assert!(!committed);
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
