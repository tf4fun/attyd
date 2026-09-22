use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::session_mirror::TurnOverlay;
use crate::session_registry::{SessionEntry, SessionRegistry};
use crate::session_state::{MirrorError, SessionAdmission, SessionState, TurnAdmission};

#[cfg(test)]
pub(crate) use crate::session_registry::SessionRegistry as RuntimeState;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeSnapshot {
    pub epoch: String,
    pub through_seq: u64,
    pub connection_revision: u64,
    pub sessions: BTreeMap<String, SessionRuntime>,
    pub request_elicitations: BTreeMap<String, PendingInteraction>,
    pub request_url_flows: BTreeMap<String, UrlFlow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionRuntime {
    pub session_id: String,
    pub incarnation: u64,
    pub revision: u64,
    pub cwd: String,
    pub session: Value,
    pub control_state: BTreeMap<String, Value>,
    pub lifecycle: SessionLifecycle,
    pub active_turn: Option<ActiveTurn>,
    pub operation: Option<SessionOperationState>,
    pub permissions: BTreeMap<String, PendingInteraction>,
    pub elicitations: BTreeMap<String, PendingInteraction>,
    pub url_flows: BTreeMap<String, UrlFlow>,
    pub terminals: BTreeMap<String, Value>,
    // View header fields let a downstream hub build a reset recovery hint
    // without holding the mirror's private session state.
    #[serde(rename = "baselineRevision")]
    pub history_revision: Option<String>,
    pub phase: crate::session_state::MirrorPhase,
    pub sync_error: Option<String>,
}

/// Mutable live resources and lifecycle. Operation and turn execution belong to SessionState.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SessionLiveState {
    pub revision: u64,
    pub cwd: PathBuf,
    pub session: Value,
    pub control_state: BTreeMap<String, Value>,
    pub lifecycle: SessionLifecycle,
    pub permissions: BTreeMap<String, PendingInteraction>,
    pub elicitations: BTreeMap<String, PendingInteraction>,
    pub url_flows: BTreeMap<String, UrlFlow>,
    pub terminals: BTreeMap<String, Value>,
    // Mirror view headers captured at the last commit so a journal snapshot or
    // hub reset only ever exposes the published view, never uncommitted state.
    pub history_revision: Option<String>,
    pub phase: crate::session_state::MirrorPhase,
    pub sync_error: Option<String>,
    resolved_permissions: VecDeque<String>,
    resolved_elicitations: VecDeque<String>,
    resolved_url_flows: VecDeque<String>,
    released_terminals: VecDeque<String>,
    attachment_candidate: Vec<Value>,
    attachment_candidate_bytes: usize,
}

impl SessionLiveState {
    pub(crate) fn modes(&self) -> Option<&Value> {
        self.session.get("modes").filter(|value| !value.is_null())
    }

    #[cfg(test)]
    pub(crate) fn current_mode_id(&self) -> Option<&str> {
        self.control_state
            .get("current_mode_update")
            .and_then(|update| update.get("currentModeId"))
            .or_else(|| self.modes().and_then(|modes| modes.get("currentModeId")))
            .and_then(Value::as_str)
    }

