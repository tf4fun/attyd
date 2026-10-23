use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::protocol::SessionControls;
use crate::protocol::documents::OperationKind;
use crate::protocol::op::OpSlot;
use crate::protocol::registry::TurnEnd;
use crate::session_registry::{SessionEntry, SessionRegistry};
use crate::session_state::{MirrorError, SessionAdmission, SessionState};

#[cfg(test)]
pub(crate) use crate::session_registry::SessionRegistry as RuntimeState;

/// A test's read of one session's live state.
#[cfg(test)]
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionRuntime {
    pub session_id: String,
    pub cwd: String,
    pub controls: SessionControls,
    pub lifecycle: SessionLifecycle,
    pub active_turn: Option<ActiveTurn>,
    pub operation: Option<SessionOperationState>,
    pub permissions: BTreeMap<String, PendingInteraction>,
    pub elicitations: BTreeMap<String, PendingInteraction>,
    pub url_flows: BTreeMap<String, UrlFlow>,
    pub terminals: BTreeMap<String, Value>,
}

/// Mutable live resources and lifecycle. Operation and turn execution belong to SessionState.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SessionLiveState {
    pub cwd: PathBuf,
    pub lifecycle: SessionLifecycle,
    pub permissions: BTreeMap<String, PendingInteraction>,
    pub elicitations: BTreeMap<String, PendingInteraction>,
    pub url_flows: BTreeMap<String, UrlFlow>,
    pub terminals: BTreeMap<String, Value>,
    /// Released terminals whose output a timeline entry still embeds: no
    /// handle, only the frozen snapshot (memory model §8.3). Kept apart from
    /// `terminals`, whose entries have a handle and make the session busy.
    pub frozen_terminals: BTreeMap<String, Value>,
    resolved_permissions: VecDeque<String>,
    resolved_elicitations: VecDeque<String>,
    resolved_url_flows: VecDeque<String>,
    released_terminals: VecDeque<String>,
    attachment_candidate: Vec<Value>,
    /// The protocol v2 operation this session's `live.op` shows.
    pub(crate) op: OpSlot,
    /// The reduced controls (protocol §4.5).
    pub(crate) controls: SessionControls,
    /// The `create` or `fork` that installed the session (protocol §4.3).
    pub(crate) origin: Option<String>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InteractionResolution {
    Applied,
    AlreadyResolved,
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
    CancelPermissionResponder {
        session_id: String,
        interaction_id: String,
    },
    CancelElicitationResponder {
        session_id: Option<String>,
        interaction_id: String,
    },
    AbortUrlFlow {
        session_id: Option<String>,
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
    BusySession,
    NoActiveTurn,
    OperationMismatch,
    UnknownInteraction,
    SessionNotActive,
    OperationCollision,
    InteractionCollision,
}

impl SessionRegistry {
    pub(crate) fn epoch(&self) -> &str {
        &self.epoch
    }

    /// A test's read of one session's live state.
    #[cfg(test)]
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
            cwd: live.cwd.to_string_lossy().into_owned(),
            controls: live.controls.clone(),
            lifecycle: live.lifecycle.clone(),
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

    /// Protocol v2: a session the previous Agent connection held, which a
    /// subscription still needs, starts this connection held, with the log it
    /// had and its waiting `load` (protocol S6, §5.4).
    pub(crate) fn seed_waiting(
        &mut self,
        waiting: crate::protocol::registry::WaitingSession,
    ) -> Result<(), RuntimeStateError> {
        let epoch = self.epoch.clone();
        self.open_session(
            &epoch,
            waiting.session_id.clone(),
            waiting.cwd,
            waiting.controls,
        )?;
        self.register_new(waiting.session_id.clone());
        let entry = self
            .sessions
            .get_mut(&waiting.session_id)
            .expect("the session was opened");
        entry.state.log = waiting.log;
        entry.state.timeline = waiting.timeline;
        entry.state.history = waiting.history;
        entry.state.timeline_epoch = waiting.timeline_epoch;
        entry
            .state
            .live
            .as_mut()
            .expect("an open session is live")
            .op = waiting.op;
        Ok(())
    }

    pub(crate) fn open_new_with_replay(
        &mut self,
        expected_epoch: &str,
        session_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
        session: Value,
        replay: Vec<Value>,
    ) -> Result<(), RuntimeStateError> {
        let controls = settle_controls(replay, &session);
        self.open_session(expected_epoch, session_id.into(), cwd.into(), controls)
    }

    pub(crate) fn start_attachment(
        &mut self,
        expected_epoch: &str,
        session_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
        operation_id: impl Into<String>,
        kind: SessionOperationKind,
    ) -> Result<(), RuntimeStateError> {
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
        let cwd: PathBuf = cwd.into();
        let op = OpSlot::running(
            OperationKind::Load,
            serde_json::json!({ "cwd": cwd.to_string_lossy() }),
        );
        self.install_live(
            &session_id,
            SessionLiveState {
                cwd,
                lifecycle: SessionLifecycle::Attaching,
                permissions: BTreeMap::new(),
                elicitations: BTreeMap::new(),
                url_flows: BTreeMap::new(),
                terminals: BTreeMap::new(),
                frozen_terminals: BTreeMap::new(),
                resolved_permissions: VecDeque::new(),
                resolved_elicitations: VecDeque::new(),
                resolved_url_flows: VecDeque::new(),
                released_terminals: VecDeque::new(),
                attachment_candidate: Vec::new(),
                op,
                controls: SessionControls::default(),
                origin: None,
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

        Ok(())
    }

    pub(crate) fn start_reload(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        operation_id: impl Into<String>,
        kind: SessionOperationKind,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        if !matches!(
            kind,
            SessionOperationKind::Load | SessionOperationKind::Resume
        ) {
            return Err(RuntimeStateError::OperationMismatch);
        }
        let operation_id = operation_id.into();
        if self.operation_in_use_except_reserved(&operation_id, Some((session_id, kind))) {
            return Err(RuntimeStateError::OperationCollision);
        }
        let owner = self.live_owner_mut(session_id)?;
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
        self.begin_session_operation(session_id, kind);

        Ok(())
    }

    pub(crate) fn append_attachment_candidate(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        update: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.live_owner_mut(session_id)?;
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
                operation.stage == "reloading"
                    && matches!(
                        operation.kind,
                        SessionOperationKind::Load | SessionOperationKind::Resume
                    )
            });
        if !collecting_attachment && !collecting_reload {
            return Err(RuntimeStateError::OperationMismatch);
        }
        let Some(update) = retain_control_update(update) else {
            return Ok(());
        };
        session.attachment_candidate.push(update);
        Ok(())
    }

    pub(crate) fn complete_reload(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        operation_id: &str,
        response: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.live_owner_mut(session_id)?;
        let session = owner.live.as_mut().expect("live owner checked");
        if session.lifecycle != SessionLifecycle::Active
            || owner.operation.as_ref().is_none_or(|operation| {
                operation.operation_id != operation_id
                    || !matches!(
                        operation.kind,
                        SessionOperationKind::Load | SessionOperationKind::Resume
                    )
                    || operation.stage != "reloading"
            })
        {
            return Err(RuntimeStateError::OperationMismatch);
        }
        let candidate = std::mem::take(&mut session.attachment_candidate);
        session.controls = settle_controls(candidate, &response);
        owner.operation = None;
        self.complete_session_operation(session_id, SessionOperationKind::Load, &Value::Null);

        Ok(())
    }

    pub(crate) fn fail_reload(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        operation_id: &str,
        error: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.live_owner_mut(session_id)?;
        let session = owner.live.as_mut().expect("live owner checked");
        if session.lifecycle != SessionLifecycle::Active
            || owner.operation.as_ref().is_none_or(|operation| {
                operation.operation_id != operation_id
                    || !matches!(
                        operation.kind,
                        SessionOperationKind::Load | SessionOperationKind::Resume
                    )
                    || operation.stage != "reloading"
            })
        {
            return Err(RuntimeStateError::OperationMismatch);
        }
        session.attachment_candidate.clear();
        owner.operation = None;
        self.fail_session_operation(session_id, SessionOperationKind::Load, &error);

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn complete_attachment(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        operation_id: &str,
        kind: SessionOperationKind,
        response: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.live_owner_mut(session_id)?;
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
        session.controls = settle_controls(candidate, &response);
        session.lifecycle = SessionLifecycle::Active;
        owner.operation = None;
        self.complete_session_operation(session_id, kind, &Value::Null);

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn fail_attachment(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        operation_id: &str,
        kind: SessionOperationKind,
        error: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        {
            let owner = self.live_owner(session_id)?;
            let session = owner.live.as_ref().expect("live owner checked");
            if session.lifecycle != SessionLifecycle::Attaching
                || owner.operation.as_ref().is_none_or(|operation| {
                    operation.operation_id != operation_id || operation.kind != kind
                })
            {
                return Err(RuntimeStateError::OperationMismatch);
            }
        }
        self.live_owner_mut(session_id)?.operation = None;
        let mut session = self
            .take_live_for_retirement(session_id)
            .ok_or(RuntimeStateError::UnknownSession)?;
        let effects = drain_session_liveness(session_id, &mut session);

        self.effects.extend(effects);
        self.fail_session_load(session_id, &error);

        Ok(())
    }

    pub(crate) fn open_forked(
        &mut self,
        expected_epoch: &str,
        session_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
        session: Value,
        source_session_id: &str,
        target_replay: Option<Vec<Value>>,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        {
            let source = self
                .live_owner(source_session_id)?
                .live
                .as_ref()
                .expect("live owner checked");
            if source.lifecycle != SessionLifecycle::Active {
                return Err(RuntimeStateError::SessionNotActive);
            }
        }
        let controls = settle_controls(target_replay.unwrap_or_default(), &session);
        self.open_session(expected_epoch, session_id.into(), cwd.into(), controls)
    }

    fn open_session(
        &mut self,
        expected_epoch: &str,
        session_id: String,
        cwd: PathBuf,
        controls: SessionControls,
    ) -> Result<(), RuntimeStateError> {
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
        self.install_live(
            &session_id,
            SessionLiveState {
                cwd,
                lifecycle: SessionLifecycle::Active,
                permissions: BTreeMap::new(),
                elicitations: BTreeMap::new(),
                url_flows: BTreeMap::new(),
                terminals: BTreeMap::new(),
                frozen_terminals: BTreeMap::new(),
                resolved_permissions: VecDeque::new(),
                resolved_elicitations: VecDeque::new(),
                resolved_url_flows: VecDeque::new(),
                released_terminals: VecDeque::new(),
                attachment_candidate: Vec::new(),
                op: OpSlot::completed(OperationKind::Open, serde_json::json!({})),
                controls,
                origin: None,
            },
        );
        Ok(())
    }

    /// Atomically admit a canonical turn and publish its live projection before Agent dispatch.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn admit_session_turn(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        client_intent_id: &str,
        prompt: Vec<Value>,
    ) -> Result<String, SessionTurnError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.live_owner(session_id)?;
        owner.can_begin(SessionAdmission::Prompt)?;
        if owner.live.as_ref().expect("live owner checked").lifecycle != SessionLifecycle::Active {
            return Err(RuntimeStateError::SessionNotActive.into());
        }
        if owner.turn_execution().is_some() || owner.operation.is_some() {
            return Err(RuntimeStateError::BusySession.into());
        }
        if self.operation_in_use(client_intent_id) {
            return Err(RuntimeStateError::OperationCollision.into());
        }
        let operation_id = self.start_turn(session_id, client_intent_id, prompt)?;
        Ok(operation_id)
    }

    /// A runtime reducer fixture: installs a turn in the same canonical overlay,
    /// without inventing a second runtime tail.
    #[cfg(test)]
    pub(crate) fn start_prompt(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        operation_id: impl Into<String>,
        prompt: Vec<Value>,
    ) -> Result<String, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let operation_id = operation_id.into();
        if self.operation_in_use(&operation_id) {
            return Err(RuntimeStateError::OperationCollision);
        }
        let owner = self.live_owner_mut(session_id)?;
        if owner.live.as_ref().expect("live owner checked").lifecycle != SessionLifecycle::Active {
            return Err(RuntimeStateError::SessionNotActive);
        }
        if owner.turn_execution().is_some() || owner.operation.is_some() {
            return Err(RuntimeStateError::BusySession);
        }
        owner.active_turn = None;
        owner.phase = crate::session_state::MirrorPhase::Ready;
        owner
            .admit_turn(operation_id.clone(), &operation_id, prompt)
            .map_err(|_| RuntimeStateError::BusySession)?;
        Ok(operation_id)
    }

    #[cfg(test)]
    pub(crate) fn append_runtime_update_for_test(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        update: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.live_owner_mut(session_id)?;
        let turn = owner
            .active_turn
            .as_mut()
            .ok_or(RuntimeStateError::NoActiveTurn)?;
        turn.execution
            .as_ref()
            .ok_or(RuntimeStateError::NoActiveTurn)?;
        turn.updates = Arc::new(fold_shared_active_turn_update(&turn.updates, &update)?);
        Ok(())
    }

    /// A later control update (protocol §4.5).
    pub(crate) fn update_control_state(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        update: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let session = self
            .live_owner_mut(session_id)?
            .live
            .as_mut()
            .expect("live owner checked");
        if !matches!(
            session.lifecycle,
            SessionLifecycle::Active
                | SessionLifecycle::Closing
                | SessionLifecycle::ClosingForDelete
        ) {
            return Err(RuntimeStateError::SessionNotActive);
        }
        session.controls.fold_update(&update);
        Ok(())
    }

    pub(crate) fn synchronize_loaded_session(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        response: Value,
        replay: Vec<Value>,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let controls = settle_controls(replay, &response);
        let owner = self.live_owner_mut(session_id)?;
        let session = owner.live.as_mut().expect("live owner checked");
        if session.lifecycle != SessionLifecycle::Active
            || owner
                .active_turn
                .as_ref()
                .is_some_and(|turn| turn.execution.is_some())
        {
            return Err(RuntimeStateError::BusySession);
        }
        session.controls = controls;
        Ok(())
    }

    pub(crate) fn request_cancel(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.live_owner_mut(session_id)?;
        let execution = owner
            .active_turn
            .as_mut()
            .and_then(|turn| turn.execution.as_mut())
            .ok_or(RuntimeStateError::NoActiveTurn)?;
        if execution.cancel_requested {
            return Ok(());
        }
        execution.cancel_requested = true;
        self.request_session_cancel(session_id);
        Ok(())
    }

    pub(crate) fn complete_prompt(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        operation_id: &str,
        response: Value,
    ) -> Result<(), RuntimeStateError> {
        self.seal_turn(
            expected_epoch,
            session_id,
            operation_id,
            TurnTerminal { lifecycle: None },
            TurnEnd::Completed(&response),
        )
    }

    pub(crate) fn fail_prompt(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        operation_id: &str,
        error: Value,
    ) -> Result<(), RuntimeStateError> {
        self.seal_turn(
            expected_epoch,
            session_id,
            operation_id,
            TurnTerminal { lifecycle: None },
            TurnEnd::Failed(&error),
        )
    }

    pub(crate) fn mark_prompt_uncertain(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        operation_id: &str,
        reason: impl Into<String>,
    ) -> Result<(), RuntimeStateError> {
        let reason = reason.into();
        self.seal_turn(
            expected_epoch,
            session_id,
            operation_id,
            TurnTerminal {
                lifecycle: Some(SessionLifecycle::Uncertain),
            },
            TurnEnd::Uncertain(&reason),
        )
    }

    fn seal_turn(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        operation_id: &str,
        terminal: TurnTerminal,
        end: TurnEnd<'_>,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.live_owner_mut(session_id)?;
        session_live::seal_turn(owner, operation_id, terminal)?;
        self.end_session_turn(session_id, end);
        Ok(())
    }

    pub(crate) fn upsert_permission(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        interaction_id: impl Into<String>,
        request: Value,
    ) -> Result<InteractionUpsert, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.live_owner_mut(session_id)?;
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
        }
        Ok(outcome)
    }

    pub(crate) fn resolve_permission(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        interaction_id: &str,
    ) -> Result<InteractionResolution, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let session = self.require_live_mut(session_id)?;
        let outcome = session_live::resolve_permission(session, interaction_id)?;
        if outcome == InteractionResolution::Applied {
        }
        Ok(outcome)
    }

    pub(crate) fn upsert_elicitation(
        &mut self,
        expected_epoch: &str,
        scope: Option<&str>,
        interaction_id: impl Into<String>,
        request: Value,
    ) -> Result<InteractionUpsert, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let interaction_id = interaction_id.into();
        match scope {
            Some(session_id) => {
                let owner = self.live_owner_mut(session_id)?;
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
                    },
                );
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
                    },
                );
            }
        }
        Ok(InteractionUpsert::Inserted)
    }

    pub(crate) fn resolve_elicitation(
        &mut self,
        expected_epoch: &str,
        scope: Option<&str>,
        interaction_id: &str,
        accepted_url_id: Option<&str>,
    ) -> Result<InteractionResolution, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        match scope {
            Some(session_id) => {
                if self
                    .require_live(session_id)?
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
                let session = self.require_live_mut(session_id)?;
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
            }
        }
        Ok(InteractionResolution::Applied)
    }

    pub(crate) fn settle_url_registration(
        &mut self,
        expected_epoch: &str,
        scope: Option<&str>,
        elicitation_id: &str,
        registration_id: &str,
        status: UrlFlowStatus,
    ) -> Result<UrlFlowResolution, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let flow = match scope {
            Some(session_id) => self.require_live(session_id)?.url_flows.get(elicitation_id),
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
        scope: Option<&str>,
        elicitation_id: &str,
        status: UrlFlowStatus,
    ) -> Result<UrlFlowResolution, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        if status == UrlFlowStatus::Waiting {
            return Err(RuntimeStateError::OperationMismatch);
        }
        match scope {
            Some(session_id) => {
                let session = self.require_live_mut(session_id)?;
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
            }
        }
        Ok(UrlFlowResolution::Applied)
    }

    pub(crate) fn upsert_terminal(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        terminal_id: impl Into<String>,
        terminal: Value,
    ) -> Result<TerminalUpsert, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let terminal_id: String = terminal_id.into();
        // Whether an entry embeds the terminal decides what its release keeps.
        let embedded = terminal_released(&terminal)
            && self
                .sessions
                .get(session_id)
                .and_then(|entry| entry.state.timeline.as_ref())
                .is_some_and(|timeline| timeline.embeds_terminal(&terminal_id));
        let session = self.require_live_mut(session_id)?;
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
            // The handle is gone; the output stays while content embeds it.
            if embedded {
                session
                    .frozen_terminals
                    .insert(terminal_id.clone(), materialized);
            }
            remember_resolved_interaction(&mut session.released_terminals, &terminal_id);
        } else {
            session.terminals.insert(terminal_id.clone(), materialized);
        }
        Ok(outcome)
    }

    pub(crate) fn start_operation(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        operation_id: impl Into<String>,
        kind: SessionOperationKind,
        stage: impl Into<String>,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let operation_id = operation_id.into();
        if self.operation_in_use_except_reserved(&operation_id, Some((session_id, kind))) {
            return Err(RuntimeStateError::OperationCollision);
        }
        let owner = self.live_owner_mut(session_id)?;
        session_live::start_operation(owner, operation_id, kind, stage.into())?;
        Ok(())
    }

    pub(crate) fn start_delete(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        operation_id: impl Into<String>,
    ) -> Result<(), RuntimeStateError> {
        let stage = match self.require_live(session_id)?.lifecycle {
            SessionLifecycle::Active => "closing",
            SessionLifecycle::Closed => "deleting",
            _ => return Err(RuntimeStateError::SessionNotActive),
        };
        self.start_operation(
            expected_epoch,
            session_id,
            operation_id,
            SessionOperationKind::Delete,
            stage,
        )
    }

    pub(crate) fn start_direct_delete(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        operation_id: impl Into<String>,
    ) -> Result<(), RuntimeStateError> {
        if self.require_live(session_id)?.lifecycle != SessionLifecycle::Active {
            return Err(RuntimeStateError::SessionNotActive);
        }
        self.start_operation(
            expected_epoch,
            session_id,
            operation_id,
            SessionOperationKind::Delete,
            "deleting_active",
        )
    }

    pub(crate) fn delete_close_succeeded(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        operation_id: &str,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.live_owner_mut(session_id)?;
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
            drain_session_liveness(session_id, session)
        };

        self.effects.extend(effects);
        Ok(())
    }

    pub(crate) fn complete_delete(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        operation_id: &str,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        {
            let owner = self.live_owner(session_id)?;
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
        let owner = self.live_owner_mut(session_id)?;
        owner
            .operation
            .as_mut()
            .expect("matched delete owner")
            .stage = "cleanup".to_string();
        if let Some(turn) = owner.active_turn.as_mut() {
            turn.execution = None;
        }
        let mut session = self
            .take_live_for_retirement(session_id)
            .ok_or(RuntimeStateError::UnknownSession)?;
        let effects = drain_session_liveness(session_id, &mut session);

        self.effects.extend(effects);
        self.complete_session_delete(session_id);

        Ok(())
    }

    pub(crate) fn mark_operation_uncertain(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        operation_id: &str,
        reason: impl Into<String>,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let reason = reason.into();
        let effects = {
            let owner = self.live_owner_mut(session_id)?;
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
            drain_session_liveness(session_id, session)
        };

        self.effects.extend(effects);

        Ok(())
    }

    pub(crate) fn complete_operation(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        operation_id: &str,
        kind: SessionOperationKind,
        result: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.live_owner_mut(session_id)?;
        if matches!(
            kind,
            SessionOperationKind::Close | SessionOperationKind::Delete
        ) || owner.operation.as_ref().is_none_or(|operation| {
            operation.operation_id != operation_id || operation.kind != kind
        }) {
            return Err(RuntimeStateError::OperationMismatch);
        }
        owner.operation = None;
        self.complete_session_operation(session_id, kind, &result);

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn complete_control_operation(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        operation_id: &str,
        kind: SessionOperationKind,
        control_update: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        if !matches!(
            kind,
            SessionOperationKind::SetMode | SessionOperationKind::SetConfig
        ) {
            return Err(RuntimeStateError::OperationMismatch);
        }
        let owner = self.live_owner_mut(session_id)?;
        if owner.operation.as_ref().is_none_or(|operation| {
            operation.operation_id != operation_id || operation.kind != kind
        }) {
            return Err(RuntimeStateError::OperationMismatch);
        }
        owner.operation = None;
        // The acknowledgement folds after every earlier notification (§4.5).
        owner
            .live
            .as_mut()
            .expect("live owner checked")
            .controls
            .fold_update(&control_update);
        self.complete_session_operation(session_id, kind, &Value::Null);

        Ok(())
    }

    pub(crate) fn fail_operation(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        operation_id: &str,
        kind: SessionOperationKind,
        error: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.live_owner_mut(session_id)?;
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
        self.fail_session_operation(session_id, kind, &error);

        Ok(())
    }

    pub(crate) fn close_session(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        operation_id: &str,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let owner = self.live_owner_mut(session_id)?;
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
        let effects = drain_session_liveness(session_id, live);
        self.effects.extend(effects);
        Ok(())
    }

    /// Called only after external resource cleanup; the exact owner fences late callbacks.
    pub(crate) fn finish_session_cleanup(
        &mut self,
        session_id: &str,
        kind: SessionAdmission,
        operation_id: &str,
    ) -> Result<(), MirrorError> {
        let owner = self
            .sessions
            .get_mut(session_id)
            .map(|entry| &mut entry.state)
            .ok_or(MirrorError::UnknownSession)?;
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

    /// A held session that is not in its final cleanup.
    fn live_owner(&self, session_id: &str) -> Result<&SessionState, RuntimeStateError> {
        self.sessions
            .get(session_id)
            .map(|entry| &entry.state)
            .filter(|owner| owner.live.is_some() && !in_cleanup(owner))
            .ok_or(RuntimeStateError::UnknownSession)
    }

    fn live_owner_mut(&mut self, session_id: &str) -> Result<&mut SessionState, RuntimeStateError> {
        self.sessions
            .get_mut(session_id)
            .map(|entry| &mut entry.state)
            .filter(|owner| owner.live.is_some() && !in_cleanup(owner))
            .ok_or(RuntimeStateError::UnknownSession)
    }

    fn require_live_mut(
        &mut self,
        session_id: &str,
    ) -> Result<&mut SessionLiveState, RuntimeStateError> {
        Ok(self
            .live_owner_mut(session_id)?
            .live
            .as_mut()
            .expect("live owner checked"))
    }

    fn require_live(&self, session_id: &str) -> Result<&SessionLiveState, RuntimeStateError> {
        Ok(self
            .live_owner(session_id)?
            .live
            .as_ref()
            .expect("live owner checked"))
    }

    /// A newly installed session replaces any idle entry of the same ID: its
    /// log, candidate and resources do not carry into the new attachment.
    fn install_live(&mut self, session_id: &str, live: SessionLiveState) {
        self.clear_history_for_replacement(session_id);
        self.retire_entry(session_id);
        self.sessions
            .entry(session_id.to_string())
            .or_insert_with(|| SessionEntry::new(SessionState::cold(session_id)))
            .state
            .live = Some(live);
    }

    fn take_live_for_retirement(&mut self, session_id: &str) -> Option<SessionLiveState> {
        // Resources may still own materialization waiters or cleanup handles.
        // Only explicit final removal or a new installation may retire them.
        self.sessions.get_mut(session_id)?.state.live.take()
    }

    pub(crate) fn operation_in_use(&self, operation_id: &str) -> bool {
        self.operation_in_use_except_reserved(operation_id, None)
    }

    fn operation_in_use_except_reserved(
        &self,
        operation_id: &str,
        reservation: Option<(&str, SessionOperationKind)>,
    ) -> bool {
        self.sessions.values().any(|entry| {
            let owner = &entry.state;
            owner
                .turn_execution()
                .is_some_and(|execution| execution.rpc_operation_id == operation_id)
                || owner.operation.as_ref().is_some_and(|operation| {
                    operation.operation_id == operation_id
                        && !reservation.is_some_and(|(session_id, kind)| {
                            owner.session_id == session_id
                                && operation.kind == kind
                                && operation.stage == "reserved"
                        })
                })
        })
    }

    pub(crate) fn url_flow_in_use(&self, elicitation_id: &str) -> bool {
        self.request_url_flows.contains_key(elicitation_id)
            || self
                .sessions
                .values()
                .filter_map(|entry| entry.state.live.as_ref())
                .any(|session| session.url_flows.contains_key(elicitation_id))
    }
}

// Single-session reducers borrow the existing live state. They neither access the
// registry or publication journal nor perform I/O, and can move with its owner.
mod session_live {
    use super::{
        InteractionResolution, InteractionUpsert, PendingInteraction, RuntimeStateError,
        SessionLifecycle, SessionLiveState, SessionOperationKind, SessionOperationState,
        SessionState, TurnTerminal, remember_resolved_interaction, remember_resolved_permission,
    };

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
    ) -> Result<(), RuntimeStateError> {
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
        // The turn's end ends no interaction (memory model §8.2): a session-scoped one is
        // answered, cancelled by the Agent or the user, or goes with the session.
        Ok(())
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

/// The controls at a setup response: the early control updates, then the
/// fields the response provides (protocol §4.5).
fn settle_controls(early: Vec<Value>, response: &Value) -> SessionControls {
    let early = early
        .into_iter()
        .filter_map(retain_control_update)
        .collect::<Vec<_>>();
    SessionControls::settle(&early, response)
}

pub(crate) fn fold_control_update(previous: Option<&Value>, update: Value) -> Value {
    let kind = update.get("sessionUpdate").and_then(Value::as_str);
    if !matches!(kind, Some("session_info_update" | "usage_update")) {
        return update;
    }
    let Some(mut merged) = previous.and_then(Value::as_object).cloned() else {
        return update;
    };
    // The metadata of a notification belongs to the update that carried it (D12).
    merged.remove(crate::protocol::controls::NOTIFICATION_META);
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

fn retain_control_update(entry: Value) -> Option<Value> {
    let update = entry.get("update").unwrap_or(&entry);
    let kind = update.get("sessionUpdate").and_then(Value::as_str)?;
    // A notification keeps its own `_meta` beside the update it carries (D12).
    let notification = entry.get("update").and_then(|_| entry.get("_meta"));
    (!is_conversation_update_kind(kind))
        .then(|| crate::protocol::controls::with_notification_meta(update, notification))
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

fn in_cleanup(owner: &SessionState) -> bool {
    owner
        .operation
        .as_ref()
        .is_some_and(|operation| operation.stage == "cleanup")
}

fn remember_resolved_interaction(resolved: &mut VecDeque<String>, interaction_id: &str) {
    resolved.push_back(interaction_id.to_string());
}

fn drain_session_liveness(session_id: &str, session: &mut SessionLiveState) -> Vec<RuntimeEffect> {
    let permissions = std::mem::take(&mut session.permissions);
    let mut effects = permissions
        .into_keys()
        .map(|interaction_id| RuntimeEffect::CancelPermissionResponder {
            session_id: session_id.to_string(),
            interaction_id,
        })
        .collect::<Vec<_>>();
    let elicitations = std::mem::take(&mut session.elicitations);
    effects.extend(elicitations.into_keys().map(|interaction_id| {
        RuntimeEffect::CancelElicitationResponder {
            session_id: Some(session_id.to_string()),
            interaction_id,
        }
    }));
    effects.extend(
        std::mem::take(&mut session.url_flows)
            .into_iter()
            .filter(|(_, flow)| flow.status == UrlFlowStatus::Waiting)
            .map(|(elicitation_id, flow)| RuntimeEffect::AbortUrlFlow {
                session_id: Some(session_id.to_string()),
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

/// A test's read of all live state: every session's and the connection's.
#[cfg(test)]
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LiveSnapshot {
    pub sessions: BTreeMap<String, SessionRuntime>,
    pub request_elicitations: BTreeMap<String, PendingInteraction>,
    pub request_url_flows: BTreeMap<String, UrlFlow>,
}

#[cfg(test)]
impl SessionRegistry {
    pub(crate) fn live_snapshot(&self) -> LiveSnapshot {
        LiveSnapshot {
            sessions: self
                .sessions
                .keys()
                .filter_map(|id| self.session(id).map(|session| (id.clone(), session)))
                .collect(),
            request_elicitations: self.request_elicitations.clone(),
            request_url_flows: self.request_url_flows.clone(),
        }
    }

    pub(crate) fn open_new(
        &mut self,
        expected_epoch: &str,
        session_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
        session: Value,
    ) -> Result<(), RuntimeStateError> {
        let session_id = session_id.into();
        let controls = SessionControls::settle([], &session);
        self.open_session(expected_epoch, session_id.clone(), cwd.into(), controls)?;
        self.register_new(session_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state() -> RuntimeState {
        RuntimeState::new("epoch")
    }

    fn open(state: &mut RuntimeState, session_id: &str) {
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
        operation_id: &str,
        prompt: Vec<Value>,
    ) -> String {
        let operation_id = operation_id.to_string();
        state
            .start_prompt("epoch", session_id, operation_id.clone(), prompt)
            .unwrap();
        operation_id
    }

    /// A tool entry that embeds `terminal` in its content, as the Agent sends it.
    fn embed_terminal(state: &mut RuntimeState, terminal: &str) {
        state
            .append_session_update(
                "session",
                json!({
                    "sessionUpdate": "tool_call", "toolCallId": "run", "title": "Run",
                    "content": [{ "type": "terminal", "terminalId": terminal }],
                }),
            )
            .unwrap();
    }

    fn snapshot(output: &str, released: bool) -> Value {
        json!({ "output": output, "released": released })
    }

    #[test]
    fn a_released_terminal_a_tool_embeds_keeps_its_frozen_output() {
        let mut state = state();
        open(&mut state, "session");
        embed_terminal(&mut state, "terminal");
        state
            .upsert_terminal("epoch", "session", "terminal", snapshot("kept output", false))
            .unwrap();
        state
            .upsert_terminal("epoch", "session", "terminal", snapshot("kept output", true))
            .unwrap();

        let live = state.live("session").unwrap();
        assert!(live.terminals.is_empty(), "the handle is gone");
        assert_eq!(live.frozen_terminals["terminal"]["output"], "kept output");
        // A late snapshot of the released terminal changes nothing.
        assert_eq!(
            state
                .upsert_terminal("epoch", "session", "terminal", snapshot("late", false))
                .unwrap(),
            TerminalUpsert::Stale
        );
        assert_eq!(
            state.live("session").unwrap().frozen_terminals["terminal"]["output"],
            "kept output"
        );
    }

    #[test]
    fn a_released_terminal_nothing_embeds_is_dropped() {
        let mut state = state();
        open(&mut state, "session");
        state
            .upsert_terminal("epoch", "session", "terminal", snapshot("gone", true))
            .unwrap();
        let live = state.live("session").unwrap();
        assert!(live.terminals.is_empty() && live.frozen_terminals.is_empty());
    }

    #[test]
    fn frozen_output_does_not_keep_the_session_busy() {
        let mut state = state();
        open(&mut state, "session");
        embed_terminal(&mut state, "terminal");
        state
            .upsert_terminal("epoch", "session", "terminal", snapshot("kept", true))
            .unwrap();
        state
            .start_reload("epoch", "session", "reload", SessionOperationKind::Load)
            .expect("only a terminal with a handle makes the session busy");
    }

    #[test]
    fn the_connection_ending_freezes_the_embedded_terminals_of_held_sessions() {
        let mut state = state();
        open(&mut state, "session");
        embed_terminal(&mut state, "embedded");
        for id in ["embedded", "loose"] {
            state
                .upsert_terminal("epoch", "session", id, snapshot(id, false))
                .unwrap();
        }
        state.end_connection(crate::protocol::documents::ProtocolError::new(
            crate::protocol::documents::ErrorCode::Unavailable,
            "the connection ended",
        ));
        let live = state.live("session").unwrap();
        assert!(live.terminals.is_empty(), "every handle is gone");
        assert_eq!(live.frozen_terminals.len(), 1);
        assert_eq!(live.frozen_terminals["embedded"]["output"], "embedded");
    }

    #[test]
    fn a_load_frees_frozen_output_its_timeline_no_longer_embeds() {
        let mut state = state();
        open(&mut state, "session");
        embed_terminal(&mut state, "terminal");
        state
            .upsert_terminal("epoch", "session", "terminal", snapshot("kept", true))
            .unwrap();
        let still = [json!({
            "sessionUpdate": "tool_call", "toolCallId": "run", "title": "Run",
            "content": [{ "type": "terminal", "terminalId": "terminal" }],
        })];
        state.replace_session_timeline(
            "session",
            crate::protocol::timeline::Timeline::from_replay(
                still.iter().map(|update| (update, None)),
            ),
        );
        assert!(state.live("session").unwrap().frozen_terminals.contains_key("terminal"));
        state.replace_session_timeline("session", crate::protocol::timeline::Timeline::default());
        assert!(state.live("session").unwrap().frozen_terminals.is_empty());
    }

    #[test]
    fn a_session_waiting_for_the_next_connection_keeps_its_timeline() {
        let mut state = state();
        open(&mut state, "session");
        let replay = [json!({
            "sessionUpdate": "agent_message_chunk", "messageId": "m",
            "content": { "type": "text", "text": "kept" }
        })];
        state.replace_session_timeline(
            "session",
            crate::protocol::timeline::Timeline::from_replay(
                replay.iter().map(|update| (update, None)),
            ),
        );
        let held = state.state("session").unwrap().timeline.clone().unwrap();
        let history = state.state("session").unwrap().history;
        assert_eq!(history, Some(crate::protocol::timeline::History::Complete));
        let epoch = state.state("session").unwrap().timeline_epoch.clone();
        assert!(epoch.is_some());

        let waiting = state
            .wait_for_next_connection("session")
            .expect("a held session waits");
        let mut next = RuntimeState::new("epoch-2");
        next.seed_waiting(waiting).unwrap();

        let restored = next.state("session").unwrap().timeline.clone().unwrap();
        assert_eq!(next.state("session").unwrap().history, history);
        assert_eq!(
            next.state("session").unwrap().timeline_epoch,
            epoch,
            "the same timeline is the same epoch across a connection"
        );
        assert!(Arc::ptr_eq(&held, &restored));
        assert_eq!(restored.entries().len(), 1);
    }

    #[test]
    fn close_cleanup_rejects_late_live_resources_without_publication_or_resurrection() {
        let mut state = state();
        open(&mut state, "session");
        state
            .start_operation(
                "epoch",
                "session",
                "close",
                SessionOperationKind::Close,
                "closing",
            )
            .unwrap();
        state.close_session("epoch", "session", "close").unwrap();
        let before = state.state("session").unwrap().clone();
        let published = state.live_snapshot();
        assert_eq!(
            state.upsert_permission("epoch", "session", "late", json!({})),
            Err(RuntimeStateError::UnknownSession)
        );
        assert_eq!(
            state.upsert_elicitation("epoch", Some("session"), "late", json!({})),
            Err(RuntimeStateError::UnknownSession)
        );
        assert_eq!(
            state.upsert_terminal("epoch", "session", "late", json!({ "output": "late" })),
            Err(RuntimeStateError::UnknownSession)
        );
        assert_eq!(state.state("session").unwrap(), &before);
        assert_eq!(state.live_snapshot(), published);
        assert!(state.take_effects().is_empty());
    }

    #[test]
    fn direct_operation_entry_cannot_bypass_canonical_history_admission() {
        let mut state = state();
        open(&mut state, "session");
        let operation_id = state
            .admit_session_turn("epoch", "session", "prompt", vec![])
            .unwrap();
        state
            .complete_prompt("epoch", "session", "prompt", json!({}))
            .unwrap();
        state
            .complete_turn(
                "session",
                &operation_id,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        let before = state.live_snapshot();
        assert_eq!(
            state.start_operation(
                "epoch",
                "session",
                "control",
                SessionOperationKind::SetMode,
                "request"
            ),
            Err(RuntimeStateError::BusySession)
        );
        assert_eq!(
            state.start_reload("epoch", "session", "load", SessionOperationKind::Load),
            Err(RuntimeStateError::BusySession)
        );
        assert_eq!(state.live_snapshot(), before);
        state
            .block_turn("session", &operation_id, "history unavailable")
            .unwrap();
        let before = state.live_snapshot();
        assert_eq!(
            state.start_operation(
                "epoch",
                "session",
                "fork",
                SessionOperationKind::Fork,
                "request"
            ),
            Err(RuntimeStateError::BusySession)
        );
        assert_eq!(state.live_snapshot(), before);
    }

    #[test]
    fn history_terminal_requires_live_retirement_and_dto_mutation_cannot_change_owner() {
        let mut state = state();
        open(&mut state, "session");
        state.register_new("session");
        let operation_id = state
            .admit_session_turn("epoch", "session", "intent", vec![])
            .unwrap();
        state
            .upsert_permission("epoch", "session", "permission", json!({}))
            .unwrap();
        let before = state.state("session").unwrap().clone();
        assert_eq!(
            state.complete_turn(
                "session",
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
            .complete_prompt("epoch", "session", "intent", json!({}))
            .unwrap();
        // The turn's end ends no interaction: the permission is still pending, with no effect.
        assert!(state.take_effects().is_empty());
        assert!(state.session("session").unwrap().permissions.contains_key("permission"));
        state
            .complete_turn(
                "session",
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
    fn unified_turn_admission_is_atomic_and_a_second_prompt_is_refused() {
        let mut state = state();
        open(&mut state, "session");
        state.register_new("session");
        state.require_live_mut("session").unwrap().lifecycle = SessionLifecycle::Closed;
        let before = state.state("session").unwrap().clone();
        let seq = state.live_snapshot();
        let next_operation = state.next_operation;
        assert_eq!(
            state.admit_session_turn("epoch", "session", "intent", vec![json!("hello")]),
            Err(SessionTurnError::Live(RuntimeStateError::SessionNotActive))
        );
        assert_eq!(state.state("session").unwrap(), &before);
        assert_eq!(state.live_snapshot(), seq);
        assert_eq!(state.next_operation, next_operation);
        state.require_live_mut("session").unwrap().lifecycle = SessionLifecycle::Active;
        state
            .admit_session_turn("epoch", "session", "intent", vec![json!("hello")])
            .unwrap();
        assert_eq!(
            state
                .state("session")
                .unwrap()
                .turn_execution()
                .unwrap()
                .rpc_operation_id,
            "intent"
        );
        let before = state.live_snapshot();
        assert_eq!(
            state.admit_session_turn("epoch", "session", "intent", vec![json!("hello")]),
            Err(SessionTurnError::History(MirrorError::WrongPhase))
        );
        assert_eq!(state.live_snapshot(), before);
    }

    #[test]
    fn a_reserved_operation_exempts_only_itself_from_global_identity_checks() {
        for kind in [SessionOperationKind::SetMode, SessionOperationKind::Load] {
            let mut state = state();
            open(&mut state, "first");
            open(&mut state, "second");
            state
                .start_prompt("epoch", "first", "shared-id", vec![])
                .unwrap();
            state.register_new("second");
            state
                .sessions
                .get_mut("second")
                .unwrap()
                .state
                .begin_exclusive(kind, "shared-id")
                .unwrap();
            let before = state.live_snapshot();
            let result = if kind == SessionOperationKind::Load {
                state.start_reload("epoch", "second", "shared-id", kind)
            } else {
                state.start_operation("epoch", "second", "shared-id", kind, "request")
            };
            assert_eq!(result, Err(RuntimeStateError::OperationCollision));
            assert_eq!(state.live_snapshot(), before);
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
    fn close_confirmation_keeps_the_unique_owner_until_exact_cleanup_finishes() {
        let mut state = state();
        open(&mut state, "session");
        state.register_new("session");
        state
            .begin_exclusive("session", SessionOperationKind::Close, "close")
            .unwrap();
        state
            .start_operation(
                "epoch",
                "session",
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
        state.close_session("epoch", "session", "close").unwrap();
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
        assert!(!state.live_snapshot().sessions.contains_key("session"));
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
            state.finish_session_cleanup("session", SessionAdmission::Close, "stale"),
            Err(MirrorError::OperationMismatch)
        );
        state
            .finish_session_cleanup("session", SessionAdmission::Close, "close")
            .unwrap();
        state.remove("session");
        assert!(state.state("session").is_none());
        open(&mut state, "session");
        assert!(state.session("session").is_some());
    }

    #[test]
    fn operation_and_prompt_uncertainty_retire_in_either_order_without_losing_owner() {
        for operation_first in [true, false] {
            let mut state = state();
            open(&mut state, "session");
            state.register_new("session");
            let operation_id = state
                .admit_session_turn("epoch", "session", "prompt", vec![])
                .unwrap();
            state
                .begin_exclusive("session", SessionOperationKind::SetMode, "control")
                .unwrap();
            state
                .start_operation(
                    "epoch",
                    "session",
                    "control",
                    SessionOperationKind::SetMode,
                    "request",
                )
                .unwrap();
            if operation_first {
                state
                    .mark_operation_uncertain("epoch", "session", "control", "EOF")
                    .unwrap();
                assert!(state.state("session").unwrap().turn_execution().is_some());
            }
            state
                .mark_prompt_uncertain("epoch", "session", "prompt", "EOF")
                .unwrap();
            state.block_turn("session", &operation_id, "EOF").unwrap();
            if !operation_first {
                state
                    .mark_operation_uncertain("epoch", "session", "control", "EOF")
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
    fn active_turn_folds_only_adjacent_compatible_text_chunks() {
        let mut state = state();
        open(&mut state, "session");
        state
            .start_prompt("epoch", "session", "prompt", Vec::new())
            .unwrap();
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
        for update in updates {
            state
                .append_runtime_update_for_test("epoch", "session", update)
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
    }

    #[test]
    fn active_turn_replaces_tool_fields_at_the_first_position_without_changing_start_kind() {
        let mut state = state();
        open(&mut state, "session");
        state
            .start_prompt("epoch", "session", "prompt", Vec::new())
            .unwrap();
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
        for update in updates {
            state
                .append_runtime_update_for_test("epoch", "session", update)
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
    }

    #[test]
    fn active_turn_replaces_plan_slots_in_place_and_removal_clears_only_its_slot() {
        let mut state = state();
        open(&mut state, "session");
        state
            .start_prompt("epoch", "session", "prompt", Vec::new())
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
                .append_runtime_update_for_test("epoch", "session", update)
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
                .append_runtime_update_for_test("epoch", "session", update)
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
        open(&mut state, "session");
        state
            .start_prompt("epoch", "session", "prompt", Vec::new())
            .unwrap();
        state
            .append_runtime_update_for_test(
                "epoch",
                "session",
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": "tool",
                    "details": { "stable": true }
                }),
            )
            .unwrap();
        let before_invalid = state.live_snapshot();
        assert_eq!(
            state.append_runtime_update_for_test(
                "epoch",
                "session",
                json!({ "sessionUpdate": "tool_call_update", "status": "invalid" }),
            ),
            Err(RuntimeStateError::OperationMismatch),
        );
        assert_eq!(state.live_snapshot(), before_invalid);

        assert_eq!(
            state.append_runtime_update_for_test(
                "epoch",
                "session",
                json!({
                    "sessionUpdate": "plan_update",
                    "plan": { "type": "markdown", "content": "missing ID" }
                }),
            ),
            Err(RuntimeStateError::OperationMismatch),
        );
        assert_eq!(state.live_snapshot(), before_invalid);

        let before_large = state.live_snapshot();
        state
            .append_runtime_update_for_test(
                "epoch",
                "session",
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "tool",
                    "details": { "oversize": "x".repeat(1_024) }
                }),
            )
            .unwrap();
        assert_ne!(state.live_snapshot(), before_large);
    }

    #[test]
    fn ten_thousand_terminal_turns_leave_no_folded_updates() {
        let mut state = state();
        open(&mut state, "session");

        for index in 0..10_000 {
            let operation_id = format!("turn-{index}");
            state
                .start_prompt("epoch", "session", &operation_id, Vec::new())
                .unwrap();
            state
                .append_runtime_update_for_test(
                    "epoch",
                    "session",
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
                    &format!("turn-{index}"),
                    json!({ "stopReason": "end_turn" }),
                )
                .unwrap();

            assert!(state.session("session").unwrap().active_turn.is_none());
        }

        assert!(
            !serde_json::to_string(&state.live_snapshot())
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

        assert!(
            retained_by.is_empty(),
            "terminal turn payload is still retained by: {}",
            retained_by.join(", ")
        );
    }

    #[test]
    fn terminal_response_drops_prompt_updates_indices_and_operation_payload() {
        let mut state = state();
        open(&mut state, "session");
        let operation_id = start_prompt_operation(
            &mut state,
            "session",
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
        open(&mut state, "session");
        let operation_id = start_prompt_operation(
            &mut state,
            "session",
            "error-client-intent",
            vec![json!({ "type": "text", "text": "error-prompt-marker" })],
        );
        state
            .append_runtime_update_for_test(
                "epoch",
                "session",
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
        open(&mut state, "session");
        let operation_id = start_prompt_operation(
            &mut state,
            "session",
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
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "uncertain-tool-id",
                    "marker": "uncertain-update-marker"
                }),
            )
            .unwrap();
        state
            .mark_prompt_uncertain("epoch", "session", &operation_id, "uncertain-result-marker")
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
        open(&mut state, "session");
        let first_operation_id = start_prompt_operation(
            &mut state,
            "session",
            "first-client-intent",
            vec![json!("first-prompt-marker")],
        );
        state
            .append_runtime_update_for_test(
                "epoch",
                "session",
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
                &first_operation_id,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();

        let second_operation_id = start_prompt_operation(
            &mut state,
            "session",
            "second-client-intent",
            vec![json!("second prompt")],
        );
        state
            .append_runtime_update_for_test(
                "epoch",
                "session",
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
        open(&mut state, "session");
        let operation_id = start_prompt_operation(
            &mut state,
            "session",
            "resource-client-intent",
            vec![json!("resource-prompt-marker")],
        );
        state
            .upsert_terminal(
                "epoch",
                "session",
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
                Some("session"),
                "url-elicitation",
                json!({ "mode": "url", "url": "https://example.test/live" }),
            )
            .unwrap();
        state
            .resolve_elicitation(
                "epoch",
                Some("session"),
                "url-elicitation",
                Some("live-url-flow"),
            )
            .unwrap();
        state
            .complete_prompt(
                "epoch",
                "session",
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
        open(&mut state, "session");

        for index in 0..32 {
            let marker = format!("sequential-turn-payload-{index}");
            let operation_id = start_prompt_operation(
                &mut state,
                "session",
                &format!("sequential-intent-{index}"),
                vec![json!({ "type": "text", "text": marker })],
            );
            state
                .append_runtime_update_for_test(
                    "epoch",
                    "session",
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
                    &operation_id,
                    json!({ "stopReason": "end_turn", "marker": marker }),
                )
                .unwrap();
        }

        let session = state.session("session").unwrap();
        assert!(session.active_turn.is_none());

        assert!(
            !serde_json::to_string(&state.live_snapshot())
                .unwrap()
                .contains("sequential-turn-payload"),
            "live state must not retain completed turn payloads"
        );
    }

    #[test]
    fn turn_terminal_event_retires_active_exactly_once() {
        let mut state = state();
        open(&mut state, "session");
        state
            .start_prompt("epoch", "session", "prompt", vec![json!("hello")])
            .unwrap();
        state
            .append_runtime_update_for_test("epoch", "session", json!("answer"))
            .unwrap();
        state
            .complete_prompt(
                "epoch",
                "session",
                "prompt",
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();

        let session = state.session("session").unwrap();
        assert!(session.active_turn.is_none());
        let settled_seq = state.live_snapshot();
        assert_eq!(
            state.complete_prompt(
                "epoch",
                "session",
                "prompt",
                json!({ "stopReason": "end_turn" }),
            ),
            Err(RuntimeStateError::NoActiveTurn),
        );
        assert_eq!(state.live_snapshot(), settled_seq);
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
            session.controls.modes.as_ref().unwrap().current_mode_id,
            "plan"
        );
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
        open(&mut state, "session");
        state
            .start_prompt("epoch", "session", "prompt", Vec::new())
            .unwrap();
        state.request_cancel("epoch", "session").unwrap();
        state
            .append_runtime_update_for_test("epoch", "session", json!("after cancel"))
            .unwrap();

        let active = state.session("session").unwrap().active_turn.unwrap();
        assert!(active.cancel_requested);
        assert_eq!(*active.updates[0], json!("after cancel"));
    }

    #[test]
    fn epoch_rejects_stale_mutations() {
        let mut state = state();
        open(&mut state, "session");
        assert_eq!(
            state.start_prompt("old", "session", "prompt", Vec::new()),
            Err(RuntimeStateError::EpochMismatch),
        );
        assert!(state.session("session").unwrap().active_turn.is_none());
    }

    #[test]
    fn turn_retirement_never_changes_live_resources() {
        let mut state = state();
        open(&mut state, "session");
        state
            .upsert_permission(
                "epoch",
                "session",
                "permission",
                json!({ "question": "allow" }),
            )
            .unwrap();
        state
            .upsert_elicitation(
                "epoch",
                Some("session"),
                "elicitation",
                json!({ "mode": "form" }),
            )
            .unwrap();
        state
            .upsert_terminal(
                "epoch",
                "session",
                "terminal",
                json!({ "output": "running" }),
            )
            .unwrap();
        for index in 0..3 {
            let operation_id = format!("prompt-{index}");
            state
                .start_prompt("epoch", "session", &operation_id, vec![json!(index)])
                .unwrap();
            state
                .complete_prompt(
                    "epoch",
                    "session",
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
        open(&mut state, "session");
        state
            .start_prompt("epoch", "session", "prompt", Vec::new())
            .unwrap();
        state
            .append_runtime_update_for_test(
                "epoch",
                "session",
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
        state.request_cancel("epoch", "session").unwrap();
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
    fn controls_and_close_coexist_with_an_active_prompt() {
        let mut state = state();
        open(&mut state, "session");
        state
            .start_prompt("epoch", "session", "prompt", Vec::new())
            .unwrap();
        for kind in [
            SessionOperationKind::SetMode,
            SessionOperationKind::SetConfig,
        ] {
            state
                .start_operation("epoch", "session", "control", kind, "changing")
                .unwrap();
            assert!(state.session("session").unwrap().active_turn.is_some());
            state
                .fail_operation(
                    "epoch",
                    "session",
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
                "close",
                SessionOperationKind::Close,
                "closing",
            )
            .unwrap();
        state
            .complete_prompt(
                "epoch",
                "session",
                "prompt",
                json!({"stopReason":"cancelled"}),
            )
            .unwrap();
        assert_eq!(
            state.session("session").unwrap().lifecycle,
            SessionLifecycle::Closing
        );
        state.close_session("epoch", "session", "close").unwrap();
        assert!(state.session("session").is_none());
    }

    #[test]
    fn different_sessions_progress_independently() {
        let mut state = state();
        open(&mut state, "a");
        open(&mut state, "b");
        state
            .start_prompt("epoch", "a", "prompt-a", Vec::new())
            .unwrap();
        state
            .start_prompt("epoch", "b", "prompt-b", Vec::new())
            .unwrap();
        assert_eq!(
            state.start_operation(
                "epoch",
                "a",
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
        open(&mut state, "session");
        state
            .start_prompt("epoch", "session", "prompt", Vec::new())
            .unwrap();
        state
            .mark_prompt_uncertain(
                "epoch",
                "session",
                "prompt",
                "transport lost after dispatch",
            )
            .unwrap();

        let session = state.session("session").unwrap();
        assert_eq!(session.lifecycle, SessionLifecycle::Uncertain);
        assert!(session.active_turn.is_none());
    }

    #[test]
    fn uncertain_prompt_terminal_retires_the_turn_in_one_transition() {
        let mut state = state();
        open(&mut state, "session");
        state
            .start_prompt("epoch", "session", "prompt", Vec::new())
            .unwrap();
        let before = state.live_snapshot();

        state
            .mark_prompt_uncertain("epoch", "session", "prompt", "response lost after dispatch")
            .unwrap();

        assert_ne!(state.live_snapshot(), before);
        let session = state.session("session").unwrap();
        assert_eq!(session.lifecycle, SessionLifecycle::Uncertain);
        assert!(session.active_turn.is_none());
        assert!(
            !serde_json::to_string(&state.live_snapshot())
                .unwrap()
                .contains("response lost after dispatch")
        );
    }

    #[test]
    fn prompt_failure_retires_partial_turn_and_operation_payload() {
        let mut state = state();
        open(&mut state, "session");
        let operation_id = "prompt".to_string();
        state
            .start_prompt("epoch", "session", &operation_id, vec![json!("hello")])
            .unwrap();
        state
            .append_runtime_update_for_test("epoch", "session", json!("partial"))
            .unwrap();
        state
            .fail_prompt("epoch", "session", &operation_id, json!({ "code": -32000 }))
            .unwrap();

        assert!(!state.operation_in_use(&operation_id));
        let session = state.session("session").unwrap();
        assert!(session.active_turn.is_none());
        assert_eq!(
            state.append_runtime_update_for_test("epoch", "session", json!("late")),
            Err(RuntimeStateError::NoActiveTurn),
        );
    }

    #[test]
    fn cancelled_prompt_response_retires_the_intent_outcome() {
        let mut state = state();
        open(&mut state, "session");
        let operation_id = "prompt".to_string();
        state
            .start_prompt("epoch", "session", &operation_id, Vec::new())
            .unwrap();
        state.request_cancel("epoch", "session").unwrap();
        state
            .complete_prompt(
                "epoch",
                "session",
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
        open(&mut state, "session");
        state
            .upsert_permission(
                "epoch",
                "session",
                "permission",
                json!({ "options": ["allow"] }),
            )
            .unwrap();

        assert_eq!(
            state
                .resolve_permission("epoch", "session", "permission")
                .unwrap(),
            InteractionResolution::Applied,
        );
        let settled_seq = state.live_snapshot();
        assert_eq!(
            state
                .resolve_permission("epoch", "session", "permission")
                .unwrap(),
            InteractionResolution::AlreadyResolved,
        );
        assert_eq!(state.live_snapshot(), settled_seq);
    }

    #[test]
    fn close_then_delete_is_irreversible_and_partial_failure_stays_closed() {
        let mut state = state();
        open(&mut state, "session");
        state
            .start_delete("epoch", "session", "delete-operation")
            .unwrap();
        assert_eq!(
            state.session("session").unwrap().lifecycle,
            SessionLifecycle::ClosingForDelete,
        );

        state
            .delete_close_succeeded("epoch", "session", "delete-operation")
            .unwrap();
        assert_eq!(
            state.session("session").unwrap().lifecycle,
            SessionLifecycle::Deleting,
        );

        state
            .fail_operation(
                "epoch",
                "session",
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
            state.start_prompt("epoch", "session", "late", Vec::new()),
            Err(RuntimeStateError::SessionNotActive),
        );
    }

    #[test]
    fn reopening_a_closed_session_starts_without_its_old_turn() {
        let mut state = state();
        open(&mut state, "session");
        state
            .start_operation(
                "epoch",
                "session",
                "close",
                SessionOperationKind::Close,
                "closing",
            )
            .unwrap();
        state.close_session("epoch", "session", "close").unwrap();
        state
            .finish_session_cleanup("session", SessionAdmission::Close, "close")
            .unwrap();
        open(&mut state, "session");

        let reopened = state.session("session").unwrap();
        assert_eq!(reopened.lifecycle, SessionLifecycle::Active);
        assert!(reopened.active_turn.is_none());
    }

    #[test]
    fn canonical_control_accessors_apply_updates_and_preserve_available_choices() {
        let mut state = state();
        let modes = json!({ "currentModeId": "build", "availableModes": [
            { "id": "build", "name": "Build" }, { "id": "plan", "name": "Plan" }
        ] });
        let options = json!([{ "id": "verbose", "name": "Verbose", "type": "boolean", "currentValue": false }]);
        state
            .open_new(
                "epoch",
                "session",
                "/workspace",
                json!({ "modes": modes, "configOptions": options }),
            )
            .unwrap();
        assert_eq!(
            Value::from(
                state
                    .live("session")
                    .unwrap()
                    .controls
                    .config_options
                    .clone()
            ),
            options
        );
        state
            .update_control_state(
                "epoch",
                "session",
                json!({ "sessionUpdate": "current_mode_update", "currentModeId": "plan" }),
            )
            .unwrap();
        state
            .update_control_state(
                "epoch",
                "session",
                json!({ "sessionUpdate": "config_option_update", "configOptions": [] }),
            )
            .unwrap();
        let live = state.live("session").unwrap();
        let live_modes = serde_json::to_value(&live.controls.modes).unwrap();
        assert_eq!(live_modes["availableModes"], modes["availableModes"]);
        assert_eq!(live_modes["currentModeId"], "plan");
        assert_eq!(live.controls.config_options, Some(Vec::new()));
        crate::semantic::validate_session_mode_reference(Some(&live_modes), "build").unwrap();
        crate::semantic::validate_session_mode_reference(Some(&live_modes), "plan").unwrap();
        assert!(
            crate::semantic::validate_session_config_reference(
                &Value::from(live.controls.config_options.clone()),
                "verbose",
                &json!(true)
            )
            .is_err()
        );
        // An authoritative response replaces both the baseline and its overlay.
        state
            .start_reload("epoch", "session", "reload", SessionOperationKind::Load)
            .unwrap();
        state
            .complete_reload(
                "epoch",
                "session",
                "reload",
                json!({ "modes": modes, "configOptions": options }),
            )
            .unwrap();
        let live = state.live("session").unwrap();
        assert_eq!(
            live.controls.modes.as_ref().unwrap().current_mode_id,
            "build"
        );
        assert_eq!(Value::from(live.controls.config_options.clone()), options);
    }

    #[test]
    fn fork_keeps_only_target_control_metadata() {
        let mut state = state();
        open(&mut state, "source");
        state
            .start_prompt("epoch", "source", "prompt", vec![json!("Q")])
            .unwrap();
        state
            .append_runtime_update_for_test("epoch", "source", json!("A"))
            .unwrap();
        state
            .complete_prompt(
                "epoch",
                "source",
                "prompt",
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        state
            .upsert_permission(
                "epoch",
                "source",
                "permission",
                json!({ "question": "allow" }),
            )
            .unwrap();
        state
            .upsert_terminal("epoch", "source", "terminal", json!({ "output": "live" }))
            .unwrap();

        state
            .open_forked(
                "epoch",
                "derived",
                "/derived",
                json!({ "sessionId": "derived" }),
                "source",
                None,
            )
            .unwrap();
        let derived = state.session("derived").unwrap();
        assert!(derived.permissions.is_empty());
        assert!(derived.terminals.is_empty());
        assert!(derived.active_turn.is_none());
        // The source's prompt and update are the JSON strings "Q" and "A".
        let derived_payload = serde_json::to_string(&derived).unwrap();
        assert!(!derived_payload.contains(r#""Q""#));
        assert!(!derived_payload.contains(r#""A""#));

        state
            .open_forked(
                "epoch",
                "replayed",
                "/replayed",
                json!({ "sessionId": "replayed" }),
                "source",
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
            replayed.controls.modes.as_ref().unwrap().current_mode_id,
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
        open(&mut state, "a");
        open(&mut state, "b");
        state
            .start_prompt("epoch", "a", "same-operation", Vec::new())
            .unwrap();

        assert_eq!(
            state.start_prompt("epoch", "b", "same-operation", Vec::new()),
            Err(RuntimeStateError::OperationCollision),
        );
        assert!(state.session("a").unwrap().active_turn.is_some());
        assert!(state.session("b").unwrap().active_turn.is_none());
    }

    #[test]
    fn retired_operation_identity_can_be_reused() {
        let mut state = state();
        open(&mut state, "a");
        open(&mut state, "b");
        state
            .start_prompt("epoch", "a", "same-operation", Vec::new())
            .unwrap();
        state
            .complete_prompt(
                "epoch",
                "a",
                "same-operation",
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        assert_eq!(
            state
                .start_prompt("epoch", "b", "same-operation", Vec::new())
                .unwrap(),
            "same-operation"
        );
        assert!(state.session("a").unwrap().active_turn.is_none());
        assert!(state.session("b").unwrap().active_turn.is_some());
    }

    #[test]
    fn duplicate_cancel_request_is_idempotent() {
        let mut state = state();
        open(&mut state, "session");
        state
            .start_prompt("epoch", "session", "prompt", Vec::new())
            .unwrap();
        state.request_cancel("epoch", "session").unwrap();
        let first_cancel_seq = state.live_snapshot();

        state.request_cancel("epoch", "session").unwrap();

        assert_eq!(state.live_snapshot(), first_cancel_seq);
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
    fn a_sealed_turn_drops_its_cancel_flag_and_ends_no_interaction() {
        let mut state = state();
        open(&mut state, "session");
        state
            .start_prompt("epoch", "session", "prompt", Vec::new())
            .unwrap();
        state.request_cancel("epoch", "session").unwrap();
        state
            .upsert_permission(
                "epoch",
                "session",
                "permission",
                json!({ "question": "allow" }),
            )
            .unwrap();
        state
            .upsert_elicitation(
                "epoch",
                Some("session"),
                "elicitation",
                json!({ "mode": "form" }),
            )
            .unwrap();

        state
            .complete_prompt(
                "epoch",
                "session",
                "prompt",
                json!({ "stopReason": "cancelled" }),
            )
            .unwrap();

        // The turn is over; the interactions the Agent asked of the session are still its own
        // (memory model §8.2), and nothing answers their responders.
        let session = state.session("session").unwrap();
        assert!(session.active_turn.is_none());
        assert!(session.permissions.contains_key("permission"));
        assert!(session.elicitations.contains_key("elicitation"));
        assert!(state.take_effects().is_empty());
    }

    #[test]
    fn duplicate_permission_request_never_overwrites_the_original() {
        let mut state = state();
        open(&mut state, "session");
        assert_eq!(
            state
                .upsert_permission("epoch", "session", "permission", json!({ "question": "A" }),)
                .unwrap(),
            InteractionUpsert::Inserted,
        );
        let inserted_seq = state.live_snapshot();
        assert_eq!(
            state
                .upsert_permission("epoch", "session", "permission", json!({ "question": "A" }),)
                .unwrap(),
            InteractionUpsert::Duplicate,
        );
        assert_eq!(state.live_snapshot(), inserted_seq);
        assert_eq!(
            state.upsert_permission("epoch", "session", "permission", json!({ "question": "B" }),),
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
        open(&mut state, "session");
        state
            .upsert_terminal(
                "epoch",
                "session",
                "terminal",
                json!({ "output": "running", "released": false }),
            )
            .unwrap();
        state
            .upsert_terminal(
                "epoch",
                "session",
                "terminal",
                json!({ "output": "done", "released": true }),
            )
            .unwrap();
        let released_seq = state.live_snapshot();

        assert_eq!(
            state
                .upsert_terminal(
                    "epoch",
                    "session",
                    "terminal",
                    json!({ "output": "late", "released": false }),
                )
                .unwrap(),
            TerminalUpsert::Stale,
        );
        assert_eq!(state.live_snapshot(), released_seq);
        assert!(
            !state
                .session("session")
                .unwrap()
                .terminals
                .contains_key("terminal")
        );
        assert!(
            !serde_json::to_string(&state.live_snapshot())
                .unwrap()
                .contains("done")
        );
    }

    #[test]
    fn closing_a_session_releases_resources_once() {
        let mut state = state();
        open(&mut state, "session");
        state
            .upsert_terminal("epoch", "session", "terminal", json!({ "released": false }))
            .unwrap();
        state
            .start_operation(
                "epoch",
                "session",
                "close",
                SessionOperationKind::Close,
                "closing",
            )
            .unwrap();
        state.close_session("epoch", "session", "close").unwrap();

        assert!(state.operation_in_use("close"));
        assert!(state.session("session").is_none());
        assert_eq!(
            state.take_effects(),
            vec![RuntimeEffect::ReleaseTerminal {
                session_id: "session".to_string(),
                terminal_id: "terminal".to_string(),
            }]
        );
        assert!(state.take_effects().is_empty());
        state
            .finish_session_cleanup("session", SessionAdmission::Close, "close")
            .unwrap();
        assert!(!state.operation_in_use("close"));
    }

    #[test]
    fn retrying_delete_from_closed_never_resurrects_the_session() {
        let mut state = state();
        open(&mut state, "session");
        state
            .start_delete("epoch", "session", "first-delete")
            .unwrap();
        state
            .delete_close_succeeded("epoch", "session", "first-delete")
            .unwrap();
        state
            .fail_operation(
                "epoch",
                "session",
                "first-delete",
                SessionOperationKind::Delete,
                json!("delete failed"),
            )
            .unwrap();

        state
            .start_delete("epoch", "session", "retry-delete")
            .unwrap();
        let retrying = state.session("session").unwrap();
        assert_eq!(retrying.lifecycle, SessionLifecycle::Deleting);
        assert_eq!(retrying.operation.as_ref().unwrap().stage, "deleting");
        state
            .fail_operation(
                "epoch",
                "session",
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
        open(&mut state, "session");
        state
            .upsert_permission(
                "epoch",
                "session",
                "permission",
                json!({ "question": "allow" }),
            )
            .unwrap();
        state
            .upsert_terminal("epoch", "session", "terminal", json!({ "released": false }))
            .unwrap();
        state.start_delete("epoch", "session", "delete").unwrap();

        state
            .delete_close_succeeded("epoch", "session", "delete")
            .unwrap();

        let session = state.session("session").unwrap();
        assert!(session.permissions.is_empty());
        assert!(session.terminals.is_empty());
        assert_eq!(
            state.take_effects(),
            vec![
                RuntimeEffect::CancelPermissionResponder {
                    session_id: "session".to_string(),
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
    fn delete_success_removes_once_and_a_late_completion_cannot_delete_a_reopened_session() {
        let mut state = state();
        open(&mut state, "session");
        state.start_delete("epoch", "session", "delete").unwrap();
        state
            .delete_close_succeeded("epoch", "session", "delete")
            .unwrap();
        let before = state.live_snapshot();

        state.complete_delete("epoch", "session", "delete").unwrap();

        assert_ne!(state.live_snapshot(), before);
        assert!(state.session("session").is_none());
        state
            .finish_session_cleanup("session", SessionAdmission::Delete, "delete")
            .unwrap();
        open(&mut state, "session");
        assert_eq!(
            state.complete_delete("epoch", "session", "delete"),
            Err(RuntimeStateError::OperationMismatch),
        );
        assert!(state.session("session").is_some());
    }

    #[test]
    fn uncertain_delete_is_one_terminal_transition_and_blocks_mutation() {
        let mut state = state();
        open(&mut state, "session");
        state.start_delete("epoch", "session", "delete").unwrap();
        let before = state.live_snapshot();

        state
            .mark_operation_uncertain("epoch", "session", "delete", "close response was lost")
            .unwrap();

        assert_ne!(state.live_snapshot(), before);
        let session = state.session("session").unwrap();
        assert_eq!(session.lifecycle, SessionLifecycle::Uncertain);
        assert_eq!(session.operation.as_ref().unwrap().stage, "uncertain");
        assert_eq!(
            state.start_prompt("epoch", "session", "prompt", Vec::new()),
            Err(RuntimeStateError::SessionNotActive),
        );
    }

    #[test]
    fn independent_session_transitions_commute() {
        fn run(a_first: bool) -> LiveSnapshot {
            let mut state = state();
            open(&mut state, "a");
            open(&mut state, "b");
            let update_a = |state: &mut RuntimeState| {
                state
                    .start_prompt("epoch", "a", "prompt-a", vec![json!("A")])
                    .unwrap();
            };
            let update_b = |state: &mut RuntimeState| {
                state
                    .start_prompt("epoch", "b", "prompt-b", vec![json!("B")])
                    .unwrap();
            };
            if a_first {
                update_a(&mut state);
                update_b(&mut state);
            } else {
                update_b(&mut state);
                update_a(&mut state);
            }
            state.live_snapshot()
        }

        assert_eq!(run(true).sessions, run(false).sessions);
    }

    #[test]
    fn terminal_result_is_not_retained_after_turn_retirement() {
        let mut state = RuntimeState::new("epoch");
        open(&mut state, "session");
        let operation_id = "prompt".to_string();
        state
            .start_prompt("epoch", "session", &operation_id, Vec::new())
            .unwrap();

        state
            .complete_prompt(
                "epoch",
                "session",
                &operation_id,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();

        assert!(state.session("session").unwrap().active_turn.is_none());
        assert!(!state.operation_in_use(&operation_id));
        assert!(
            !serde_json::to_string(&state.live_snapshot())
                .unwrap()
                .contains("stopReason")
        );
    }

    #[test]
    fn rejected_transition_leaves_live_state_unchanged() {
        let mut state = state();
        open(&mut state, "session");
        state
            .start_prompt("epoch", "session", "active", Vec::new())
            .unwrap();
        let before = state.live_snapshot();

        assert_eq!(
            state.start_prompt("epoch", "session", "rejected", Vec::new()),
            Err(RuntimeStateError::BusySession),
        );

        assert_eq!(state.live_snapshot(), before);
    }

    #[test]
    fn definite_close_failure_restores_active_and_releases_operation_guard() {
        let mut state = state();
        open(&mut state, "session");
        state
            .start_operation(
                "epoch",
                "session",
                "close",
                SessionOperationKind::Close,
                "closing",
            )
            .unwrap();

        state
            .fail_operation(
                "epoch",
                "session",
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
            .start_prompt("epoch", "session", "next", Vec::new())
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
            open(&mut state, "session");
            let operation_id = format!("{kind:?}");
            state
                .start_operation("epoch", "session", &operation_id, kind, "running")
                .unwrap();
            if succeeds {
                state
                    .complete_operation(
                        "epoch",
                        "session",
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
                state.complete_operation("epoch", "session", &operation_id, kind, Value::Null,),
                Err(RuntimeStateError::OperationMismatch),
            );
        }
    }

    #[test]
    fn control_success_updates_value_and_retires_result_payload_atomically() {
        let mut state = state();
        open(&mut state, "session");
        state
            .start_operation(
                "epoch",
                "session",
                "mode",
                SessionOperationKind::SetMode,
                "setting",
            )
            .unwrap();
        let before = state.live_snapshot();

        state
            .complete_control_operation(
                "epoch",
                "session",
                "mode",
                SessionOperationKind::SetMode,
                json!({ "sessionUpdate": "current_mode_update", "currentModeId": "plan" }),
            )
            .unwrap();

        assert_ne!(state.live_snapshot(), before);
        let session = state.session("session").unwrap();
        assert!(session.operation.is_none());
        assert_eq!(
            session.controls.modes.as_ref().unwrap().current_mode_id,
            "plan",
        );
        assert!(!state.operation_in_use("mode"));
        assert!(
            !serde_json::to_string(&state.live_snapshot())
                .unwrap()
                .contains("\"ok\":true")
        );
    }

    #[test]
    fn load_candidate_is_invisible_until_success_then_commits_atomically() {
        let mut state = state();
        state
            .start_attachment(
                "epoch",
                "session",
                "/workspace",
                "load",
                SessionOperationKind::Load,
            )
            .unwrap();
        let started_seq = state.live_snapshot();
        state
            .append_attachment_candidate(
                "epoch",
                "session",
                json!({ "sessionUpdate": "agent_message_chunk", "content": "partial" }),
            )
            .unwrap();
        state
            .append_attachment_candidate(
                "epoch",
                "session",
                json!({ "sessionUpdate": "current_mode_update", "currentModeId": "plan" }),
            )
            .unwrap();

        assert_eq!(state.live_snapshot(), started_seq);
        let attaching = state.session("session").unwrap();
        assert_eq!(attaching.lifecycle, SessionLifecycle::Attaching);
        assert_eq!(state.live("session").unwrap().attachment_candidate.len(), 1);
        assert!(
            !serde_json::to_string(&state.live("session").unwrap().attachment_candidate)
                .unwrap()
                .contains("partial")
        );
        assert!(
            serde_json::to_value(state.live_snapshot())
                .unwrap()
                .to_string()
                .find("partial")
                .is_none()
        );

        state
            .complete_attachment(
                "epoch",
                "session",
                "load",
                SessionOperationKind::Load,
                json!({ "sessionId": "session" }),
            )
            .unwrap();

        assert_ne!(state.live_snapshot(), started_seq);
        let loaded = state.session("session").unwrap();
        assert_eq!(loaded.lifecycle, SessionLifecycle::Active);
        assert!(loaded.operation.is_none());
        assert_eq!(
            loaded.controls.modes.as_ref().unwrap().current_mode_id,
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
            state
                .start_attachment("epoch", session_id, "/workspace", "attach", kind)
                .unwrap();
            state
                .append_attachment_candidate(
                    "epoch",
                    session_id,
                    json!({ "sessionUpdate": "agent_message_chunk", "content": "history" }),
                )
                .unwrap();
            state
                .append_attachment_candidate(
                    "epoch",
                    session_id,
                    json!({ "sessionUpdate": "current_mode_update", "currentModeId": "plan" }),
                )
                .unwrap();
            state
                .complete_attachment(
                    "epoch",
                    session_id,
                    "attach",
                    kind,
                    json!({ "sessionId": session_id }),
                )
                .unwrap();

            let session = state.session(session_id).unwrap();
            assert_eq!(
                session.controls.modes.as_ref().unwrap().current_mode_id,
                "plan"
            );
            assert!(!serde_json::to_string(&session).unwrap().contains("history"));
        }
    }

    #[test]
    fn failed_load_discards_candidate_and_resume_never_claims_replayed_history() {
        let mut state = state();
        state
            .start_attachment(
                "epoch",
                "failed",
                "/workspace",
                "load-failed",
                SessionOperationKind::Load,
            )
            .unwrap();
        state
            .append_attachment_candidate("epoch", "failed", json!({ "content": "must disappear" }))
            .unwrap();
        state
            .fail_attachment(
                "epoch",
                "failed",
                "load-failed",
                SessionOperationKind::Load,
                json!({ "message": "not found" }),
            )
            .unwrap();
        assert!(state.session("failed").is_none());
        assert!(
            !serde_json::to_string(&state.live_snapshot())
                .unwrap()
                .contains("must disappear")
        );

        state
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
                json!({ "content": "compatibility update" }),
            )
            .unwrap();
        state
            .complete_attachment(
                "epoch",
                "resumed",
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
        state
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
                "permission",
                json!({ "question": "allow" }),
            )
            .unwrap();
        state
            .upsert_elicitation(
                "epoch",
                Some("session"),
                "elicitation",
                json!({ "mode": "url" }),
            )
            .unwrap();
        state
            .resolve_elicitation("epoch", Some("session"), "elicitation", Some("url"))
            .unwrap();
        state
            .upsert_terminal("epoch", "session", "terminal", json!({ "released": false }))
            .unwrap();

        state
            .fail_attachment(
                "epoch",
                "session",
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
                    interaction_id: "permission".to_string(),
                },
                RuntimeEffect::AbortUrlFlow {
                    session_id: Some("session".to_string()),
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
        open(&mut state, "session");
        state
            .start_prompt("epoch", "session", "prompt", Vec::new())
            .unwrap();
        state
            .upsert_elicitation(
                "epoch",
                Some("session"),
                "request-id",
                json!({ "mode": "url", "elicitationId": "url-id" }),
            )
            .unwrap();
        state
            .resolve_elicitation("epoch", Some("session"), "request-id", Some("url-id"))
            .unwrap();
        let resolved_seq = state.live_snapshot();
        assert_eq!(
            state
                .resolve_elicitation("epoch", Some("session"), "request-id", Some("url-id"),)
                .unwrap(),
            InteractionResolution::AlreadyResolved,
        );
        assert_eq!(state.live_snapshot(), resolved_seq);
        state
            .complete_prompt(
                "epoch",
                "session",
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
                .settle_url_flow("epoch", Some("session"), "url-id", UrlFlowStatus::Completed,)
                .unwrap(),
            UrlFlowResolution::Applied,
        );
        let completed_seq = state.live_snapshot();
        assert_eq!(
            state
                .settle_url_flow("epoch", Some("session"), "url-id", UrlFlowStatus::Cancelled,)
                .unwrap(),
            UrlFlowResolution::AlreadyTerminal,
        );
        assert_eq!(state.live_snapshot(), completed_seq);
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
        open(&mut state, "session");
        state
            .upsert_elicitation("epoch", None, "request-scope", json!({ "mode": "form" }))
            .unwrap();

        assert!(
            state
                .live_snapshot()
                .request_elicitations
                .contains_key("request-scope")
        );
        state
            .start_operation(
                "epoch",
                "session",
                "close",
                SessionOperationKind::Close,
                "closing",
            )
            .unwrap();
        state.close_session("epoch", "session", "close").unwrap();
        assert!(
            state
                .live_snapshot()
                .request_elicitations
                .contains_key("request-scope")
        );
    }

    #[test]
    fn same_session_reload_success_does_not_create_a_second_business_session() {
        let mut state = state();
        open(&mut state, "session");
        let old_options = json!([{
            "type": "boolean", "id": "old-only", "name": "Old", "currentValue": false,
        }]);
        state
            .update_control_state(
                "epoch",
                "session",
                json!({ "sessionUpdate": "current_mode_update", "currentModeId": "old" }),
            )
            .unwrap();
        state
            .update_control_state(
                "epoch",
                "session",
                json!({ "sessionUpdate": "config_option_update", "configOptions": old_options.clone() }),
            )
            .unwrap();
        let operation_id = "reload-intent".to_string();

        state
            .start_reload(
                "epoch",
                "session",
                &operation_id,
                SessionOperationKind::Load,
            )
            .unwrap();
        state
            .append_attachment_candidate(
                "epoch",
                "session",
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
                json!({ "sessionUpdate": "current_mode_update", "currentModeId": "new" }),
            )
            .unwrap();

        let staging = state.session("session").unwrap();
        assert_eq!(
            staging.controls.modes.as_ref().unwrap().current_mode_id,
            "old"
        );
        assert_eq!(
            Value::from(staging.controls.config_options.clone()),
            old_options
        );
        assert_eq!(state.live("session").unwrap().attachment_candidate.len(), 1);
        let before_complete = state.live_snapshot();

        state
            .complete_reload(
                "epoch",
                "session",
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
        assert!(
            !serde_json::to_string(&reloaded)
                .unwrap()
                .contains("must-not-survive-reload")
        );
        assert_eq!(
            reloaded.controls.modes.as_ref().unwrap().current_mode_id,
            "new"
        );
        assert_eq!(reloaded.controls.config_options, None);
        assert!(reloaded.operation.is_none());
        assert!(
            state
                .live("session")
                .unwrap()
                .attachment_candidate
                .is_empty()
        );
        assert!(!state.operation_in_use(&operation_id));
        assert_ne!(state.live_snapshot(), before_complete);
        assert!(
            !serde_json::to_string(&state.live_snapshot())
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
        open(&mut state, "session");
        for update in &updates {
            state
                .update_control_state("epoch", "session", update.clone())
                .unwrap();
        }
        let controls = &state.session("session").unwrap().controls;
        assert_eq!(controls.info.title.as_deref(), Some("Keep"));
        assert_eq!(controls.info.updated_at.as_deref(), Some("after"));
        let usage = controls.usage.as_ref().unwrap();
        assert_eq!(
            (usage.used, usage.cost.as_ref().unwrap()["amount"].clone()),
            (3, json!(2))
        );
        assert_eq!(&settle_controls(updates, &json!({})), controls);
        state
            .update_control_state(
                "epoch",
                "session",
                json!({ "sessionUpdate": "session_info_update", "title": null }),
            )
            .unwrap();
        let controls = &state.session("session").unwrap().controls;
        assert_eq!(controls.info.title, None);
        assert_eq!(controls.info.updated_at.as_deref(), Some("after"));
    }

    #[test]
    fn same_session_reload_error_remains_actionable_and_does_not_loop() {
        let mut state = state();
        open(&mut state, "session");
        state
            .update_control_state(
                "epoch",
                "session",
                json!({ "sessionUpdate": "current_mode_update", "currentModeId": "old" }),
            )
            .unwrap();
        let old_controls = state.session("session").unwrap().controls.clone();

        state
            .start_reload("epoch", "session", "reload", SessionOperationKind::Load)
            .unwrap();
        state
            .append_attachment_candidate(
                "epoch",
                "session",
                json!({ "sessionUpdate": "current_mode_update", "currentModeId": "partial" }),
            )
            .unwrap();
        state
            .fail_reload(
                "epoch",
                "session",
                "reload",
                json!({ "message": "load rejected" }),
            )
            .unwrap();

        let session = state.session("session").unwrap();
        assert_eq!(session.lifecycle, SessionLifecycle::Active);
        assert_eq!(session.controls, old_controls);
        assert!(session.operation.is_none());
        assert!(
            state
                .live("session")
                .unwrap()
                .attachment_candidate
                .is_empty()
        );
        assert!(!state.operation_in_use("reload"));
        assert!(
            !serde_json::to_string(&state.live_snapshot())
                .unwrap()
                .contains("load rejected")
        );
        state
            .start_prompt("epoch", "session", "next-turn", Vec::new())
            .unwrap();
    }

    #[test]
    fn same_session_reload_rejects_wrong_kind_busy_collision_and_epoch() {
        let mut state = state();
        open(&mut state, "session");
        open(&mut state, "other");
        let unchanged = state.live_snapshot();

        assert_eq!(
            state.start_reload("old-epoch", "session", "reload", SessionOperationKind::Load,),
            Err(RuntimeStateError::EpochMismatch)
        );
        assert_eq!(state.live_snapshot(), unchanged);
        assert_eq!(
            state.start_reload("epoch", "session", "reload", SessionOperationKind::Fork,),
            Err(RuntimeStateError::OperationMismatch)
        );

        state
            .start_prompt("epoch", "other", "collision", Vec::new())
            .unwrap();
        assert_eq!(
            state.start_reload("epoch", "session", "collision", SessionOperationKind::Load,),
            Err(RuntimeStateError::OperationCollision)
        );
        state
            .start_prompt("epoch", "session", "active", Vec::new())
            .unwrap();
        assert_eq!(
            state.start_reload("epoch", "session", "busy", SessionOperationKind::Load,),
            Err(RuntimeStateError::BusySession)
        );
    }

    #[test]
    fn same_session_reload_requires_no_operation_interaction_or_live_resource() {
        fn assert_reload_busy(state: &mut RuntimeState, session_id: &str) {
            assert_eq!(
                state.start_reload(
                    "epoch",
                    session_id,
                    format!("reload-{session_id}"),
                    SessionOperationKind::Load,
                ),
                Err(RuntimeStateError::BusySession)
            );
        }

        let mut state = state();

        open(&mut state, "operation");
        state
            .start_operation(
                "epoch",
                "operation",
                "mode",
                SessionOperationKind::SetMode,
                "setting",
            )
            .unwrap();
        assert_reload_busy(&mut state, "operation");

        open(&mut state, "permission");
        state
            .upsert_permission(
                "epoch",
                "permission",
                "permission",
                json!({ "question": "allow" }),
            )
            .unwrap();
        assert_reload_busy(&mut state, "permission");

        open(&mut state, "elicitation");
        state
            .upsert_elicitation(
                "epoch",
                Some("elicitation"),
                "elicitation",
                json!({ "mode": "form" }),
            )
            .unwrap();
        assert_reload_busy(&mut state, "elicitation");

        open(&mut state, "url");
        state
            .upsert_elicitation("epoch", Some("url"), "url-form", json!({ "mode": "url" }))
            .unwrap();
        state
            .resolve_elicitation("epoch", Some("url"), "url-form", Some("url-flow"))
            .unwrap();
        assert_reload_busy(&mut state, "url");

        open(&mut state, "terminal");
        state
            .upsert_terminal(
                "epoch",
                "terminal",
                "terminal",
                json!({ "released": false }),
            )
            .unwrap();
        assert_reload_busy(&mut state, "terminal");
    }

    #[test]
    fn completed_url_identifiers_can_be_reused_in_session_and_request_scopes() {
        let mut state = state();
        open(&mut state, "session");
        for scope in [Some("session"), None] {
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
            open(&mut state, "session");
            let scope = (!request_scoped).then_some("session");
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
            let seq = state.live_snapshot();
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
            assert_eq!(state.live_snapshot(), seq);
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
        open(&mut state, "session");
        state
            .upsert_elicitation("epoch", Some("session"), "form", json!({ "mode": "url" }))
            .unwrap();
        state
            .resolve_elicitation("epoch", Some("session"), "form", Some("url"))
            .unwrap();

        assert_eq!(
            state
                .settle_url_flow("epoch", Some("session"), "url", UrlFlowStatus::Completed,)
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
        let settled_seq = state.live_snapshot();
        assert_eq!(
            state
                .settle_url_flow("epoch", Some("session"), "url", UrlFlowStatus::Cancelled,)
                .unwrap(),
            UrlFlowResolution::AlreadyTerminal
        );
        assert_eq!(state.live_snapshot(), settled_seq);

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
        let request_settled_seq = state.live_snapshot();
        assert_eq!(
            state
                .settle_url_flow("epoch", None, "request-url", UrlFlowStatus::Completed)
                .unwrap(),
            UrlFlowResolution::AlreadyTerminal
        );
        assert_eq!(state.live_snapshot(), request_settled_seq);
    }

    #[test]
    fn released_terminal_is_not_retained_or_resurrected() {
        let mut state = state();
        open(&mut state, "session");
        state
            .upsert_terminal(
                "epoch",
                "session",
                "terminal",
                json!({ "output": "running", "released": false }),
            )
            .unwrap();
        state
            .upsert_terminal(
                "epoch",
                "session",
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
            !serde_json::to_string(&state.live_snapshot())
                .unwrap()
                .contains("released-output")
        );
        let released_seq = state.live_snapshot();
        assert_eq!(
            state
                .upsert_terminal(
                    "epoch",
                    "session",
                    "terminal",
                    json!({ "output": "late", "released": false }),
                )
                .unwrap(),
            TerminalUpsert::Stale
        );
        assert_eq!(state.live_snapshot(), released_seq);
    }

    #[test]
    fn terminal_output_reassembles_split_utf8_before_exit() {
        let mut state = state();
        open(&mut state, "session");
        let bytes = "中😀tail".as_bytes();
        for (index, byte) in bytes.iter().enumerate() {
            state
                .upsert_terminal(
                    "epoch",
                    "session",
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

        state
            .upsert_terminal(
                "epoch",
                "session",
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
        open(&mut state, "session");
        state
            .upsert_terminal(
                "epoch",
                "session",
                "terminal",
                json!({ "output": "running", "released": false }),
            )
            .unwrap();
        state
            .synchronize_loaded_session(
                "epoch",
                "session",
                json!({ "modes": { "currentModeId": "plan" } }),
                vec![json!({
                    "sessionUpdate": "available_commands_update",
                    "availableCommands": [{ "name": "inspect" }],
                })],
            )
            .unwrap();

        let session = state.session("session").unwrap();
        assert_eq!(
            session.controls.commands,
            vec![json!({ "name": "inspect" })]
        );
        assert_eq!(
            session.controls.modes.as_ref().unwrap().current_mode_id,
            "plan"
        );
        assert!(session.terminals.contains_key("terminal"));
    }
}
