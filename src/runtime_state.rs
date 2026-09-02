use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Copy)]
pub(crate) struct RuntimeLimits {
    pub max_observed_turns_per_session: usize,
    pub max_observed_bytes_per_session: usize,
    pub max_active_turn_bytes_per_session: usize,
    pub max_queued_prompts_per_session: usize,
    pub max_queued_prompt_bytes_per_session: usize,
    pub max_delta_events: usize,
    pub max_delta_bytes: usize,
    pub max_intent_records: usize,
    pub max_intent_results: usize,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            max_observed_turns_per_session: 1_000,
            max_observed_bytes_per_session: 32 * 1024 * 1024,
            max_active_turn_bytes_per_session: 32 * 1024 * 1024,
            max_queued_prompts_per_session: 32,
            max_queued_prompt_bytes_per_session: 8 * 1024 * 1024,
            max_delta_events: 4_096,
            max_delta_bytes: 16 * 1024 * 1024,
            max_intent_records: 4_096,
            max_intent_results: 4_096,
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
    pub intent_results: BTreeMap<String, IntentResult>,
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
    pub transcript: TranscriptBaseline,
    pub observed_turns: VecDeque<ObservedTurn>,
    pub active_turn: Option<ActiveTurn>,
    pub queued_prompts: VecDeque<QueuedPrompt>,
    pub operation: Option<SessionOperationState>,
    pub permissions: BTreeMap<String, PendingInteraction>,
    pub elicitations: BTreeMap<String, PendingInteraction>,
    pub url_flows: BTreeMap<String, UrlFlow>,
    pub terminals: BTreeMap<String, Value>,
    pub history_gap: Option<HistoryGap>,
    #[serde(skip)]
    observed_bytes: usize,
    #[serde(skip)]
    queued_prompt_bytes: usize,
    #[serde(skip)]
    resolved_permissions: VecDeque<String>,
    #[serde(skip)]
    resolved_elicitations: VecDeque<String>,
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
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum TranscriptBaseline {
    PendingAttachment,
    Empty,
    AgentReplay {
        entries: Vec<Value>,
        turn_boundaries_known: bool,
    },
    HistoryUnavailable,
    ClientDerivedFork {
        source_session_id: String,
        source_incarnation: u64,
        source_revision: u64,
        entries: Vec<Value>,
    },
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueuedPrompt {
    pub operation_id: String,
    pub prompt: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ObservedTurn {
    pub turn_id: String,
    pub prompt: Vec<Value>,
    pub updates: Vec<Value>,
    pub cancel_requested: bool,
    pub outcome: ObservedTurnOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum ObservedTurnOutcome {
    Completed { response: Value },
    Failed { error: Value },
    Uncertain { reason: String },
}

struct TurnTerminal {
    outcome: ObservedTurnOutcome,
    status: IntentStatus,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HistoryGap {
    pub evicted_turns: usize,
    pub evicted_bytes: usize,
    pub through_revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeDelta {
    pub epoch: String,
    pub seq: u64,
    pub scope_revision: Option<u64>,
    pub change: RuntimeChange,
    pub intent_results: Vec<IntentResult>,
    #[serde(default)]
    pub evicted_intent_result_ids: Vec<String>,
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
    AgentAcknowledged,
    Rejected,
    Failed,
    Cancelled,
    Uncertain,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IntentResult {
    pub operation_id: String,
    pub session_id: Option<String>,
    pub status: IntentStatus,
    pub result: Option<Value>,
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
    payload_hash: String,
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
    intents: BTreeMap<ClientIntentKey, IntentRecord>,
    intent_order: VecDeque<ClientIntentKey>,
    intent_results: BTreeMap<String, IntentResult>,
    intent_result_order: VecDeque<String>,
    pending_intent_result_removals: Vec<String>,
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
            intents: BTreeMap::new(),
            intent_order: VecDeque::new(),
            intent_results: BTreeMap::new(),
            intent_result_order: VecDeque::new(),
            pending_intent_result_removals: Vec::new(),
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
            intent_results: self.intent_results.clone(),
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
        payload_hash: impl Into<String>,
    ) -> Result<IntentAck, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let key = ClientIntentKey {
            client_intent_id: client_intent_id.into(),
        };
        let payload_hash = payload_hash.into();
        if let Some(existing) = self.intents.get(&key) {
            return Ok(if existing.payload_hash == payload_hash {
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
        self.intent_order.push_back(key.clone());
        self.intents.insert(
            key,
            IntentRecord {
                payload_hash,
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
        session_id: Option<String>,
        reason: Value,
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
        intent.status = IntentStatus::Rejected;
        let result = self.record_intent_result(
            operation_id,
            session_id,
            IntentStatus::Rejected,
            Some(reason),
        );
        self.commit_connection_with_results(vec![result]);
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
        let (replay, control_state) = partition_replay_entries(replay);
        let transcript = if replay.is_empty() {
            TranscriptBaseline::Empty
        } else {
            TranscriptBaseline::AgentReplay {
                entries: replay,
                turn_boundaries_known: false,
            }
        };
        self.open_session(
            expected_epoch,
            session_id.into(),
            cwd.into(),
            session,
            transcript,
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
                transcript: TranscriptBaseline::PendingAttachment,
                observed_turns: VecDeque::new(),
                active_turn: None,
                queued_prompts: VecDeque::new(),
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
                history_gap: None,
                observed_bytes: 0,
                queued_prompt_bytes: 0,
                resolved_permissions: VecDeque::new(),
                resolved_elicitations: VecDeque::new(),
                attachment_candidate: Vec::new(),
                attachment_candidate_bytes: 0,
            },
        );
        self.set_intent_status(&operation_id, IntentStatus::InFlight);
        self.commit_session(&session_id);
        Ok(incarnation)
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
        if session.lifecycle != SessionLifecycle::Attaching {
            return Err(RuntimeStateError::OperationMismatch);
        }
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
        let (conversation, controls) = partition_replay_entries(candidate);
        session.attachment_candidate_bytes = 0;
        session.transcript = match kind {
            SessionOperationKind::Load => TranscriptBaseline::AgentReplay {
                entries: conversation,
                turn_boundaries_known: false,
            },
            SessionOperationKind::Resume => TranscriptBaseline::HistoryUnavailable,
            _ => unreachable!("attachment kind checked above"),
        };
        session.control_state.extend(controls);
        session.session = response.clone();
        session.lifecycle = SessionLifecycle::Active;
        session.operation = None;
        self.set_intent_status(operation_id, IntentStatus::AgentAcknowledged);
        let intent_result = self.record_intent_result(
            operation_id,
            Some(session_id.to_string()),
            IntentStatus::AgentAcknowledged,
            Some(response),
        );
        self.commit_session_with_results(session_id, vec![intent_result]);
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
        error: Value,
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
        let mut intent_results = self.cancel_queued_intents(session_id, queued);
        self.effects.extend(effects);
        self.set_intent_status(operation_id, IntentStatus::Failed);
        let intent_result = self.record_intent_result(
            operation_id,
            Some(session_id.to_string()),
            IntentStatus::Failed,
            Some(error),
        );
        intent_results.push(intent_result);
        self.commit_removal_with_results(session_id, incarnation, intent_results);
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
        let (transcript, control_state) = if let Some(entries) = target_replay {
            let (entries, control_state) = partition_replay_entries(entries);
            (
                TranscriptBaseline::AgentReplay {
                    entries,
                    turn_boundaries_known: false,
                },
                control_state,
            )
        } else {
            let source = self.require_session(source_session_id, source_incarnation)?;
            if source.lifecycle != SessionLifecycle::Active {
                return Err(RuntimeStateError::SessionNotActive);
            }
            let mut entries = transcript_entries(&source.transcript);
            entries.extend(
                source
                    .observed_turns
                    .iter()
                    .filter_map(|turn| serde_json::to_value(turn).ok()),
            );
            (
                TranscriptBaseline::ClientDerivedFork {
                    source_session_id: source_session_id.to_string(),
                    source_incarnation,
                    source_revision: source.revision,
                    entries,
                },
                BTreeMap::new(),
            )
        };
        self.open_session(
            expected_epoch,
            session_id.into(),
            cwd.into(),
            session,
            transcript,
            control_state,
        )
    }

    fn open_session(
        &mut self,
        expected_epoch: &str,
        session_id: String,
        cwd: String,
        session: Value,
        transcript: TranscriptBaseline,
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
                transcript,
                observed_turns: VecDeque::new(),
                active_turn: None,
                queued_prompts: VecDeque::new(),
                operation: None,
                permissions: BTreeMap::new(),
                elicitations: BTreeMap::new(),
                url_flows: BTreeMap::new(),
                terminals: BTreeMap::new(),
                history_gap: None,
                observed_bytes: 0,
                queued_prompt_bytes: 0,
                resolved_permissions: VecDeque::new(),
                resolved_elicitations: VecDeque::new(),
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
        if serialized_len(&active_turn) > self.limits.max_active_turn_bytes_per_session {
            return Err(RuntimeStateError::ResourceLimit);
        }
        let session = self.require_session_mut(session_id, incarnation)?;
        if session.lifecycle != SessionLifecycle::Active {
            return Err(RuntimeStateError::SessionNotActive);
        }
        if session.active_turn.is_some()
            || !session.queued_prompts.is_empty()
            || session.operation.is_some()
        {
            return Err(RuntimeStateError::BusySession);
        }
        session.active_turn = Some(active_turn);
        self.set_intent_status(&operation_id, IntentStatus::InFlight);
        self.commit_session(session_id);
        Ok(turn_id)
    }

    pub(crate) fn enqueue_prompt(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: impl Into<String>,
        prompt: Vec<Value>,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let operation_id = operation_id.into();
        if self.operation_in_use(&operation_id) {
            return Err(RuntimeStateError::OperationCollision);
        }
        let queued = QueuedPrompt {
            operation_id: operation_id.clone(),
            prompt,
        };
        let queued_bytes = serialized_len(&queued);
        let max_queued_prompts = self.limits.max_queued_prompts_per_session;
        let max_queued_bytes = self.limits.max_queued_prompt_bytes_per_session;
        let session = self.require_session_mut(session_id, incarnation)?;
        if session.lifecycle != SessionLifecycle::Active {
            return Err(RuntimeStateError::SessionNotActive);
        }
        if session.queued_prompts.len() >= max_queued_prompts
            || session.queued_prompt_bytes.saturating_add(queued_bytes) > max_queued_bytes
        {
            return Err(RuntimeStateError::ResourceLimit);
        }
        session.queued_prompts.push_back(queued);
        session.queued_prompt_bytes = session.queued_prompt_bytes.saturating_add(queued_bytes);
        self.set_intent_status(&operation_id, IntentStatus::Accepted);
        self.commit_session(session_id);
        Ok(())
    }

    pub(crate) fn start_next_queued_prompt(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
    ) -> Result<Option<String>, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let max_active_bytes = self.limits.max_active_turn_bytes_per_session;
        let session = self.require_session_mut(session_id, incarnation)?;
        if session.lifecycle != SessionLifecycle::Active {
            return Err(RuntimeStateError::SessionNotActive);
        }
        if session.active_turn.is_some() || session.operation.is_some() {
            return Err(RuntimeStateError::BusySession);
        }
        let Some(next) = session.queued_prompts.front() else {
            return Ok(None);
        };
        let candidate = ActiveTurn {
            turn_id: next.operation_id.clone(),
            operation_id: next.operation_id.clone(),
            prompt: next.prompt.clone(),
            updates: Vec::new(),
            cancel_requested: false,
        };
        if serialized_len(&candidate) > max_active_bytes {
            return Err(RuntimeStateError::ResourceLimit);
        }
        let queued = session
            .queued_prompts
            .pop_front()
            .expect("queued prompt was checked above");
        session.queued_prompt_bytes = session
            .queued_prompt_bytes
            .saturating_sub(serialized_len(&queued));
        let operation_id = queued.operation_id.clone();
        session.active_turn = Some(candidate);
        self.set_intent_status(&operation_id, IntentStatus::InFlight);
        self.commit_session(session_id);
        Ok(Some(operation_id))
    }

    pub(crate) fn start_expected_queued_prompt(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        expected_operation_id: &str,
    ) -> Result<bool, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let is_front = self
            .require_session(session_id, incarnation)?
            .queued_prompts
            .front()
            .is_some_and(|queued| queued.operation_id == expected_operation_id);
        if !is_front {
            return Ok(false);
        }
        Ok(self
            .start_next_queued_prompt(expected_epoch, session_id, incarnation)?
            .is_some())
    }

    pub(crate) fn cancel_queued_prompt(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        reason: Value,
    ) -> Result<bool, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let removed = {
            let session = self.require_session_mut(session_id, incarnation)?;
            let Some(position) = session
                .queued_prompts
                .iter()
                .position(|queued| queued.operation_id == operation_id)
            else {
                return Ok(false);
            };
            let queued = session
                .queued_prompts
                .remove(position)
                .expect("queued prompt position was checked above");
            session.queued_prompt_bytes = session
                .queued_prompt_bytes
                .saturating_sub(serialized_len(&queued));
            queued
        };
        self.set_intent_status(operation_id, IntentStatus::Cancelled);
        let result = self.record_intent_result(
            operation_id,
            Some(session_id.to_string()),
            IntentStatus::Cancelled,
            Some(reason),
        );
        self.commit_session_with_results(session_id, vec![result]);
        drop(removed);
        Ok(true)
    }

    pub(crate) fn cancel_all_queued_prompts(
        &mut self,
        expected_epoch: &str,
        reason: Value,
    ) -> Result<usize, RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let queued = self
            .sessions
            .values()
            .flat_map(|session| {
                session.queued_prompts.iter().map(|prompt| {
                    (
                        session.session_id.clone(),
                        session.incarnation,
                        prompt.operation_id.clone(),
                    )
                })
            })
            .collect::<Vec<_>>();
        for (session_id, incarnation, operation_id) in &queued {
            self.cancel_queued_prompt(
                expected_epoch,
                session_id,
                *incarnation,
                operation_id,
                reason.clone(),
            )?;
        }
        Ok(queued.len())
    }

    pub(crate) fn append_turn_update(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        update: Value,
    ) -> Result<(), RuntimeStateError> {
        self.require_epoch(expected_epoch)?;
        let max_active_bytes = self.limits.max_active_turn_bytes_per_session;
        let session = self.require_session_mut(session_id, incarnation)?;
        let turn = session
            .active_turn
            .as_mut()
            .ok_or(RuntimeStateError::NoActiveTurn)?;
        if serialized_len(turn).saturating_add(serialized_len(&update)) > max_active_bytes {
            return Err(RuntimeStateError::ResourceLimit);
        }
        let operation_id = turn.operation_id.clone();
        turn.updates.push(update.clone());
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
        let status = if prompt_response_cancelled(&response) {
            IntentStatus::Cancelled
        } else {
            IntentStatus::AgentAcknowledged
        };
        self.seal_turn(
            expected_epoch,
            session_id,
            incarnation,
            operation_id,
            TurnTerminal {
                outcome: ObservedTurnOutcome::Completed { response },
                status,
                lifecycle: None,
            },
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
        self.seal_turn(
            expected_epoch,
            session_id,
            incarnation,
            operation_id,
            TurnTerminal {
                outcome: ObservedTurnOutcome::Failed { error },
                status: IntentStatus::Failed,
                lifecycle: None,
            },
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
        self.seal_turn(
            expected_epoch,
            session_id,
            incarnation,
            operation_id,
            TurnTerminal {
                outcome: ObservedTurnOutcome::Uncertain {
                    reason: reason.into(),
                },
                status: IntentStatus::Uncertain,
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
        let TurnTerminal {
            outcome,
            status,
            lifecycle,
        } = terminal;
        self.require_epoch(expected_epoch)?;
        let limits = self.limits;
        let result_payload = match &outcome {
            ObservedTurnOutcome::Completed { response } => Some(response.clone()),
            ObservedTurnOutcome::Failed { error } => Some(error.clone()),
            ObservedTurnOutcome::Uncertain { reason } => Some(Value::String(reason.clone())),
        };
        let session = self.require_session_mut(session_id, incarnation)?;
        let active = session
            .active_turn
            .take()
            .ok_or(RuntimeStateError::NoActiveTurn)?;
        if active.operation_id != operation_id {
            session.active_turn = Some(active);
            return Err(RuntimeStateError::OperationMismatch);
        }
        let observed = ObservedTurn {
            turn_id: active.turn_id,
            prompt: active.prompt,
            updates: active.updates,
            cancel_requested: active.cancel_requested,
            outcome,
        };
        session.observed_bytes = session
            .observed_bytes
            .saturating_add(serialized_len(&observed));
        session.observed_turns.push_back(observed);
        enforce_observed_budget(session, limits);
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
        self.set_intent_status(operation_id, status.clone());
        let intent_result = self.record_intent_result(
            operation_id,
            Some(session_id.to_string()),
            status,
            result_payload,
        );
        self.commit_session_with_results(session_id, vec![intent_result]);
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
        session
            .permissions
            .remove(interaction_id)
            .ok_or(RuntimeStateError::UnknownInteraction)?;
        remember_resolved_permission(session, interaction_id);
        self.commit_session(session_id);
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
                remember_resolved_interaction(&mut session.resolved_elicitations, interaction_id);
                if let Some(elicitation_id) = accepted_url_id {
                    session.url_flows.insert(
                        elicitation_id.to_string(),
                        UrlFlow {
                            elicitation_id: elicitation_id.to_string(),
                            request: pending.request,
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
                remember_resolved_interaction(
                    &mut self.resolved_request_elicitations,
                    interaction_id,
                );
                if let Some(elicitation_id) = accepted_url_id {
                    self.request_url_flows.insert(
                        elicitation_id.to_string(),
                        UrlFlow {
                            elicitation_id: elicitation_id.to_string(),
                            request: pending.request,
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
                let flow = session
                    .url_flows
                    .get_mut(elicitation_id)
                    .ok_or(RuntimeStateError::UnknownInteraction)?;
                if flow.status != UrlFlowStatus::Waiting {
                    return Ok(UrlFlowResolution::AlreadyTerminal);
                }
                flow.status = status;
                self.commit_session(session_id);
            }
            None => {
                let flow = self
                    .request_url_flows
                    .get_mut(elicitation_id)
                    .ok_or(RuntimeStateError::UnknownInteraction)?;
                if flow.status != UrlFlowStatus::Waiting {
                    return Ok(UrlFlowResolution::AlreadyTerminal);
                }
                flow.status = status;
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
        let outcome = match session.terminals.get(&terminal_id) {
            Some(existing) if existing == &terminal => return Ok(TerminalUpsert::Duplicate),
            Some(existing) if terminal_released(existing) => return Ok(TerminalUpsert::Stale),
            Some(_) => TerminalUpsert::Updated,
            None => TerminalUpsert::Inserted,
        };
        session.terminals.insert(terminal_id, terminal);
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
        let intent_results = self.cancel_queued_intents(session_id, queued);
        self.effects.extend(effects);
        self.commit_session_with_results(session_id, intent_results);
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
        let mut intent_results = self.cancel_queued_intents(session_id, queued);
        self.effects.extend(effects);
        self.set_intent_status(operation_id, IntentStatus::AgentAcknowledged);
        let intent_result = self.record_intent_result(
            operation_id,
            Some(session_id.to_string()),
            IntentStatus::AgentAcknowledged,
            None,
        );
        intent_results.push(intent_result);
        self.commit_removal_with_results(session_id, incarnation, intent_results);
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
        let mut intent_results = self.cancel_queued_intents(session_id, queued);
        self.effects.extend(effects);
        self.set_intent_status(operation_id, IntentStatus::Uncertain);
        let intent_result = self.record_intent_result(
            operation_id,
            Some(session_id.to_string()),
            IntentStatus::Uncertain,
            Some(Value::String(reason)),
        );
        intent_results.push(intent_result);
        self.commit_session_with_results(session_id, intent_results);
        Ok(())
    }

    pub(crate) fn complete_operation(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        kind: SessionOperationKind,
        result: Value,
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
        self.set_intent_status(operation_id, IntentStatus::AgentAcknowledged);
        let intent_result = self.record_intent_result(
            operation_id,
            Some(session_id.to_string()),
            IntentStatus::AgentAcknowledged,
            Some(result),
        );
        self.commit_session_with_results(session_id, vec![intent_result]);
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
        result: Value,
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
        self.set_intent_status(operation_id, IntentStatus::AgentAcknowledged);
        let intent_result = self.record_intent_result(
            operation_id,
            Some(session_id.to_string()),
            IntentStatus::AgentAcknowledged,
            Some(result),
        );
        self.commit_session_with_results(session_id, vec![intent_result]);
        Ok(())
    }

    pub(crate) fn fail_operation(
        &mut self,
        expected_epoch: &str,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        kind: SessionOperationKind,
        error: Value,
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
        self.set_intent_status(operation_id, IntentStatus::Failed);
        let intent_result = self.record_intent_result(
            operation_id,
            Some(session_id.to_string()),
            IntentStatus::Failed,
            Some(error),
        );
        self.commit_session_with_results(session_id, vec![intent_result]);
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
        let mut intent_results = self.cancel_queued_intents(session_id, queued);
        self.effects.extend(effects);
        self.set_intent_status(operation_id, IntentStatus::AgentAcknowledged);
        let intent_result = self.record_intent_result(
            operation_id,
            Some(session_id.to_string()),
            IntentStatus::AgentAcknowledged,
            None,
        );
        intent_results.push(intent_result);
        self.commit_removal_with_results(session_id, session.incarnation, intent_results);
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
        self.intent_results.contains_key(operation_id)
            || self.intents.values().any(|intent| {
                intent.operation_id == operation_id && intent.status != IntentStatus::Accepted
            })
            || self.sessions.values().any(|session| {
                session
                    .active_turn
                    .as_ref()
                    .is_some_and(|turn| turn.operation_id == operation_id)
                    || session
                        .queued_prompts
                        .iter()
                        .any(|prompt| prompt.operation_id == operation_id)
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
            })
            || self.request_elicitations.values().any(|interaction| {
                interaction.responding_operation_id.as_deref() == Some(operation_id)
            })
    }

    fn reserve_intent_record_capacity(&mut self) -> bool {
        let limit = self.limits.max_intent_records;
        while self.intents.len() >= limit && limit > 0 {
            let Some(position) = self.intent_order.iter().position(|key| {
                self.intents
                    .get(key)
                    .is_some_and(|intent| intent_status_is_evictable(&intent.status))
            }) else {
                return false;
            };
            if let Some(key) = self.intent_order.remove(position) {
                self.intents.remove(&key);
            }
        }
        limit > 0
    }

    fn url_flow_in_use(&self, elicitation_id: &str) -> bool {
        self.request_url_flows.contains_key(elicitation_id)
            || self
                .sessions
                .values()
                .any(|session| session.url_flows.contains_key(elicitation_id))
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

    fn record_intent_result(
        &mut self,
        operation_id: &str,
        session_id: Option<String>,
        status: IntentStatus,
        result: Option<Value>,
    ) -> IntentResult {
        let intent_result = IntentResult {
            operation_id: operation_id.to_string(),
            session_id,
            status,
            result,
        };
        if !self.intent_results.contains_key(operation_id) {
            self.intent_result_order.push_back(operation_id.to_string());
        }
        self.intent_results
            .insert(operation_id.to_string(), intent_result.clone());
        while self.intent_result_order.len() > self.limits.max_intent_results {
            if let Some(evicted) = self.intent_result_order.pop_front() {
                self.intent_results.remove(&evicted);
                self.pending_intent_result_removals.push(evicted);
            }
        }
        intent_result
    }

    fn cancel_queued_intents(
        &mut self,
        session_id: &str,
        operation_ids: Vec<String>,
    ) -> Vec<IntentResult> {
        operation_ids
            .into_iter()
            .map(|operation_id| {
                self.set_intent_status(&operation_id, IntentStatus::Cancelled);
                self.record_intent_result(
                    &operation_id,
                    Some(session_id.to_string()),
                    IntentStatus::Cancelled,
                    None,
                )
            })
            .collect()
    }

    fn commit_session(&mut self, session_id: &str) {
        self.commit_session_with_results(session_id, Vec::new());
    }

    fn commit_connection(&mut self) {
        self.commit_connection_with_results(Vec::new());
    }

    fn commit_connection_with_results(&mut self, intent_results: Vec<IntentResult>) {
        self.connection_revision = self.connection_revision.wrapping_add(1).max(1);
        self.commit_delta(
            Some(self.connection_revision),
            RuntimeChange::ConnectionUpsert {
                request_elicitations: self.request_elicitations.clone(),
                request_url_flows: self.request_url_flows.clone(),
            },
            intent_results,
        );
    }

    fn commit_session_with_results(&mut self, session_id: &str, intent_results: Vec<IntentResult>) {
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
            intent_results,
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
            Vec::new(),
        );
    }

    fn commit_removal_with_results(
        &mut self,
        session_id: &str,
        incarnation: u64,
        intent_results: Vec<IntentResult>,
    ) {
        self.commit_delta(
            None,
            RuntimeChange::SessionRemoved {
                session_id: session_id.to_string(),
                incarnation,
            },
            intent_results,
        );
    }

    fn commit_delta(
        &mut self,
        scope_revision: Option<u64>,
        change: RuntimeChange,
        intent_results: Vec<IntentResult>,
    ) {
        self.seq = self.seq.wrapping_add(1).max(1);
        let delta = RuntimeDelta {
            epoch: self.epoch.clone(),
            seq: self.seq,
            scope_revision,
            change,
            intent_results,
            evicted_intent_result_ids: std::mem::take(&mut self.pending_intent_result_removals),
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

fn enforce_observed_budget(session: &mut SessionRuntime, limits: RuntimeLimits) {
    while session.observed_turns.len() > limits.max_observed_turns_per_session
        || session.observed_bytes > limits.max_observed_bytes_per_session
    {
        let Some(removed) = session.observed_turns.pop_front() else {
            break;
        };
        let bytes = serialized_len(&removed);
        session.observed_bytes = session.observed_bytes.saturating_sub(bytes);
        let gap = session.history_gap.get_or_insert_with(HistoryGap::default);
        gap.evicted_turns = gap.evicted_turns.saturating_add(1);
        gap.evicted_bytes = gap.evicted_bytes.saturating_add(bytes);
        gap.through_revision = session.revision.saturating_add(1);
    }
}

fn transcript_entries(transcript: &TranscriptBaseline) -> Vec<Value> {
    match transcript {
        TranscriptBaseline::PendingAttachment
        | TranscriptBaseline::Empty
        | TranscriptBaseline::HistoryUnavailable => Vec::new(),
        TranscriptBaseline::AgentReplay { entries, .. }
        | TranscriptBaseline::ClientDerivedFork { entries, .. } => entries.clone(),
    }
}

fn partition_replay_entries(entries: Vec<Value>) -> (Vec<Value>, BTreeMap<String, Value>) {
    let mut conversation = Vec::new();
    let mut controls = BTreeMap::new();
    for entry in entries {
        let update = entry.get("update").unwrap_or(&entry);
        let Some(kind) = update.get("sessionUpdate").and_then(Value::as_str) else {
            conversation.push(entry);
            continue;
        };
        if is_conversation_update_kind(kind) {
            conversation.push(entry);
        } else {
            controls.insert(kind.to_string(), update.clone());
        }
    }
    (conversation, controls)
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

fn prompt_response_cancelled(response: &Value) -> bool {
    response
        .get("stopReason")
        .or_else(|| response.get("stop_reason"))
        .and_then(Value::as_str)
        == Some("cancelled")
}

fn terminal_released(terminal: &Value) -> bool {
    terminal
        .get("released")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn intent_status_is_evictable(status: &IntentStatus) -> bool {
    matches!(
        status,
        IntentStatus::AgentAcknowledged
            | IntentStatus::Rejected
            | IntentStatus::Failed
            | IntentStatus::Cancelled
    )
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
    let queued = session
        .queued_prompts
        .drain(..)
        .map(|prompt| prompt.operation_id)
        .collect::<Vec<_>>();
    session.queued_prompt_bytes = 0;
    let mut effects = std::mem::take(&mut session.permissions)
        .into_keys()
        .map(|interaction_id| RuntimeEffect::CancelPermissionResponder {
            session_id: session_id.to_string(),
            interaction_id,
        })
        .collect::<Vec<_>>();
    effects.extend(
        std::mem::take(&mut session.elicitations)
            .into_keys()
            .map(|interaction_id| RuntimeEffect::CancelElicitationResponder {
                session_id: Some(session_id.to_string()),
                interaction_id,
            }),
    );
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
    (queued, effects)
}

fn serialized_len(value: &impl Serialize) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
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
            TranscriptBaseline::Empty,
            BTreeMap::new(),
        )
    }

    pub(crate) fn open_loaded(
        &mut self,
        expected_epoch: &str,
        session_id: impl Into<String>,
        cwd: impl Into<String>,
        session: Value,
        replay: Vec<Value>,
    ) -> Result<u64, RuntimeStateError> {
        let (replay, control_state) = partition_replay_entries(replay);
        self.open_session(
            expected_epoch,
            session_id.into(),
            cwd.into(),
            session,
            TranscriptBaseline::AgentReplay {
                entries: replay,
                turn_boundaries_known: false,
            },
            control_state,
        )
    }

    pub(crate) fn open_resumed(
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
            TranscriptBaseline::HistoryUnavailable,
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
                max_observed_turns_per_session: 2,
                max_observed_bytes_per_session: 16_384,
                max_active_turn_bytes_per_session: 16_384,
                max_queued_prompts_per_session: 8,
                max_queued_prompt_bytes_per_session: 16_384,
                max_delta_events: 128,
                max_delta_bytes: 1_000_000,
                max_intent_records: 4_096,
                max_intent_results: 4_096,
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

    #[test]
    fn turn_terminal_event_moves_active_to_observed_exactly_once() {
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
        assert_eq!(session.observed_turns.len(), 1);
        assert_eq!(session.observed_turns[0].updates, vec![json!("answer")]);
        assert_eq!(
            session.transcript,
            TranscriptBaseline::Empty,
            "a live PromptResponse seals bridge-observed history but does not claim Agent persistence"
        );
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
        assert_eq!(state.session("session").unwrap().observed_turns.len(), 1);
    }

    #[test]
    fn load_and_resume_have_distinct_history_semantics() {
        let mut state = state();
        state
            .open_loaded(
                "epoch",
                "loaded",
                "/loaded",
                json!({ "sessionId": "loaded" }),
                vec![json!({ "sessionUpdate": "agent_message_chunk" })],
            )
            .unwrap();
        state
            .open_resumed(
                "epoch",
                "resumed",
                "/resumed",
                json!({ "sessionId": "resumed" }),
            )
            .unwrap();

        assert!(matches!(
            state.session("loaded").unwrap().transcript,
            TranscriptBaseline::AgentReplay {
                turn_boundaries_known: false,
                ..
            }
        ));
        assert!(matches!(
            state.session("resumed").unwrap().transcript,
            TranscriptBaseline::HistoryUnavailable
        ));
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
    fn intent_tombstones_are_bounded_without_evicting_inflight_intents() {
        let mut state = RuntimeState::new(
            "epoch",
            RuntimeLimits {
                max_intent_records: 2,
                ..RuntimeLimits::default()
            },
        );
        let IntentAck::Accepted {
            operation_id: completed,
        } = state
            .accept_intent("epoch", 1, "completed", "payload")
            .unwrap()
        else {
            panic!("completed intent was not accepted")
        };
        let IntentAck::Accepted {
            operation_id: inflight,
        } = state
            .accept_intent("epoch", 1, "inflight", "payload")
            .unwrap()
        else {
            panic!("inflight intent was not accepted")
        };
        state.set_intent_status(&completed, IntentStatus::Failed);
        state.record_intent_result(&completed, None, IntentStatus::Failed, None);

        assert!(matches!(
            state.accept_intent("epoch", 2, "replacement", "payload"),
            Ok(IntentAck::Accepted { .. })
        ));
        assert_eq!(state.intents.len(), 2);
        assert_eq!(state.intent_status(&completed), None);
        assert_eq!(state.intent_status(&inflight), Some(IntentStatus::Accepted));
    }

    #[test]
    fn uncertain_intent_tombstones_are_pinned_against_automatic_retry() {
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
        state.record_intent_result(&uncertain, None, IntentStatus::Uncertain, None);

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
    fn whole_turn_eviction_never_changes_live_resources() {
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
        assert_eq!(session.observed_turns.len(), 2);
        assert_eq!(session.history_gap.as_ref().unwrap().evicted_turns, 1);
        assert!(session.permissions.contains_key("permission"));
        assert!(session.terminals.contains_key("terminal"));
    }

    #[test]
    fn active_turn_has_a_hard_byte_limit_without_losing_cancellability() {
        let mut state = RuntimeState::new(
            "epoch",
            RuntimeLimits {
                max_observed_turns_per_session: 2,
                max_observed_bytes_per_session: 256,
                max_active_turn_bytes_per_session: 256,
                max_queued_prompts_per_session: 8,
                max_queued_prompt_bytes_per_session: 16_384,
                max_delta_events: 128,
                max_delta_bytes: 1_000_000,
                max_intent_records: 4_096,
                max_intent_results: 4_096,
            },
        );
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        let before_rejection = state.snapshot();

        assert_eq!(
            state.append_turn_update(
                "epoch",
                "session",
                incarnation,
                json!({ "content": "x".repeat(512) }),
            ),
            Err(RuntimeStateError::ResourceLimit),
        );
        assert_eq!(state.snapshot(), before_rejection);
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
    fn accepted_queued_prompt_survives_without_a_subscriber_owner() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "first", Vec::new())
            .unwrap();
        state
            .enqueue_prompt(
                "epoch",
                "session",
                incarnation,
                "second",
                vec![json!("follow up")],
            )
            .unwrap();
        state
            .complete_prompt(
                "epoch",
                "session",
                incarnation,
                "first",
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();

        assert_eq!(
            state
                .start_next_queued_prompt("epoch", "session", incarnation)
                .unwrap(),
            Some("second".to_string()),
        );
        assert_eq!(
            state
                .session("session")
                .unwrap()
                .active_turn
                .as_ref()
                .unwrap()
                .prompt,
            vec![json!("follow up")],
        );
    }

    #[test]
    fn queued_prompt_claim_is_fifo_and_shutdown_cancellation_is_terminal() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "first", Vec::new())
            .unwrap();
        let second = match state
            .accept_intent("epoch", 1, "second-client", "second-payload")
            .unwrap()
        {
            IntentAck::Accepted { operation_id } => operation_id,
            other => panic!("second intent was not accepted: {other:?}"),
        };
        let third = match state
            .accept_intent("epoch", 1, "third-client", "third-payload")
            .unwrap()
        {
            IntentAck::Accepted { operation_id } => operation_id,
            other => panic!("third intent was not accepted: {other:?}"),
        };
        state
            .enqueue_prompt(
                "epoch",
                "session",
                incarnation,
                &second,
                vec![json!("second")],
            )
            .unwrap();
        state
            .enqueue_prompt(
                "epoch",
                "session",
                incarnation,
                &third,
                vec![json!("third")],
            )
            .unwrap();
        state
            .complete_prompt(
                "epoch",
                "session",
                incarnation,
                "first",
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        let before_out_of_order_claim = state.snapshot();

        assert!(
            !state
                .start_expected_queued_prompt("epoch", "session", incarnation, &third)
                .unwrap()
        );
        assert_eq!(state.snapshot(), before_out_of_order_claim);
        assert!(
            state
                .start_expected_queued_prompt("epoch", "session", incarnation, &second)
                .unwrap()
        );
        assert_eq!(
            state
                .session("session")
                .unwrap()
                .active_turn
                .as_ref()
                .unwrap()
                .operation_id,
            second,
        );
        assert!(
            state
                .cancel_queued_prompt(
                    "epoch",
                    "session",
                    incarnation,
                    &third,
                    json!("bridge_shutdown"),
                )
                .unwrap()
        );
        assert_eq!(state.intent_status(&third), Some(IntentStatus::Cancelled));
        assert_eq!(
            state.snapshot().intent_results[&third].result,
            Some(json!("bridge_shutdown")),
        );
    }

    #[test]
    fn new_prompt_at_handoff_cannot_bypass_an_existing_queue() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "first", Vec::new())
            .unwrap();
        state
            .enqueue_prompt(
                "epoch",
                "session",
                incarnation,
                "second",
                vec![json!("second")],
            )
            .unwrap();
        state
            .complete_prompt(
                "epoch",
                "session",
                incarnation,
                "first",
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        let handoff = state.snapshot();

        assert_eq!(
            state.start_prompt(
                "epoch",
                "session",
                incarnation,
                "new-arrival",
                vec![json!("must wait")],
            ),
            Err(RuntimeStateError::BusySession),
        );
        assert_eq!(state.snapshot(), handoff);
        state
            .enqueue_prompt(
                "epoch",
                "session",
                incarnation,
                "new-arrival",
                vec![json!("must wait")],
            )
            .unwrap();
        assert!(
            state
                .start_expected_queued_prompt("epoch", "session", incarnation, "second")
                .unwrap()
        );
    }

    #[test]
    fn shutdown_cancels_every_queued_prompt_with_exact_terminal_results() {
        let mut state = state();
        let first_incarnation = open(&mut state, "first-session");
        let second_incarnation = open(&mut state, "second-session");
        for (intent_id, session_id, incarnation) in [
            ("first-queued", "first-session", first_incarnation),
            ("second-queued", "second-session", second_incarnation),
        ] {
            let operation_id = match state
                .accept_intent("epoch", 1, intent_id, format!("payload-{intent_id}"))
                .unwrap()
            {
                IntentAck::Accepted { operation_id } => operation_id,
                other => panic!("unexpected admission: {other:?}"),
            };
            state
                .enqueue_prompt("epoch", session_id, incarnation, operation_id, Vec::new())
                .unwrap();
        }

        assert_eq!(
            state
                .cancel_all_queued_prompts("epoch", json!("bridge_shutdown"))
                .unwrap(),
            2
        );
        let snapshot = state.snapshot();
        assert!(
            snapshot
                .sessions
                .values()
                .all(|session| session.queued_prompts.is_empty())
        );
        assert_eq!(snapshot.intent_results.len(), 2);
        assert!(snapshot.intent_results.values().all(|result| {
            result.status == IntentStatus::Cancelled
                && result.result == Some(json!("bridge_shutdown"))
        }));
        assert_eq!(
            state
                .cancel_all_queued_prompts("epoch", json!("bridge_shutdown"))
                .unwrap(),
            0,
            "shutdown cancellation must be idempotent after the queue is terminal"
        );
    }

    #[test]
    fn queued_prompt_admission_has_count_and_byte_limits_without_partial_mutation() {
        let mut count_limited = RuntimeState::new(
            "epoch",
            RuntimeLimits {
                max_queued_prompts_per_session: 1,
                max_queued_prompt_bytes_per_session: 16_384,
                ..RuntimeLimits::default()
            },
        );
        let incarnation = open(&mut count_limited, "session");
        count_limited
            .start_prompt("epoch", "session", incarnation, "active", Vec::new())
            .unwrap();
        count_limited
            .enqueue_prompt(
                "epoch",
                "session",
                incarnation,
                "queued-1",
                vec![json!("first")],
            )
            .unwrap();
        let before_rejection = count_limited.snapshot();
        assert_eq!(
            count_limited.enqueue_prompt(
                "epoch",
                "session",
                incarnation,
                "queued-2",
                vec![json!("second")],
            ),
            Err(RuntimeStateError::ResourceLimit),
        );
        assert_eq!(count_limited.snapshot(), before_rejection);

        let mut byte_limited = RuntimeState::new(
            "epoch",
            RuntimeLimits {
                max_queued_prompts_per_session: 8,
                max_queued_prompt_bytes_per_session: 64,
                ..RuntimeLimits::default()
            },
        );
        let incarnation = open(&mut byte_limited, "session");
        byte_limited
            .start_prompt("epoch", "session", incarnation, "active", Vec::new())
            .unwrap();
        let before_rejection = byte_limited.snapshot();
        assert_eq!(
            byte_limited.enqueue_prompt(
                "epoch",
                "session",
                incarnation,
                "oversized",
                vec![json!("x".repeat(128))],
            ),
            Err(RuntimeStateError::ResourceLimit),
        );
        assert_eq!(byte_limited.snapshot(), before_rejection);
    }

    #[test]
    fn queued_prompt_is_rechecked_against_the_active_turn_limit_before_dispatch() {
        let mut state = RuntimeState::new(
            "epoch",
            RuntimeLimits {
                max_active_turn_bytes_per_session: 256,
                max_queued_prompt_bytes_per_session: 16_384,
                ..RuntimeLimits::default()
            },
        );
        let incarnation = open(&mut state, "session");
        state
            .start_prompt("epoch", "session", incarnation, "active", Vec::new())
            .unwrap();
        state
            .enqueue_prompt(
                "epoch",
                "session",
                incarnation,
                "oversized-queued",
                vec![json!("x".repeat(512))],
            )
            .unwrap();
        state
            .complete_prompt(
                "epoch",
                "session",
                incarnation,
                "active",
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        let before_rejection = state.snapshot();

        assert_eq!(
            state.start_next_queued_prompt("epoch", "session", incarnation),
            Err(RuntimeStateError::ResourceLimit),
        );
        assert_eq!(state.snapshot(), before_rejection);
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
        assert!(matches!(
            session.observed_turns.back().unwrap().outcome,
            ObservedTurnOutcome::Uncertain { .. }
        ));
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
        assert!(matches!(
            session.observed_turns.back().unwrap().outcome,
            ObservedTurnOutcome::Uncertain { .. }
        ));
    }

    #[test]
    fn prompt_failure_seals_partial_turn_as_failed_not_rejected() {
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

        assert_eq!(
            state.intent_status(&operation_id),
            Some(IntentStatus::Failed)
        );
        let session = state.session("session").unwrap();
        assert!(session.active_turn.is_none());
        assert_eq!(
            session.observed_turns.back().unwrap().updates,
            vec![json!("partial")]
        );
        assert!(matches!(
            session.observed_turns.back().unwrap().outcome,
            ObservedTurnOutcome::Failed { .. }
        ));
        assert_eq!(
            state.append_turn_update("epoch", "session", incarnation, json!("late")),
            Err(RuntimeStateError::NoActiveTurn),
        );
    }

    #[test]
    fn cancelled_prompt_response_has_a_cancelled_intent_outcome() {
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

        assert_eq!(
            state.intent_status(&operation_id),
            Some(IntentStatus::Cancelled)
        );
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
                    "response-1",
                )
                .unwrap(),
            InteractionResponseStart::Applied,
        );
        let responding_seq = state.seq();
        assert_eq!(
            state.session("session").unwrap().permissions["permission"]
                .responding_operation_id
                .as_deref(),
            Some("response-1"),
        );
        assert_eq!(
            state
                .begin_permission_response(
                    "epoch",
                    "session",
                    incarnation,
                    "permission",
                    "response-1",
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
                    "response-1",
                )
                .unwrap(),
            InteractionResolution::Applied,
        );
        assert!(state.session("session").unwrap().permissions.is_empty());
    }

    #[test]
    fn delta_resume_requires_snapshot_for_future_or_evicted_sequence() {
        let mut state = RuntimeState::new(
            "epoch",
            RuntimeLimits {
                max_observed_turns_per_session: 2,
                max_observed_bytes_per_session: 16_384,
                max_active_turn_bytes_per_session: 16_384,
                max_queued_prompts_per_session: 8,
                max_queued_prompt_bytes_per_session: 16_384,
                max_delta_events: 2,
                max_delta_bytes: 1_000_000,
                max_intent_records: 4_096,
                max_intent_results: 4_096,
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
        assert_eq!(
            state.snapshot().intent_results["delete-operation"].status,
            IntentStatus::Failed,
        );
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
    fn fork_copies_only_stable_history_and_prefers_agent_target_replay() {
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
        assert!(matches!(
            derived.transcript,
            TranscriptBaseline::ClientDerivedFork { .. }
        ));
        assert!(derived.permissions.is_empty());
        assert!(derived.terminals.is_empty());
        assert!(derived.active_turn.is_none());

        state
            .open_forked(
                "epoch",
                "replayed",
                "/replayed",
                json!({ "sessionId": "replayed" }),
                ("source", source),
                Some(vec![json!({ "sessionUpdate": "agent_message_chunk" })]),
            )
            .unwrap();
        assert!(matches!(
            state.session("replayed").unwrap().transcript,
            TranscriptBaseline::AgentReplay { .. }
        ));
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
    fn terminal_operation_identity_cannot_be_reused() {
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
        let before = state.snapshot();

        assert_eq!(
            state.start_prompt("epoch", "b", b, "same-operation", Vec::new()),
            Err(RuntimeStateError::OperationCollision),
        );
        assert_eq!(state.snapshot(), before);
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
    fn sealed_turn_preserves_cancel_request_and_cleans_owned_interactions() {
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
        assert!(session.observed_turns.back().unwrap().cancel_requested);
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
        assert_eq!(
            state.session("session").unwrap().terminals["terminal"],
            json!({ "output": "done", "released": true }),
        );
    }

    #[test]
    fn closing_a_session_cancels_queued_intents_and_releases_resources_once() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        let IntentAck::Accepted {
            operation_id: queued,
        } = state
            .accept_intent("epoch", 1, "queued", "payload")
            .unwrap()
        else {
            panic!("queued intent was not accepted")
        };
        state
            .enqueue_prompt(
                "epoch",
                "session",
                incarnation,
                &queued,
                vec![json!("later")],
            )
            .unwrap();
        let IntentAck::Accepted {
            operation_id: queued_second,
        } = state
            .accept_intent("epoch", 2, "queued-second", "payload")
            .unwrap()
        else {
            panic!("second queued intent was not accepted")
        };
        state
            .enqueue_prompt(
                "epoch",
                "session",
                incarnation,
                &queued_second,
                vec![json!("even later")],
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

        assert_eq!(state.intent_status(&queued), Some(IntentStatus::Cancelled));
        assert_eq!(
            state.intent_status(&queued_second),
            Some(IntentStatus::Cancelled)
        );
        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.intent_results[&queued].status,
            IntentStatus::Cancelled
        );
        assert_eq!(
            snapshot.intent_results[&queued_second].status,
            IntentStatus::Cancelled
        );
        let close_delta = &state.deltas_after(before_close).unwrap()[0];
        assert_eq!(close_delta.intent_results.len(), 3);
        assert_eq!(
            close_delta
                .intent_results
                .iter()
                .filter(|result| result.status == IntentStatus::Cancelled)
                .count(),
            2,
        );
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
    fn terminal_result_survives_immediate_observed_history_eviction() {
        let mut state = RuntimeState::new(
            "epoch",
            RuntimeLimits {
                max_observed_turns_per_session: 0,
                max_observed_bytes_per_session: 0,
                max_active_turn_bytes_per_session: 16_384,
                max_queued_prompts_per_session: 8,
                max_queued_prompt_bytes_per_session: 16_384,
                max_delta_events: 128,
                max_delta_bytes: 1_000_000,
                max_intent_records: 4_096,
                max_intent_results: 4_096,
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

        assert!(state.session("session").unwrap().observed_turns.is_empty());
        let result = &state.snapshot().intent_results[&operation_id];
        assert_eq!(result.status, IntentStatus::AgentAcknowledged);
        assert_eq!(result.session_id.as_deref(), Some("session"));
        let deltas = state.deltas_after(before).unwrap();
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].intent_results[0].operation_id, operation_id,);
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
    fn every_successful_session_transition_advances_one_contiguous_revision() {
        let mut state = state();
        let incarnation = open(&mut state, "session");
        let before = state.snapshot();
        state
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
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

        let deltas = state.deltas_after(before.through_seq).unwrap();
        assert_eq!(deltas.len(), 3);
        for (index, delta) in deltas.iter().enumerate() {
            assert_eq!(delta.epoch, "epoch");
            assert_eq!(delta.seq, before.through_seq + index as u64 + 1);
            assert_eq!(
                delta.scope_revision,
                Some(before.sessions["session"].revision + index as u64 + 1),
            );
        }
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
    fn control_success_updates_value_releases_guard_and_reports_result_atomically() {
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
        assert_eq!(
            state.deltas_after(before).unwrap()[0].intent_results[0].operation_id,
            "mode",
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

        assert_eq!(state.seq(), started_seq);
        let attaching = state.session("session").unwrap();
        assert_eq!(attaching.lifecycle, SessionLifecycle::Attaching);
        assert!(matches!(
            attaching.transcript,
            TranscriptBaseline::PendingAttachment
        ));
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
        assert!(matches!(
            &loaded.transcript,
            TranscriptBaseline::AgentReplay { entries, .. }
                if entries.len() == 1 && entries[0]["content"] == "partial"
        ));
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
            match &session.transcript {
                TranscriptBaseline::AgentReplay { entries, .. } => {
                    assert_eq!(entries.len(), 1);
                    assert_eq!(entries[0]["content"], "history");
                }
                TranscriptBaseline::HistoryUnavailable => {}
                baseline => panic!("unexpected attachment baseline: {baseline:?}"),
            }
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
        assert!(matches!(
            state.session("resumed").unwrap().transcript,
            TranscriptBaseline::HistoryUnavailable
        ));
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
        assert_eq!(
            state.session("session").unwrap().url_flows["url-id"].status,
            UrlFlowStatus::Completed,
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
                    "response-1",
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
                    "response-1",
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
                "response-1",
                Some("url-flow"),
            )
            .unwrap();
        let session = state.session("session").unwrap();
        assert!(!session.elicitations.contains_key("session-form"));
        assert_eq!(session.url_flows["url-flow"].status, UrlFlowStatus::Waiting);

        state
            .upsert_elicitation("epoch", None, "request-form", json!({ "mode": "form" }))
            .unwrap();
        state
            .begin_elicitation_response("epoch", None, "request-form", "request-response")
            .unwrap();
        assert_eq!(
            state.snapshot().request_elicitations["request-form"]
                .responding_operation_id
                .as_deref(),
            Some("request-response"),
        );
        state
            .complete_elicitation_response("epoch", None, "request-form", "request-response", None)
            .unwrap();
        assert!(
            !state
                .snapshot()
                .request_elicitations
                .contains_key("request-form")
        );
    }
}