    pub(crate) fn config_options(&self) -> &Value {
        static EMPTY: Value = Value::Array(Vec::new());
        self.control_state
            .get("config_option_update")
            .and_then(|update| update.get("configOptions"))
            .or_else(|| self.session.get("configOptions"))
            .filter(|value| value.is_array())
            .unwrap_or(&EMPTY)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionLifecycle {
    Attaching,
    Active,
    Closing,
    ClosingForDelete,
    Deleting,
    Closed,
    Uncertain,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ActiveTurn {
    pub operation_id: String,
    pub turn_id: String,
    pub prompt: Arc<Vec<Value>>,
    pub updates: SharedTurnUpdates,
    pub cancel_requested: bool,
}

pub(crate) type SharedTurnUpdates = Arc<Vec<Arc<Value>>>;

struct TurnTerminal {
    lifecycle: Option<SessionLifecycle>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionOperationState {
    pub operation_id: String,
    pub kind: SessionOperationKind,
    pub stage: String,
    pub uncertainty_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionOperationKind {
    Load,
    Resume,
    Fork,
    Close,
    Delete,
    SetMode,
    SetConfig,
}

impl SessionOperationKind {
    pub(crate) fn admission(self) -> SessionAdmission {
        match self {
            Self::Load | Self::Resume => SessionAdmission::Attachment,
            Self::Fork => SessionAdmission::Fork,
            Self::Close => SessionAdmission::Close,
            Self::Delete => SessionAdmission::Delete,
            Self::SetMode | Self::SetConfig => SessionAdmission::Control,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionTurnError {
    History(MirrorError),
    Live(RuntimeStateError),
}

impl From<MirrorError> for SessionTurnError {
    fn from(error: MirrorError) -> Self {
        Self::History(error)
    }
}
impl From<RuntimeStateError> for SessionTurnError {
    fn from(error: RuntimeStateError) -> Self {
        Self::Live(error)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingInteraction {
    pub interaction_id: String,
    pub request: Value,
    pub operation_id: Option<String>,
    pub responding_operation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UrlFlow {
    pub elicitation_id: String,
    #[serde(skip)]
    pub(crate) registration_id: String,
    pub request: Value,
    pub status: UrlFlowStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UrlFlowStatus {
    Waiting,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeDelta {
    pub epoch: String,
    pub seq: u64,
    pub scope_revision: Option<u64>,
    pub change: RuntimeChange,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum RuntimeChange {
    ConnectionUpsert {
        request_elicitations: BTreeMap<String, PendingInteraction>,
        request_url_flows: BTreeMap<String, UrlFlow>,
    },
    SessionUpsert {
        session: Box<SessionRuntime>,
    },
    SessionControlUpdated {
        session_id: String,
        incarnation: u64,
        revision: u64,
        key: String,
        update: Value,
        history_revision: Option<String>,
        phase: crate::session_state::MirrorPhase,
        sync_error: Option<String>,
    },
    TurnUpdateAppended {
        session_id: String,
        incarnation: u64,
        revision: u64,
        operation_id: String,
        update: Value,
    },
    TerminalUpdated {
        session_id: String,
        incarnation: u64,
        revision: u64,
        terminal_id: String,
        terminal: Value,
    },
    SessionRemoved {
        session_id: String,
        incarnation: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InteractionResolution {
    Applied,
    AlreadyResolved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InteractionResponseStart {
    Applied,
    Duplicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UrlFlowResolution {
    Applied,
    AlreadyTerminal,
    StaleRegistration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InteractionUpsert {
    Inserted,
    Duplicate,
    AlreadyResolved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalUpsert {
    Inserted,
    Updated,
    Duplicate,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeEffect {
    ResourcesCancelled {
        session_id: String,
        incarnation: u64,
        permission_ids: Vec<String>,
        elicitation_ids: Vec<String>,
        url_ids: Vec<(String, String)>,
        reason: String,
    },
    CancelPermissionResponder {
        session_id: String,
        incarnation: u64,
        interaction_id: String,
    },
    CancelElicitationResponder {
        session_id: Option<String>,
        incarnation: Option<u64>,
        interaction_id: String,
    },
    AbortUrlFlow {
        session_id: Option<String>,
        incarnation: Option<u64>,
        elicitation_id: String,
        registration_id: String,
    },
    ReleaseTerminal {
        session_id: String,
        terminal_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeStateError {
    EpochMismatch,
    UnknownSession,
    StaleIncarnation,
    BusySession,
    NoActiveTurn,
    OperationMismatch,
    UnknownInteraction,
    SessionNotActive,
    OperationCollision,
    InteractionCollision,
}

/// A journaled delta plus its serialized length, so a release or retirement
/// does not have to serialize the payload a second time to keep `bytes` exact.
#[derive(Serialize)]
struct JournalDelta {
    #[serde(flatten)]
    delta: RuntimeDelta,
    #[serde(skip)]
    bytes: usize,
}

/// Retains the published delta suffix without owning session business state.
/// The caller supplies committed changes and decides when a turn is retired.
pub(crate) struct RuntimeJournal {
    seq: u64,
    deltas: VecDeque<JournalDelta>,
    bytes: usize,
}

impl RuntimeJournal {
    pub(crate) fn new() -> Self {
        Self {
            seq: 0,
            deltas: VecDeque::new(),
            bytes: 0,
        }
    }

    fn through_seq(&self) -> u64 {
        self.seq
    }

    fn deltas_after(&self, seq: u64) -> Option<Vec<RuntimeDelta>> {
        if seq > self.seq {
            return None;
        }
        let first = self
            .deltas
            .front()
            .map_or(self.seq + 1, |entry| entry.delta.seq);
        if seq.saturating_add(1) < first {
            return None;
        }
        Some(
            self.deltas
                .iter()
                .filter(|entry| entry.delta.seq > seq)
                .map(|entry| entry.delta.clone())
                .collect(),
        )
    }

    fn commit(&mut self, epoch: &str, scope_revision: Option<u64>, change: RuntimeChange) {
        self.seq = self.seq.wrapping_add(1).max(1);
        let delta = RuntimeDelta {
            epoch: epoch.to_string(),
            seq: self.seq,
            scope_revision,
            change,
        };
        let bytes = serialized_len(&delta);
        self.bytes = self.bytes.saturating_add(bytes);
        self.deltas.push_back(JournalDelta { delta, bytes });
    }

    /// Drop the contiguous prefix that a successful publication covered.
    /// Unpublished records must stay so a later flush can still deliver them.
    fn release_through(&mut self, seq: u64) {
        while let Some(entry) = self.deltas.front() {
            if entry.delta.seq > seq {
                break;
            }
            let bytes = entry.bytes;
            self.deltas.pop_front();
            self.bytes = self.bytes.saturating_sub(bytes);
        }
    }

    fn retire_turn_payload(&mut self, session_id: &str, incarnation: u64, operation_id: &str) {
        // A journal suffix must remain sequence-contiguous. Removing only this session's entries
        // would leave holes when sessions interleave, so invalidate the whole prefix through the
        // last delta that could own this turn's payload.
        let discard_through = self
            .deltas
            .iter()
            .filter(|entry| match &entry.delta.change {
                RuntimeChange::SessionUpsert { session } => {
                    session.session_id == session_id && session.incarnation == incarnation
                }
                RuntimeChange::SessionControlUpdated { .. } => false,
                RuntimeChange::TurnUpdateAppended {
                    session_id: delta_session_id,
                    incarnation: delta_incarnation,
                    operation_id: delta_operation_id,
                    ..
                } => {
                    (delta_session_id == session_id && *delta_incarnation == incarnation)
                        || delta_operation_id == operation_id
                }
                RuntimeChange::TerminalUpdated { .. }
                | RuntimeChange::ConnectionUpsert { .. }
                | RuntimeChange::SessionRemoved { .. } => false,
            })
            .map(|entry| entry.delta.seq)
            .max();
        if let Some(discard_through) = discard_through {
            while self
                .deltas
                .front()
                .is_some_and(|entry| entry.delta.seq <= discard_through)
            {
                self.deltas.pop_front();
            }
        }
        self.bytes = self.deltas.iter().map(|entry| entry.bytes).sum();
    }
}

impl SessionRegistry {
    pub(crate) fn epoch(&self) -> &str {
        &self.epoch
    }

    pub(crate) fn snapshot(&self) -> RuntimeSnapshot {
        RuntimeSnapshot {
            epoch: self.epoch.clone(),
            through_seq: self.journal.through_seq(),
            connection_revision: self.connection_revision,
            sessions: self
                .sessions
                .iter()
                .filter_map(|(id, _state)| self.session(id).map(|session| (id.clone(), session)))
                .collect(),
            request_elicitations: self.request_elicitations.clone(),
            request_url_flows: self.request_url_flows.clone(),
        }
    }

    pub(crate) fn deltas_after(&self, seq: u64) -> Option<Vec<RuntimeDelta>> {
        self.journal.deltas_after(seq)
    }

    pub(crate) fn release_published_runtime(&mut self, seq: u64) {
        self.journal.release_through(seq);
    }

    /// Build a disposable wire projection from the one mutable session owner.
    pub(crate) fn session(&self, session_id: &str) -> Option<SessionRuntime> {
        let state = &self.sessions.get(session_id)?.state;
        let live = state.live.as_ref()?;
        if live.lifecycle == SessionLifecycle::Closed
            && state
                .operation
                .as_ref()
                .is_some_and(|operation| operation.stage == "cleanup")
        {
            return None;
        }
        Some(SessionRuntime {
            session_id: state.session_id.clone(),
            incarnation: state.incarnation,
            revision: live.revision,
            cwd: live.cwd.to_string_lossy().into_owned(),
            session: live.session.clone(),
            control_state: live.control_state.clone(),
            lifecycle: live.lifecycle.clone(),
            history_revision: live.history_revision.clone(),
            phase: live.phase,
            sync_error: live.sync_error.clone(),
            active_turn: state.active_turn.as_ref().and_then(|turn| {
                turn.execution.as_ref().map(|execution| ActiveTurn {
                    operation_id: execution.rpc_operation_id.clone(),
                    turn_id: execution.rpc_operation_id.clone(),
                    prompt: turn.prompt.clone(),
                    updates: turn.updates.clone(),
                    cancel_requested: execution.cancel_requested,
                })
            }),
            operation: state.operation.clone(),
            permissions: live.permissions.clone(),
            elicitations: live.elicitations.clone(),
            url_flows: live.url_flows.clone(),
            terminals: live.terminals.clone(),
        })
    }

    pub(crate) fn live(&self, session_id: &str) -> Option<&SessionLiveState> {
        self.sessions
            .get(session_id)
            .and_then(|entry| entry.state.live.as_ref())
    }

    pub(crate) fn take_effects(&mut self) -> Vec<RuntimeEffect> {
        self.effects.drain(..).collect()
    }

    pub(crate) fn open_new_with_replay(
        &mut self,
        expected_epoch: &str,
        session_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
        session: Value,
        replay: Vec<Value>,
    ) -> Result<u64, RuntimeStateError> {
        let control_state = extract_control_state(replay);
        self.open_session(
            expected_epoch,
            session_id.into(),
            cwd.into(),
            session,
            control_state,
        )
    }

    pub(crate) fn start_attachment(
        &mut self,
        expected_epoch: &str,
        session_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
        operation_id: impl Into<String>,
        kind: SessionOperationKind,
    ) -> Result<u64, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        if !matches!(
            kind,
            SessionOperationKind::Load | SessionOperationKind::Resume
        ) {
            return Err(RuntimeStateError::OperationMismatch);
        }
        let session_id = session_id.into();
        let operation_id = operation_id.into();
        if self.operation_in_use(&operation_id) {
            return Err(RuntimeStateError::OperationCollision);
        }
        if self.sessions.get(&session_id).is_some_and(|entry| {
            let state = &entry.state;
            state.operation.is_some()
                || state
                    .live
                    .as_ref()
                    .is_some_and(|live| live.lifecycle != SessionLifecycle::Closed)
        }) {
            return Err(RuntimeStateError::BusySession);
        }
        self.next_incarnation = self.next_incarnation.wrapping_add(1).max(1);
        let incarnation = self.next_incarnation;
        self.install_live(
            &session_id,
            incarnation,
            SessionLiveState {
                revision: 0,
                cwd: cwd.into(),
                session: Value::Null,
                control_state: BTreeMap::new(),
                lifecycle: SessionLifecycle::Attaching,
                permissions: BTreeMap::new(),
                elicitations: BTreeMap::new(),
                url_flows: BTreeMap::new(),
                terminals: BTreeMap::new(),
                resolved_permissions: VecDeque::new(),
                resolved_elicitations: VecDeque::new(),
                resolved_url_flows: VecDeque::new(),
                released_terminals: VecDeque::new(),
                attachment_candidate: Vec::new(),
                attachment_candidate_bytes: 0,
                history_revision: None,
                phase: crate::session_state::MirrorPhase::Ready,
                sync_error: None,
            },
        );
        self.sessions
            .get_mut(&session_id)
            .expect("installed attachment")
            .state
            .operation = Some(SessionOperationState {
            operation_id,
            kind,
            stage: "attaching".to_string(),
            uncertainty_reason: None,
        });

        self.commit_session(&session_id);
        Ok(incarnation)
    }

    pub(crate) fn start_reload(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: impl Into<String>,
        kind: SessionOperationKind,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        if kind != SessionOperationKind::Load {
            return Err(RuntimeStateError::OperationMismatch);
        }
        let operation_id = operation_id.into();
        if self
            .operation_in_use_except_reserved(&operation_id, Some((session_id, incarnation, kind)))
        {
            return Err(RuntimeStateError::OperationCollision);
        }
        let owner = self.require_live_owner_mut(session_id, incarnation)?;
        if owner.operation.is_none() {
            owner
                .can_begin(SessionAdmission::Attachment)
                .map_err(|_| RuntimeStateError::BusySession)?;
        }
        let session = owner.live.as_mut().expect("live owner checked");
        if session.lifecycle != SessionLifecycle::Active {
            return Err(RuntimeStateError::SessionNotActive);
        }
        if owner
            .active_turn
            .as_ref()
            .is_some_and(|turn| turn.execution.is_some())
            || owner.operation.as_ref().is_some_and(|operation| {
                operation.operation_id != operation_id
                    || operation.kind != kind
                    || operation.stage != "reserved"
            })
            || !session.permissions.is_empty()
            || !session.elicitations.is_empty()
            || !session.url_flows.is_empty()
            || !session.terminals.is_empty()
            || !session.attachment_candidate.is_empty()
        {
            return Err(RuntimeStateError::BusySession);
        }
        owner.operation = Some(SessionOperationState {
            operation_id: operation_id.clone(),
            kind,
            stage: "reloading".to_string(),
            uncertainty_reason: None,
        });
        session.attachment_candidate_bytes = 0;

        self.commit_session(session_id);
        Ok(())
    }

    pub(crate) fn append_attachment_candidate(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        update: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.require_live_owner_mut(session_id, incarnation)?;
        let session = owner.live.as_mut().expect("live owner checked");
        let collecting_attachment = session.lifecycle == SessionLifecycle::Attaching
            && owner.operation.as_ref().is_some_and(|operation| {
                operation.stage == "attaching"
                    && matches!(
                        operation.kind,
                        SessionOperationKind::Load | SessionOperationKind::Resume
                    )
            });
        let collecting_reload = session.lifecycle == SessionLifecycle::Active
            && owner.operation.as_ref().is_some_and(|operation| {
                operation.stage == "reloading" && operation.kind == SessionOperationKind::Load
            });
        if !collecting_attachment && !collecting_reload {
            return Err(RuntimeStateError::OperationMismatch);
        }
        let Some(update) = retain_control_update(update) else {
            return Ok(());
        };
        let bytes = serialized_len(&update);
        session.attachment_candidate.push(update);
        session.attachment_candidate_bytes =
            session.attachment_candidate_bytes.saturating_add(bytes);
        Ok(())
    }

    pub(crate) fn complete_reload(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        response: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let response = retain_session_metadata(session_id, response);
        let owner = self.require_live_owner_mut(session_id, incarnation)?;
        let session = owner.live.as_mut().expect("live owner checked");
        if session.lifecycle != SessionLifecycle::Active
            || owner.operation.as_ref().is_none_or(|operation| {
                operation.operation_id != operation_id
                    || operation.kind != SessionOperationKind::Load
                    || operation.stage != "reloading"
            })
        {
            return Err(RuntimeStateError::OperationMismatch);
        }
        let candidate = std::mem::take(&mut session.attachment_candidate);
        session.attachment_candidate_bytes = 0;
        session.control_state = extract_control_state(candidate);
        session.session = response;
        owner.operation = None;

        self.commit_session(session_id);
        Ok(())
    }

    pub(crate) fn fail_reload(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        _error: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.require_live_owner_mut(session_id, incarnation)?;
        let session = owner.live.as_mut().expect("live owner checked");
        if session.lifecycle != SessionLifecycle::Active
            || owner.operation.as_ref().is_none_or(|operation| {
                operation.operation_id != operation_id
                    || operation.kind != SessionOperationKind::Load
                    || operation.stage != "reloading"
            })
        {
            return Err(RuntimeStateError::OperationMismatch);
        }
        session.attachment_candidate.clear();
        session.attachment_candidate_bytes = 0;
        owner.operation = None;

        self.commit_session(session_id);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn complete_attachment(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        kind: SessionOperationKind,
        response: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let response = retain_session_metadata(session_id, response);
        let owner = self.require_live_owner_mut(session_id, incarnation)?;
        let session = owner.live.as_mut().expect("live owner checked");
        if session.lifecycle != SessionLifecycle::Attaching
            || owner.operation.as_ref().is_none_or(|operation| {
                operation.operation_id != operation_id || operation.kind != kind
            })
            || !matches!(
                kind,
                SessionOperationKind::Load | SessionOperationKind::Resume
            )
        {
            return Err(RuntimeStateError::OperationMismatch);
        }
        let candidate = std::mem::take(&mut session.attachment_candidate);
        let controls = extract_control_state(candidate);
        session.attachment_candidate_bytes = 0;
        session.control_state.extend(controls);
        session.session = response;
        session.lifecycle = SessionLifecycle::Active;
        owner.operation = None;

        self.commit_session(session_id);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn fail_attachment(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        kind: SessionOperationKind,
        _error: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        {
            let owner = self.require_live_owner(session_id, incarnation)?;
            let session = owner.live.as_ref().expect("live owner checked");
            if session.lifecycle != SessionLifecycle::Attaching
                || owner.operation.as_ref().is_none_or(|operation| {
                    operation.operation_id != operation_id || operation.kind != kind
                })
            {
                return Err(RuntimeStateError::OperationMismatch);
            }
        }
        self.require_live_owner_mut(session_id, incarnation)?
            .operation = None;
        let mut session = self
            .take_live_for_retirement(session_id, incarnation)
            .ok_or(RuntimeStateError::UnknownSession)?;
        let effects = drain_session_liveness(session_id, incarnation, &mut session);

        self.effects.extend(effects);

        self.commit_removal(session_id, incarnation);
        Ok(())
    }

    pub(crate) fn open_forked(
        &mut self,
        expected_epoch: &str,
        session_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
        session: Value,
        source: (&str, u64),
        target_replay: Option<Vec<Value>>,
    ) -> Result<u64, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let (source_session_id, source_incarnation) = source;
        {
            let source = self.require_live(source_session_id, source_incarnation)?;
            if source.lifecycle != SessionLifecycle::Active {
                return Err(RuntimeStateError::SessionNotActive);
            }
        }
        let control_state = target_replay.map_or_else(BTreeMap::new, extract_control_state);
        self.open_session(
            expected_epoch,
            session_id.into(),
            cwd.into(),
            session,
            control_state,
        )
    }

    fn open_session(
        &mut self,
        expected_epoch: &str,
        session_id: String,
        cwd: PathBuf,
        session: Value,
        control_state: BTreeMap<String, Value>,
    ) -> Result<u64, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        if self.sessions.get(&session_id).is_some_and(|entry| {
            let state = &entry.state;
            state.operation.is_some()
                || state
                    .live
                    .as_ref()
                    .is_some_and(|live| live.lifecycle != SessionLifecycle::Closed)
        }) {
            return Err(RuntimeStateError::BusySession);
        }
        self.next_incarnation = self.next_incarnation.wrapping_add(1).max(1);
        let incarnation = self.next_incarnation;
        let session = retain_session_metadata(&session_id, session);
        self.install_live(
            &session_id,
            incarnation,
            SessionLiveState {
                revision: 0,
                cwd,
                session,
                control_state,
                lifecycle: SessionLifecycle::Active,
                permissions: BTreeMap::new(),
                elicitations: BTreeMap::new(),
                url_flows: BTreeMap::new(),
                terminals: BTreeMap::new(),
                resolved_permissions: VecDeque::new(),
                resolved_elicitations: VecDeque::new(),
                resolved_url_flows: VecDeque::new(),
                released_terminals: VecDeque::new(),
                attachment_candidate: Vec::new(),
                attachment_candidate_bytes: 0,
                history_revision: None,
                phase: crate::session_state::MirrorPhase::Ready,
                sync_error: None,
            },
        );
        self.commit_session(&session_id);
        Ok(incarnation)
    }

    /// Atomically admit a canonical turn and publish its live projection before Agent dispatch.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn admit_session_turn(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        expected_history_revision: &str,
        client_intent_id: &str,
        prompt: Vec<Value>,
    ) -> Result<TurnAdmission, SessionTurnError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.require_live_owner(session_id, incarnation)?;
        if let Some(duplicate) =
            owner.check_turn_admission(expected_history_revision, client_intent_id, &prompt)?
        {
            return Ok(duplicate);
        }
        if owner.live.as_ref().expect("live owner checked").lifecycle != SessionLifecycle::Active {
            return Err(RuntimeStateError::SessionNotActive.into());
        }
        if owner.turn_execution().is_some() || owner.operation.is_some() {
            return Err(RuntimeStateError::BusySession.into());
        }
        if self.operation_in_use(client_intent_id) {
            return Err(RuntimeStateError::OperationCollision.into());
        }
        let admission = self.start_turn(
            session_id,
            incarnation,
            expected_history_revision,
            client_intent_id,
            prompt,
        )?;
        self.commit_session(session_id);
        Ok(admission)
    }

    /// A runtime reducer fixture: installs a turn in the same canonical overlay,
    /// without inventing a second runtime tail or requiring a history baseline.
    #[cfg(test)]
    pub(crate) fn start_prompt(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: impl Into<String>,
        prompt: Vec<Value>,
    ) -> Result<String, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let operation_id = operation_id.into();
        if self.operation_in_use(&operation_id) {
            return Err(RuntimeStateError::OperationCollision);
        }
        let owner = self.require_live_owner_mut(session_id, incarnation)?;
        if owner.live.as_ref().expect("live owner checked").lifecycle != SessionLifecycle::Active {
            return Err(RuntimeStateError::SessionNotActive);
        }
        if owner.turn_execution().is_some() || owner.operation.is_some() {
            return Err(RuntimeStateError::BusySession);
        }
        let old_bytes = owner.active_overlay_bytes;
        owner.active_turn = None;
        owner.phase = crate::session_state::MirrorPhase::Ready;
        let revision = owner
            .history_revision
            .get_or_insert_with(|| "runtime-fixture".to_string())
            .clone();
        owner
            .admit_turn(operation_id.clone(), &revision, &operation_id, prompt)
            .map_err(|_| RuntimeStateError::BusySession)?;
        let new_bytes = owner.active_overlay_bytes;
        self.overlay_bytes = self
            .overlay_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        self.commit_session(session_id);
        Ok(operation_id)
    }

    #[cfg(test)]
    pub(crate) fn append_runtime_update_for_test(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        update: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.require_live_owner_mut(session_id, incarnation)?;
        let turn = owner
            .active_turn
            .as_mut()
            .ok_or(RuntimeStateError::NoActiveTurn)?;
        let execution = turn
            .execution
            .as_ref()
            .ok_or(RuntimeStateError::NoActiveTurn)?;
        let operation_id = execution.rpc_operation_id.clone();
        turn.updates = Arc::new(fold_shared_active_turn_update(&turn.updates, &update)?);
        self.commit_turn_update(session_id, incarnation, operation_id, update);
        Ok(())
    }

    pub(crate) fn project_turn_update(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        overlay: &TurnOverlay,
        update: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.require_live_owner(session_id, incarnation)?;
        let turn = owner
            .active_turn
            .as_ref()
            .ok_or(RuntimeStateError::NoActiveTurn)?;
        let execution = turn
            .execution
            .as_ref()
            .ok_or(RuntimeStateError::NoActiveTurn)?;
        if turn.operation_id != overlay.operation_id
            || execution.rpc_operation_id != overlay.client_intent_id
        {
            return Err(RuntimeStateError::OperationMismatch);
        }
        let operation_id = execution.rpc_operation_id.clone();
        self.commit_turn_update(session_id, incarnation, operation_id, update);
        Ok(())
    }

    pub(crate) fn update_control_state(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        key: impl Into<String>,
        update: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let session = self.require_live_mut(session_id, incarnation)?;
        if !matches!(
            session.lifecycle,
            SessionLifecycle::Active
                | SessionLifecycle::Closing
                | SessionLifecycle::ClosingForDelete
        ) {
            return Err(RuntimeStateError::SessionNotActive);
        }
        let key = key.into();
        let update = fold_control_update(session.control_state.get(&key), update);
        if session.control_state.get(&key) == Some(&update) {
            return Ok(());
        }
        session.control_state.insert(key.clone(), update.clone());
        self.commit_control_update(session_id, incarnation, key, update);
        Ok(())
    }

    pub(crate) fn synchronize_loaded_session(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        response: Value,
        replay: Vec<Value>,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let response = retain_session_metadata(session_id, response);
        let controls = extract_control_state(replay);
        let owner = self.require_live_owner_mut(session_id, incarnation)?;
        let session = owner.live.as_mut().expect("live owner checked");
        if session.lifecycle != SessionLifecycle::Active
            || owner
                .active_turn
                .as_ref()
                .is_some_and(|turn| turn.execution.is_some())
        {
            return Err(RuntimeStateError::BusySession);
        }
        session.session = response;
        session.control_state = controls;
        self.commit_session(session_id);
        Ok(())
    }

    pub(crate) fn request_cancel(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.require_live_owner_mut(session_id, incarnation)?;
        let execution = owner
            .active_turn
            .as_mut()
            .and_then(|turn| turn.execution.as_mut())
            .ok_or(RuntimeStateError::NoActiveTurn)?;
        if execution.cancel_requested {
            return Ok(());
        }
        execution.cancel_requested = true;
        self.commit_session(session_id);
        Ok(())
    }

    pub(crate) fn complete_prompt(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        response: Value,
    ) -> Result<(), RuntimeStateError> {
        drop(response);
        self.seal_turn(
            expected_epoch,
            session_id,
            incarnation,
            operation_id,
            TurnTerminal { lifecycle: None },
        )
    }

    pub(crate) fn fail_prompt(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        error: Value,
    ) -> Result<(), RuntimeStateError> {
        drop(error);
        self.seal_turn(
            expected_epoch,
            session_id,
            incarnation,
            operation_id,
            TurnTerminal { lifecycle: None },
        )
    }

    pub(crate) fn mark_prompt_uncertain(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        reason: impl Into<String>,
    ) -> Result<(), RuntimeStateError> {
        drop(reason.into());
        self.seal_turn(
            expected_epoch,
            session_id,
            incarnation,
            operation_id,
            TurnTerminal {
                lifecycle: Some(SessionLifecycle::Uncertain),
            },
        )
    }

    fn seal_turn(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        terminal: TurnTerminal,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.require_live_owner_mut(session_id, incarnation)?;
        let retired = session_live::seal_turn(owner, operation_id, terminal)?;
        self.effects
            .extend(retired.permissions.into_iter().map(|interaction_id| {
                RuntimeEffect::CancelPermissionResponder {
                    session_id: session_id.to_string(),
                    incarnation: incarnation,
                    interaction_id,
                }
            }));
        self.effects
            .extend(retired.elicitations.into_iter().map(|interaction_id| {
                RuntimeEffect::CancelElicitationResponder {
                    session_id: Some(session_id.to_string()),
                    incarnation: Some(incarnation),
                    interaction_id,
                }
            }));
        self.drop_turn_delivery_payload(session_id, incarnation, operation_id);
        self.commit_session(session_id);
        Ok(())
    }

    pub(crate) fn upsert_permission(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        interaction_id: impl Into<String>,
        request: Value,
    ) -> Result<InteractionUpsert, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.require_live_owner_mut(session_id, incarnation)?;
        let active_operation = owner
            .turn_execution()
            .map(|execution| execution.rpc_operation_id.clone());
        let session = owner.live.as_mut().expect("live owner checked");
        let outcome = session_live::upsert_permission(
            session,
            interaction_id.into(),
            request,
            active_operation,
        )?;
        if outcome == InteractionUpsert::Inserted {
            self.commit_session(session_id);
        }
        Ok(outcome)
    }

    pub(crate) fn resolve_permission(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        interaction_id: &str,
    ) -> Result<InteractionResolution, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let session = self.require_live_mut(session_id, incarnation)?;
        let outcome = session_live::resolve_permission(session, interaction_id)?;
        if outcome == InteractionResolution::Applied {
            self.commit_session(session_id);
        }
        Ok(outcome)
    }

    pub(crate) fn begin_permission_response(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        interaction_id: &str,
        operation_id: impl Into<String>,
    ) -> Result<InteractionResponseStart, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let operation_id = operation_id.into();
        let operation_in_use = self.operation_in_use(&operation_id);
        let session = self.require_live_mut(session_id, incarnation)?;
        let outcome = session_live::begin_permission_response(
            session,
            interaction_id,
            operation_id,
            operation_in_use,
        )?;
        if outcome == InteractionResponseStart::Applied {
            self.commit_session(session_id);
        }
        Ok(outcome)
    }

    pub(crate) fn complete_permission_response(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        interaction_id: &str,
        operation_id: &str,
    ) -> Result<InteractionResolution, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let session = self.require_live_mut(session_id, incarnation)?;
        let outcome =
            session_live::complete_permission_response(session, interaction_id, operation_id)?;
        if outcome == InteractionResolution::Applied {
            self.commit_session(session_id);
        }
        Ok(outcome)
    }

    pub(crate) fn upsert_elicitation(
        &mut self,
        expected_epoch: &str,
        scope: Option<(&str, u64)>,
        interaction_id: impl Into<String>,
        request: Value,
    ) -> Result<InteractionUpsert, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let interaction_id = interaction_id.into();
        match scope {
            Some((session_id, incarnation)) => {
                let owner = self.require_live_owner_mut(session_id, incarnation)?;
                let session = owner.live.as_mut().expect("live owner checked");
                if session
                    .resolved_elicitations
                    .iter()
                    .any(|resolved| resolved == &interaction_id)
                {
                    return Ok(InteractionUpsert::AlreadyResolved);
                }
                let active_operation = owner
                    .active_turn
                    .as_ref()
                    .and_then(|turn| turn.execution.as_ref())
                    .map(|execution| execution.rpc_operation_id.clone());
                if let Some(existing) = session.elicitations.get(&interaction_id) {
                    return if existing.request == request
                        && existing.operation_id == active_operation
                    {
                        Ok(InteractionUpsert::Duplicate)
                    } else {
                        Err(RuntimeStateError::InteractionCollision)
                    };
                }
                session.elicitations.insert(
                    interaction_id.clone(),
                    PendingInteraction {
                        interaction_id,
                        request,
                        operation_id: active_operation,
                        responding_operation_id: None,
                    },
                );
                self.commit_session(session_id);
            }
            None => {
                if self
                    .resolved_request_elicitations
                    .iter()
                    .any(|resolved| resolved == &interaction_id)
                {
                    return Ok(InteractionUpsert::AlreadyResolved);
                }
                if let Some(existing) = self.request_elicitations.get(&interaction_id) {
                    return if existing.request == request {
                        Ok(InteractionUpsert::Duplicate)
                    } else {
                        Err(RuntimeStateError::InteractionCollision)
                    };
                }
                self.request_elicitations.insert(
                    interaction_id.clone(),
                    PendingInteraction {
                        interaction_id,
                        request,
                        operation_id: None,
                        responding_operation_id: None,
                    },
                );
                self.commit_connection();
            }
        }
        Ok(InteractionUpsert::Inserted)
    }

    pub(crate) fn resolve_elicitation(
        &mut self,
        expected_epoch: &str,
        scope: Option<(&str, u64)>,
        interaction_id: &str,
        accepted_url_id: Option<&str>,
    ) -> Result<InteractionResolution, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        match scope {
            Some((session_id, incarnation)) => {
                if self
                    .require_live(session_id, incarnation)?
                    .resolved_elicitations
                    .iter()
                    .any(|resolved| resolved == interaction_id)
                {
                    return Ok(InteractionResolution::AlreadyResolved);
                }
                if let Some(url_id) = accepted_url_id
                    && self.url_flow_in_use(url_id)
                {
                    return Err(RuntimeStateError::InteractionCollision);
                }
                let session = self.require_live_mut(session_id, incarnation)?;
                let pending = session
                    .elicitations
                    .remove(interaction_id)
                    .ok_or(RuntimeStateError::UnknownInteraction)?;
                let PendingInteraction { request, .. } = pending;
                remember_resolved_interaction(&mut session.resolved_elicitations, interaction_id);
                if let Some(elicitation_id) = accepted_url_id {
                    session
                        .resolved_url_flows
                        .retain(|resolved| resolved != elicitation_id);
                    session.url_flows.insert(
                        elicitation_id.to_string(),
                        UrlFlow {
                            elicitation_id: elicitation_id.to_string(),
                            registration_id: interaction_id.to_string(),
                            request,
                            status: UrlFlowStatus::Waiting,
                        },
                    );
                }
                self.commit_session(session_id);
            }
            None => {
                if self
                    .resolved_request_elicitations
                    .iter()
                    .any(|resolved| resolved == interaction_id)
                {
                    return Ok(InteractionResolution::AlreadyResolved);
                }
                if let Some(url_id) = accepted_url_id
                    && self.url_flow_in_use(url_id)
                {
                    return Err(RuntimeStateError::InteractionCollision);
                }
                let pending = self
                    .request_elicitations
                    .remove(interaction_id)
                    .ok_or(RuntimeStateError::UnknownInteraction)?;
                let PendingInteraction { request, .. } = pending;
                remember_resolved_interaction(
                    &mut self.resolved_request_elicitations,
                    interaction_id,
                );
                if let Some(elicitation_id) = accepted_url_id {
                    self.resolved_request_url_flows
                        .retain(|resolved| resolved != elicitation_id);
                    self.request_url_flows.insert(
                        elicitation_id.to_string(),
                        UrlFlow {
                            elicitation_id: elicitation_id.to_string(),
                            registration_id: interaction_id.to_string(),
                            request,
                            status: UrlFlowStatus::Waiting,
                        },
                    );
                }
                self.commit_connection();
            }
        }
        Ok(InteractionResolution::Applied)
    }

    pub(crate) fn begin_elicitation_response(
        &mut self,
        expected_epoch: &str,
        scope: Option<(&str, u64)>,
        interaction_id: &str,
        operation_id: impl Into<String>,
    ) -> Result<InteractionResponseStart, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let operation_id = operation_id.into();
        let responding_operation_id = match scope {
            Some((session_id, incarnation)) => self
                .require_live(session_id, incarnation)?
                .elicitations
                .get(interaction_id)
                .ok_or(RuntimeStateError::UnknownInteraction)?
                .responding_operation_id
                .as_deref(),
            None => self
                .request_elicitations
                .get(interaction_id)
                .ok_or(RuntimeStateError::UnknownInteraction)?
                .responding_operation_id
                .as_deref(),
        };
        match responding_operation_id {
            Some(existing) if existing == operation_id => {
                return Ok(InteractionResponseStart::Duplicate);
            }
            Some(_) => return Err(RuntimeStateError::InteractionCollision),
            None => {}
        }
        if self.operation_in_use(&operation_id) {
            return Err(RuntimeStateError::OperationCollision);
        }

        match scope {
            Some((session_id, incarnation)) => {
                self.require_live_mut(session_id, incarnation)?
                    .elicitations
                    .get_mut(interaction_id)
                    .ok_or(RuntimeStateError::UnknownInteraction)?
                    .responding_operation_id = Some(operation_id);
                self.commit_session(session_id);
            }
            None => {
                self.request_elicitations
                    .get_mut(interaction_id)
                    .ok_or(RuntimeStateError::UnknownInteraction)?
                    .responding_operation_id = Some(operation_id);
                self.commit_connection();
            }
        }
        Ok(InteractionResponseStart::Applied)
    }

    pub(crate) fn complete_elicitation_response(
        &mut self,
        expected_epoch: &str,
        scope: Option<(&str, u64)>,
        interaction_id: &str,
        operation_id: &str,
        accepted_url_id: Option<&str>,
    ) -> Result<InteractionResolution, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let already_resolved = match scope {
            Some((session_id, incarnation)) => self
                .require_live(session_id, incarnation)?
                .resolved_elicitations
                .iter()
                .any(|resolved| resolved == interaction_id),
            None => self
                .resolved_request_elicitations
                .iter()
                .any(|resolved| resolved == interaction_id),
        };
        if already_resolved {
            return Ok(InteractionResolution::AlreadyResolved);
        }
        let responding_operation_id = match scope {
            Some((session_id, incarnation)) => self
                .require_live(session_id, incarnation)?
                .elicitations
                .get(interaction_id)
                .ok_or(RuntimeStateError::UnknownInteraction)?
                .responding_operation_id
                .as_deref(),
            None => self
                .request_elicitations
                .get(interaction_id)
                .ok_or(RuntimeStateError::UnknownInteraction)?
                .responding_operation_id
                .as_deref(),
        };
        if responding_operation_id != Some(operation_id) {
            return Err(RuntimeStateError::OperationMismatch);
        }
        self.resolve_elicitation(expected_epoch, scope, interaction_id, accepted_url_id)
    }

    pub(crate) fn settle_url_registration(
        &mut self,
        expected_epoch: &str,
        scope: Option<(&str, u64)>,
        elicitation_id: &str,
        registration_id: &str,
        status: UrlFlowStatus,
    ) -> Result<UrlFlowResolution, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let flow = match scope {
            Some((session_id, incarnation)) => self
                .require_live(session_id, incarnation)?
                .url_flows
                .get(elicitation_id),
            None => self.request_url_flows.get(elicitation_id),
        };
        if flow.is_some_and(|flow| flow.registration_id != registration_id) {
            return Ok(UrlFlowResolution::StaleRegistration);
        }
        self.settle_url_flow(expected_epoch, scope, elicitation_id, status)
    }

    fn settle_url_flow(
        &mut self,
        expected_epoch: &str,
        scope: Option<(&str, u64)>,
        elicitation_id: &str,
        status: UrlFlowStatus,
    ) -> Result<UrlFlowResolution, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        if status == UrlFlowStatus::Waiting {
            return Err(RuntimeStateError::OperationMismatch);
        }
        match scope {
            Some((session_id, incarnation)) => {
                let session = self.require_live_mut(session_id, incarnation)?;
                if session
                    .resolved_url_flows
                    .iter()
                    .any(|resolved| resolved == elicitation_id)
                {
                    return Ok(UrlFlowResolution::AlreadyTerminal);
                }
                let flow = session
                    .url_flows
                    .remove(elicitation_id)
                    .ok_or(RuntimeStateError::UnknownInteraction)?;
                if flow.status != UrlFlowStatus::Waiting {
                    return Ok(UrlFlowResolution::AlreadyTerminal);
                }
                remember_resolved_interaction(&mut session.resolved_url_flows, elicitation_id);
                self.commit_session(session_id);
            }
            None => {
                if self
                    .resolved_request_url_flows
                    .iter()
                    .any(|resolved| resolved == elicitation_id)
                {
                    return Ok(UrlFlowResolution::AlreadyTerminal);
                }
                let flow = self
                    .request_url_flows
                    .remove(elicitation_id)
                    .ok_or(RuntimeStateError::UnknownInteraction)?;
                if flow.status != UrlFlowStatus::Waiting {
                    return Ok(UrlFlowResolution::AlreadyTerminal);
                }
                remember_resolved_interaction(&mut self.resolved_request_url_flows, elicitation_id);
                self.commit_connection();
            }
        }
        Ok(UrlFlowResolution::Applied)
    }

    pub(crate) fn upsert_terminal(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        terminal_id: impl Into<String>,
        terminal: Value,
    ) -> Result<TerminalUpsert, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let session = self.require_live_mut(session_id, incarnation)?;
        let terminal_id = terminal_id.into();
        if session
            .released_terminals
            .iter()
            .any(|released| released == &terminal_id)
        {
            return Ok(TerminalUpsert::Stale);
        }
        let materialized = fold_terminal_snapshot(session.terminals.get(&terminal_id), &terminal);
        let outcome = match session.terminals.get(&terminal_id) {
            Some(existing) if existing == &materialized => return Ok(TerminalUpsert::Duplicate),
            Some(_) => TerminalUpsert::Updated,
            None => TerminalUpsert::Inserted,
        };
        if terminal_released(&terminal) {
            session.terminals.remove(&terminal_id);
            remember_resolved_interaction(&mut session.released_terminals, &terminal_id);
        } else {
            session.terminals.insert(terminal_id.clone(), materialized);
        }
        session.revision = session.revision.wrapping_add(1).max(1);
        let revision = session.revision;
        self.commit_delta(
            Some(revision),
            RuntimeChange::TerminalUpdated {
                session_id: session_id.to_string(),
                incarnation,
                revision,
                terminal_id,
                terminal,
            },
        );
        Ok(outcome)
    }

    pub(crate) fn start_operation(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: impl Into<String>,
        kind: SessionOperationKind,
        stage: impl Into<String>,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let operation_id = operation_id.into();
        if self
            .operation_in_use_except_reserved(&operation_id, Some((session_id, incarnation, kind)))
        {
            return Err(RuntimeStateError::OperationCollision);
        }
        let owner = self.require_live_owner_mut(session_id, incarnation)?;
        session_live::start_operation(owner, operation_id, kind, stage.into())?;
        self.commit_session(session_id);
        Ok(())
    }

    pub(crate) fn start_delete(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: impl Into<String>,
    ) -> Result<(), RuntimeStateError> {
        let stage = match self.require_live(session_id, incarnation)?.lifecycle {
            SessionLifecycle::Active => "closing",
            SessionLifecycle::Closed => "deleting",
            _ => return Err(RuntimeStateError::SessionNotActive),
        };
        self.start_operation(
            expected_epoch,
            session_id,
            incarnation,
            operation_id,
            SessionOperationKind::Delete,
            stage,
        )
    }

    pub(crate) fn start_direct_delete(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: impl Into<String>,
    ) -> Result<(), RuntimeStateError> {
        if self.require_live(session_id, incarnation)?.lifecycle != SessionLifecycle::Active {
            return Err(RuntimeStateError::SessionNotActive);
        }
        self.start_operation(
            expected_epoch,
            session_id,
            incarnation,
            operation_id,
            SessionOperationKind::Delete,
            "deleting_active",
        )
    }

    pub(crate) fn delete_close_succeeded(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.require_live_owner_mut(session_id, incarnation)?;
        let session = owner.live.as_mut().expect("live owner checked");
        let Some(operation) = owner.operation.as_mut() else {
            return Err(RuntimeStateError::OperationMismatch);
        };
        if operation.operation_id != operation_id
            || operation.kind != SessionOperationKind::Delete
            || operation.stage != "closing"
            || session.lifecycle != SessionLifecycle::ClosingForDelete
        {
            return Err(RuntimeStateError::OperationMismatch);
        }
        operation.stage = "deleting".to_string();
        session.lifecycle = SessionLifecycle::Deleting;
        let effects = {
            if let Some(turn) = owner.active_turn.as_mut() {
                turn.execution = None;
            }
            drain_session_liveness(session_id, incarnation, session)
        };

        self.effects.extend(effects);
        self.commit_session(session_id);
        Ok(())
    }

    pub(crate) fn complete_delete(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        {
            let owner = self.require_live_owner(session_id, incarnation)?;
            let session = owner.live.as_ref().expect("live owner checked");
            if session.lifecycle != SessionLifecycle::Deleting
                || owner.operation.as_ref().is_none_or(|operation| {
                    operation.operation_id != operation_id
                        || operation.kind != SessionOperationKind::Delete
                        || !matches!(operation.stage.as_str(), "deleting" | "deleting_active")
                })
            {
                return Err(RuntimeStateError::OperationMismatch);
            }
        }
        let owner = self.require_live_owner_mut(session_id, incarnation)?;
        owner
            .operation
            .as_mut()
            .expect("matched delete owner")
            .stage = "cleanup".to_string();
        if let Some(turn) = owner.active_turn.as_mut() {
            turn.execution = None;
        }
        let mut session = self
            .take_live_for_retirement(session_id, incarnation)
            .ok_or(RuntimeStateError::UnknownSession)?;
        let effects = drain_session_liveness(session_id, incarnation, &mut session);

        self.effects.extend(effects);

        self.commit_removal(session_id, incarnation);
        Ok(())
    }

    pub(crate) fn mark_operation_uncertain(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        reason: impl Into<String>,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let reason = reason.into();
        let effects = {
            let owner = self.require_live_owner_mut(session_id, incarnation)?;
            let session = owner.live.as_mut().expect("live owner checked");
            let Some(operation) = owner.operation.as_mut() else {
                return Err(RuntimeStateError::OperationMismatch);
            };
            if operation.operation_id != operation_id {
                return Err(RuntimeStateError::OperationMismatch);
            }
            operation.stage = "uncertain".to_string();
            operation.uncertainty_reason = Some(reason.clone());
            session.lifecycle = SessionLifecycle::Uncertain;
            drain_session_liveness(session_id, incarnation, session)
        };

        self.effects.extend(effects);

        self.commit_session(session_id);
        Ok(())
    }

    pub(crate) fn complete_operation(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        kind: SessionOperationKind,
        _result: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.require_live_owner_mut(session_id, incarnation)?;
        if matches!(
            kind,
            SessionOperationKind::Close | SessionOperationKind::Delete
        ) || owner.operation.as_ref().is_none_or(|operation| {
            operation.operation_id != operation_id || operation.kind != kind
        }) {
            return Err(RuntimeStateError::OperationMismatch);
        }
        owner.operation = None;

        self.commit_session(session_id);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn complete_control_operation(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        kind: SessionOperationKind,
        control_key: impl Into<String>,
        control_update: Value,
        _result: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        if !matches!(
            kind,
            SessionOperationKind::SetMode | SessionOperationKind::SetConfig
        ) {
            return Err(RuntimeStateError::OperationMismatch);
        }
        let owner = self.require_live_owner_mut(session_id, incarnation)?;
        let session = owner.live.as_mut().expect("live owner checked");
        if owner.operation.as_ref().is_none_or(|operation| {
            operation.operation_id != operation_id || operation.kind != kind
        }) {
            return Err(RuntimeStateError::OperationMismatch);
        }
        session
            .control_state
            .insert(control_key.into(), control_update);
        owner.operation = None;

        self.commit_session(session_id);
        Ok(())
    }

    pub(crate) fn fail_operation(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        kind: SessionOperationKind,
        _error: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.require_live_owner_mut(session_id, incarnation)?;
        let session = owner.live.as_mut().expect("live owner checked");
        let Some(operation) = owner.operation.as_ref() else {
            return Err(RuntimeStateError::OperationMismatch);
        };
        if operation.operation_id != operation_id || operation.kind != kind {
            return Err(RuntimeStateError::OperationMismatch);
        }
        let stage = operation.stage.clone();
        owner.operation = None;
        session.lifecycle = match kind {
            SessionOperationKind::Close => SessionLifecycle::Active,
            SessionOperationKind::Delete if stage == "deleting" => SessionLifecycle::Closed,
            SessionOperationKind::Delete => SessionLifecycle::Active,
            _ => session.lifecycle.clone(),
        };

        self.commit_session(session_id);
        Ok(())
    }

    pub(crate) fn close_session(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.require_live_owner_mut(session_id, incarnation)?;
        let operation = owner
            .operation
            .as_mut()
            .ok_or(RuntimeStateError::OperationMismatch)?;
        if operation.operation_id != operation_id
            || operation.kind != SessionOperationKind::Close
            || operation.stage == "cleanup"
        {
            return Err(RuntimeStateError::OperationMismatch);
        }
        operation.stage = "cleanup".to_string();
        if let Some(turn) = owner.active_turn.as_mut() {
            turn.execution = None;
        }
        let live = owner.live.as_mut().expect("live owner checked");
        live.lifecycle = SessionLifecycle::Closed;
        let effects = drain_session_liveness(session_id, incarnation, live);
        self.effects.extend(effects);
        self.commit_removal(session_id, incarnation);
        Ok(())
    }

    /// Called only after external resource cleanup; the exact owner fences late callbacks.
    pub(crate) fn finish_session_cleanup(
        &mut self,
        session_id: &str,
        incarnation: u64,
        kind: SessionAdmission,
        operation_id: &str,
    ) -> Result<(), MirrorError> {
        let owner = self
            .sessions
            .get_mut(session_id)
            .map(|entry| &mut entry.state)
            .ok_or(MirrorError::UnknownSession)?;
        if owner.incarnation != incarnation {
            return Err(MirrorError::StaleIncarnation);
        }
        if owner.operation.as_ref().is_none_or(|operation| {
            operation.operation_id != operation_id || operation.kind.admission() != kind
        }) || owner
            .live
            .as_ref()
            .is_some_and(|live| live.lifecycle != SessionLifecycle::Closed)
        {
            return Err(MirrorError::OperationMismatch);
        }
        owner.operation = None;
        owner.live = None;
        Ok(())
    }

    fn require_epoch(&self, expected_epoch: &str) -> Result<(), RuntimeStateError> {
        if expected_epoch == self.epoch {
            Ok(())
        } else {
            Err(RuntimeStateError::EpochMismatch)
        }
    }

    fn require_live_owner(
        &self,
        session_id: &str,
        incarnation: u64,
    ) -> Result<&SessionState, RuntimeStateError> {
        let owner = self
            .sessions
            .get(session_id)
            .map(|entry| &entry.state)
            .ok_or(RuntimeStateError::UnknownSession)?;
        if owner.incarnation != incarnation {
            return Err(RuntimeStateError::StaleIncarnation);
        }
        if owner.live.is_none()
            || owner
                .operation
                .as_ref()
                .is_some_and(|operation| operation.stage == "cleanup")
        {
            return Err(RuntimeStateError::UnknownSession);
        }
        Ok(owner)
    }

    fn require_live_owner_mut(
        &mut self,
        session_id: &str,
        incarnation: u64,
    ) -> Result<&mut SessionState, RuntimeStateError> {
        let owner = self
            .sessions
            .get_mut(session_id)
            .map(|entry| &mut entry.state)
            .ok_or(RuntimeStateError::UnknownSession)?;
        if owner.incarnation != incarnation {
            return Err(RuntimeStateError::StaleIncarnation);
        }
        if owner.live.is_none()
            || owner
                .operation
                .as_ref()
                .is_some_and(|operation| operation.stage == "cleanup")
        {
            return Err(RuntimeStateError::UnknownSession);
        }
        Ok(owner)
    }

    fn require_live_mut(
        &mut self,
        session_id: &str,
        incarnation: u64,
    ) -> Result<&mut SessionLiveState, RuntimeStateError> {
        Ok(self
            .require_live_owner_mut(session_id, incarnation)?
            .live
            .as_mut()
            .expect("live owner checked"))
    }

    fn require_live(
        &self,
        session_id: &str,
        incarnation: u64,
    ) -> Result<&SessionLiveState, RuntimeStateError> {
        Ok(self
            .require_live_owner(session_id, incarnation)?
            .live
            .as_ref()
            .expect("live owner checked"))
    }

    fn install_live(&mut self, session_id: &str, incarnation: u64, live: SessionLiveState) {
        if let Some(previous) = self
            .sessions
            .get(session_id)
            .map(|entry| entry.state.incarnation)
            && previous != incarnation
        {
            self.clear_history_for_replacement(session_id, previous);
            self.retire_entry(session_id, previous, "session incarnation was replaced");
        }
        self.sessions
            .entry(session_id.to_string())
            .or_insert_with(|| SessionEntry::new(SessionState::cold(session_id, incarnation)))
            .state
            .live = Some(live);
    }

    fn take_live_for_retirement(
        &mut self,
        session_id: &str,
        incarnation: u64,
    ) -> Option<SessionLiveState> {
        let entry = self.sessions.get_mut(session_id)?;
        if entry.state.incarnation != incarnation {
            return None;
        }
        // Resources may still own materialization waiters or cleanup handles.
        // Only explicit final removal or incarnation replacement may retire them.
        entry.state.live.take()
    }

    pub(crate) fn operation_in_use(&self, operation_id: &str) -> bool {
        self.operation_in_use_except_reserved(operation_id, None)
    }

    fn operation_in_use_except_reserved(
        &self,
        operation_id: &str,
        reservation: Option<(&str, u64, SessionOperationKind)>,
    ) -> bool {
        self.sessions.values().any(|entry| {
            let owner = &entry.state;
            owner
                .turn_execution()
                .is_some_and(|execution| execution.rpc_operation_id == operation_id)
                || owner.operation.as_ref().is_some_and(|operation| {
                    operation.operation_id == operation_id
                        && !reservation.is_some_and(|(session_id, incarnation, kind)| {
                            owner.session_id == session_id
                                && owner.incarnation == incarnation
                                && operation.kind == kind
                                && operation.stage == "reserved"
                        })
                })
                || owner.live.as_ref().is_some_and(|live| {
                    live.permissions
                        .values()
                        .chain(live.elicitations.values())
                        .any(|interaction| {
                            interaction.responding_operation_id.as_deref() == Some(operation_id)
                        })
                })
        }) || self
            .request_elicitations
            .values()
            .any(|interaction| interaction.responding_operation_id.as_deref() == Some(operation_id))
    }

    fn url_flow_in_use(&self, elicitation_id: &str) -> bool {
        self.request_url_flows.contains_key(elicitation_id)
            || self
                .sessions
                .values()
                .filter_map(|entry| entry.state.live.as_ref())
                .any(|session| session.url_flows.contains_key(elicitation_id))
    }

    fn drop_turn_delivery_payload(
        &mut self,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
    ) {
        self.journal
            .retire_turn_payload(session_id, incarnation, operation_id);
    }

    fn commit_session(&mut self, session_id: &str) {
        let Some(entry) = self.sessions.get_mut(session_id) else {
            return;
        };
        let Some(live) = entry.state.live.as_mut() else {
            return;
        };
        live.revision = live.revision.wrapping_add(1).max(1);
        live.history_revision
            .clone_from(&entry.state.history_revision);
        live.phase = entry.state.phase;
        live.sync_error.clone_from(&entry.state.sync_error);
        let revision = live.revision;
        let session = self
            .session(session_id)
            .expect("published session remains live");
        self.commit_delta(
            Some(revision),
            RuntimeChange::SessionUpsert {
                session: Box::new(session),
            },
        );
    }

    fn commit_connection(&mut self) {
        self.connection_revision = self.connection_revision.wrapping_add(1).max(1);
        self.commit_delta(
            Some(self.connection_revision),
            RuntimeChange::ConnectionUpsert {
                request_elicitations: self.request_elicitations.clone(),
                request_url_flows: self.request_url_flows.clone(),
            },
        );
    }

    fn commit_control_update(
        &mut self,
        session_id: &str,
        incarnation: u64,
        key: String,
        update: Value,
    ) {
        let Some(entry) = self.sessions.get_mut(session_id) else {
            return;
        };
        let Some(live) = entry.state.live.as_mut() else {
            return;
        };
        live.revision = live.revision.wrapping_add(1).max(1);
        live.history_revision
            .clone_from(&entry.state.history_revision);
        live.phase = entry.state.phase;
        live.sync_error.clone_from(&entry.state.sync_error);
        let revision = live.revision;
        let history_revision = live.history_revision.clone();
        let phase = live.phase;
        let sync_error = live.sync_error.clone();
        self.commit_delta(
            Some(revision),
            RuntimeChange::SessionControlUpdated {
                session_id: session_id.to_string(),
                incarnation,
                revision,
                key,
                update,
                history_revision,
                phase,
                sync_error,
            },
        );
    }

    fn commit_turn_update(
        &mut self,
        session_id: &str,
        incarnation: u64,
        operation_id: String,
        update: Value,
    ) {
        let Some(session) = self
            .sessions
            .get_mut(session_id)
            .and_then(|entry| entry.state.live.as_mut())
        else {
            return;
        };
        session.revision = session.revision.wrapping_add(1).max(1);
        let revision = session.revision;
        self.commit_delta(
            Some(revision),
            RuntimeChange::TurnUpdateAppended {
                session_id: session_id.to_string(),
                incarnation,
                revision,
                operation_id,
                update,
            },
        );
    }

    fn commit_removal(&mut self, session_id: &str, incarnation: u64) {
        self.commit_delta(
            None,
            RuntimeChange::SessionRemoved {
                session_id: session_id.to_string(),
                incarnation,
            },
        );
    }

    fn commit_delta(&mut self, scope_revision: Option<u64>, change: RuntimeChange) {
        self.journal.commit(&self.epoch, scope_revision, change);
    }
}

// Single-session reducers borrow the existing live state. They neither access the
// registry or publication journal nor perform I/O, and can move with its owner.
mod session_live {
    use super::{
        InteractionResolution, InteractionResponseStart, InteractionUpsert, PendingInteraction,
        RuntimeStateError, SessionLifecycle, SessionLiveState, SessionOperationKind,
        SessionOperationState, SessionState, TurnTerminal, remember_resolved_interaction,
        remember_resolved_permission,
    };

    pub(super) struct TurnRetirement {
        pub(super) permissions: Vec<String>,
        pub(super) elicitations: Vec<String>,
    }

    pub(super) fn start_operation(
        owner: &mut SessionState,
        operation_id: String,
        kind: SessionOperationKind,
        stage: String,
    ) -> Result<(), RuntimeStateError> {
        if owner.operation.is_none() {
            owner
                .can_begin(kind.admission())
                .map_err(|_| RuntimeStateError::BusySession)?;
        }
        let session = owner
            .live
            .as_mut()
            .ok_or(RuntimeStateError::UnknownSession)?;
        let allowed_lifecycle = match kind {
            SessionOperationKind::Delete => matches!(
                session.lifecycle,
                SessionLifecycle::Active | SessionLifecycle::Closed
            ),
            _ => session.lifecycle == SessionLifecycle::Active,
        };
        if !allowed_lifecycle {
            return Err(RuntimeStateError::SessionNotActive);
        }
        if owner.operation.as_ref().is_some_and(|operation| {
            operation.operation_id != operation_id
                || operation.kind != kind
                || operation.stage != "reserved"
        }) || (owner
            .active_turn
            .as_ref()
            .is_some_and(|turn| turn.execution.is_some())
            && !matches!(
                kind,
                SessionOperationKind::Close
                    | SessionOperationKind::SetMode
                    | SessionOperationKind::SetConfig
            ))
        {
            return Err(RuntimeStateError::BusySession);
        }
        owner.operation = Some(SessionOperationState {
            operation_id,
            kind,
            stage,
            uncertainty_reason: None,
        });
        match kind {
            SessionOperationKind::Close => session.lifecycle = SessionLifecycle::Closing,
            SessionOperationKind::Delete if session.lifecycle == SessionLifecycle::Closed => {
                session.lifecycle = SessionLifecycle::Deleting;
            }
            SessionOperationKind::Delete
                if owner
                    .operation
                    .as_ref()
                    .is_some_and(|operation| operation.stage == "deleting_active") =>
            {
                session.lifecycle = SessionLifecycle::Deleting;
            }
            SessionOperationKind::Delete => {
                session.lifecycle = SessionLifecycle::ClosingForDelete;
            }
            _ => {}
        }

        Ok(())
    }

    pub(super) fn seal_turn(
        owner: &mut SessionState,
        operation_id: &str,
        terminal: TurnTerminal,
    ) -> Result<TurnRetirement, RuntimeStateError> {
        let session = owner
            .live
            .as_mut()
            .ok_or(RuntimeStateError::UnknownSession)?;
        let TurnTerminal { lifecycle } = terminal;
        let overlay = owner
            .active_turn
            .as_mut()
            .ok_or(RuntimeStateError::NoActiveTurn)?;
        let execution = overlay
            .execution
            .as_ref()
            .ok_or(RuntimeStateError::NoActiveTurn)?;
        if execution.rpc_operation_id != operation_id {
            return Err(RuntimeStateError::OperationMismatch);
        }
        overlay.execution = None;
        if let Some(lifecycle) = lifecycle {
            session.lifecycle = lifecycle;
        }
        let resolved_permissions = session
            .permissions
            .iter()
            .filter(|(_, interaction)| interaction.operation_id.as_deref() == Some(operation_id))
            .map(|(interaction_id, _)| interaction_id.clone())
            .collect::<Vec<_>>();
        for interaction_id in &resolved_permissions {
            session.permissions.remove(interaction_id);
            remember_resolved_permission(session, interaction_id);
        }
        let resolved_elicitations = session
            .elicitations
            .iter()
            .filter(|(_, interaction)| interaction.operation_id.as_deref() == Some(operation_id))
            .map(|(interaction_id, _)| interaction_id.clone())
            .collect::<Vec<_>>();
        for interaction_id in &resolved_elicitations {
            session.elicitations.remove(interaction_id);
            remember_resolved_interaction(&mut session.resolved_elicitations, interaction_id);
        }
        Ok(TurnRetirement {
            permissions: resolved_permissions,
            elicitations: resolved_elicitations,
        })
    }

    pub(super) fn upsert_permission(
        session: &mut SessionLiveState,
        interaction_id: String,
        request: serde_json::Value,
        active_operation: Option<String>,
    ) -> Result<InteractionUpsert, RuntimeStateError> {
        if session
            .resolved_permissions
            .iter()
            .any(|resolved| resolved == &interaction_id)
        {
            return Ok(InteractionUpsert::AlreadyResolved);
        }
        if let Some(existing) = session.permissions.get(&interaction_id) {
            return if existing.request == request && existing.operation_id == active_operation {
                Ok(InteractionUpsert::Duplicate)
            } else {
                Err(RuntimeStateError::InteractionCollision)
            };
        }
        session.permissions.insert(
            interaction_id.clone(),
            PendingInteraction {
                interaction_id,
                request,
                operation_id: active_operation,
                responding_operation_id: None,
            },
        );
        Ok(InteractionUpsert::Inserted)
    }

    pub(super) fn resolve_permission(
        session: &mut SessionLiveState,
        interaction_id: &str,
    ) -> Result<InteractionResolution, RuntimeStateError> {
        if session
            .resolved_permissions
            .iter()
            .any(|resolved| resolved == interaction_id)
        {
            return Ok(InteractionResolution::AlreadyResolved);
        }
        session
            .permissions
            .remove(interaction_id)
            .ok_or(RuntimeStateError::UnknownInteraction)?;
        remember_resolved_permission(session, interaction_id);
        Ok(InteractionResolution::Applied)
    }

    pub(super) fn begin_permission_response(
        session: &mut SessionLiveState,
        interaction_id: &str,
        operation_id: String,
        operation_in_use: bool,
    ) -> Result<InteractionResponseStart, RuntimeStateError> {
        let pending = session
            .permissions
            .get(interaction_id)
            .ok_or(RuntimeStateError::UnknownInteraction)?;
        match pending.responding_operation_id.as_deref() {
            Some(existing) if existing == operation_id => {
                return Ok(InteractionResponseStart::Duplicate);
            }
            Some(_) => return Err(RuntimeStateError::InteractionCollision),
            None => {}
        }
        if operation_in_use {
            return Err(RuntimeStateError::OperationCollision);
        }
        let pending = session
            .permissions
            .get_mut(interaction_id)
            .expect("permission was checked before mutation");
        pending.responding_operation_id = Some(operation_id);
        Ok(InteractionResponseStart::Applied)
    }

    pub(super) fn complete_permission_response(
        session: &mut SessionLiveState,
        interaction_id: &str,
        operation_id: &str,
    ) -> Result<InteractionResolution, RuntimeStateError> {
        if session
            .resolved_permissions
            .iter()
            .any(|resolved| resolved == interaction_id)
        {
            return Ok(InteractionResolution::AlreadyResolved);
        }
        let pending = session
            .permissions
            .get(interaction_id)
            .ok_or(RuntimeStateError::UnknownInteraction)?;
        if pending.responding_operation_id.as_deref() != Some(operation_id) {
            return Err(RuntimeStateError::OperationMismatch);
        }
        session.permissions.remove(interaction_id);
        remember_resolved_permission(session, interaction_id);

        Ok(InteractionResolution::Applied)
    }
}

pub(crate) fn fold_terminal_snapshot(previous: Option<&Value>, incoming: &Value) -> Value {
    fn output_bytes(value: &Value) -> Vec<u8> {
        value
            .get("outputBytes")
            .and_then(Value::as_str)
            .and_then(|encoded| BASE64_STANDARD.decode(encoded).ok())
            .unwrap_or_else(|| {
                value
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .as_bytes()
                    .to_vec()
            })
    }
    let append = incoming.get("outputAppend").and_then(Value::as_bool) == Some(true);
    let mut output = if append {
        previous.map(output_bytes).unwrap_or_default()
    } else {
        Vec::new()
    };
    output.extend(output_bytes(incoming));
    let limit = incoming
        .get("retainedBytes")
        .and_then(Value::as_u64)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .unwrap_or(output.len());
    if output.len() > limit {
        output.drain(..output.len() - limit);
    }
    while output.first().is_some_and(|byte| byte & 0xc0 == 0x80) {
        output.remove(0);
    }
    let mut snapshot = incoming.clone();
    let visible = if incoming.get("exitStatus").is_none_or(Value::is_null)
        && incoming.get("released").and_then(Value::as_bool) != Some(true)
    {
        match std::str::from_utf8(&output) {
            Err(error) if error.error_len().is_none() => &output[..error.valid_up_to()],
            _ => &output,
        }
    } else {
        &output
    };
    snapshot["output"] = Value::String(String::from_utf8_lossy(visible).into_owned());
    snapshot["outputBytes"] = Value::String(BASE64_STANDARD.encode(&output));
    if let Some(snapshot) = snapshot.as_object_mut() {
        snapshot.remove("outputAppend");
        snapshot.remove("retainedBytes");
    }
    snapshot
}

fn extract_control_state(entries: Vec<Value>) -> BTreeMap<String, Value> {
    let mut controls = BTreeMap::new();
    for entry in entries {
        if let Some(update) = retain_control_update(entry) {
            let kind = update["sessionUpdate"]
                .as_str()
                .expect("retained control update has a kind")
                .to_string();
            let update = fold_control_update(controls.get(&kind), update);
            controls.insert(kind, update);
        }
    }
    controls
}

pub(crate) fn fold_control_update(previous: Option<&Value>, update: Value) -> Value {
    let kind = update.get("sessionUpdate").and_then(Value::as_str);
    if !matches!(kind, Some("session_info_update" | "usage_update")) {
        return update;
    }
    let Some(mut merged) = previous.and_then(Value::as_object).cloned() else {
        return update;
    };
    if let Some(fields) = update.as_object() {
        for (key, value) in fields {
            // Session info uses absent/clear/set patches. Usage's optional cost
            // carries the last known total until the Agent supplies a new value.
            if kind == Some("usage_update") && key == "cost" && value.is_null() {
                continue;
            }
            merged.insert(key.clone(), value.clone());
        }
    }
    Value::Object(merged)
}

fn retain_session_metadata(session_id: &str, response: Value) -> Value {
    let mut retained = Map::new();
    retained.insert(
        "sessionId".to_string(),
        Value::String(session_id.to_string()),
    );
    for key in ["modes", "configOptions"] {
        if let Some(value) = response.get(key).filter(|value| !value.is_null()) {
            retained.insert(key.to_string(), value.clone());
        }
    }
    Value::Object(retained)
}

fn retain_control_update(entry: Value) -> Option<Value> {
    let update = entry.get("update").unwrap_or(&entry);
    let kind = update.get("sessionUpdate").and_then(Value::as_str)?;
    (!is_conversation_update_kind(kind)).then(|| update.clone())
}

fn is_conversation_update_kind(kind: &str) -> bool {
    matches!(
        kind,
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
}

fn terminal_released(terminal: &Value) -> bool {
    terminal
        .get("released")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn remember_resolved_permission(session: &mut SessionLiveState, interaction_id: &str) {
    remember_resolved_interaction(&mut session.resolved_permissions, interaction_id);
}

fn remember_resolved_interaction(resolved: &mut VecDeque<String>, interaction_id: &str) {
    resolved.push_back(interaction_id.to_string());
}

fn drain_session_liveness(
    session_id: &str,
    incarnation: u64,
    session: &mut SessionLiveState,
) -> Vec<RuntimeEffect> {
    let permissions = std::mem::take(&mut session.permissions);
    let mut effects = permissions
        .into_keys()
        .map(|interaction_id| RuntimeEffect::CancelPermissionResponder {
            session_id: session_id.to_string(),
            incarnation: incarnation,
            interaction_id,
        })
        .collect::<Vec<_>>();
    let elicitations = std::mem::take(&mut session.elicitations);
    effects.extend(elicitations.into_keys().map(|interaction_id| {
        RuntimeEffect::CancelElicitationResponder {
            session_id: Some(session_id.to_string()),
            incarnation: Some(incarnation),
            interaction_id,
        }
    }));
    effects.extend(
        std::mem::take(&mut session.url_flows)
            .into_iter()
            .filter(|(_, flow)| flow.status == UrlFlowStatus::Waiting)
            .map(|(elicitation_id, flow)| RuntimeEffect::AbortUrlFlow {
                session_id: Some(session_id.to_string()),
                incarnation: Some(incarnation),
                elicitation_id,
                registration_id: flow.registration_id,
            }),
    );
    effects.extend(
        std::mem::take(&mut session.terminals)
            .into_iter()
            .filter(|(_, terminal)| !terminal_released(terminal))
            .map(|(terminal_id, _)| RuntimeEffect::ReleaseTerminal {
                session_id: session_id.to_string(),
                terminal_id,
            }),
    );
    effects
}

fn serialized_len(value: &impl Serialize) -> usize {
    let mut counter = SerializedByteCounter(0);
    serde_json::to_writer(&mut counter, value).map_or(usize::MAX, |_| counter.0)
}

struct SerializedByteCounter(usize);

impl Write for SerializedByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn fold_active_turn_update(
    retained: &[Value],
    update: &Value,
) -> Result<Vec<Value>, RuntimeStateError> {
    let retained = retained.iter().cloned().map(Arc::new).collect::<Vec<_>>();
    let folded = fold_shared_active_turn_update(&retained, update)?;
    drop(retained);
    Ok(folded
        .into_iter()
        .map(|value| Arc::try_unwrap(value).unwrap_or_else(|value| (*value).clone()))
        .collect())
}

pub(crate) fn fold_shared_active_turn_update(
    retained: &[Arc<Value>],
    update: &Value,
) -> Result<Vec<Arc<Value>>, RuntimeStateError> {
    let mut folded = retained.to_vec();
    let Some(kind) = update.get("sessionUpdate").and_then(Value::as_str) else {
        folded.push(Arc::new(update.clone()));
        return Ok(folded);
    };

    match kind {
        "user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk" => {
            let Some((shape, text)) = text_chunk_shape(update)? else {
                folded.push(Arc::new(update.clone()));
                return Ok(folded);
            };
            if let Some(previous) = folded.last_mut() {
                if text_chunk_shape(previous)?
                    .as_ref()
                    .is_some_and(|(previous_shape, _)| previous_shape == &shape)
                {
                    let previous = Arc::make_mut(previous);
                    let previous_text = previous
                        .get_mut("content")
                        .and_then(Value::as_object_mut)
                        .and_then(|content| content.get_mut("text"))
                        .and_then(|value| value.as_str())
                        .ok_or(RuntimeStateError::OperationMismatch)?;
                    let mut merged = String::with_capacity(previous_text.len() + text.len());
                    merged.push_str(previous_text);
                    merged.push_str(text);
                    previous["content"]["text"] = Value::String(merged);
                    return Ok(folded);
                }
            }
            folded.push(Arc::new(update.clone()));
        }
        "tool_call" | "tool_call_update" => {
            let tool_call_id = required_fold_string(update, "toolCallId")?;
            if let Some(position) = folded.iter().position(|candidate| {
                matches!(
                    candidate.get("sessionUpdate").and_then(Value::as_str),
                    Some("tool_call" | "tool_call_update")
                ) && candidate.get("toolCallId").and_then(Value::as_str) == Some(tool_call_id)
            }) {
                let retained_kind = folded[position]["sessionUpdate"].clone();
                let target = Arc::make_mut(&mut folded[position]);
                replace_tool_fields(target, update)?;
                target["sessionUpdate"] = if kind == "tool_call" {
                    Value::String("tool_call".to_string())
                } else {
                    retained_kind
                };
                target["toolCallId"] = Value::String(tool_call_id.to_string());
            } else {
                folded.push(Arc::new(update.clone()));
            }
        }
        "plan" => {
            if !update.get("entries").is_some_and(Value::is_array) {
                return Err(RuntimeStateError::OperationMismatch);
            }
            replace_shared_fold_slot(
                &mut folded,
                |candidate| candidate.get("sessionUpdate").and_then(Value::as_str) == Some("plan"),
                update,
            );
        }
        "plan_update" => {
            let plan = update
                .get("plan")
                .and_then(Value::as_object)
                .ok_or(RuntimeStateError::OperationMismatch)?;
            let plan_id = plan
                .get("planId")
                .and_then(Value::as_str)
                .filter(|plan_id| !plan_id.is_empty())
                .ok_or(RuntimeStateError::OperationMismatch)?;
            if plan.get("type").and_then(Value::as_str) == Some("items")
                && !plan.get("entries").is_some_and(Value::is_array)
            {
                return Err(RuntimeStateError::OperationMismatch);
            }
            replace_shared_fold_slot(
                &mut folded,
                |candidate| {
                    candidate.get("sessionUpdate").and_then(Value::as_str) == Some("plan_update")
                        && candidate
                            .get("plan")
                            .and_then(|plan| plan.get("planId"))
                            .and_then(Value::as_str)
                            == Some(plan_id)
                },
                update,
            );
        }
        "plan_removed" => {
            let plan_id = required_fold_string(update, "planId")?;
            if let Some(position) = folded.iter().position(|candidate| {
                candidate.get("sessionUpdate").and_then(Value::as_str) == Some("plan_update")
                    && candidate
                        .get("plan")
                        .and_then(|plan| plan.get("planId"))
                        .and_then(Value::as_str)
                        == Some(plan_id)
            }) {
                folded.remove(position);
            }
        }
        _ => folded.push(Arc::new(update.clone())),
    }
    Ok(folded)
}

fn text_chunk_shape(update: &Value) -> Result<Option<(Value, &str)>, RuntimeStateError> {
    if !matches!(
        update.get("sessionUpdate").and_then(Value::as_str),
        Some("user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk")
    ) {
        return Ok(None);
    }
    let content = update
        .get("content")
        .and_then(Value::as_object)
        .ok_or(RuntimeStateError::OperationMismatch)?;
    let content_type = content
        .get("type")
        .and_then(Value::as_str)
        .ok_or(RuntimeStateError::OperationMismatch)?;
    if update
        .get("messageId")
        .is_some_and(|message_id| !message_id.is_null() && !message_id.is_string())
    {
        return Err(RuntimeStateError::OperationMismatch);
    }
    if content_type != "text" {
        return Ok(None);
    }
    let text = content
        .get("text")
        .and_then(Value::as_str)
        .ok_or(RuntimeStateError::OperationMismatch)?;
    let mut shape = update.clone();
    if shape.get("messageId").is_some_and(Value::is_null) {
        shape
            .as_object_mut()
            .expect("a session update with fields is an object")
            .remove("messageId");
    }
    shape["content"]["text"] = Value::String(String::new());
    Ok(Some((shape, text)))
}

fn required_fold_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, RuntimeStateError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(RuntimeStateError::OperationMismatch)
}

fn replace_shared_fold_slot(
    folded: &mut Vec<Arc<Value>>,
    matches_slot: impl Fn(&Value) -> bool,
    update: &Value,
) {
    if let Some(position) = folded
        .iter()
        .position(|candidate| matches_slot(candidate.as_ref()))
    {
        folded[position] = Arc::new(update.clone());
    } else {
        folded.push(Arc::new(update.clone()));
    }
}

fn replace_tool_fields(target: &mut Value, patch: &Value) -> Result<(), RuntimeStateError> {
    let target = target
        .as_object_mut()
        .ok_or(RuntimeStateError::OperationMismatch)?;
    let patch = patch
        .as_object()
        .ok_or(RuntimeStateError::OperationMismatch)?;
    for (key, patch_value) in patch {
        if !patch_value.is_null() {
            target.insert(key.clone(), patch_value.clone());
        }
    }
    Ok(())
}

#[cfg(test)]
impl SessionRegistry {
    pub(crate) fn seq(&self) -> u64 {
        self.journal.through_seq()
    }

    pub(crate) fn open_new(
        &mut self,
        expected_epoch: &str,
        session_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
        session: Value,
    ) -> Result<u64, RuntimeStateError> {
        let session_id = session_id.into();
        let incarnation = self.open_session(
            expected_epoch,
            session_id.clone(),
            cwd.into(),
            session,
            BTreeMap::new(),
        )?;
        self.register_new(session_id, incarnation);
        Ok(incarnation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state() -> RuntimeState {
        RuntimeState::new("epoch")
    }

    fn open(state: &mut RuntimeState, session_id: &str) -> u64 {
        state
            .open_new(
                "epoch",
                session_id,
                format!("/{session_id}"),
                json!({ "sessionId": session_id }),
            )
            .unwrap()
    }

    fn start_prompt_operation(
        state: &mut RuntimeState,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        prompt: Vec<Value>,
    ) -> String {
        let operation_id = operation_id.to_string();
        state
            .start_prompt(
                "epoch",
                session_id,
                incarnation,
                operation_id.clone(),
                prompt,
            )
            .unwrap();
        operation_id
    }

    #[test]
    fn close_cleanup_rejects_late_live_resources_without_publication_or_resurrection() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_operation(
                "epoch",
                "session",
                incarnation,
                "close",
                SessionOperationKind::Close,
                "closing",
            )
            .unwrap();
        state
            .close_session("epoch", "session", incarnation, "close")
            .unwrap();
        let before = state.state("session").unwrap().clone();
        let published = state.snapshot();
        assert_eq!(
            state.upsert_permission("epoch", "session", incarnation, "late", json!({})),
            Err(RuntimeStateError::UnknownSession)
        );
        assert_eq!(
            state.upsert_elicitation("epoch", Some(("session", incarnation)), "late", json!({})),
            Err(RuntimeStateError::UnknownSession)
        );
        assert_eq!(
            state.upsert_terminal(
                "epoch",
                "session",
                incarnation,
                "late",
                json!({ "output": "late" })
            ),
            Err(RuntimeStateError::UnknownSession)
        );
        assert_eq!(state.state("session").unwrap(), &before);
        assert_eq!(state.snapshot(), published);
        assert!(state.take_effects().is_empty());
    }

    #[test]
    fn direct_operation_entry_cannot_bypass_canonical_history_admission() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        let revision = state
            .state("session")
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        let TurnAdmission::Accepted { operation_id } = state
            .admit_session_turn("epoch", "session", incarnation, &revision, "prompt", vec![])
            .unwrap()
        else {
            panic!("accepted");
        };
        state
            .complete_prompt("epoch", "session", incarnation, "prompt", json!({}))
            .unwrap();
        state
            .complete_turn(
                "session",
                incarnation,
                &operation_id,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        let before = state.snapshot();
        assert_eq!(
            state.start_operation(
                "epoch",
                "session",
                incarnation,
                "control",
                SessionOperationKind::SetMode,
                "request"
            ),
            Err(RuntimeStateError::BusySession)
        );
        assert_eq!(
            state.start_reload(
                "epoch",
                "session",
                incarnation,
                "load",
                SessionOperationKind::Load
            ),
            Err(RuntimeStateError::BusySession)
        );
        assert_eq!(state.snapshot(), before);
        state
            .block_turn("session", incarnation, &operation_id, "history unavailable")
            .unwrap();
        let before = state.snapshot();
        assert_eq!(
            state.start_operation(
                "epoch",
                "session",
                incarnation,
                "fork",
                SessionOperationKind::Fork,
                "request"
            ),
            Err(RuntimeStateError::BusySession)
        );
        assert_eq!(state.snapshot(), before);
    }

    #[test]
    fn history_terminal_requires_live_retirement_and_dto_mutation_cannot_change_owner() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state.register_new("session", incarnation);
        let revision = state
            .state("session")
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        let TurnAdmission::Accepted { operation_id } = state
            .admit_session_turn("epoch", "session", incarnation, &revision, "intent", vec![])
            .unwrap()
        else {
            panic!("accepted");
        };
        state
            .upsert_permission("epoch", "session", incarnation, "permission", json!({}))
            .unwrap();
        let before = state.state("session").unwrap().clone();
        assert_eq!(
            state.complete_turn(
                "session",
                incarnation,
                &operation_id,
                json!({ "stopReason": "end_turn" })
            ),
            Err(MirrorError::OperationMismatch)
        );
        assert_eq!(state.state("session").unwrap(), &before);
        let mut disposable = state.session("session").unwrap();
        disposable.active_turn.as_mut().unwrap().cancel_requested = true;
        disposable.permissions.clear();
        assert_eq!(state.state("session").unwrap(), &before);
        state
            .complete_prompt("epoch", "session", incarnation, "intent", json!({}))
            .unwrap();
        assert_eq!(
            state.take_effects(),
            vec![RuntimeEffect::CancelPermissionResponder {
                session_id: "session".to_string(),
                incarnation: incarnation,
                interaction_id: "permission".to_string()
            }]
        );
        state
            .complete_turn(
                "session",
                incarnation,
                &operation_id,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        assert!(state.state("session").unwrap().turn_execution().is_none());
        assert!(state.session("session").unwrap().active_turn.is_none());
        assert_eq!(
            state.state("session").unwrap().phase,
            crate::session_state::MirrorPhase::Reconciling
        );
    }

    #[test]
    fn unified_turn_admission_is_atomic_and_duplicate_publication_is_neutral() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state.register_new("session", incarnation);
        let revision = state
            .state("session")
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        state
            .require_live_mut("session", incarnation)
            .unwrap()
            .lifecycle = SessionLifecycle::Closed;
        let before = state.state("session").unwrap().clone();
        let seq = state.seq();
        let next_operation = state.next_operation;
        assert_eq!(
            state.admit_session_turn(
                "epoch",
                "session",
                incarnation,
                &revision,
                "intent",
                vec![json!("hello")]
            ),
            Err(SessionTurnError::Live(RuntimeStateError::SessionNotActive))
        );
        assert_eq!(state.state("session").unwrap(), &before);
        assert_eq!(state.seq(), seq);
        assert_eq!(state.next_operation, next_operation);
        state
            .require_live_mut("session", incarnation)
            .unwrap()
            .lifecycle = SessionLifecycle::Active;
        let accepted = state
            .admit_session_turn(
                "epoch",
                "session",
                incarnation,
                &revision,
                "intent",
                vec![json!("hello")],
            )
            .unwrap();
        let TurnAdmission::Accepted { operation_id } = accepted else {
            panic!("first intent is accepted");
        };
        assert_eq!(
            state
                .state("session")
                .unwrap()
                .turn_execution()
                .unwrap()
                .rpc_operation_id,
            "intent"
        );
        let before = state.snapshot();
        assert_eq!(
            state.admit_session_turn(
                "epoch",
                "session",
                incarnation,
                &revision,
                "intent",
                vec![json!("hello")]
            ),
            Ok(TurnAdmission::Duplicate { operation_id })
        );
        assert_eq!(state.snapshot(), before);
    }

    #[test]
    fn a_reserved_operation_exempts_only_itself_from_global_identity_checks() {
        for kind in [SessionOperationKind::SetMode, SessionOperationKind::Load] {
            let mut state = state();
            let first = open(&mut state, "first");
            let second = open(&mut state, "second");
            state
                .start_prompt("epoch", "first", first, "shared-id", vec![])
                .unwrap();
            state.register_new("second", second);
            state
                .sessions
                .get_mut("second")
                .unwrap()
                .state
                .begin_exclusive(kind, "shared-id")
                .unwrap();
            let before = state.snapshot();
            let result = if kind == SessionOperationKind::Load {
                state.start_reload("epoch", "second", second, "shared-id", kind)
            } else {
                state.start_operation("epoch", "second", second, "shared-id", kind, "request")
            };
            assert_eq!(result, Err(RuntimeStateError::OperationCollision));
            assert_eq!(state.snapshot(), before);
            assert_eq!(
                state
                    .state("second")
                    .unwrap()
                    .operation
                    .as_ref()
                    .unwrap()
                    .stage,
                "reserved"
            );
        }
    }

    #[test]
    fn a_reserved_operation_still_checks_request_response_identities() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state.register_new("session", incarnation);
        state
            .upsert_elicitation("epoch", None, "elicitation", json!({}))
            .unwrap();
        state
            .begin_elicitation_response("epoch", None, "elicitation", "response-id")
            .unwrap();
        state
            .sessions
            .get_mut("session")
            .unwrap()
            .state
            .begin_exclusive(SessionOperationKind::SetMode, "response-id")
            .unwrap();
        assert_eq!(
            state.start_operation(
                "epoch",
                "session",
                incarnation,
                "response-id",
                SessionOperationKind::SetMode,
                "request"
            ),
            Err(RuntimeStateError::OperationCollision)
        );
    }

    #[test]
    fn close_confirmation_keeps_the_unique_owner_until_exact_cleanup_finishes() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state.register_new("session", incarnation);
        state
            .begin_exclusive("session", incarnation, SessionOperationKind::Close, "close")
            .unwrap();
        state
            .start_operation(
                "epoch",
                "session",
                incarnation,
                "close",
                SessionOperationKind::Close,
                "closing",
            )
            .unwrap();
        assert_eq!(
            state
                .state("session")
                .unwrap()
                .operation
                .as_ref()
                .unwrap()
                .stage,
            "closing"
        );
        state
            .close_session("epoch", "session", incarnation, "close")
            .unwrap();
        assert_eq!(
            state.live("session").unwrap().lifecycle,
            SessionLifecycle::Closed
        );
        assert_eq!(
            state
                .state("session")
                .unwrap()
                .operation
                .as_ref()
                .unwrap()
                .stage,
            "cleanup"
        );
        assert!(state.session("session").is_none());
        assert!(!state.snapshot().sessions.contains_key("session"));
        assert_eq!(
            state.open_new("epoch", "session", "/new", json!({})),
            Err(RuntimeStateError::BusySession)
        );
        assert_eq!(
            state.start_attachment(
                "epoch",
                "session",
                "/new",
                "attach",
                SessionOperationKind::Load
            ),
            Err(RuntimeStateError::BusySession)
        );
        assert_eq!(
            state.finish_session_cleanup("session", incarnation, SessionAdmission::Close, "stale"),
            Err(MirrorError::OperationMismatch)
        );
        assert_eq!(
            state.finish_session_cleanup(
                "session",
                incarnation + 1,
                SessionAdmission::Close,
                "close"
            ),
            Err(MirrorError::StaleIncarnation)
        );
        state
            .finish_session_cleanup("session", incarnation, SessionAdmission::Close, "close")
            .unwrap();
        state.remove("session", incarnation);
        assert!(state.state("session").is_none());
        assert!(open(&mut state, "session") > incarnation);
    }

    #[test]
    fn operation_and_prompt_uncertainty_retire_in_either_order_without_losing_owner() {
        for operation_first in [true, false] {
            let mut state = state();
            let incarnation = open(&mut state, "session");
            state.register_new("session", incarnation);
            let revision = state
                .state("session")
                .unwrap()
                .history_revision
                .clone()
                .unwrap();
            let TurnAdmission::Accepted { operation_id } = state
                .admit_session_turn("epoch", "session", incarnation, &revision, "prompt", vec![])
                .unwrap()
            else {
                panic!("accepted");
            };
            state
                .begin_exclusive(
                    "session",
                    incarnation,
                    SessionOperationKind::SetMode,
                    "control",
                )
                .unwrap();
            state
                .start_operation(
                    "epoch",
                    "session",
                    incarnation,
                    "control",
                    SessionOperationKind::SetMode,
                    "request",
                )
                .unwrap();
            if operation_first {
                state
                    .mark_operation_uncertain("epoch", "session", incarnation, "control", "EOF")
                    .unwrap();
                assert!(state.state("session").unwrap().turn_execution().is_some());
            }
            state
                .mark_prompt_uncertain("epoch", "session", incarnation, "prompt", "EOF")
                .unwrap();
            state
                .block_turn("session", incarnation, &operation_id, "EOF")
                .unwrap();
            if !operation_first {
                state
                    .mark_operation_uncertain("epoch", "session", incarnation, "control", "EOF")
                    .unwrap();
            }
            let owner = state.state("session").unwrap();
            assert_eq!(owner.phase, crate::session_state::MirrorPhase::Blocked);
            assert!(owner.active_turn.as_ref().unwrap().terminal.is_none());
            assert!(owner.turn_execution().is_none());
            assert_eq!(owner.operation.as_ref().unwrap().stage, "uncertain");
            assert!(state.session("session").unwrap().active_turn.is_none());
        }
    }

    #[test]
    fn active_turn_folds_only_adjacent_compatible_text_chunks_but_publishes_raw_deltas() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        let before = state.seq();
        let updates = [
            json!({
                "sessionUpdate": "agent_message_chunk",
                "messageId": "answer",
                "content": { "type": "text", "text": "hello " }
            }),
            json!({
                "sessionUpdate": "agent_message_chunk",
                "messageId": "answer",
                "content": { "type": "text", "text": "world" }
            }),
            json!({
                "sessionUpdate": "agent_thought_chunk",
                "messageId": "thought",
                "content": { "type": "text", "text": "reason " }
            }),
            json!({
                "sessionUpdate": "agent_thought_chunk",
                "messageId": "thought",
                "content": { "type": "text", "text": "carefully" }
            }),
            json!({
                "sessionUpdate": "user_message_chunk",
                "messageId": "echo",
                "content": { "type": "text", "text": "user" }
            }),
            json!({
                "sessionUpdate": "agent_message_chunk",
                "messageId": "answer",
                "content": { "type": "text", "text": " later" }
            }),
        ];
        for update in updates.clone() {
            state
                .append_runtime_update_for_test("epoch", "session", incarnation, update)
                .unwrap();
        }

        let retained = state
            .session("session")
            .unwrap()
            .active_turn
            .unwrap()
            .updates;
        assert_eq!(retained.len(), 4);
        assert_eq!(retained[0]["content"]["text"], "hello world");
        assert_eq!(retained[1]["content"]["text"], "reason carefully");
        assert_eq!(retained[2]["content"]["text"], "user");
        assert_eq!(retained[3]["content"]["text"], " later");

        let published = state
            .deltas_after(before)
            .unwrap()
            .into_iter()
            .map(|delta| match delta.change {
                RuntimeChange::TurnUpdateAppended { update, .. } => update,
                other => panic!("unexpected retained turn delta: {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(published, updates);
    }

    #[test]
    fn active_turn_replaces_tool_fields_at_the_first_position_without_changing_start_kind() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        let before = state.seq();
        let updates = [
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "tool",
                "status": "pending",
                "rawInput": { "path": "/first", "nested": { "left": 1 } },
                "rawOutput": { "obsolete": true },
                "locations": [{ "path": "/first" }]
            }),
            json!({
                "sessionUpdate": "agent_message_chunk",
                "messageId": "between",
                "content": { "type": "text", "text": "between" }
            }),
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "tool",
                "status": "in_progress",
                "rawInput": { "nested": { "right": 2 } },
                "rawOutput": { "result": "ok" },
                "locations": []
            }),
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "tool",
                "rawInput": { "path": "/last", "nested": { "left": 3 } },
                "status": null
            }),
        ];
        for update in updates.clone() {
            state
                .append_runtime_update_for_test("epoch", "session", incarnation, update)
                .unwrap();
        }

        let retained = state
            .session("session")
            .unwrap()
            .active_turn
            .unwrap()
            .updates;
        assert_eq!(retained.len(), 2);
        assert_eq!(retained[0]["sessionUpdate"], "tool_call");
        assert_eq!(retained[0]["toolCallId"], "tool");
        assert_eq!(retained[0]["status"], "in_progress");
        assert_eq!(
            retained[0]["rawInput"],
            json!({ "path": "/last", "nested": { "left": 3 } })
        );
        assert_eq!(retained[0]["rawOutput"], json!({ "result": "ok" }));
        assert_eq!(retained[0]["locations"], json!([]));
        assert_eq!(retained[1]["content"]["text"], "between");
        let published = state
            .deltas_after(before)
            .unwrap()
            .into_iter()
            .map(|delta| match delta.change {
                RuntimeChange::TurnUpdateAppended { update, .. } => update,
                other => panic!("unexpected retained turn delta: {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(published, updates);
    }

    #[test]
    fn active_turn_replaces_plan_slots_in_place_and_removal_clears_only_its_slot() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        for update in [
            json!({
                "sessionUpdate": "plan_update",
                "plan": { "type": "markdown", "planId": "one", "content": "first" }
            }),
            json!({
                "sessionUpdate": "agent_message_chunk",
                "messageId": "between",
                "content": { "type": "text", "text": "between" }
            }),
            json!({
                "sessionUpdate": "plan_update",
                "plan": { "type": "markdown", "planId": "two", "content": "other" }
            }),
            json!({
                "sessionUpdate": "plan_update",
                "plan": { "type": "markdown", "planId": "one", "content": "latest" }
            }),
        ] {
            state
                .append_runtime_update_for_test("epoch", "session", incarnation, update)
                .unwrap();
        }
        let retained = state
            .session("session")
            .unwrap()
            .active_turn
            .unwrap()
            .updates;
        assert_eq!(retained.len(), 3);
        assert_eq!(retained[0]["plan"]["planId"], "one");
        assert_eq!(retained[0]["plan"]["content"], "latest");
        assert_eq!(retained[1]["content"]["text"], "between");
        assert_eq!(retained[2]["plan"]["planId"], "two");

        for update in [
            json!({ "sessionUpdate": "plan_removed", "planId": "one" }),
            json!({
                "sessionUpdate": "plan",
                "entries": [{ "content": "legacy one" }]
            }),
            json!({
                "sessionUpdate": "plan",
                "entries": [{ "content": "legacy latest" }]
            }),
        ] {
            state
                .append_runtime_update_for_test("epoch", "session", incarnation, update)
                .unwrap();
        }

        let retained = state
            .session("session")
            .unwrap()
            .active_turn
            .unwrap()
            .updates;
        assert_eq!(retained.len(), 3);
        assert_eq!(retained[0]["content"]["text"], "between");
        assert_eq!(retained[1]["plan"]["planId"], "two");
        assert_eq!(retained[2]["sessionUpdate"], "plan");
        assert_eq!(retained[2]["entries"][0]["content"], "legacy latest");
        assert!(retained.iter().all(|update| {
            update.get("planId").and_then(Value::as_str) != Some("one")
                && update
                    .get("plan")
                    .and_then(|plan| plan.get("planId"))
                    .and_then(Value::as_str)
                    != Some("one")
        }));
    }

    #[test]
    fn invalid_fold_replacements_are_transactional_and_large_valid_merge_commits() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        state
            .append_runtime_update_for_test(
                "epoch",
                "session",
                incarnation,
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": "tool",
                    "details": { "stable": true }
                }),
            )
            .unwrap();
        let before_invalid = state.snapshot();
        assert_eq!(
            state.append_runtime_update_for_test(
                "epoch",
                "session",
                incarnation,
                json!({ "sessionUpdate": "tool_call_update", "status": "invalid" }),
            ),
            Err(RuntimeStateError::OperationMismatch),
        );
        assert_eq!(state.snapshot(), before_invalid);
        assert!(
            state
                .deltas_after(before_invalid.through_seq)
                .unwrap()
                .is_empty()
        );

        assert_eq!(
            state.append_runtime_update_for_test(
                "epoch",
                "session",
                incarnation,
                json!({
                    "sessionUpdate": "plan_update",
                    "plan": { "type": "markdown", "content": "missing ID" }
                }),
            ),
            Err(RuntimeStateError::OperationMismatch),
        );
        assert_eq!(state.snapshot(), before_invalid);
        assert!(
            state
                .deltas_after(before_invalid.through_seq)
                .unwrap()
                .is_empty()
        );

        let before_large = state.snapshot();
        state
            .append_runtime_update_for_test(
                "epoch",
                "session",
                incarnation,
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "tool",
                    "details": { "oversize": "x".repeat(1_024) }
                }),
            )
            .unwrap();
        assert_ne!(state.snapshot(), before_large);
        assert_eq!(
            state.deltas_after(before_large.through_seq).unwrap().len(),
            1
        );
    }

