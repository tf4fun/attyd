use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Debug, Clone, Copy)]
pub(crate) struct RuntimeLimits {
    pub max_delta_events: usize,
    pub max_delta_bytes: usize,
    pub max_intent_records: usize,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            max_delta_events: 4_096,
            max_delta_bytes: 16 * 1024 * 1024,
            max_intent_records: 4_096,
        }
    }
}

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
    #[serde(skip)]
    resolved_permissions: VecDeque<String>,
    #[serde(skip)]
    resolved_elicitations: VecDeque<String>,
    #[serde(skip)]
    resolved_url_flows: VecDeque<String>,
    #[serde(skip)]
    released_terminals: VecDeque<String>,
    #[serde(skip)]
    attachment_candidate: Vec<Value>,
    #[serde(skip)]
    attachment_candidate_bytes: usize,
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
    pub prompt: Vec<Value>,
    pub updates: Vec<Value>,
    pub cancel_requested: bool,
}

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
    TurnUpdateAppended {
        session_id: String,
        incarnation: u64,
        revision: u64,
        operation_id: String,
        update: Value,
    },
    SessionRemoved {
        session_id: String,
        incarnation: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ClientIntentKey {
    client_intent_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IntentStatus {
    Accepted,
    InFlight,
    Uncertain,
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
    },
    ReleaseTerminal {
        session_id: String,
        terminal_id: String,
    },
}

#[derive(Debug, Clone)]
struct IntentRecord {
    payload_digest: [u8; 32],
    operation_id: String,
    status: IntentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IntentAck {
    Accepted {
        operation_id: String,
    },
    Duplicate {
        operation_id: String,
        status: IntentStatus,
    },
    Collision,
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
    ResourceLimit,
}

pub(crate) struct RuntimeState {
    epoch: String,
    seq: u64,
    connection_revision: u64,
    next_incarnation: u64,
    next_operation_id: u64,
    sessions: BTreeMap<String, SessionRuntime>,
    request_elicitations: BTreeMap<String, PendingInteraction>,
    request_url_flows: BTreeMap<String, UrlFlow>,
    resolved_request_elicitations: VecDeque<String>,
    resolved_request_url_flows: VecDeque<String>,
    intents: BTreeMap<ClientIntentKey, IntentRecord>,
    effects: VecDeque<RuntimeEffect>,
    delta_journal: VecDeque<RuntimeDelta>,
    delta_bytes: usize,
    limits: RuntimeLimits,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self::new(Uuid::new_v4().to_string(), RuntimeLimits::default())
    }
}

impl RuntimeState {
    pub(crate) fn new(epoch: impl Into<String>, limits: RuntimeLimits) -> Self {
        Self {
            epoch: epoch.into(),
            seq: 0,
            connection_revision: 0,
            next_incarnation: 0,
            next_operation_id: 0,
            sessions: BTreeMap::new(),
            request_elicitations: BTreeMap::new(),
            request_url_flows: BTreeMap::new(),
            resolved_request_elicitations: VecDeque::new(),
            resolved_request_url_flows: VecDeque::new(),
            intents: BTreeMap::new(),
            effects: VecDeque::new(),
            delta_journal: VecDeque::new(),
            delta_bytes: 0,
            limits,
        }
    }

    pub(crate) fn epoch(&self) -> &str {
        &self.epoch
    }

    pub(crate) fn snapshot(&self) -> RuntimeSnapshot {
        RuntimeSnapshot {
            epoch: self.epoch.clone(),
            through_seq: self.seq,
            connection_revision: self.connection_revision,
            sessions: self.sessions.clone(),
            request_elicitations: self.request_elicitations.clone(),
            request_url_flows: self.request_url_flows.clone(),
        }
    }

    pub(crate) fn deltas_after(&self, seq: u64) -> Option<Vec<RuntimeDelta>> {
        if seq > self.seq {
            return None;
        }
        let first = self
            .delta_journal
            .front()
            .map_or(self.seq + 1, |delta| delta.seq);
        if seq.saturating_add(1) < first {
            return None;
        }
        Some(
            self.delta_journal
                .iter()
                .filter(|delta| delta.seq > seq)
                .cloned()
                .collect(),
        )
    }

    pub(crate) fn session(&self, session_id: &str) -> Option<&SessionRuntime> {
        self.sessions.get(session_id)
    }

    pub(crate) fn intent_status(&self, operation_id: &str) -> Option<IntentStatus> {
        self.intents
            .values()
            .find(|intent| intent.operation_id == operation_id)
            .map(|intent| intent.status.clone())
    }

    pub(crate) fn take_effects(&mut self) -> Vec<RuntimeEffect> {
        self.effects.drain(..).collect()
    }

    pub(crate) fn accept_intent(
        &mut self,
        expected_epoch: &str,
        _subscriber_id: u64,
        client_intent_id: impl Into<String>,
        payload: impl AsRef<str>,
    ) -> Result<IntentAck, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let key = ClientIntentKey {
            client_intent_id: client_intent_id.into(),
        };
        let payload_digest: [u8; 32] = Sha256::digest(payload.as_ref().as_bytes()).into();
        if let Some(existing) = self.intents.get(&key) {
            return Ok(if existing.payload_digest == payload_digest {
                IntentAck::Duplicate {
                    operation_id: existing.operation_id.clone(),
                    status: existing.status.clone(),
                }
            } else {
                IntentAck::Collision
            });
        }
        if !self.reserve_intent_record_capacity() {
            return Err(RuntimeStateError::ResourceLimit);
        }
        self.next_operation_id = self.next_operation_id.wrapping_add(1).max(1);
        let operation_id = format!("{}:{}", self.epoch, self.next_operation_id);
        self.intents.insert(
            key,
            IntentRecord {
                payload_digest,
                operation_id: operation_id.clone(),
                status: IntentStatus::Accepted,
            },
        );
        Ok(IntentAck::Accepted { operation_id })
    }

    pub(crate) fn reject_intent(
        &mut self,
        expected_epoch: &str,
        operation_id: &str,
        _session_id: Option<String>,
        _reason: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let Some(intent) = self
            .intents
            .values_mut()
            .find(|intent| intent.operation_id == operation_id)
        else {
            return Err(RuntimeStateError::OperationMismatch);
        };
        if intent.status != IntentStatus::Accepted {
            return Err(RuntimeStateError::OperationMismatch);
        }
        self.retire_intent(operation_id);
        Ok(())
    }

    pub(crate) fn open_new_with_replay(
        &mut self,
        expected_epoch: &str,
        session_id: impl Into<String>,
        cwd: impl Into<String>,
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
        cwd: impl Into<String>,
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
        if let Some(existing) = self.sessions.get(&session_id) {
            if existing.lifecycle != SessionLifecycle::Closed {
                return Err(RuntimeStateError::BusySession);
            }
            self.sessions.remove(&session_id);
        }
        self.next_incarnation = self.next_incarnation.wrapping_add(1).max(1);
        let incarnation = self.next_incarnation;
        self.sessions.insert(
            session_id.clone(),
            SessionRuntime {
                session_id: session_id.clone(),
                incarnation,
                revision: 0,
                cwd: cwd.into(),
                session: Value::Null,
                control_state: BTreeMap::new(),
                lifecycle: SessionLifecycle::Attaching,
                active_turn: None,
                operation: Some(SessionOperationState {
                    operation_id: operation_id.clone(),
                    kind,
                    stage: "attaching".to_string(),
                    uncertainty_reason: None,
                }),
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
            },
        );
        self.set_intent_status(&operation_id, IntentStatus::InFlight);
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
        if self.operation_in_use(&operation_id) {
            return Err(RuntimeStateError::OperationCollision);
        }
        let session = self.require_session_mut(session_id, incarnation)?;
        if session.lifecycle != SessionLifecycle::Active {
            return Err(RuntimeStateError::SessionNotActive);
        }
        if session.active_turn.is_some()
            || session.operation.is_some()
            || !session.permissions.is_empty()
            || !session.elicitations.is_empty()
            || !session.url_flows.is_empty()
            || !session.terminals.is_empty()
            || !session.attachment_candidate.is_empty()
        {
            return Err(RuntimeStateError::BusySession);
        }
        session.operation = Some(SessionOperationState {
            operation_id: operation_id.clone(),
            kind,
            stage: "reloading".to_string(),
            uncertainty_reason: None,
        });
        session.attachment_candidate_bytes = 0;
        self.set_intent_status(&operation_id, IntentStatus::InFlight);
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
        let session = self.require_session_mut(session_id, incarnation)?;
        let collecting_attachment = session.lifecycle == SessionLifecycle::Attaching
            && session.operation.as_ref().is_some_and(|operation| {
                operation.stage == "attaching"
                    && matches!(
                        operation.kind,
                        SessionOperationKind::Load | SessionOperationKind::Resume
                    )
            });
        let collecting_reload = session.lifecycle == SessionLifecycle::Active
            && session.operation.as_ref().is_some_and(|operation| {
                operation.stage == "reloading" && operation.kind == SessionOperationKind::Load
            });
        if !collecting_attachment && !collecting_reload {
            return Err(RuntimeStateError::OperationMismatch);
        }
        let Some(update) = retain_control_update(update) else {
            return Ok(());
        };
        let bytes = serialized_len(&update);
        if session.attachment_candidate.len() >= 10_000
            || session.attachment_candidate_bytes.saturating_add(bytes) > 1_000_000
        {
            return Err(RuntimeStateError::ResourceLimit);
        }
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
        let session = self.require_session_mut(session_id, incarnation)?;
        if session.lifecycle != SessionLifecycle::Active
            || session.operation.as_ref().is_none_or(|operation| {
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
        session.operation = None;
        self.retire_intent(operation_id);
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
        let session = self.require_session_mut(session_id, incarnation)?;
        if session.lifecycle != SessionLifecycle::Active
            || session.operation.as_ref().is_none_or(|operation| {
                operation.operation_id != operation_id
                    || operation.kind != SessionOperationKind::Load
                    || operation.stage != "reloading"
            })
        {
            return Err(RuntimeStateError::OperationMismatch);
        }
        session.attachment_candidate.clear();
        session.attachment_candidate_bytes = 0;
        session.operation = None;
        self.retire_intent(operation_id);
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
        let session = self.require_session_mut(session_id, incarnation)?;
        if session.lifecycle != SessionLifecycle::Attaching
            || session.operation.as_ref().is_none_or(|operation| {
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
        session.operation = None;
        self.retire_intent(operation_id);
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
            let session = self.require_session(session_id, incarnation)?;
            if session.lifecycle != SessionLifecycle::Attaching
                || session.operation.as_ref().is_none_or(|operation| {
                    operation.operation_id != operation_id || operation.kind != kind
                })
            {
                return Err(RuntimeStateError::OperationMismatch);
            }
        }
        let mut session = self
            .sessions
            .remove(session_id)
            .ok_or(RuntimeStateError::UnknownSession)?;
        let (queued, effects) = drain_session_liveness(session_id, &mut session);
        self.retire_intents(queued);
        self.effects.extend(effects);
        self.retire_intent(operation_id);
        self.commit_removal(session_id, incarnation);
        Ok(())
    }

    pub(crate) fn open_forked(
        &mut self,
        expected_epoch: &str,
        session_id: impl Into<String>,
        cwd: impl Into<String>,
        session: Value,
        source: (&str, u64),
        target_replay: Option<Vec<Value>>,
    ) -> Result<u64, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let (source_session_id, source_incarnation) = source;
        {
            let source = self.require_session(source_session_id, source_incarnation)?;
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
        cwd: String,
        session: Value,
        control_state: BTreeMap<String, Value>,
    ) -> Result<u64, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        if let Some(existing) = self.sessions.get(&session_id) {
            if existing.lifecycle != SessionLifecycle::Closed {
                return Err(RuntimeStateError::BusySession);
            }
            self.sessions.remove(&session_id);
        }
        self.next_incarnation = self.next_incarnation.wrapping_add(1).max(1);
        let incarnation = self.next_incarnation;
        let session = retain_session_metadata(&session_id, session);
        self.sessions.insert(
            session_id.clone(),
            SessionRuntime {
                session_id: session_id.clone(),
                incarnation,
                revision: 0,
                cwd,
                session,
                control_state,
                lifecycle: SessionLifecycle::Active,
                active_turn: None,
                operation: None,
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
            },
        );
        self.commit_session(&session_id);
        Ok(incarnation)
    }

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
        let turn_id = operation_id.clone();
        let active_turn = ActiveTurn {
            operation_id: operation_id.clone(),
            turn_id: turn_id.clone(),
            prompt,
            updates: Vec::new(),
            cancel_requested: false,
        };
        let session = self.require_session_mut(session_id, incarnation)?;
        if session.lifecycle != SessionLifecycle::Active {
            return Err(RuntimeStateError::SessionNotActive);
        }
        if session.active_turn.is_some() || session.operation.is_some() {
            return Err(RuntimeStateError::BusySession);
        }
        session.active_turn = Some(active_turn);
        self.set_intent_status(&operation_id, IntentStatus::InFlight);
        self.commit_session(session_id);
        Ok(turn_id)
    }

    pub(crate) fn append_turn_update(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        update: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let session = self.require_session_mut(session_id, incarnation)?;
        let turn = session
            .active_turn
            .as_mut()
            .ok_or(RuntimeStateError::NoActiveTurn)?;
        let folded_updates = fold_active_turn_update(&turn.updates, &update)?;
        let operation_id = turn.operation_id.clone();
        turn.updates = folded_updates;
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
        let session = self.require_session_mut(session_id, incarnation)?;
        if !matches!(
            session.lifecycle,
            SessionLifecycle::Active
                | SessionLifecycle::Closing
                | SessionLifecycle::ClosingForDelete
        ) {
            return Err(RuntimeStateError::SessionNotActive);
        }
        let key = key.into();
        if session.control_state.get(&key) == Some(&update) {
            return Ok(());
        }
        session.control_state.insert(key, update);
        self.commit_session(session_id);
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
        let session = self.require_session_mut(session_id, incarnation)?;
        if session.lifecycle != SessionLifecycle::Active || session.active_turn.is_some() {
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
        let session = self.require_session_mut(session_id, incarnation)?;
        let turn = session
            .active_turn
            .as_mut()
            .ok_or(RuntimeStateError::NoActiveTurn)?;
        if turn.cancel_requested {
            return Ok(());
        }
        turn.cancel_requested = true;
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
        let TurnTerminal { lifecycle } = terminal;
        self.require_epoch(expected_epoch)?;
        let session = self.require_session_mut(session_id, incarnation)?;
        let active = session
            .active_turn
            .take()
            .ok_or(RuntimeStateError::NoActiveTurn)?;
        if active.operation_id != operation_id {
            session.active_turn = Some(active);
            return Err(RuntimeStateError::OperationMismatch);
        }
        drop(active);
        if let Some(lifecycle) = lifecycle {
            session.lifecycle = lifecycle;
        }
        let resolved_permissions = session
            .permissions
            .iter()
            .filter(|(_, interaction)| interaction.operation_id.as_deref() == Some(operation_id))
            .map(|(interaction_id, _)| interaction_id.clone())
            .collect::<Vec<_>>();
        let mut responding_operation_ids = resolved_permissions
            .iter()
            .filter_map(|interaction_id| {
                session
                    .permissions
                    .get(interaction_id)
                    .and_then(|interaction| interaction.responding_operation_id.clone())
            })
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
        responding_operation_ids.extend(resolved_elicitations.iter().filter_map(
            |interaction_id| {
                session
                    .elicitations
                    .get(interaction_id)
                    .and_then(|interaction| interaction.responding_operation_id.clone())
            },
        ));
        for interaction_id in &resolved_elicitations {
            session.elicitations.remove(interaction_id);
            remember_resolved_interaction(&mut session.resolved_elicitations, interaction_id);
        }
        self.effects
            .extend(resolved_permissions.into_iter().map(|interaction_id| {
                RuntimeEffect::CancelPermissionResponder {
                    session_id: session_id.to_string(),
                    interaction_id,
                }
            }));
        self.effects
            .extend(resolved_elicitations.into_iter().map(|interaction_id| {
                RuntimeEffect::CancelElicitationResponder {
                    session_id: Some(session_id.to_string()),
                    interaction_id,
                }
            }));
        self.retire_intents(responding_operation_ids);
        self.retire_intent(operation_id);
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
        let interaction_id = interaction_id.into();
        let session = self.require_session_mut(session_id, incarnation)?;
        if session
            .resolved_permissions
            .iter()
            .any(|resolved| resolved == &interaction_id)
        {
            return Ok(InteractionUpsert::AlreadyResolved);
        }
        let active_operation = session
            .active_turn
            .as_ref()
            .map(|turn| turn.operation_id.clone());
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
        self.commit_session(session_id);
        Ok(InteractionUpsert::Inserted)
    }

    pub(crate) fn resolve_permission(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        interaction_id: &str,
    ) -> Result<InteractionResolution, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let session = self.require_session_mut(session_id, incarnation)?;
        if session
            .resolved_permissions
            .iter()
            .any(|resolved| resolved == interaction_id)
        {
            return Ok(InteractionResolution::AlreadyResolved);
        }
        let pending = session
            .permissions
            .remove(interaction_id)
            .ok_or(RuntimeStateError::UnknownInteraction)?;
        let responding_operation_id = pending.responding_operation_id;
        remember_resolved_permission(session, interaction_id);
        self.commit_session(session_id);
        if let Some(operation_id) = responding_operation_id {
            self.retire_intent(&operation_id);
        }
        Ok(InteractionResolution::Applied)
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
        {
            let session = self.require_session(session_id, incarnation)?;
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
        }
        if self.operation_in_use(&operation_id) {
            return Err(RuntimeStateError::OperationCollision);
        }
        self.set_intent_status(&operation_id, IntentStatus::InFlight);
        let session = self.require_session_mut(session_id, incarnation)?;
        let pending = session
            .permissions
            .get_mut(interaction_id)
            .ok_or(RuntimeStateError::UnknownInteraction)?;
        pending.responding_operation_id = Some(operation_id);
        self.commit_session(session_id);
        Ok(InteractionResponseStart::Applied)
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
        let session = self.require_session_mut(session_id, incarnation)?;
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
        self.retire_intent(operation_id);
        self.commit_session(session_id);
        Ok(InteractionResolution::Applied)
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
                let session = self.require_session_mut(session_id, incarnation)?;
                if session
                    .resolved_elicitations
                    .iter()
                    .any(|resolved| resolved == &interaction_id)
                {
                    return Ok(InteractionUpsert::AlreadyResolved);
                }
                let active_operation = session
                    .active_turn
                    .as_ref()
                    .map(|turn| turn.operation_id.clone());
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
                    .require_session(session_id, incarnation)?
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
                let session = self.require_session_mut(session_id, incarnation)?;
                let pending = session
                    .elicitations
                    .remove(interaction_id)
                    .ok_or(RuntimeStateError::UnknownInteraction)?;
                let PendingInteraction {
                    request,
                    responding_operation_id,
                    ..
                } = pending;
                remember_resolved_interaction(&mut session.resolved_elicitations, interaction_id);
                if let Some(elicitation_id) = accepted_url_id {
                    session.url_flows.insert(
                        elicitation_id.to_string(),
                        UrlFlow {
                            elicitation_id: elicitation_id.to_string(),
                            request,
                            status: UrlFlowStatus::Waiting,
                        },
                    );
                }
                self.commit_session(session_id);
                if let Some(operation_id) = responding_operation_id {
                    self.retire_intent(&operation_id);
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
                let PendingInteraction {
                    request,
                    responding_operation_id,
                    ..
                } = pending;
                remember_resolved_interaction(
                    &mut self.resolved_request_elicitations,
                    interaction_id,
                );
                if let Some(elicitation_id) = accepted_url_id {
                    self.request_url_flows.insert(
                        elicitation_id.to_string(),
                        UrlFlow {
                            elicitation_id: elicitation_id.to_string(),
                            request,
                            status: UrlFlowStatus::Waiting,
                        },
                    );
                }
                self.commit_connection();
                if let Some(operation_id) = responding_operation_id {
                    self.retire_intent(&operation_id);
                }
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
                .require_session(session_id, incarnation)?
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
        self.set_intent_status(&operation_id, IntentStatus::InFlight);
        match scope {
            Some((session_id, incarnation)) => {
                self.require_session_mut(session_id, incarnation)?
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
                .require_session(session_id, incarnation)?
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
                .require_session(session_id, incarnation)?
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

    pub(crate) fn settle_url_flow(
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
                let session = self.require_session_mut(session_id, incarnation)?;
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
        let session = self.require_session_mut(session_id, incarnation)?;
        let terminal_id = terminal_id.into();
        if session
            .released_terminals
            .iter()
            .any(|released| released == &terminal_id)
        {
            return Ok(TerminalUpsert::Stale);
        }
        let outcome = match session.terminals.get(&terminal_id) {
            Some(existing) if existing == &terminal => return Ok(TerminalUpsert::Duplicate),
            Some(_) => TerminalUpsert::Updated,
            None => TerminalUpsert::Inserted,
        };
        if terminal_released(&terminal) {
            session.terminals.remove(&terminal_id);
            remember_resolved_interaction(&mut session.released_terminals, &terminal_id);
        } else {
            session.terminals.insert(terminal_id, terminal);
        }
        self.commit_session(session_id);
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
        if self.operation_in_use(&operation_id) {
            return Err(RuntimeStateError::OperationCollision);
        }
        let session = self.require_session_mut(session_id, incarnation)?;
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
        if session.active_turn.is_some() || session.operation.is_some() {
            return Err(RuntimeStateError::BusySession);
        }
        session.operation = Some(SessionOperationState {
            operation_id: operation_id.clone(),
            kind,
            stage: stage.into(),
            uncertainty_reason: None,
        });
        match kind {
            SessionOperationKind::Close => session.lifecycle = SessionLifecycle::Closing,
            SessionOperationKind::Delete if session.lifecycle == SessionLifecycle::Closed => {
                session.lifecycle = SessionLifecycle::Deleting;
            }
            SessionOperationKind::Delete
                if session
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
        self.set_intent_status(&operation_id, IntentStatus::InFlight);
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
        let stage = match self.require_session(session_id, incarnation)?.lifecycle {
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
        if self.require_session(session_id, incarnation)?.lifecycle != SessionLifecycle::Active {
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
        let session = self.require_session_mut(session_id, incarnation)?;
        let Some(operation) = session.operation.as_mut() else {
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
        let (queued, effects) = drain_session_liveness(session_id, session);
        self.retire_intents(queued);
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
            let session = self.require_session(session_id, incarnation)?;
            if session.lifecycle != SessionLifecycle::Deleting
                || session.operation.as_ref().is_none_or(|operation| {
                    operation.operation_id != operation_id
                        || operation.kind != SessionOperationKind::Delete
                        || !matches!(operation.stage.as_str(), "deleting" | "deleting_active")
                })
            {
                return Err(RuntimeStateError::OperationMismatch);
            }
        }
        let mut session = self
            .sessions
            .remove(session_id)
            .ok_or(RuntimeStateError::UnknownSession)?;
        let (queued, effects) = drain_session_liveness(session_id, &mut session);
        self.retire_intents(queued);
        self.effects.extend(effects);
        self.retire_intent(operation_id);
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
        let (queued, effects) = {
            let session = self.require_session_mut(session_id, incarnation)?;
            let Some(operation) = session.operation.as_mut() else {
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
        self.retire_intents(queued);
        self.effects.extend(effects);
        self.set_intent_status(operation_id, IntentStatus::Uncertain);
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
        let session = self.require_session_mut(session_id, incarnation)?;
        if matches!(
            kind,
            SessionOperationKind::Close | SessionOperationKind::Delete
        ) || session.operation.as_ref().is_none_or(|operation| {
            operation.operation_id != operation_id || operation.kind != kind
        }) {
            return Err(RuntimeStateError::OperationMismatch);
        }
        session.operation = None;
        self.retire_intent(operation_id);
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
        let session = self.require_session_mut(session_id, incarnation)?;
        if session.operation.as_ref().is_none_or(|operation| {
            operation.operation_id != operation_id || operation.kind != kind
        }) {
            return Err(RuntimeStateError::OperationMismatch);
        }
        session
            .control_state
            .insert(control_key.into(), control_update);
        session.operation = None;
        self.retire_intent(operation_id);
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
        let session = self.require_session_mut(session_id, incarnation)?;
        let Some(operation) = session.operation.as_ref() else {
            return Err(RuntimeStateError::OperationMismatch);
        };
        if operation.operation_id != operation_id || operation.kind != kind {
            return Err(RuntimeStateError::OperationMismatch);
        }
        let stage = operation.stage.clone();
        session.operation = None;
        session.lifecycle = match kind {
            SessionOperationKind::Close => SessionLifecycle::Active,
            SessionOperationKind::Delete if stage == "deleting" => SessionLifecycle::Closed,
            SessionOperationKind::Delete => SessionLifecycle::Active,
            _ => session.lifecycle.clone(),
        };
        self.retire_intent(operation_id);
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
        {
            let session = self.require_session(session_id, incarnation)?;
            if session.operation.as_ref().is_none_or(|operation| {
                operation.operation_id != operation_id
                    || operation.kind != SessionOperationKind::Close
            }) {
                return Err(RuntimeStateError::OperationMismatch);
            }
        }
        let mut session = self
            .sessions
            .remove(session_id)
            .ok_or(RuntimeStateError::UnknownSession)?;
        let (queued, effects) = drain_session_liveness(session_id, &mut session);
        self.retire_intents(queued);
        self.effects.extend(effects);
        self.retire_intent(operation_id);
        self.commit_removal(session_id, session.incarnation);
        Ok(())
    }

    fn require_epoch(&self, expected_epoch: &str) -> Result<(), RuntimeStateError> {
        if expected_epoch == self.epoch {
            Ok(())
        } else {
            Err(RuntimeStateError::EpochMismatch)
        }
    }

    fn require_session_mut(
        &mut self,
        session_id: &str,
        incarnation: u64,
    ) -> Result<&mut SessionRuntime, RuntimeStateError> {
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or(RuntimeStateError::UnknownSession)?;
        if session.incarnation != incarnation {
            return Err(RuntimeStateError::StaleIncarnation);
        }
        Ok(session)
    }

    fn require_session(
        &self,
        session_id: &str,
        incarnation: u64,
    ) -> Result<&SessionRuntime, RuntimeStateError> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or(RuntimeStateError::UnknownSession)?;
        if session.incarnation != incarnation {
            return Err(RuntimeStateError::StaleIncarnation);
        }
        Ok(session)
    }

    fn operation_in_use(&self, operation_id: &str) -> bool {
        self.intents.values().any(|intent| {
            intent.operation_id == operation_id && intent.status != IntentStatus::Accepted
        }) || self.sessions.values().any(|session| {
            session
                .active_turn
                .as_ref()
                .is_some_and(|turn| turn.operation_id == operation_id)
                || session
                    .operation
                    .as_ref()
                    .is_some_and(|operation| operation.operation_id == operation_id)
                || session.permissions.values().any(|interaction| {
                    interaction.responding_operation_id.as_deref() == Some(operation_id)
                })
                || session.elicitations.values().any(|interaction| {
                    interaction.responding_operation_id.as_deref() == Some(operation_id)
                })
        }) || self
            .request_elicitations
            .values()
            .any(|interaction| interaction.responding_operation_id.as_deref() == Some(operation_id))
    }

    fn reserve_intent_record_capacity(&mut self) -> bool {
        let limit = self.limits.max_intent_records;
        limit > 0 && self.intents.len() < limit
    }

    fn url_flow_in_use(&self, elicitation_id: &str) -> bool {
        self.request_url_flows.contains_key(elicitation_id)
            || self
                .resolved_request_url_flows
                .iter()
                .any(|resolved| resolved == elicitation_id)
            || self.sessions.values().any(|session| {
                session.url_flows.contains_key(elicitation_id)
                    || session
                        .resolved_url_flows
                        .iter()
                        .any(|resolved| resolved == elicitation_id)
            })
    }

    fn set_intent_status(&mut self, operation_id: &str, status: IntentStatus) {
        if let Some(intent) = self
            .intents
            .values_mut()
            .find(|intent| intent.operation_id == operation_id)
        {
            intent.status = status;
        }
    }

    fn retire_intent(&mut self, operation_id: &str) {
        let key = self
            .intents
            .iter()
            .find_map(|(key, intent)| (intent.operation_id == operation_id).then(|| key.clone()));
        if let Some(key) = key {
            self.intents.remove(&key);
        }
    }

    fn drop_turn_delivery_payload(
        &mut self,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
    ) {
        // A journal suffix must remain sequence-contiguous. Removing only this session's entries
        // would leave holes when sessions interleave, so invalidate the whole prefix through the
        // last delta that could own this turn's payload.
        let discard_through =
            self.delta_journal
                .iter()
                .filter(|delta| match &delta.change {
                    RuntimeChange::SessionUpsert { session } => {
                        session.session_id == session_id && session.incarnation == incarnation
                    }
                    RuntimeChange::TurnUpdateAppended {
                        session_id: delta_session_id,
                        incarnation: delta_incarnation,
                        operation_id: delta_operation_id,
                        ..
                    } => {
                        (delta_session_id == session_id && *delta_incarnation == incarnation)
                            || delta_operation_id == operation_id
                    }
                    RuntimeChange::ConnectionUpsert { .. }
                    | RuntimeChange::SessionRemoved { .. } => false,
                })
                .map(|delta| delta.seq)
                .max();
        if let Some(discard_through) = discard_through {
            while self
                .delta_journal
                .front()
                .is_some_and(|delta| delta.seq <= discard_through)
            {
                self.delta_journal.pop_front();
            }
        }
        self.delta_bytes = self.delta_journal.iter().map(serialized_len).sum();
    }

    fn retire_intents(&mut self, operation_ids: Vec<String>) {
        for operation_id in operation_ids {
            self.retire_intent(&operation_id);
        }
    }

    fn commit_session(&mut self, session_id: &str) {
        let Some(session) = self.sessions.get_mut(session_id) else {
            return;
        };
        session.revision = session.revision.wrapping_add(1).max(1);
        let revision = session.revision;
        let session = session.clone();
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

    fn commit_turn_update(
        &mut self,
        session_id: &str,
        incarnation: u64,
        operation_id: String,
        update: Value,
    ) {
        let Some(session) = self.sessions.get_mut(session_id) else {
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
        self.seq = self.seq.wrapping_add(1).max(1);
        let delta = RuntimeDelta {
            epoch: self.epoch.clone(),
            seq: self.seq,
            scope_revision,
            change,
        };
        self.delta_bytes = self.delta_bytes.saturating_add(serialized_len(&delta));
        self.delta_journal.push_back(delta);
        while self.delta_journal.len() > self.limits.max_delta_events
            || self.delta_bytes > self.limits.max_delta_bytes
        {
            let Some(removed) = self.delta_journal.pop_front() else {
                break;
            };
            self.delta_bytes = self.delta_bytes.saturating_sub(serialized_len(&removed));
        }
    }
}

fn extract_control_state(entries: Vec<Value>) -> BTreeMap<String, Value> {
    let mut controls = BTreeMap::new();
    for entry in entries {
        if let Some(update) = retain_control_update(entry) {
            let kind = update["sessionUpdate"]
                .as_str()
                .expect("retained control update has a kind")
                .to_string();
            controls.insert(kind, update);
        }
    }
    controls
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

fn remember_resolved_permission(session: &mut SessionRuntime, interaction_id: &str) {
    remember_resolved_interaction(&mut session.resolved_permissions, interaction_id);
}

fn remember_resolved_interaction(resolved: &mut VecDeque<String>, interaction_id: &str) {
    resolved.push_back(interaction_id.to_string());
    while resolved.len() > 1_024 {
        resolved.pop_front();
    }
}

fn drain_session_liveness(
    session_id: &str,
    session: &mut SessionRuntime,
) -> (Vec<String>, Vec<RuntimeEffect>) {
    let mut operation_ids = Vec::new();
    let permissions = std::mem::take(&mut session.permissions);
    operation_ids.extend(
        permissions
            .values()
            .filter_map(|pending| pending.responding_operation_id.clone()),
    );
    let mut effects = permissions
        .into_keys()
        .map(|interaction_id| RuntimeEffect::CancelPermissionResponder {
            session_id: session_id.to_string(),
            interaction_id,
        })
        .collect::<Vec<_>>();
    let elicitations = std::mem::take(&mut session.elicitations);
    operation_ids.extend(
        elicitations
            .values()
            .filter_map(|pending| pending.responding_operation_id.clone()),
    );
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
            .map(|(elicitation_id, _)| RuntimeEffect::AbortUrlFlow {
                session_id: Some(session_id.to_string()),
                elicitation_id,
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
    (operation_ids, effects)
}

fn serialized_len(value: &impl Serialize) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

pub(crate) fn fold_active_turn_update(
    retained: &[Value],
    update: &Value,
) -> Result<Vec<Value>, RuntimeStateError> {
    let mut folded = retained.to_vec();
    let Some(kind) = update.get("sessionUpdate").and_then(Value::as_str) else {
        folded.push(update.clone());
        return Ok(folded);
    };

    match kind {
        "user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk" => {
            let Some((shape, text)) = text_chunk_shape(update)? else {
                folded.push(update.clone());
                return Ok(folded);
            };
            if let Some(previous) = folded.last_mut() {
                if text_chunk_shape(previous)?
                    .as_ref()
                    .is_some_and(|(previous_shape, _)| previous_shape == &shape)
                {
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
            folded.push(update.clone());
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
                deep_merge_json_object(&mut folded[position], update)?;
                folded[position]["sessionUpdate"] = if kind == "tool_call" {
                    Value::String("tool_call".to_string())
                } else {
                    retained_kind
                };
                folded[position]["toolCallId"] = Value::String(tool_call_id.to_string());
            } else {
                folded.push(update.clone());
            }
        }
        "plan" => {
            if !update.get("entries").is_some_and(Value::is_array) {
                return Err(RuntimeStateError::OperationMismatch);
            }
            replace_fold_slot(
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
            replace_fold_slot(
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
        _ => folded.push(update.clone()),
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

fn replace_fold_slot(
    folded: &mut Vec<Value>,
    matches_slot: impl Fn(&Value) -> bool,
    update: &Value,
) {
    if let Some(position) = folded.iter().position(matches_slot) {
        folded[position] = update.clone();
    } else {
        folded.push(update.clone());
    }
}

fn deep_merge_json_object(target: &mut Value, patch: &Value) -> Result<(), RuntimeStateError> {
    let target = target
        .as_object_mut()
        .ok_or(RuntimeStateError::OperationMismatch)?;
    let patch = patch
        .as_object()
        .ok_or(RuntimeStateError::OperationMismatch)?;
    for (key, patch_value) in patch {
        match (target.get_mut(key), patch_value) {
            (Some(Value::Object(target_object)), Value::Object(patch_object)) => {
                deep_merge_json_maps(target_object, patch_object);
            }
            _ => {
                target.insert(key.clone(), patch_value.clone());
            }
        }
    }
    Ok(())
}

fn deep_merge_json_maps(
    target: &mut serde_json::Map<String, Value>,
    patch: &serde_json::Map<String, Value>,
) {
    for (key, patch_value) in patch {
        match (target.get_mut(key), patch_value) {
            (Some(Value::Object(target_object)), Value::Object(patch_object)) => {
                deep_merge_json_maps(target_object, patch_object);
            }
            _ => {
                target.insert(key.clone(), patch_value.clone());
            }
        }
    }
}

#[cfg(test)]
impl RuntimeState {
    pub(crate) fn seq(&self) -> u64 {
        self.seq
    }

    pub(crate) fn open_new(
        &mut self,
        expected_epoch: &str,
        session_id: impl Into<String>,
        cwd: impl Into<String>,
        session: Value,
    ) -> Result<u64, RuntimeStateError> {
        self.open_session(
            expected_epoch,
            session_id.into(),
            cwd.into(),
            session,
            BTreeMap::new(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state() -> RuntimeState {
        RuntimeState::new(
            "epoch",
            RuntimeLimits {
                max_delta_events: 128,
                max_delta_bytes: 1_000_000,
                max_intent_records: 4_096,
            },
        )
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

    fn accept_and_start_prompt(
        state: &mut RuntimeState,
        session_id: &str,
        incarnation: u64,
        client_intent_id: &str,
        payload: &str,
        prompt: Vec<Value>,
    ) -> String {
        let IntentAck::Accepted { operation_id } = state
            .accept_intent("epoch", 1, client_intent_id.to_string(), payload)
            .unwrap()
        else {
            panic!("a fresh client intent must be accepted");
        };
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
    fn in_flight_intent_keeps_fixed_digest_not_full_command() {
        let mut state = state();
        let command = format!("sensitive-command-{}", "x".repeat(8_192));
        let IntentAck::Accepted { operation_id } =
            state.accept_intent("epoch", 1, "intent", &command).unwrap()
        else {
            panic!("fresh intent must be accepted");
        };
        let record = state
            .intents
            .values()
            .find(|record| record.operation_id == operation_id)
            .unwrap();

        let expected: [u8; 32] = Sha256::digest(command.as_bytes()).into();
        assert_eq!(record.payload_digest, expected);
        assert_eq!(std::mem::size_of_val(&record.payload_digest), 32);
    }

    #[test]
    fn sequential_completed_intents_do_not_grow_intent_maps() {
        let mut state = state();
        let incarnation = open(&mut state, "session");

        for index in 0..32 {
            let IntentAck::Accepted { operation_id } = state
                .accept_intent(
                    "epoch",
                    1,
                    format!("control-intent-{index}"),
                    format!("control-payload-{index}"),
                )
                .unwrap()
            else {
                panic!("fresh control intent must be accepted");
            };
            state
                .start_operation(
                    "epoch",
                    "session",
                    incarnation,
                    &operation_id,
                    SessionOperationKind::SetMode,
                    "setting_mode",
                )
                .unwrap();
            state
                .complete_control_operation(
                    "epoch",
                    "session",
                    incarnation,
                    &operation_id,
                    SessionOperationKind::SetMode,
                    "current_mode_update",
                    json!({ "currentModeId": "plan" }),
                    json!({ "ok": true }),
                )
                .unwrap();

            assert!(state.intents.is_empty());
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
                .append_turn_update("epoch", "session", incarnation, update)
                .unwrap();
        }

        let retained = &state
            .session("session")
            .unwrap()
            .active_turn
            .as_ref()
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
    fn active_turn_deep_merges_tool_updates_at_the_first_position_without_changing_start_kind() {
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
                "details": { "path": "/first", "nested": { "left": 1 } }
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
                "details": { "nested": { "right": 2 } }
            }),
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "tool",
                "details": { "path": "/last", "nested": { "left": 3 } }
            }),
        ];
        for update in updates.clone() {
            state
                .append_turn_update("epoch", "session", incarnation, update)
                .unwrap();
        }

        let retained = &state
            .session("session")
            .unwrap()
            .active_turn
            .as_ref()
            .unwrap()
            .updates;
        assert_eq!(retained.len(), 2);
        assert_eq!(retained[0]["sessionUpdate"], "tool_call");
        assert_eq!(retained[0]["toolCallId"], "tool");
        assert_eq!(retained[0]["status"], "in_progress");
        assert_eq!(retained[0]["details"]["path"], "/last");
        assert_eq!(retained[0]["details"]["nested"]["left"], 3);
        assert_eq!(retained[0]["details"]["nested"]["right"], 2);
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
                .append_turn_update("epoch", "session", incarnation, update)
                .unwrap();
        }
        let retained = &state
            .session("session")
            .unwrap()
            .active_turn
            .as_ref()
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
                .append_turn_update("epoch", "session", incarnation, update)
                .unwrap();
        }

        let retained = &state
            .session("session")
            .unwrap()
            .active_turn
            .as_ref()
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
            .append_turn_update(
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
            state.append_turn_update(
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
            state.append_turn_update(
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
            .append_turn_update(
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
                .append_turn_update(
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
            !serde_json::to_string(&state.delta_journal)
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
        let session_payload = serde_json::to_string(session).unwrap();
        let active_turn = serde_json::to_string(&session.active_turn).unwrap();
        let delta_journal = serde_json::to_string(&state.delta_journal).unwrap();
        let mut retained_by = Vec::new();

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
        if state
            .intents
            .values()
            .any(|intent| intent.operation_id == operation_id)
        {
            retained_by.push("intent_record");
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
        let operation_id = accept_and_start_prompt(
            &mut state,
            "session",
            incarnation,
            "success-client-intent",
            "success-command-payload-marker",
            vec![json!({
                "type": "text",
                "text": "success-prompt-marker"
            })],
        );
        state
            .append_turn_update(
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
                "success-command-payload-marker",
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
        let operation_id = accept_and_start_prompt(
            &mut state,
            "session",
            incarnation,
            "error-client-intent",
            "error-command-payload-marker",
            vec![json!({ "type": "text", "text": "error-prompt-marker" })],
        );
        state
            .append_turn_update(
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
                "error-command-payload-marker",
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
        let operation_id = accept_and_start_prompt(
            &mut state,
            "session",
            incarnation,
            "uncertain-client-intent",
            "uncertain-command-payload-marker",
            vec![json!({
                "type": "text",
                "text": "uncertain-prompt-marker"
            })],
        );
        state
            .append_turn_update(
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
                "uncertain-command-payload-marker",
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
        let first_operation_id = accept_and_start_prompt(
            &mut state,
            "session",
            incarnation,
            "first-client-intent",
            "first-command-payload-marker",
            vec![json!("first-prompt-marker")],
        );
        state
            .append_turn_update(
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
            .append_turn_update(
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

        let second_operation_id = accept_and_start_prompt(
            &mut state,
            "session",
            incarnation,
            "second-client-intent",
            "second-command-digest",
            vec![json!("second prompt")],
        );
        state
            .append_turn_update(
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
            .append_turn_update(
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
        let session_payload = serde_json::to_string(session).unwrap();
        assert!(!session_payload.contains("first-message-marker"));
        assert!(!session_payload.contains("first-tool-marker"));
        assert!(
            !state
                .intents
                .values()
                .any(|intent| intent.operation_id == first_operation_id)
        );
    }

    #[test]
    fn turn_retirement_does_not_release_a_live_terminal() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        let operation_id = accept_and_start_prompt(
            &mut state,
            "session",
            incarnation,
            "resource-client-intent",
            "resource-command-payload-marker",
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
            &[
                "resource-command-payload-marker",
                "resource-prompt-marker",
                "resource-result-marker",
            ],
        );
    }

    #[test]
    fn sequential_completed_turns_leave_active_and_completed_payload_bytes_at_zero() {
        let mut state = state();
        let incarnation = open(&mut state, "session");

        for index in 0..32 {
            let marker = format!("sequential-turn-payload-{index}");
            let operation_id = accept_and_start_prompt(
                &mut state,
                "session",
                incarnation,
                &format!("sequential-intent-{index}"),
                &format!("sequential-command-payload-{index}"),
                vec![json!({ "type": "text", "text": marker })],
            );
            state
                .append_turn_update(
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
        assert!(state.intents.is_empty());
        assert!(
            !serde_json::to_string(&state.delta_journal)
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
            .append_turn_update("epoch", "session", incarnation, json!("answer"))
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
            !serde_json::to_string(session)
                .unwrap()
                .contains("conversation-must-not-be-stored")
        );
        assert!(
            !serde_json::to_string(session)
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
            .append_turn_update("epoch", "session", incarnation, json!("after cancel"))
            .unwrap();

        let active = state
            .session("session")
            .unwrap()
            .active_turn
            .as_ref()
            .unwrap();
        assert!(active.cancel_requested);
        assert_eq!(active.updates, vec![json!("after cancel")]);
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
    fn reconnecting_subscriber_reuses_the_same_epoch_scoped_intent() {
        let mut state = state();
        let first = state
            .accept_intent("epoch", 1, "same", "payload-a")
            .unwrap();
        let duplicate = state
            .accept_intent("epoch", 1, "same", "payload-a")
            .unwrap();
        let collision = state
            .accept_intent("epoch", 1, "same", "payload-b")
            .unwrap();
        let other_subscriber = state
            .accept_intent("epoch", 2, "same", "payload-a")
            .unwrap();

        let IntentAck::Accepted {
            operation_id: first_id,
        } = first
        else {
            panic!("first intent was not accepted")
        };
        assert_eq!(
            duplicate,
            IntentAck::Duplicate {
                operation_id: first_id.clone(),
                status: IntentStatus::Accepted,
            }
        );
        assert_eq!(collision, IntentAck::Collision);
        assert_eq!(
            other_subscriber,
            IntentAck::Duplicate {
                operation_id: first_id,
                status: IntentStatus::Accepted,
            }
        );
    }

    #[test]
    fn active_intent_capacity_is_reused_after_retirement() {
        let mut state = RuntimeState::new(
            "epoch",
            RuntimeLimits {
                max_intent_records: 2,
                ..RuntimeLimits::default()
            },
        );
        let IntentAck::Accepted {
            operation_id: retired,
        } = state
            .accept_intent("epoch", 1, "completed", "payload")
            .unwrap()
        else {
            panic!("first intent was not accepted")
        };
        let IntentAck::Accepted {
            operation_id: inflight,
        } = state
            .accept_intent("epoch", 1, "inflight", "payload")
            .unwrap()
        else {
            panic!("inflight intent was not accepted")
        };
        assert_eq!(
            state.accept_intent("epoch", 2, "replacement", "payload"),
            Err(RuntimeStateError::ResourceLimit),
        );
        state.retire_intent(&retired);

        assert!(matches!(
            state.accept_intent("epoch", 2, "replacement", "payload"),
            Ok(IntentAck::Accepted { .. })
        ));
        assert_eq!(state.intents.len(), 2);
        assert_eq!(state.intent_status(&retired), None);
        assert_eq!(state.intent_status(&inflight), Some(IntentStatus::Accepted));
    }

    #[test]
    fn uncertain_intent_identity_is_retained_without_result_payload() {
        let mut state = RuntimeState::new(
            "epoch",
            RuntimeLimits {
                max_intent_records: 1,
                ..RuntimeLimits::default()
            },
        );
        let IntentAck::Accepted {
            operation_id: uncertain,
        } = state
            .accept_intent("epoch", 1, "uncertain", "payload")
            .unwrap()
        else {
            panic!("uncertain intent was not accepted")
        };
        state.set_intent_status(&uncertain, IntentStatus::Uncertain);

        assert_eq!(
            state.accept_intent("epoch", 2, "replacement", "payload"),
            Err(RuntimeStateError::ResourceLimit),
        );
        assert_eq!(
            state
                .accept_intent("epoch", 3, "uncertain", "payload")
                .unwrap(),
            IntentAck::Duplicate {
                operation_id: uncertain,
                status: IntentStatus::Uncertain,
            },
        );
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
                    turn.updates.push(update);
                    session.revision = revision;
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
        let mut state = RuntimeState::new(
            "epoch",
            RuntimeLimits {
                max_delta_events: 128,
                max_delta_bytes: 1_000_000,
                max_intent_records: 4_096,
            },
        );
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        state
            .append_turn_update(
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
        let mut state = RuntimeState::new("epoch", RuntimeLimits::default());
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        let before = state.seq();
        for index in 0..100 {
            state
                .append_turn_update(
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
            Err(RuntimeStateError::BusySession),
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
        let IntentAck::Accepted { operation_id } = state
            .accept_intent("epoch", 1, "prompt", "payload")
            .unwrap()
        else {
            panic!("prompt intent was not accepted")
        };
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
            .append_turn_update("epoch", "session", incarnation, json!("partial"))
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

        assert_eq!(state.intent_status(&operation_id), None);
        let session = state.session("session").unwrap();
        assert!(session.active_turn.is_none());
        assert_eq!(
            state.append_turn_update("epoch", "session", incarnation, json!("late")),
            Err(RuntimeStateError::NoActiveTurn),
        );
    }

    #[test]
    fn cancelled_prompt_response_retires_the_intent_outcome() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        let IntentAck::Accepted { operation_id } = state
            .accept_intent("epoch", 1, "prompt", "payload")
            .unwrap()
        else {
            panic!("prompt intent was not accepted")
        };
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

        assert_eq!(state.intent_status(&operation_id), None);
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
        let IntentAck::Accepted {
            operation_id: response_operation,
        } = state
            .accept_intent("epoch", 1, "permission-response", "allow-once")
            .unwrap()
        else {
            panic!("permission response intent was not accepted")
        };
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
        assert_eq!(state.intent_status(&response_operation), None);
    }

    #[test]
    fn delta_resume_requires_snapshot_for_future_or_evicted_sequence() {
        let mut state = RuntimeState::new(
            "epoch",
            RuntimeLimits {
                max_delta_events: 2,
                max_delta_bytes: 1_000_000,
                max_intent_records: 4_096,
            },
        );
        open(&mut state, "a");
        open(&mut state, "b");
        open(&mut state, "c");

        assert!(state.deltas_after(state.seq() + 1).is_none());
        assert!(state.deltas_after(0).is_none());
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
        assert_eq!(state.intent_status("delete-operation"), None);
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
        let second = open(&mut state, "session");

        assert_ne!(first, second);
        assert_eq!(
            state.start_prompt("epoch", "session", first, "stale", Vec::new()),
            Err(RuntimeStateError::StaleIncarnation),
        );
        assert!(state.session("session").unwrap().active_turn.is_none());
    }

    #[test]
    fn fork_keeps_only_target_control_metadata() {
        let mut state = state();
        let source = open(&mut state, "source");
        state
            .start_prompt("epoch", "source", source, "prompt", vec![json!("Q")])
            .unwrap();
        state
            .append_turn_update("epoch", "source", source, json!("A"))
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
        let derived_payload = serde_json::to_string(derived).unwrap();
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
            !serde_json::to_string(replayed)
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
                    interaction_id: "permission".to_string(),
                },
                RuntimeEffect::CancelElicitationResponder {
                    session_id: Some("session".to_string()),
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

        assert_eq!(state.intent_status("close"), None);
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
        let mut state = RuntimeState::new(
            "epoch",
            RuntimeLimits {
                max_delta_events: 128,
                max_delta_bytes: 1_000_000,
                max_intent_records: 4_096,
            },
        );
        let incarnation = open(&mut state, "session");
        let IntentAck::Accepted { operation_id } = state
            .accept_intent("epoch", 1, "prompt", "payload")
            .unwrap()
        else {
            panic!("prompt intent was not accepted")
        };
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
        assert_eq!(state.intent_status(&operation_id), None);
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
            .append_turn_update("epoch", "session", incarnation, json!("answer"))
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
            .append_turn_update("epoch", "other", other_incarnation, json!("other answer"))
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
        assert_eq!(state.intent_status("close"), None);
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
        assert_eq!(state.intent_status("mode"), None);
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
        assert_eq!(attaching.attachment_candidate.len(), 1);
        assert!(
            !serde_json::to_string(&attaching.attachment_candidate)
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
        assert!(!serde_json::to_string(loaded).unwrap().contains("partial"));
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
            assert!(!serde_json::to_string(session).unwrap().contains("history"));
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
            !serde_json::to_string(state.session("resumed").unwrap())
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
                    interaction_id: "permission".to_string(),
                },
                RuntimeEffect::AbortUrlFlow {
                    session_id: Some("session".to_string()),
                    elicitation_id: "url".to_string(),
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
        let IntentAck::Accepted {
            operation_id: session_response,
        } = state
            .accept_intent("epoch", 1, "session-form-response", "session-form-value")
            .unwrap()
        else {
            panic!("session elicitation response intent was not accepted")
        };
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
        assert_eq!(state.intent_status(&session_response), None);

        state
            .upsert_elicitation("epoch", None, "request-form", json!({ "mode": "form" }))
            .unwrap();
        let IntentAck::Accepted {
            operation_id: request_response,
        } = state
            .accept_intent("epoch", 1, "request-form-response", "request-form-value")
            .unwrap()
        else {
            panic!("request elicitation response intent was not accepted")
        };
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
        assert_eq!(state.intent_status(&request_response), None);
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
        let IntentAck::Accepted { operation_id } = state
            .accept_intent("epoch", 1, "reload-intent", "session/load")
            .unwrap()
        else {
            panic!("reload intent was not accepted")
        };

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
        assert_eq!(staging.attachment_candidate.len(), 1);
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
            !serde_json::to_string(reloaded)
                .unwrap()
                .contains("must-not-survive-reload")
        );
        assert_eq!(
            reloaded.control_state["current_mode_update"]["currentModeId"],
            "new"
        );
        assert!(!reloaded.control_state.contains_key("config_option_update"));
        assert!(reloaded.operation.is_none());
        assert!(reloaded.attachment_candidate.is_empty());
        assert_eq!(reloaded.attachment_candidate_bytes, 0);
        assert_eq!(state.intent_status(&operation_id), None);
        assert_eq!(state.seq(), before_complete + 1);
        assert!(
            !serde_json::to_string(&state.snapshot())
                .unwrap()
                .contains("replacement-history-must-not-be-retained")
        );
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
        assert!(session.attachment_candidate.is_empty());
        assert_eq!(session.attachment_candidate_bytes, 0);
        assert_eq!(state.intent_status("reload"), None);
        assert!(
            !serde_json::to_string(&state.delta_journal)
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