    #[test]
    fn ten_thousand_terminal_turns_leave_no_folded_updates_or_payload_deltas() {
        let mut state = state();
        let incarnation = open(&mut state, "session");

        for index in 0..10_000 {
            let operation_id = format!("turn-{index}");
            state
                .start_prompt("epoch", "session", incarnation, &operation_id, Vec::new())
                .unwrap();
            state
                .append_runtime_update_for_test(
                    "epoch",
                    "session",
                    incarnation,
                    json!({
                        "sessionUpdate": "agent_message_chunk",
                        "messageId": operation_id,
                        "content": { "type": "text", "text": format!("turn-payload-{index}") }
                    }),
                )
                .unwrap();
            state
                .complete_prompt(
                    "epoch",
                    "session",
                    incarnation,
                    &format!("turn-{index}"),
                    json!({ "stopReason": "end_turn" }),
                )
                .unwrap();

            assert!(state.session("session").unwrap().active_turn.is_none());
        }

        assert!(
            !serde_json::to_string(&state.journal.deltas)
                .unwrap()
                .contains("turn-payload-")
        );
    }

    fn assert_terminal_turn_payload_was_dropped(
        state: &RuntimeState,
        session_id: &str,
        operation_id: &str,
        markers: &[&str],
    ) {
        let session = state.session(session_id).unwrap();
        let session_payload = serde_json::to_string(&session).unwrap();
        let active_turn = serde_json::to_string(&session.active_turn).unwrap();
        let delta_journal = serde_json::to_string(&state.journal.deltas).unwrap();
        let mut retained_by = Vec::new();

        assert!(!state.operation_in_use(operation_id));
        if session.active_turn.is_some() {
            retained_by.push("active_turn");
        }
        if markers
            .iter()
            .any(|marker| session_payload.contains(marker))
        {
            retained_by.push("session");
        }
        if markers.iter().any(|marker| active_turn.contains(marker)) {
            retained_by.push("active_turn_payload");
        }
        if markers.iter().any(|marker| delta_journal.contains(marker)) {
            retained_by.push("delta_journal");
        }

        assert!(
            retained_by.is_empty(),
            "terminal turn payload is still retained by: {}",
            retained_by.join(", ")
        );
    }

    #[test]
    fn terminal_response_drops_prompt_updates_indices_and_operation_payload() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        let operation_id = start_prompt_operation(
            &mut state,
            "session",
            incarnation,
            "success-client-intent",
            vec![json!({
                "type": "text",
                "text": "success-prompt-marker"
            })],
        );
        state
            .append_runtime_update_for_test(
                "epoch",
                "session",
                incarnation,
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "success-message-id",
                    "content": { "type": "text", "text": "success-update-marker" }
                }),
            )
            .unwrap();
        state
            .complete_prompt(
                "epoch",
                "session",
                incarnation,
                &operation_id,
                json!({
                    "stopReason": "end_turn",
                    "marker": "success-result-marker"
                }),
            )
            .unwrap();

        assert_terminal_turn_payload_was_dropped(
            &state,
            "session",
            &operation_id,
            &[
                "success-prompt-marker",
                "success-update-marker",
                "success-result-marker",
            ],
        );
    }

    #[test]
    fn prompt_error_drops_all_turn_payload_after_terminal_delivery() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        let operation_id = start_prompt_operation(
            &mut state,
            "session",
            incarnation,
            "error-client-intent",
            vec![json!({ "type": "text", "text": "error-prompt-marker" })],
        );
        state
            .append_runtime_update_for_test(
                "epoch",
                "session",
                incarnation,
                json!({
                    "sessionUpdate": "agent_thought_chunk",
                    "content": { "type": "text", "text": "error-update-marker" }
                }),
            )
            .unwrap();
        state
            .fail_prompt(
                "epoch",
                "session",
                incarnation,
                &operation_id,
                json!({ "message": "error-result-marker" }),
            )
            .unwrap();

        assert_terminal_turn_payload_was_dropped(
            &state,
            "session",
            &operation_id,
            &[
                "error-prompt-marker",
                "error-update-marker",
                "error-result-marker",
            ],
        );
    }

    #[test]
    fn transport_loss_produces_one_uncertain_terminal_then_drops_the_turn() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        let operation_id = start_prompt_operation(
            &mut state,
            "session",
            incarnation,
            "uncertain-client-intent",
            vec![json!({
                "type": "text",
                "text": "uncertain-prompt-marker"
            })],
        );
        state
            .append_runtime_update_for_test(
                "epoch",
                "session",
                incarnation,
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "uncertain-tool-id",
                    "marker": "uncertain-update-marker"
                }),
            )
            .unwrap();
        state
            .mark_prompt_uncertain(
                "epoch",
                "session",
                incarnation,
                &operation_id,
                "uncertain-result-marker",
            )
            .unwrap();

        assert_terminal_turn_payload_was_dropped(
            &state,
            "session",
            &operation_id,
            &[
                "uncertain-prompt-marker",
                "uncertain-update-marker",
                "uncertain-result-marker",
            ],
        );
    }

    #[test]
    fn new_turn_can_reuse_prior_message_and_tool_ids() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        let first_operation_id = start_prompt_operation(
            &mut state,
            "session",
            incarnation,
            "first-client-intent",
            vec![json!("first-prompt-marker")],
        );
        state
            .append_runtime_update_for_test(
                "epoch",
                "session",
                incarnation,
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "reused-message-id",
                    "content": { "type": "text", "text": "first-message-marker" }
                }),
            )
            .unwrap();
        state
            .append_runtime_update_for_test(
                "epoch",
                "session",
                incarnation,
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": "reused-tool-id",
                    "marker": "first-tool-marker"
                }),
            )
            .unwrap();
        state
            .complete_prompt(
                "epoch",
                "session",
                incarnation,
                &first_operation_id,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();

        let second_operation_id = start_prompt_operation(
            &mut state,
            "session",
            incarnation,
            "second-client-intent",
            vec![json!("second prompt")],
        );
        state
            .append_runtime_update_for_test(
                "epoch",
                "session",
                incarnation,
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "reused-message-id",
                    "content": { "type": "text", "text": "second message" }
                }),
            )
            .unwrap();
        state
            .append_runtime_update_for_test(
                "epoch",
                "session",
                incarnation,
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": "reused-tool-id",
                    "marker": "second tool"
                }),
            )
            .unwrap();

        let session = state.session("session").unwrap();
        let active = session.active_turn.as_ref().unwrap();
        assert_eq!(active.operation_id, second_operation_id);
        assert_eq!(active.updates.len(), 2);
        let session_payload = serde_json::to_string(&session).unwrap();
        assert!(!session_payload.contains("first-message-marker"));
        assert!(!session_payload.contains("first-tool-marker"));
    }

    #[test]
    fn turn_retirement_does_not_release_a_live_terminal() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        let operation_id = start_prompt_operation(
            &mut state,
            "session",
            incarnation,
            "resource-client-intent",
            vec![json!("resource-prompt-marker")],
        );
        state
            .upsert_terminal(
                "epoch",
                "session",
                incarnation,
                "live-terminal",
                json!({
                    "terminalId": "live-terminal",
                    "output": "live-terminal-output",
                    "released": false
                }),
            )
            .unwrap();
        state
            .upsert_elicitation(
                "epoch",
                Some(("session", incarnation)),
                "url-elicitation",
                json!({ "mode": "url", "url": "https://example.test/live" }),
            )
            .unwrap();
        state
            .resolve_elicitation(
                "epoch",
                Some(("session", incarnation)),
                "url-elicitation",
                Some("live-url-flow"),
            )
            .unwrap();
        state
            .complete_prompt(
                "epoch",
                "session",
                incarnation,
                &operation_id,
                json!({
                    "stopReason": "end_turn",
                    "marker": "resource-result-marker"
                }),
            )
            .unwrap();

        let session = state.session("session").unwrap();
        assert_eq!(
            session.terminals["live-terminal"]["output"],
            json!("live-terminal-output")
        );
        assert_eq!(
            session.url_flows["live-url-flow"].status,
            UrlFlowStatus::Waiting
        );
        assert_terminal_turn_payload_was_dropped(
            &state,
            "session",
            &operation_id,
            &["resource-prompt-marker", "resource-result-marker"],
        );
    }

    #[test]
    fn sequential_completed_turns_leave_active_and_completed_payload_bytes_at_zero() {
        let mut state = state();
        let incarnation = open(&mut state, "session");

        for index in 0..32 {
            let marker = format!("sequential-turn-payload-{index}");
            let operation_id = start_prompt_operation(
                &mut state,
                "session",
                incarnation,
                &format!("sequential-intent-{index}"),
                vec![json!({ "type": "text", "text": marker })],
            );
            state
                .append_runtime_update_for_test(
                    "epoch",
                    "session",
                    incarnation,
                    json!({
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": marker }
                    }),
                )
                .unwrap();
            state
                .complete_prompt(
                    "epoch",
                    "session",
                    incarnation,
                    &operation_id,
                    json!({ "stopReason": "end_turn", "marker": marker }),
                )
                .unwrap();
        }

        let session = state.session("session").unwrap();
        assert!(session.active_turn.is_none());

        assert!(
            !serde_json::to_string(&state.journal.deltas)
                .unwrap()
                .contains("sequential-turn-payload"),
            "delivery storage must not retain completed turn payloads"
        );
    }

    #[test]
    fn turn_terminal_event_retires_active_exactly_once() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_prompt(
                "epoch",
                "session",
                incarnation,
                "prompt",
                vec![json!("hello")],
            )
            .unwrap();
        state
            .append_runtime_update_for_test("epoch", "session", incarnation, json!("answer"))
            .unwrap();
        state
            .complete_prompt(
                "epoch",
                "session",
                incarnation,
                "prompt",
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();

        let session = state.session("session").unwrap();
        assert!(session.active_turn.is_none());
        let settled_seq = state.seq();
        assert_eq!(
            state.complete_prompt(
                "epoch",
                "session",
                incarnation,
                "prompt",
                json!({ "stopReason": "end_turn" }),
            ),
            Err(RuntimeStateError::NoActiveTurn),
        );
        assert_eq!(state.seq(), settled_seq);
        assert!(state.session("session").unwrap().active_turn.is_none());
    }

    #[test]
    fn new_replay_keeps_control_metadata_without_conversation() {
        let mut state = state();
        state
            .open_new_with_replay(
                "epoch",
                "session",
                "/session",
                json!({
                    "sessionId": "session",
                    "modes": {
                        "currentModeId": "plan",
                        "availableModes": [{ "id": "plan", "name": "Plan" }]
                    },
                    "configOptions": [],
                    "_meta": { "debugPayload": "session-meta-must-not-be-retained" }
                }),
                vec![
                    json!({
                        "sessionUpdate": "agent_message_chunk",
                        "content": "conversation-must-not-be-stored"
                    }),
                    json!({
                        "sessionUpdate": "current_mode_update",
                        "currentModeId": "plan"
                    }),
                ],
            )
            .unwrap();

        let session = state.session("session").unwrap();
        assert_eq!(
            session.control_state["current_mode_update"]["currentModeId"],
            "plan"
        );
        assert_eq!(session.session["modes"]["currentModeId"], "plan");
        assert!(
            !serde_json::to_string(&session)
                .unwrap()
                .contains("conversation-must-not-be-stored")
        );
        assert!(
            !serde_json::to_string(&session)
                .unwrap()
                .contains("session-meta-must-not-be-retained")
        );
    }

    #[test]
    fn cancel_request_is_nonterminal_until_prompt_settles() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        state
            .request_cancel("epoch", "session", incarnation)
            .unwrap();
        state
            .append_runtime_update_for_test("epoch", "session", incarnation, json!("after cancel"))
            .unwrap();

        let active = state.session("session").unwrap().active_turn.unwrap();
        assert!(active.cancel_requested);
        assert_eq!(*active.updates[0], json!("after cancel"));
    }

    #[test]
    fn runtime_delivery_shares_the_authoritative_turn_payload() {
        use crate::session_mirror::TurnAdmission;

        let mut state = state();
        let incarnation = open(&mut state, "session");
        state.register_new("session", incarnation);
        let revision = state
            .state("session")
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        let TurnAdmission::Accepted { operation_id } = state
            .admit_session_turn(
                "epoch",
                "session",
                incarnation,
                &revision,
                "intent",
                vec![json!({ "type": "text", "text": "hello" })],
            )
            .unwrap()
        else {
            panic!("new turn must be accepted");
        };
        let overlay = state
            .state("session")
            .unwrap()
            .active_turn
            .as_ref()
            .unwrap()
            .clone();
        let published = state.snapshot();
        let projected = published.sessions["session"].active_turn.as_ref().unwrap();
        assert!(Arc::ptr_eq(&projected.prompt, &overlay.prompt));
        assert!(Arc::ptr_eq(&projected.updates, &overlay.updates));

        let update = json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "answer" } });
        state
            .append_turn_update("session", incarnation, &operation_id, update.clone())
            .unwrap();
        let overlay = state
            .state("session")
            .unwrap()
            .active_turn
            .as_ref()
            .unwrap()
            .clone();
        state
            .project_turn_update("epoch", "session", incarnation, &overlay, update.clone())
            .unwrap();
        let current = state
            .session("session")
            .unwrap()
            .active_turn
            .as_ref()
            .unwrap()
            .clone();
        assert!(Arc::ptr_eq(&current.prompt, &overlay.prompt));
        assert!(Arc::ptr_eq(&current.updates, &overlay.updates));
        assert_eq!(*current.updates[0], update);
        assert!(
            projected.updates.is_empty(),
            "an already published view is immutable"
        );

        let mut stale = overlay.clone();
        stale.client_intent_id = "another-intent".to_string();
        let before = state.snapshot();
        assert_eq!(
            state.project_turn_update("epoch", "session", incarnation, &stale, json!({})),
            Err(RuntimeStateError::OperationMismatch)
        );
        assert_eq!(state.snapshot(), before);
    }

    #[test]
    fn epoch_and_incarnation_reject_stale_mutations() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        assert_eq!(
            state.start_prompt("old", "session", incarnation, "prompt", Vec::new()),
            Err(RuntimeStateError::EpochMismatch),
        );
        assert_eq!(
            state.start_prompt("epoch", "session", incarnation + 1, "prompt", Vec::new(),),
            Err(RuntimeStateError::StaleIncarnation),
        );
        assert!(state.session("session").unwrap().active_turn.is_none());
    }

    #[test]
    fn snapshot_plus_live_deltas_equals_uninterrupted_state() {
        let mut state = state();
        let a = open(&mut state, "a");
        let snapshot = state.snapshot();
        state
            .start_prompt("epoch", "a", a, "prompt-a", vec![json!("A")])
            .unwrap();
        let b = open(&mut state, "b");
        state
            .start_prompt("epoch", "b", b, "prompt-b", vec![json!("B")])
            .unwrap();

        let deltas = state.deltas_after(snapshot.through_seq).unwrap();
        let mut projection = snapshot.sessions;
        for delta in deltas {
            match delta.change {
                RuntimeChange::ConnectionUpsert { .. } => {}
                RuntimeChange::SessionUpsert { session } => {
                    projection.insert(session.session_id.clone(), *session);
                }
                RuntimeChange::SessionControlUpdated {
                    session_id,
                    incarnation,
                    revision,
                    key,
                    update,
                    history_revision,
                    phase,
                    sync_error,
                } => {
                    let session = projection.get_mut(&session_id).unwrap();
                    assert_eq!(session.incarnation, incarnation);
                    assert_eq!(session.revision + 1, revision);
                    session.control_state.insert(key, update);
                    session.revision = revision;
                    session.history_revision = history_revision;
                    session.phase = phase;
                    session.sync_error = sync_error;
                }
                RuntimeChange::TurnUpdateAppended {
                    session_id,
                    incarnation,
                    revision,
                    operation_id,
                    update,
                } => {
                    let session = projection.get_mut(&session_id).unwrap();
                    assert_eq!(session.incarnation, incarnation);
                    assert_eq!(session.revision + 1, revision);
                    let turn = session.active_turn.as_mut().unwrap();
                    assert_eq!(turn.operation_id, operation_id);
                    Arc::make_mut(&mut turn.updates).push(Arc::new(update));
                    session.revision = revision;
                }
                RuntimeChange::TerminalUpdated { .. } => {
                    unreachable!("this case creates no terminals")
                }
                RuntimeChange::SessionRemoved { session_id, .. } => {
                    projection.remove(&session_id);
                }
            }
        }
        assert_eq!(projection, state.snapshot().sessions);
    }

    #[test]
    fn turn_retirement_never_changes_live_resources() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .upsert_permission(
                "epoch",
                "session",
                incarnation,
                "permission",
                json!({ "question": "allow" }),
            )
            .unwrap();
        state
            .upsert_elicitation(
                "epoch",
                Some(("session", incarnation)),
                "elicitation",
                json!({ "mode": "form" }),
            )
            .unwrap();
        state
            .upsert_terminal(
                "epoch",
                "session",
                incarnation,
                "terminal",
                json!({ "output": "running" }),
            )
            .unwrap();
        for index in 0..3 {
            let operation_id = format!("prompt-{index}");
            state
                .start_prompt(
                    "epoch",
                    "session",
                    incarnation,
                    &operation_id,
                    vec![json!(index)],
                )
                .unwrap();
            state
                .complete_prompt(
                    "epoch",
                    "session",
                    incarnation,
                    &operation_id,
                    json!({ "stopReason": "end_turn" }),
                )
                .unwrap();
        }

        let session = state.session("session").unwrap();
        assert!(session.permissions.contains_key("permission"));
        assert!(session.elicitations.contains_key("elicitation"));
        assert!(session.terminals.contains_key("terminal"));
    }

    #[test]
    fn active_turn_accounting_does_not_reject_a_large_valid_update() {
        let mut state = RuntimeState::new("epoch");
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        state
            .append_runtime_update_for_test(
                "epoch",
                "session",
                incarnation,
                json!({ "content": "x".repeat(512) }),
            )
            .unwrap();
        assert_eq!(
            state
                .session("session")
                .unwrap()
                .active_turn
                .as_ref()
                .unwrap()
                .updates
                .len(),
            1
        );
        state
            .request_cancel("epoch", "session", incarnation)
            .unwrap();
        assert!(
            state
                .session("session")
                .unwrap()
                .active_turn
                .as_ref()
                .unwrap()
                .cancel_requested
        );
    }

    #[test]
    fn streaming_turn_deltas_do_not_republish_the_full_active_tail() {
        let mut state = RuntimeState::new("epoch");
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        let before = state.seq();
        for index in 0..100 {
            state
                .append_runtime_update_for_test(
                    "epoch",
                    "session",
                    incarnation,
                    json!({ "index": index, "text": "x".repeat(128) }),
                )
                .unwrap();
        }

        let deltas = state.deltas_after(before).unwrap();
        let sizes = deltas.iter().map(serialized_len).collect::<Vec<_>>();
        assert_eq!(sizes.len(), 100);
        assert!(
            sizes.last().unwrap() <= &(sizes[0] * 2),
            "constant-sized chunks must produce constant-sized deltas: {sizes:?}"
        );
        assert!(
            sizes.iter().sum::<usize>() <= sizes[0] * sizes.len() * 2,
            "stream publication must grow linearly with the chunk count"
        );
    }

    #[test]
    fn controls_and_close_coexist_with_an_active_prompt() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        for kind in [
            SessionOperationKind::SetMode,
            SessionOperationKind::SetConfig,
        ] {
            state
                .start_operation("epoch", "session", incarnation, "control", kind, "changing")
                .unwrap();
            assert!(state.session("session").unwrap().active_turn.is_some());
            state
                .fail_operation(
                    "epoch",
                    "session",
                    incarnation,
                    "control",
                    kind,
                    json!({"message":"rejected"}),
                )
                .unwrap();
        }
        state
            .start_operation(
                "epoch",
                "session",
                incarnation,
                "close",
                SessionOperationKind::Close,
                "closing",
            )
            .unwrap();
        state
            .complete_prompt(
                "epoch",
                "session",
                incarnation,
                "prompt",
                json!({"stopReason":"cancelled"}),
            )
            .unwrap();
        assert_eq!(
            state.session("session").unwrap().lifecycle,
            SessionLifecycle::Closing
        );
        state
            .close_session("epoch", "session", incarnation, "close")
            .unwrap();
        assert!(state.session("session").is_none());
    }

    #[test]
    fn different_sessions_progress_independently() {
        let mut state = state();
        let a = open(&mut state, "a");
        let b = open(&mut state, "b");
        state
            .start_prompt("epoch", "a", a, "prompt-a", Vec::new())
            .unwrap();
        state
            .start_prompt("epoch", "b", b, "prompt-b", Vec::new())
            .unwrap();
        assert_eq!(
            state.start_operation(
                "epoch",
                "a",
                a,
                "close-a",
                SessionOperationKind::Close,
                "closing",
            ),
            Ok(()),
        );
        state
            .complete_prompt(
                "epoch",
                "b",
                b,
                "prompt-b",
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        assert!(state.session("a").unwrap().active_turn.is_some());
        assert!(state.session("b").unwrap().active_turn.is_none());
    }

    #[test]
    fn possible_agent_commit_is_reported_as_uncertain() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        state
            .mark_prompt_uncertain(
                "epoch",
                "session",
                incarnation,
                "prompt",
                "transport lost after dispatch",
            )
            .unwrap();

        let session = state.session("session").unwrap();
        assert_eq!(session.lifecycle, SessionLifecycle::Uncertain);
        assert!(session.active_turn.is_none());
    }

    #[test]
    fn uncertain_prompt_terminal_is_one_atomic_commit() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        let before = state.seq();

        state
            .mark_prompt_uncertain(
                "epoch",
                "session",
                incarnation,
                "prompt",
                "response lost after dispatch",
            )
            .unwrap();

        assert_eq!(state.seq(), before + 1);
        let deltas = state.deltas_after(before).unwrap();
        assert_eq!(deltas.len(), 1);
        let RuntimeChange::SessionUpsert { session } = &deltas[0].change else {
            panic!("uncertain prompt did not publish a session update")
        };
        assert_eq!(session.lifecycle, SessionLifecycle::Uncertain);
        assert!(session.active_turn.is_none());
        assert!(
            !serde_json::to_string(&deltas)
                .unwrap()
                .contains("response lost after dispatch")
        );
    }

    #[test]
    fn prompt_failure_retires_partial_turn_and_operation_payload() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        let operation_id = "prompt".to_string();
        state
            .start_prompt(
                "epoch",
                "session",
                incarnation,
                &operation_id,
                vec![json!("hello")],
            )
            .unwrap();
        state
            .append_runtime_update_for_test("epoch", "session", incarnation, json!("partial"))
            .unwrap();
        state
            .fail_prompt(
                "epoch",
                "session",
                incarnation,
                &operation_id,
                json!({ "code": -32000 }),
            )
            .unwrap();

        assert!(!state.operation_in_use(&operation_id));
        let session = state.session("session").unwrap();
        assert!(session.active_turn.is_none());
        assert_eq!(
            state.append_runtime_update_for_test("epoch", "session", incarnation, json!("late")),
            Err(RuntimeStateError::NoActiveTurn),
        );
    }

    #[test]
    fn cancelled_prompt_response_retires_the_intent_outcome() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        let operation_id = "prompt".to_string();
        state
            .start_prompt("epoch", "session", incarnation, &operation_id, Vec::new())
            .unwrap();
        state
            .request_cancel("epoch", "session", incarnation)
            .unwrap();
        state
            .complete_prompt(
                "epoch",
                "session",
                incarnation,
                &operation_id,
                json!({ "stopReason": "cancelled" }),
            )
            .unwrap();

        assert!(!state.operation_in_use(&operation_id));
        assert!(state.session("session").unwrap().active_turn.is_none());
    }

    #[test]
    fn resolving_an_interaction_twice_is_idempotent() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .upsert_permission(
                "epoch",
                "session",
                incarnation,
                "permission",
                json!({ "options": ["allow"] }),
            )
            .unwrap();

        assert_eq!(
            state
                .resolve_permission("epoch", "session", incarnation, "permission")
                .unwrap(),
            InteractionResolution::Applied,
        );
        let settled_seq = state.seq();
        assert_eq!(
            state
                .resolve_permission("epoch", "session", incarnation, "permission")
                .unwrap(),
            InteractionResolution::AlreadyResolved,
        );
        assert_eq!(state.seq(), settled_seq);
    }

    #[test]
    fn permission_response_is_visible_and_exactly_correlated() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        let response_operation = "permission-response".to_string();
        state
            .upsert_permission(
                "epoch",
                "session",
                incarnation,
                "permission",
                json!({ "options": ["allow"] }),
            )
            .unwrap();

        assert_eq!(
            state
                .begin_permission_response(
                    "epoch",
                    "session",
                    incarnation,
                    "permission",
                    &response_operation,
                )
                .unwrap(),
            InteractionResponseStart::Applied,
        );
        let responding_seq = state.seq();
        assert_eq!(
            state.session("session").unwrap().permissions["permission"]
                .responding_operation_id
                .as_deref(),
            Some(response_operation.as_str()),
        );
        assert_eq!(
            state
                .begin_permission_response(
                    "epoch",
                    "session",
                    incarnation,
                    "permission",
                    &response_operation,
                )
                .unwrap(),
            InteractionResponseStart::Duplicate,
        );
        assert_eq!(state.seq(), responding_seq);
        assert_eq!(
            state.begin_permission_response(
                "epoch",
                "session",
                incarnation,
                "permission",
                "response-2",
            ),
            Err(RuntimeStateError::InteractionCollision),
        );

        let before_stale_completion = state.snapshot();
        assert_eq!(
            state.complete_permission_response(
                "epoch",
                "session",
                incarnation,
                "permission",
                "stale-response",
            ),
            Err(RuntimeStateError::OperationMismatch),
        );
        assert_eq!(state.snapshot(), before_stale_completion);
        assert_eq!(
            state
                .complete_permission_response(
                    "epoch",
                    "session",
                    incarnation,
                    "permission",
                    &response_operation,
                )
                .unwrap(),
            InteractionResolution::Applied,
        );
        assert!(state.session("session").unwrap().permissions.is_empty());
        assert!(!state.operation_in_use(&response_operation));
    }

    #[test]
    fn delta_resume_rejects_future_sequence_and_preserves_all_deltas() {
        let mut state = RuntimeState::new("epoch");
        open(&mut state, "a");
        open(&mut state, "b");
        open(&mut state, "c");

        assert!(state.deltas_after(state.seq() + 1).is_none());
        assert_eq!(state.deltas_after(0).unwrap().len(), 3);
        assert_eq!(state.deltas_after(1).unwrap().len(), 2);
    }

    #[test]
    fn close_then_delete_is_irreversible_and_partial_failure_stays_closed() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_delete("epoch", "session", incarnation, "delete-operation")
            .unwrap();
        assert_eq!(
            state.session("session").unwrap().lifecycle,
            SessionLifecycle::ClosingForDelete,
        );

        state
            .delete_close_succeeded("epoch", "session", incarnation, "delete-operation")
            .unwrap();
        assert_eq!(
            state.session("session").unwrap().lifecycle,
            SessionLifecycle::Deleting,
        );

        state
            .fail_operation(
                "epoch",
                "session",
                incarnation,
                "delete-operation",
                SessionOperationKind::Delete,
                json!("delete failed"),
            )
            .unwrap();
        let session = state.session("session").unwrap();
        assert_eq!(session.lifecycle, SessionLifecycle::Closed);
        assert!(session.operation.is_none());
        assert!(!state.operation_in_use("delete-operation"));
        assert_eq!(
            state.start_prompt("epoch", "session", incarnation, "late", Vec::new()),
            Err(RuntimeStateError::SessionNotActive),
        );
    }

    #[test]
    fn reopening_a_closed_session_uses_a_new_incarnation() {
        let mut state = state();
        let first = open(&mut state, "session");
        state
            .start_operation(
                "epoch",
                "session",
                first,
                "close",
                SessionOperationKind::Close,
                "closing",
            )
            .unwrap();
        state
            .close_session("epoch", "session", first, "close")
            .unwrap();
        state
            .finish_session_cleanup("session", first, SessionAdmission::Close, "close")
            .unwrap();
        let second = open(&mut state, "session");

        assert_ne!(first, second);
        assert_eq!(
            state.start_prompt("epoch", "session", first, "stale", Vec::new()),
            Err(RuntimeStateError::StaleIncarnation),
        );
        assert!(state.session("session").unwrap().active_turn.is_none());
    }

    #[test]
    fn canonical_control_accessors_apply_updates_and_preserve_available_choices() {
        let mut state = state();
        let modes = json!({ "currentModeId": "build", "availableModes": [
            { "id": "build", "name": "Build" }, { "id": "plan", "name": "Plan" }
        ] });
        let options = json!([{ "id": "verbose", "name": "Verbose", "type": "boolean", "currentValue": false }]);
        let incarnation = state
            .open_new(
                "epoch",
                "session",
                "/workspace",
                json!({ "modes": modes, "configOptions": options }),
            )
            .unwrap();
        assert_eq!(state.live("session").unwrap().config_options(), &options);
        state
            .update_control_state(
                "epoch",
                "session",
                incarnation,
                "current_mode_update",
                json!({ "sessionUpdate": "current_mode_update", "currentModeId": "plan" }),
            )
            .unwrap();
        state
            .update_control_state(
                "epoch",
                "session",
                incarnation,
                "config_option_update",
                json!({ "sessionUpdate": "config_option_update", "configOptions": [] }),
            )
            .unwrap();
        let live = state.live("session").unwrap();
        assert_eq!(live.modes(), Some(&modes));
        assert_eq!(live.current_mode_id(), Some("plan"));
        assert_eq!(live.config_options(), &json!([]));
        crate::semantic::validate_session_mode_reference(live.modes(), "build").unwrap();
        crate::semantic::validate_session_mode_reference(live.modes(), "plan").unwrap();
        assert!(
            crate::semantic::validate_session_config_reference(
                live.config_options(),
                "verbose",
                &json!(true)
            )
            .is_err()
        );
        // An authoritative response replaces both the baseline and its overlay.
        state
            .start_reload(
                "epoch",
                "session",
                incarnation,
                "reload",
                SessionOperationKind::Load,
            )
            .unwrap();
        state
            .complete_reload(
                "epoch",
                "session",
                incarnation,
                "reload",
                json!({ "modes": modes, "configOptions": options }),
            )
            .unwrap();
        let live = state.live("session").unwrap();
        assert_eq!(live.current_mode_id(), Some("build"));
        assert_eq!(live.config_options(), &options);
    }

    #[test]
    fn fork_keeps_only_target_control_metadata() {
        let mut state = state();
        let source = open(&mut state, "source");
        state
            .start_prompt("epoch", "source", source, "prompt", vec![json!("Q")])
            .unwrap();
        state
            .append_runtime_update_for_test("epoch", "source", source, json!("A"))
            .unwrap();
        state
            .complete_prompt(
                "epoch",
                "source",
                source,
                "prompt",
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        state
            .upsert_permission(
                "epoch",
                "source",
                source,
                "permission",
                json!({ "question": "allow" }),
            )
            .unwrap();
        state
            .upsert_terminal(
                "epoch",
                "source",
                source,
                "terminal",
                json!({ "output": "live" }),
            )
            .unwrap();

        state
            .open_forked(
                "epoch",
                "derived",
                "/derived",
                json!({ "sessionId": "derived" }),
                ("source", source),
                None,
            )
            .unwrap();
        let derived = state.session("derived").unwrap();
        assert!(derived.permissions.is_empty());
        assert!(derived.terminals.is_empty());
        assert!(derived.active_turn.is_none());
        let derived_payload = serde_json::to_string(&derived).unwrap();
        assert!(!derived_payload.contains("Q"));
        assert!(!derived_payload.contains("A"));

        state
            .open_forked(
                "epoch",
                "replayed",
                "/replayed",
                json!({ "sessionId": "replayed" }),
                ("source", source),
                Some(vec![
                    json!({
                        "sessionUpdate": "agent_message_chunk",
                        "content": "target-conversation"
                    }),
                    json!({
                        "sessionUpdate": "current_mode_update",
                        "currentModeId": "plan"
                    }),
                ]),
            )
            .unwrap();
        let replayed = state.session("replayed").unwrap();
        assert_eq!(
            replayed.control_state["current_mode_update"]["currentModeId"],
            "plan"
        );
        assert!(
            !serde_json::to_string(&replayed)
                .unwrap()
                .contains("target-conversation")
        );
    }

    #[test]
    fn operation_identity_cannot_be_active_in_two_sessions() {
        let mut state = state();
        let a = open(&mut state, "a");
        let b = open(&mut state, "b");
        state
            .start_prompt("epoch", "a", a, "same-operation", Vec::new())
            .unwrap();

        assert_eq!(
            state.start_prompt("epoch", "b", b, "same-operation", Vec::new()),
            Err(RuntimeStateError::OperationCollision),
        );
        assert!(state.session("a").unwrap().active_turn.is_some());
        assert!(state.session("b").unwrap().active_turn.is_none());
    }

    #[test]
    fn retired_operation_identity_can_be_reused() {
        let mut state = state();
        let a = open(&mut state, "a");
        let b = open(&mut state, "b");
        state
            .start_prompt("epoch", "a", a, "same-operation", Vec::new())
            .unwrap();
        state
            .complete_prompt(
                "epoch",
                "a",
                a,
                "same-operation",
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        assert_eq!(
            state
                .start_prompt("epoch", "b", b, "same-operation", Vec::new())
                .unwrap(),
            "same-operation"
        );
        assert!(state.session("a").unwrap().active_turn.is_none());
        assert!(state.session("b").unwrap().active_turn.is_some());
    }

    #[test]
    fn duplicate_cancel_request_is_idempotent() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        state
            .request_cancel("epoch", "session", incarnation)
            .unwrap();
        let first_cancel_seq = state.seq();

        state
            .request_cancel("epoch", "session", incarnation)
            .unwrap();

        assert_eq!(state.seq(), first_cancel_seq);
        assert!(
            state
                .session("session")
                .unwrap()
                .active_turn
                .as_ref()
                .unwrap()
                .cancel_requested
        );
    }

    #[test]
    fn sealed_turn_drops_cancel_flag_and_cleans_owned_interactions() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        state
            .request_cancel("epoch", "session", incarnation)
            .unwrap();
        state
            .upsert_permission(
                "epoch",
                "session",
                incarnation,
                "permission",
                json!({ "question": "allow" }),
            )
            .unwrap();
        state
            .upsert_elicitation(
                "epoch",
                Some(("session", incarnation)),
                "elicitation",
                json!({ "mode": "form" }),
            )
            .unwrap();

        state
            .complete_prompt(
                "epoch",
                "session",
                incarnation,
                "prompt",
                json!({ "stopReason": "cancelled" }),
            )
            .unwrap();

        let session = state.session("session").unwrap();
        assert!(session.active_turn.is_none());
        assert!(session.permissions.is_empty());
        assert!(session.elicitations.is_empty());
        assert_eq!(
            state.take_effects(),
            vec![
                RuntimeEffect::CancelPermissionResponder {
                    session_id: "session".to_string(),
                    incarnation: incarnation,
                    interaction_id: "permission".to_string(),
                },
                RuntimeEffect::CancelElicitationResponder {
                    session_id: Some("session".to_string()),
                    incarnation: Some(incarnation),
                    interaction_id: "elicitation".to_string(),
                },
            ]
        );
        assert!(state.take_effects().is_empty());
    }

    #[test]
    fn duplicate_permission_request_never_overwrites_the_original() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        assert_eq!(
            state
                .upsert_permission(
                    "epoch",
                    "session",
                    incarnation,
                    "permission",
                    json!({ "question": "A" }),
                )
                .unwrap(),
            InteractionUpsert::Inserted,
        );
        let inserted_seq = state.seq();
        assert_eq!(
            state
                .upsert_permission(
                    "epoch",
                    "session",
                    incarnation,
                    "permission",
                    json!({ "question": "A" }),
                )
                .unwrap(),
            InteractionUpsert::Duplicate,
        );
        assert_eq!(state.seq(), inserted_seq);
        assert_eq!(
            state.upsert_permission(
                "epoch",
                "session",
                incarnation,
                "permission",
                json!({ "question": "B" }),
            ),
            Err(RuntimeStateError::InteractionCollision),
        );
        assert_eq!(
            state.session("session").unwrap().permissions["permission"].request,
            json!({ "question": "A" }),
        );
    }

    #[test]
    fn released_terminal_cannot_be_resurrected_by_late_state() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .upsert_terminal(
                "epoch",
                "session",
                incarnation,
                "terminal",
                json!({ "output": "running", "released": false }),
            )
            .unwrap();
        state
            .upsert_terminal(
                "epoch",
                "session",
                incarnation,
                "terminal",
                json!({ "output": "done", "released": true }),
            )
            .unwrap();
        let released_seq = state.seq();

        assert_eq!(
            state
                .upsert_terminal(
                    "epoch",
                    "session",
                    incarnation,
                    "terminal",
                    json!({ "output": "late", "released": false }),
                )
                .unwrap(),
            TerminalUpsert::Stale,
        );
        assert_eq!(state.seq(), released_seq);
        assert!(
            !state
                .session("session")
                .unwrap()
                .terminals
                .contains_key("terminal")
        );
        assert!(
            !serde_json::to_string(&state.snapshot())
                .unwrap()
                .contains("done")
        );
    }

    #[test]
    fn closing_a_session_releases_resources_once() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .upsert_terminal(
                "epoch",
                "session",
                incarnation,
                "terminal",
                json!({ "released": false }),
            )
            .unwrap();
        state
            .start_operation(
                "epoch",
                "session",
                incarnation,
                "close",
                SessionOperationKind::Close,
                "closing",
            )
            .unwrap();
        let before_close = state.seq();
        state
            .close_session("epoch", "session", incarnation, "close")
            .unwrap();

        assert!(state.operation_in_use("close"));
        let close_delta = &state.deltas_after(before_close).unwrap()[0];
        assert!(matches!(
            close_delta.change,
            RuntimeChange::SessionRemoved { .. }
        ));
        assert_eq!(
            state.take_effects(),
            vec![RuntimeEffect::ReleaseTerminal {
                session_id: "session".to_string(),
                terminal_id: "terminal".to_string(),
            }]
        );
        assert!(state.take_effects().is_empty());
        state
            .finish_session_cleanup("session", incarnation, SessionAdmission::Close, "close")
            .unwrap();
        assert!(!state.operation_in_use("close"));
    }

    #[test]
    fn retrying_delete_from_closed_never_resurrects_the_session() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_delete("epoch", "session", incarnation, "first-delete")
            .unwrap();
        state
            .delete_close_succeeded("epoch", "session", incarnation, "first-delete")
            .unwrap();
        state
            .fail_operation(
                "epoch",
                "session",
                incarnation,
                "first-delete",
                SessionOperationKind::Delete,
                json!("delete failed"),
            )
            .unwrap();

        state
            .start_delete("epoch", "session", incarnation, "retry-delete")
            .unwrap();
        let retrying = state.session("session").unwrap();
        assert_eq!(retrying.lifecycle, SessionLifecycle::Deleting);
        assert_eq!(retrying.operation.as_ref().unwrap().stage, "deleting");
        state
            .fail_operation(
                "epoch",
                "session",
                incarnation,
                "retry-delete",
                SessionOperationKind::Delete,
                json!("delete failed"),
            )
            .unwrap();

        assert_eq!(
            state.session("session").unwrap().lifecycle,
            SessionLifecycle::Closed,
        );
    }

    #[test]
    fn delete_close_success_releases_resources_before_delete_response() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .upsert_permission(
                "epoch",
                "session",
                incarnation,
                "permission",
                json!({ "question": "allow" }),
            )
            .unwrap();
        state
            .upsert_terminal(
                "epoch",
                "session",
                incarnation,
                "terminal",
                json!({ "released": false }),
            )
            .unwrap();
        state
            .start_delete("epoch", "session", incarnation, "delete")
            .unwrap();

        state
            .delete_close_succeeded("epoch", "session", incarnation, "delete")
            .unwrap();

        let session = state.session("session").unwrap();
        assert!(session.permissions.is_empty());
        assert!(session.terminals.is_empty());
        assert_eq!(
            state.take_effects(),
            vec![
                RuntimeEffect::CancelPermissionResponder {
                    session_id: "session".to_string(),
                    incarnation: incarnation,
                    interaction_id: "permission".to_string(),
                },
                RuntimeEffect::ReleaseTerminal {
                    session_id: "session".to_string(),
                    terminal_id: "terminal".to_string(),
                },
            ],
        );
    }

    #[test]
    fn delete_success_removes_once_and_old_incarnation_stays_stale_after_reopen() {
        let mut state = state();
        let first = open(&mut state, "session");
        state
            .start_delete("epoch", "session", first, "delete")
            .unwrap();
        state
            .delete_close_succeeded("epoch", "session", first, "delete")
            .unwrap();
        let before = state.seq();

        state
            .complete_delete("epoch", "session", first, "delete")
            .unwrap();

        assert_eq!(state.seq(), before + 1);
        assert!(state.session("session").is_none());
        state
            .finish_session_cleanup("session", first, SessionAdmission::Delete, "delete")
            .unwrap();
        let second = open(&mut state, "session");
        assert_ne!(first, second);
        assert_eq!(
            state.complete_delete("epoch", "session", first, "delete"),
            Err(RuntimeStateError::StaleIncarnation),
        );
    }

    #[test]
    fn uncertain_delete_is_one_terminal_transition_and_blocks_mutation() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_delete("epoch", "session", incarnation, "delete")
            .unwrap();
        let before = state.seq();

        state
            .mark_operation_uncertain(
                "epoch",
                "session",
                incarnation,
                "delete",
                "close response was lost",
            )
            .unwrap();

        assert_eq!(state.seq(), before + 1);
        let session = state.session("session").unwrap();
        assert_eq!(session.lifecycle, SessionLifecycle::Uncertain);
        assert_eq!(session.operation.as_ref().unwrap().stage, "uncertain");
        assert_eq!(
            state.start_prompt("epoch", "session", incarnation, "prompt", Vec::new()),
            Err(RuntimeStateError::SessionNotActive),
        );
    }

    #[test]
    fn independent_session_transitions_commute() {
        fn run(a_first: bool) -> RuntimeSnapshot {
            let mut state = state();
            let a = open(&mut state, "a");
            let b = open(&mut state, "b");
            let update_a = |state: &mut RuntimeState| {
                state
                    .start_prompt("epoch", "a", a, "prompt-a", vec![json!("A")])
                    .unwrap();
            };
            let update_b = |state: &mut RuntimeState| {
                state
                    .start_prompt("epoch", "b", b, "prompt-b", vec![json!("B")])
                    .unwrap();
            };
            if a_first {
                update_a(&mut state);
                update_b(&mut state);
            } else {
                update_b(&mut state);
                update_a(&mut state);
            }
            state.snapshot()
        }

        assert_eq!(run(true).sessions, run(false).sessions);
    }

    #[test]
    fn terminal_result_is_not_retained_after_turn_retirement() {
        let mut state = RuntimeState::new("epoch");
        let incarnation = open(&mut state, "session");
        let operation_id = "prompt".to_string();
        state
            .start_prompt("epoch", "session", incarnation, &operation_id, Vec::new())
            .unwrap();
        let before = state.seq();

        state
            .complete_prompt(
                "epoch",
                "session",
                incarnation,
                &operation_id,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();

        assert!(state.session("session").unwrap().active_turn.is_none());
        assert!(!state.operation_in_use(&operation_id));
        let deltas = state.deltas_after(before).unwrap();
        assert_eq!(deltas.len(), 1);
        assert!(
            !serde_json::to_string(&deltas)
                .unwrap()
                .contains("stopReason")
        );
    }

    #[test]
    fn rejected_transition_is_revision_and_delta_neutral() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "active", Vec::new())
            .unwrap();
        let before = state.snapshot();

        assert_eq!(
            state.start_prompt("epoch", "session", incarnation, "rejected", Vec::new()),
            Err(RuntimeStateError::BusySession),
        );

        assert_eq!(state.snapshot(), before);
        assert!(state.deltas_after(before.through_seq).unwrap().is_empty());
    }

    #[test]
    fn terminal_retirement_invalidates_payload_bearing_delta_prefix() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        let other_incarnation = open(&mut state, "other");
        let before = state.snapshot();
        state
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        state
            .append_runtime_update_for_test("epoch", "session", incarnation, json!("answer"))
            .unwrap();
        state
            .start_prompt(
                "epoch",
                "other",
                other_incarnation,
                "other-prompt",
                Vec::new(),
            )
            .unwrap();
        state
            .append_runtime_update_for_test(
                "epoch",
                "other",
                other_incarnation,
                json!("other answer"),
            )
            .unwrap();
        state
            .complete_prompt(
                "epoch",
                "session",
                incarnation,
                "prompt",
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();

        assert!(state.deltas_after(before.through_seq).is_none());
        let suffix = state.deltas_after(before.through_seq + 2).unwrap();
        assert_eq!(suffix.len(), 3);
        assert!(suffix.windows(2).all(|pair| pair[0].seq + 1 == pair[1].seq));
        let settled = state.snapshot();
        let deltas = state.deltas_after(settled.through_seq - 1).unwrap();
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].epoch, "epoch");
        assert_eq!(deltas[0].seq, settled.through_seq);
        assert_eq!(
            deltas[0].scope_revision,
            Some(settled.sessions["session"].revision)
        );
    }

    #[test]
    fn definite_close_failure_restores_active_and_releases_operation_guard() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_operation(
                "epoch",
                "session",
                incarnation,
                "close",
                SessionOperationKind::Close,
                "closing",
            )
            .unwrap();

        state
            .fail_operation(
                "epoch",
                "session",
                incarnation,
                "close",
                SessionOperationKind::Close,
                json!({ "message": "refused" }),
            )
            .unwrap();

        let session = state.session("session").unwrap();
        assert_eq!(session.lifecycle, SessionLifecycle::Active);
        assert!(session.operation.is_none());
        assert!(!state.operation_in_use("close"));
        state
            .start_prompt("epoch", "session", incarnation, "next", Vec::new())
            .unwrap();
    }

    #[test]
    fn generic_operation_success_and_failure_release_exactly_the_matching_guard() {
        for (kind, succeeds) in [
            (SessionOperationKind::Fork, true),
            (SessionOperationKind::SetMode, false),
            (SessionOperationKind::SetConfig, true),
        ] {
            let mut state = state();
            let incarnation = open(&mut state, "session");
            let operation_id = format!("{kind:?}");
            state
                .start_operation(
                    "epoch",
                    "session",
                    incarnation,
                    &operation_id,
                    kind,
                    "running",
                )
                .unwrap();
            if succeeds {
                state
                    .complete_operation(
                        "epoch",
                        "session",
                        incarnation,
                        &operation_id,
                        kind,
                        json!({ "ok": true }),
                    )
                    .unwrap();
            } else {
                state
                    .fail_operation(
                        "epoch",
                        "session",
                        incarnation,
                        &operation_id,
                        kind,
                        json!({ "message": "failed" }),
                    )
                    .unwrap();
            }
            assert!(state.session("session").unwrap().operation.is_none());
            assert_eq!(
                state.session("session").unwrap().lifecycle,
                SessionLifecycle::Active
            );
            assert_eq!(
                state.complete_operation(
                    "epoch",
                    "session",
                    incarnation,
                    &operation_id,
                    kind,
                    Value::Null,
                ),
                Err(RuntimeStateError::OperationMismatch),
            );
        }
    }

    #[test]
    fn control_success_updates_value_and_retires_result_payload_atomically() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_operation(
                "epoch",
                "session",
                incarnation,
                "mode",
                SessionOperationKind::SetMode,
                "setting",
            )
            .unwrap();
        let before = state.seq();

        state
            .complete_control_operation(
                "epoch",
                "session",
                incarnation,
                "mode",
                SessionOperationKind::SetMode,
                "current_mode_update",
                json!({ "sessionUpdate": "current_mode_update", "currentModeId": "plan" }),
                json!({ "ok": true }),
            )
            .unwrap();

        assert_eq!(state.seq(), before + 1);
        let session = state.session("session").unwrap();
        assert!(session.operation.is_none());
        assert_eq!(
            session.control_state["current_mode_update"]["currentModeId"],
            "plan",
        );
        assert!(!state.operation_in_use("mode"));
        assert!(
            !serde_json::to_string(&state.deltas_after(before).unwrap())
                .unwrap()
                .contains("\"ok\":true")
        );
    }

    #[test]
    fn load_candidate_is_invisible_until_success_then_commits_atomically() {
        let mut state = state();
        let incarnation = state
            .start_attachment(
                "epoch",
                "session",
                "/workspace",
                "load",
                SessionOperationKind::Load,
            )
            .unwrap();
        let started_seq = state.seq();
        state
            .append_attachment_candidate(
                "epoch",
                "session",
                incarnation,
                json!({ "sessionUpdate": "agent_message_chunk", "content": "partial" }),
            )
            .unwrap();
        state
            .append_attachment_candidate(
                "epoch",
                "session",
                incarnation,
                json!({ "sessionUpdate": "current_mode_update", "currentModeId": "plan" }),
            )
            .unwrap();

        assert_eq!(state.seq(), started_seq);
        let attaching = state.session("session").unwrap();
        assert_eq!(attaching.lifecycle, SessionLifecycle::Attaching);
        assert_eq!(state.live("session").unwrap().attachment_candidate.len(), 1);
        assert!(
            !serde_json::to_string(&state.live("session").unwrap().attachment_candidate)
                .unwrap()
                .contains("partial")
        );
        assert!(
            serde_json::to_value(state.snapshot())
                .unwrap()
                .to_string()
                .find("partial")
                .is_none()
        );

        state
            .complete_attachment(
                "epoch",
                "session",
                incarnation,
                "load",
                SessionOperationKind::Load,
                json!({ "sessionId": "session" }),
            )
            .unwrap();

        assert_eq!(state.seq(), started_seq + 1);
        let loaded = state.session("session").unwrap();
        assert_eq!(loaded.lifecycle, SessionLifecycle::Active);
        assert!(loaded.operation.is_none());
        assert_eq!(
            loaded.control_state["current_mode_update"]["currentModeId"],
            "plan"
        );
        assert!(!serde_json::to_string(&loaded).unwrap().contains("partial"));
    }

    #[test]
    fn attachment_control_updates_commit_to_the_control_plane_not_transcript() {
        for kind in [SessionOperationKind::Load, SessionOperationKind::Resume] {
            let mut state = state();
            let session_id = if kind == SessionOperationKind::Load {
                "loaded"
            } else {
                "resumed"
            };
            let incarnation = state
                .start_attachment("epoch", session_id, "/workspace", "attach", kind)
                .unwrap();
            state
                .append_attachment_candidate(
                    "epoch",
                    session_id,
                    incarnation,
                    json!({ "sessionUpdate": "agent_message_chunk", "content": "history" }),
                )
                .unwrap();
            state
                .append_attachment_candidate(
                    "epoch",
                    session_id,
                    incarnation,
                    json!({ "sessionUpdate": "current_mode_update", "currentModeId": "plan" }),
                )
                .unwrap();
            state
                .complete_attachment(
                    "epoch",
                    session_id,
                    incarnation,
                    "attach",
                    kind,
                    json!({ "sessionId": session_id }),
                )
                .unwrap();

            let session = state.session(session_id).unwrap();
            assert_eq!(
                session.control_state["current_mode_update"]["currentModeId"],
                "plan"
            );
            assert!(!serde_json::to_string(&session).unwrap().contains("history"));
        }
    }

    #[test]
    fn failed_load_discards_candidate_and_resume_never_claims_replayed_history() {
        let mut state = state();
        let failed = state
            .start_attachment(
                "epoch",
                "failed",
                "/workspace",
                "load-failed",
                SessionOperationKind::Load,
            )
            .unwrap();
        state
            .append_attachment_candidate(
                "epoch",
                "failed",
                failed,
                json!({ "content": "must disappear" }),
            )
            .unwrap();
        state
            .fail_attachment(
                "epoch",
                "failed",
                failed,
                "load-failed",
                SessionOperationKind::Load,
                json!({ "message": "not found" }),
            )
            .unwrap();
        assert!(state.session("failed").is_none());
        assert!(
            !serde_json::to_string(&state.snapshot())
                .unwrap()
                .contains("must disappear")
        );

        let resumed = state
            .start_attachment(
                "epoch",
                "resumed",
                "/workspace",
                "resume",
                SessionOperationKind::Resume,
            )
            .unwrap();
        state
            .append_attachment_candidate(
                "epoch",
                "resumed",
                resumed,
                json!({ "content": "compatibility update" }),
            )
            .unwrap();
        state
            .complete_attachment(
                "epoch",
                "resumed",
                resumed,
                "resume",
                SessionOperationKind::Resume,
                json!({ "sessionId": "resumed" }),
            )
            .unwrap();
        assert!(
            !serde_json::to_string(&state.session("resumed").unwrap())
                .unwrap()
                .contains("compatibility update")
        );
    }

    #[test]
    fn failed_attachment_releases_every_pre_response_live_resource() {
        let mut state = state();
        let incarnation = state
            .start_attachment(
                "epoch",
                "session",
                "/workspace",
                "load",
                SessionOperationKind::Load,
            )
            .unwrap();
        state
            .upsert_permission(
                "epoch",
                "session",
                incarnation,
                "permission",
                json!({ "question": "allow" }),
            )
            .unwrap();
        state
            .upsert_elicitation(
                "epoch",
                Some(("session", incarnation)),
                "elicitation",
                json!({ "mode": "url" }),
            )
            .unwrap();
        state
            .resolve_elicitation(
                "epoch",
                Some(("session", incarnation)),
                "elicitation",
                Some("url"),
            )
            .unwrap();
        state
            .upsert_terminal(
                "epoch",
                "session",
                incarnation,
                "terminal",
                json!({ "released": false }),
            )
            .unwrap();

        state
            .fail_attachment(
                "epoch",
                "session",
                incarnation,
                "load",
                SessionOperationKind::Load,
                json!({ "message": "failed" }),
            )
            .unwrap();

        assert!(state.session("session").is_none());
        assert_eq!(
            state.take_effects(),
            vec![
                RuntimeEffect::CancelPermissionResponder {
                    session_id: "session".to_string(),
                    incarnation: incarnation,
                    interaction_id: "permission".to_string(),
                },
                RuntimeEffect::AbortUrlFlow {
                    session_id: Some("session".to_string()),
                    incarnation: Some(incarnation),
                    elicitation_id: "url".to_string(),
                    registration_id: "elicitation".to_string(),
                },
                RuntimeEffect::ReleaseTerminal {
                    session_id: "session".to_string(),
                    terminal_id: "terminal".to_string(),
                },
            ]
        );
    }

    #[test]
    fn accepted_url_elicitation_outlives_turn_and_has_a_monotonic_terminal_state() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        state
            .upsert_elicitation(
                "epoch",
                Some(("session", incarnation)),
                "request-id",
                json!({ "mode": "url", "elicitationId": "url-id" }),
            )
            .unwrap();
        state
            .resolve_elicitation(
                "epoch",
                Some(("session", incarnation)),
                "request-id",
                Some("url-id"),
            )
            .unwrap();
        let resolved_seq = state.seq();
        assert_eq!(
            state
                .resolve_elicitation(
                    "epoch",
                    Some(("session", incarnation)),
                    "request-id",
                    Some("url-id"),
                )
                .unwrap(),
            InteractionResolution::AlreadyResolved,
        );
        assert_eq!(state.seq(), resolved_seq);
        state
            .complete_prompt(
                "epoch",
                "session",
                incarnation,
                "prompt",
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();

        assert_eq!(
            state.session("session").unwrap().url_flows["url-id"].status,
            UrlFlowStatus::Waiting,
        );
        assert_eq!(
            state
                .settle_url_flow(
                    "epoch",
                    Some(("session", incarnation)),
                    "url-id",
                    UrlFlowStatus::Completed,
                )
                .unwrap(),
            UrlFlowResolution::Applied,
        );
        let completed_seq = state.seq();
        assert_eq!(
            state
                .settle_url_flow(
                    "epoch",
                    Some(("session", incarnation)),
                    "url-id",
                    UrlFlowStatus::Cancelled,
                )
                .unwrap(),
            UrlFlowResolution::AlreadyTerminal,
        );
        assert_eq!(state.seq(), completed_seq);
        assert!(
            !state
                .session("session")
                .unwrap()
                .url_flows
                .contains_key("url-id")
        );
    }

    #[test]
    fn request_scoped_elicitation_is_connection_state_not_session_state() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .upsert_elicitation("epoch", None, "request-scope", json!({ "mode": "form" }))
            .unwrap();

        assert!(
            state
                .snapshot()
                .request_elicitations
                .contains_key("request-scope")
        );
        state
            .start_operation(
                "epoch",
                "session",
                incarnation,
                "close",
                SessionOperationKind::Close,
                "closing",
            )
            .unwrap();
        state
            .close_session("epoch", "session", incarnation, "close")
            .unwrap();
        assert!(
            state
                .snapshot()
                .request_elicitations
                .contains_key("request-scope")
        );
    }

    #[test]
    fn elicitation_response_is_transactional_for_session_and_request_scopes() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        let session_response = "session-form-response".to_string();
        state
            .upsert_elicitation(
                "epoch",
                Some(("session", incarnation)),
                "session-form",
                json!({ "mode": "form" }),
            )
            .unwrap();
        assert_eq!(
            state
                .begin_elicitation_response(
                    "epoch",
                    Some(("session", incarnation)),
                    "session-form",
                    &session_response,
                )
                .unwrap(),
            InteractionResponseStart::Applied,
        );
        assert_eq!(
            state
                .begin_elicitation_response(
                    "epoch",
                    Some(("session", incarnation)),
                    "session-form",
                    &session_response,
                )
                .unwrap(),
            InteractionResponseStart::Duplicate,
        );
        assert_eq!(
            state.begin_elicitation_response(
                "epoch",
                Some(("session", incarnation)),
                "session-form",
                "response-2",
            ),
            Err(RuntimeStateError::InteractionCollision),
        );
        assert_eq!(
            state.complete_elicitation_response(
                "epoch",
                Some(("session", incarnation)),
                "session-form",
                "stale-response",
                None,
            ),
            Err(RuntimeStateError::OperationMismatch),
        );
        state
            .complete_elicitation_response(
                "epoch",
                Some(("session", incarnation)),
                "session-form",
                &session_response,
                Some("url-flow"),
            )
            .unwrap();
        let session = state.session("session").unwrap();
        assert!(!session.elicitations.contains_key("session-form"));
        assert_eq!(session.url_flows["url-flow"].status, UrlFlowStatus::Waiting);
        assert!(!state.operation_in_use(&session_response));

        state
            .upsert_elicitation("epoch", None, "request-form", json!({ "mode": "form" }))
            .unwrap();
        let request_response = "request-form-response".to_string();
        state
            .begin_elicitation_response("epoch", None, "request-form", &request_response)
            .unwrap();
        assert_eq!(
            state.snapshot().request_elicitations["request-form"]
                .responding_operation_id
                .as_deref(),
            Some(request_response.as_str()),
        );
        state
            .complete_elicitation_response("epoch", None, "request-form", &request_response, None)
            .unwrap();
        assert!(
            !state
                .snapshot()
                .request_elicitations
                .contains_key("request-form")
        );
        assert!(!state.operation_in_use(&request_response));
    }

    #[test]
    fn same_session_reload_success_does_not_create_a_second_business_session() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .update_control_state(
                "epoch",
                "session",
                incarnation,
                "current_mode_update",
                json!({ "sessionUpdate": "current_mode_update", "currentModeId": "old" }),
            )
            .unwrap();
        state
            .update_control_state(
                "epoch",
                "session",
                incarnation,
                "config_option_update",
                json!({ "sessionUpdate": "config_option_update", "configId": "old-only" }),
            )
            .unwrap();
        let operation_id = "reload-intent".to_string();

        state
            .start_reload(
                "epoch",
                "session",
                incarnation,
                &operation_id,
                SessionOperationKind::Load,
            )
            .unwrap();
        state
            .append_attachment_candidate(
                "epoch",
                "session",
                incarnation,
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": "replacement-history-must-not-be-retained"
                }),
            )
            .unwrap();
        state
            .append_attachment_candidate(
                "epoch",
                "session",
                incarnation,
                json!({ "sessionUpdate": "current_mode_update", "currentModeId": "new" }),
            )
            .unwrap();

        let staging = state.session("session").unwrap();
        assert_eq!(staging.incarnation, incarnation);
        assert_eq!(staging.session, json!({ "sessionId": "session" }));
        assert_eq!(
            staging.control_state["current_mode_update"]["currentModeId"],
            "old"
        );
        assert_eq!(state.live("session").unwrap().attachment_candidate.len(), 1);
        let before_complete = state.seq();

        state
            .complete_reload(
                "epoch",
                "session",
                incarnation,
                &operation_id,
                json!({
                    "modes": {
                        "currentModeId": "new",
                        "availableModes": [{ "id": "new", "name": "New" }]
                    },
                    "_meta": { "debugPayload": "must-not-survive-reload" }
                }),
            )
            .unwrap();

        let reloaded = state.session("session").unwrap();
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(reloaded.incarnation, incarnation);
        assert_eq!(reloaded.session["modes"]["currentModeId"], "new");
        assert!(
            !serde_json::to_string(&reloaded)
                .unwrap()
                .contains("must-not-survive-reload")
        );
        assert_eq!(
            reloaded.control_state["current_mode_update"]["currentModeId"],
            "new"
        );
        assert!(!reloaded.control_state.contains_key("config_option_update"));
        assert!(reloaded.operation.is_none());
        assert!(
            state
                .live("session")
                .unwrap()
                .attachment_candidate
                .is_empty()
        );
        assert_eq!(state.live("session").unwrap().attachment_candidate_bytes, 0);
        assert!(!state.operation_in_use(&operation_id));
        assert_eq!(state.seq(), before_complete + 1);
        assert!(
            !serde_json::to_string(&state.snapshot())
                .unwrap()
                .contains("replacement-history-must-not-be-retained")
        );
    }

    #[test]
    fn sparse_session_controls_preserve_omitted_fields_in_live_and_replay_snapshots() {
        let updates = vec![
            json!({ "sessionUpdate": "session_info_update", "title": "Keep", "updatedAt": "before" }),
            json!({ "sessionUpdate": "usage_update", "used": 1, "size": 100, "cost": { "amount": 2, "currency": "USD" } }),
            json!({ "sessionUpdate": "session_info_update", "updatedAt": "after" }),
            json!({ "sessionUpdate": "usage_update", "used": 3, "size": 100, "cost": null }),
        ];
        let mut state = state();
        let incarnation = open(&mut state, "session");
        for update in &updates {
            state
                .update_control_state(
                    "epoch",
                    "session",
                    incarnation,
                    update["sessionUpdate"].as_str().unwrap(),
                    update.clone(),
                )
                .unwrap();
        }
        let controls = &state.session("session").unwrap().control_state;
        assert_eq!(controls["session_info_update"]["title"], "Keep");
        assert_eq!(controls["session_info_update"]["updatedAt"], "after");
        assert_eq!(controls["usage_update"]["cost"]["amount"], 2);
        assert_eq!(&extract_control_state(updates), controls);
        state
            .update_control_state(
                "epoch",
                "session",
                incarnation,
                "session_info_update",
                json!({ "sessionUpdate": "session_info_update", "title": null }),
            )
            .unwrap();
        let controls = &state.session("session").unwrap().control_state;
        assert_eq!(controls["session_info_update"]["title"], Value::Null);
        assert_eq!(controls["session_info_update"]["updatedAt"], "after");
    }

    #[test]
    fn same_session_reload_error_remains_actionable_and_does_not_loop() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .update_control_state(
                "epoch",
                "session",
                incarnation,
                "current_mode_update",
                json!({ "sessionUpdate": "current_mode_update", "currentModeId": "old" }),
            )
            .unwrap();
        let old_session = state.session("session").unwrap().session.clone();
        let old_controls = state.session("session").unwrap().control_state.clone();

        state
            .start_reload(
                "epoch",
                "session",
                incarnation,
                "reload",
                SessionOperationKind::Load,
            )
            .unwrap();
        state
            .append_attachment_candidate(
                "epoch",
                "session",
                incarnation,
                json!({ "sessionUpdate": "current_mode_update", "currentModeId": "partial" }),
            )
            .unwrap();
        state
            .fail_reload(
                "epoch",
                "session",
                incarnation,
                "reload",
                json!({ "message": "load rejected" }),
            )
            .unwrap();

        let session = state.session("session").unwrap();
        assert_eq!(session.incarnation, incarnation);
        assert_eq!(session.lifecycle, SessionLifecycle::Active);
        assert_eq!(session.session, old_session);
        assert_eq!(session.control_state, old_controls);
        assert!(session.operation.is_none());
        assert!(
            state
                .live("session")
                .unwrap()
                .attachment_candidate
                .is_empty()
        );
        assert_eq!(state.live("session").unwrap().attachment_candidate_bytes, 0);
        assert!(!state.operation_in_use("reload"));
        assert!(
            !serde_json::to_string(&state.journal.deltas)
                .unwrap()
                .contains("load rejected")
        );
        state
            .start_prompt("epoch", "session", incarnation, "next-turn", Vec::new())
            .unwrap();
    }

    #[test]
    fn same_session_reload_rejects_wrong_kind_busy_collision_and_epoch() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        let other = open(&mut state, "other");
        let unchanged = state.snapshot();

        assert_eq!(
            state.start_reload(
                "old-epoch",
                "session",
                incarnation,
                "reload",
                SessionOperationKind::Load,
            ),
            Err(RuntimeStateError::EpochMismatch)
        );
        assert_eq!(state.snapshot(), unchanged);
        assert_eq!(
            state.start_reload(
                "epoch",
                "session",
                incarnation,
                "reload",
                SessionOperationKind::Resume,
            ),
            Err(RuntimeStateError::OperationMismatch)
        );

        state
            .start_prompt("epoch", "other", other, "collision", Vec::new())
            .unwrap();
        assert_eq!(
            state.start_reload(
                "epoch",
                "session",
                incarnation,
                "collision",
                SessionOperationKind::Load,
            ),
            Err(RuntimeStateError::OperationCollision)
        );
        state
            .start_prompt("epoch", "session", incarnation, "active", Vec::new())
            .unwrap();
        assert_eq!(
            state.start_reload(
                "epoch",
                "session",
                incarnation,
                "busy",
                SessionOperationKind::Load,
            ),
            Err(RuntimeStateError::BusySession)
        );
    }

    #[test]
    fn same_session_reload_requires_no_operation_interaction_or_live_resource() {
        fn assert_reload_busy(state: &mut RuntimeState, session_id: &str, incarnation: u64) {
            assert_eq!(
                state.start_reload(
                    "epoch",
                    session_id,
                    incarnation,
                    format!("reload-{session_id}"),
                    SessionOperationKind::Load,
                ),
                Err(RuntimeStateError::BusySession)
            );
        }

        let mut state = state();

        let operation = open(&mut state, "operation");
        state
            .start_operation(
                "epoch",
                "operation",
                operation,
                "mode",
                SessionOperationKind::SetMode,
                "setting",
            )
            .unwrap();
        assert_reload_busy(&mut state, "operation", operation);

        let permission = open(&mut state, "permission");
        state
            .upsert_permission(
                "epoch",
                "permission",
                permission,
                "permission",
                json!({ "question": "allow" }),
            )
            .unwrap();
        assert_reload_busy(&mut state, "permission", permission);

        let elicitation = open(&mut state, "elicitation");
        state
            .upsert_elicitation(
                "epoch",
                Some(("elicitation", elicitation)),
                "elicitation",
                json!({ "mode": "form" }),
            )
            .unwrap();
        assert_reload_busy(&mut state, "elicitation", elicitation);

        let url = open(&mut state, "url");
        state
            .upsert_elicitation(
                "epoch",
                Some(("url", url)),
                "url-form",
                json!({ "mode": "url" }),
            )
            .unwrap();
        state
            .resolve_elicitation("epoch", Some(("url", url)), "url-form", Some("url-flow"))
            .unwrap();
        assert_reload_busy(&mut state, "url", url);

        let terminal = open(&mut state, "terminal");
        state
            .upsert_terminal(
                "epoch",
                "terminal",
                terminal,
                "terminal",
                json!({ "released": false }),
            )
            .unwrap();
        assert_reload_busy(&mut state, "terminal", terminal);
    }

    #[test]
    fn completed_url_identifiers_can_be_reused_in_session_and_request_scopes() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        for scope in [Some(("session", incarnation)), None] {
            for index in 0..2 {
                let interaction = format!("{scope:?}-{index}");
                state
                    .upsert_elicitation("epoch", scope, &interaction, json!({"mode": "url"}))
                    .unwrap();
                state
                    .resolve_elicitation("epoch", scope, &interaction, Some("reusable"))
                    .unwrap();
                assert_eq!(
                    state
                        .settle_url_flow("epoch", scope, "reusable", UrlFlowStatus::Completed)
                        .unwrap(),
                    UrlFlowResolution::Applied
                );
            }
        }
    }

    #[test]
    fn url_registration_fences_reuse_without_exposing_internal_identity() {
        for request_scoped in [false, true] {
            let mut state = state();
            let incarnation = open(&mut state, "session");
            let scope = (!request_scoped).then_some(("session", incarnation));
            for interaction in ["old-registration", "new-registration"] {
                state
                    .upsert_elicitation("epoch", scope, interaction, json!({ "mode": "url" }))
                    .unwrap();
                state
                    .resolve_elicitation("epoch", scope, interaction, Some("reusable-url"))
                    .unwrap();
                if interaction == "old-registration" {
                    assert_eq!(
                        state
                            .settle_url_registration(
                                "epoch",
                                scope,
                                "reusable-url",
                                interaction,
                                UrlFlowStatus::Completed
                            )
                            .unwrap(),
                        UrlFlowResolution::Applied
                    );
                }
            }
            let seq = state.seq();
            assert_eq!(
                state
                    .settle_url_registration(
                        "epoch",
                        scope,
                        "reusable-url",
                        "old-registration",
                        UrlFlowStatus::Cancelled
                    )
                    .unwrap(),
                UrlFlowResolution::StaleRegistration
            );
            assert_eq!(state.seq(), seq);
            let flow = if request_scoped {
                state.request_url_flows["reusable-url"].clone()
            } else {
                state.session("session").unwrap().url_flows["reusable-url"].clone()
            };
            assert_eq!(flow.registration_id, "new-registration");
            assert_eq!(
                serde_json::to_value(flow).unwrap(),
                json!({
                    "elicitationId": "reusable-url", "request": { "mode": "url" }, "status": "waiting"
                })
            );
            assert_eq!(
                state
                    .settle_url_registration(
                        "epoch",
                        scope,
                        "reusable-url",
                        "new-registration",
                        UrlFlowStatus::Cancelled
                    )
                    .unwrap(),
                UrlFlowResolution::Applied
            );
        }
    }

    #[test]
    fn settled_url_flows_are_removed_from_live_state_but_remain_idempotent() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .upsert_elicitation(
                "epoch",
                Some(("session", incarnation)),
                "form",
                json!({ "mode": "url" }),
            )
            .unwrap();
        state
            .resolve_elicitation("epoch", Some(("session", incarnation)), "form", Some("url"))
            .unwrap();

        assert_eq!(
            state
                .settle_url_flow(
                    "epoch",
                    Some(("session", incarnation)),
                    "url",
                    UrlFlowStatus::Completed,
                )
                .unwrap(),
            UrlFlowResolution::Applied
        );
        assert!(
            !state
                .session("session")
                .unwrap()
                .url_flows
                .contains_key("url")
        );
        let settled_seq = state.seq();
        assert_eq!(
            state
                .settle_url_flow(
                    "epoch",
                    Some(("session", incarnation)),
                    "url",
                    UrlFlowStatus::Cancelled,
                )
                .unwrap(),
            UrlFlowResolution::AlreadyTerminal
        );
        assert_eq!(state.seq(), settled_seq);

        state
            .upsert_elicitation("epoch", None, "request-form", json!({ "mode": "url" }))
            .unwrap();
        state
            .resolve_elicitation("epoch", None, "request-form", Some("request-url"))
            .unwrap();
        assert_eq!(
            state
                .settle_url_flow("epoch", None, "request-url", UrlFlowStatus::Cancelled)
                .unwrap(),
            UrlFlowResolution::Applied
        );
        assert!(!state.request_url_flows.contains_key("request-url"));
        let request_settled_seq = state.seq();
        assert_eq!(
            state
                .settle_url_flow("epoch", None, "request-url", UrlFlowStatus::Completed)
                .unwrap(),
            UrlFlowResolution::AlreadyTerminal
        );
        assert_eq!(state.seq(), request_settled_seq);
    }

    #[test]
    fn released_terminal_is_not_retained_or_resurrected() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .upsert_terminal(
                "epoch",
                "session",
                incarnation,
                "terminal",
                json!({ "output": "running", "released": false }),
            )
            .unwrap();
        state
            .upsert_terminal(
                "epoch",
                "session",
                incarnation,
                "terminal",
                json!({ "output": "released-output", "released": true }),
            )
            .unwrap();

        assert!(
            !state
                .session("session")
                .unwrap()
                .terminals
                .contains_key("terminal")
        );
        assert!(
            !serde_json::to_string(&state.snapshot())
                .unwrap()
                .contains("released-output")
        );
        let released_seq = state.seq();
        assert_eq!(
            state
                .upsert_terminal(
                    "epoch",
                    "session",
                    incarnation,
                    "terminal",
                    json!({ "output": "late", "released": false }),
                )
                .unwrap(),
            TerminalUpsert::Stale
        );
        assert_eq!(state.seq(), released_seq);
    }

    #[test]
    fn terminal_deltas_are_linear_and_reassemble_split_utf8_before_exit() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        let start = state.seq();
        let bytes = "中😀tail".as_bytes();
        for (index, byte) in bytes.iter().enumerate() {
            state
                .upsert_terminal(
                    "epoch",
                    "session",
                    incarnation,
                    "terminal",
                    json!({
                        "sessionId": "session", "terminalId": "terminal",
                        "output": String::from_utf8_lossy(&[*byte]),
                        "outputBytes": BASE64_STANDARD.encode([*byte]),
                        "outputAppend": true, "retainedBytes": index + 1,
                        "released": false,
                    }),
                )
                .unwrap();
            let live = state.live("session").unwrap();
            let output = live.terminals["terminal"]["output"].as_str().unwrap();
            assert!(
                !output.contains('\u{fffd}'),
                "incomplete UTF-8 must wait for its next byte"
            );
        }
        let terminal = &state.session("session").unwrap().terminals["terminal"];
        assert_eq!(terminal["output"], "中😀tail");
        assert_eq!(
            BASE64_STANDARD
                .decode(terminal["outputBytes"].as_str().unwrap())
                .unwrap(),
            bytes
        );
        let deltas = state.deltas_after(start).unwrap();
        assert_eq!(deltas.len(), bytes.len());
        assert_eq!(
            deltas
                .iter()
                .map(|delta| {
                    let RuntimeChange::TerminalUpdated { terminal, .. } = &delta.change else {
                        panic!("output must not republish a whole runtime session");
                    };
                    BASE64_STANDARD
                        .decode(terminal["outputBytes"].as_str().unwrap())
                        .unwrap()
                        .len()
                })
                .sum::<usize>(),
            bytes.len()
        );

        state
            .upsert_terminal(
                "epoch",
                "session",
                incarnation,
                "terminal",
                json!({
                    "output": "!", "outputBytes": BASE64_STANDARD.encode(b"!"),
                    "outputAppend": true, "retainedBytes": 8, "released": false,
                }),
            )
            .unwrap();
        assert_eq!(
            state.session("session").unwrap().terminals["terminal"]["output"],
            "tail!"
        );
    }

    #[test]
    fn authoritative_load_replaces_controls_without_retiring_live_resources() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .upsert_terminal(
                "epoch",
                "session",
                incarnation,
                "terminal",
                json!({ "output": "running", "released": false }),
            )
            .unwrap();
        state
            .synchronize_loaded_session(
                "epoch",
                "session",
                incarnation,
                json!({ "modes": { "currentModeId": "plan" } }),
                vec![json!({
                    "sessionUpdate": "available_commands_update",
                    "availableCommands": [{ "name": "inspect" }],
                })],
            )
            .unwrap();

        let session = state.session("session").unwrap();
        assert!(
            session
                .control_state
                .contains_key("available_commands_update")
        );
        assert!(session.terminals.contains_key("terminal"));
    }
}
