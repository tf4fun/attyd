use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::history_cache::{
    HistoryCache, HistoryCacheError, HistorySnapshot, SessionKey, fold_history_update,
    normalize_history_updates,
};
use crate::runtime_state::fold_active_turn_update;

const RECENT_CONSUMPTION_LIMIT: usize = 64;
const CONSUMED_INTENT_FILTER_WORDS: usize = 4096;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MirrorPhase {
    Cold,
    Loading,
    Ready,
    Running,
    Reconciling,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TurnOverlay {
    pub operation_id: String,
    pub client_intent_id: String,
    pub prompt: Vec<Value>,
    pub updates: Vec<Value>,
    pub terminal: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompletedTurnOutcome {
    pub operation_id: String,
    pub after_update: usize,
    pub response: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MirrorSessionState {
    pub session_id: String,
    pub incarnation: u64,
    pub view_revision: u64,
    pub history_revision: Option<String>,
    pub phase: MirrorPhase,
    pub active_turn: Option<TurnOverlay>,
    pub turn_outcomes: Vec<CompletedTurnOutcome>,
    pub sync_error: Option<String>,
    #[serde(default)]
    pub history_notice: Option<String>,
    #[serde(skip)]
    active_payload_digest: Option<[u8; 32]>,
    #[serde(skip)]
    recent_consumptions: VecDeque<LastConsumption>,
    #[serde(skip)]
    consumed_intents: ConsumedIntentFilter,
    #[serde(skip)]
    load_attempt: Option<String>,
    #[serde(skip)]
    active_overlay_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LastConsumption {
    operation_id: String,
    client_intent_id: String,
    payload_digest: [u8; 32],
    consumed_revision: String,
    successor_revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConsumedIntentFilter {
    words: Vec<u64>,
}

impl Default for ConsumedIntentFilter {
    fn default() -> Self {
        Self {
            words: vec![0; CONSUMED_INTENT_FILTER_WORDS],
        }
    }
}

impl ConsumedIntentFilter {
    fn indexes(client_intent_id: &str) -> [usize; 4] {
        let digest = Sha256::digest(client_intent_id.as_bytes());
        std::array::from_fn(|index| {
            let offset = index * 4;
            let value = u32::from_le_bytes(
                digest[offset..offset + 4]
                    .try_into()
                    .expect("SHA-256 contains four 32-bit indexes"),
            );
            value as usize % (CONSUMED_INTENT_FILTER_WORDS * u64::BITS as usize)
        })
    }

    fn contains(&self, client_intent_id: &str) -> bool {
        Self::indexes(client_intent_id).into_iter().all(|index| {
            self.words[index / u64::BITS as usize] & (1_u64 << (index % u64::BITS as usize)) != 0
        })
    }

    fn insert(&mut self, client_intent_id: &str) {
        for index in Self::indexes(client_intent_id) {
            self.words[index / u64::BITS as usize] |= 1_u64 << (index % u64::BITS as usize);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TurnAdmission {
    Accepted { operation_id: String },
    Duplicate { operation_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MirrorError {
    UnknownSession,
    StaleIncarnation,
    WrongPhase,
    StaleHistory,
    IdempotencyConflict,
    OperationMismatch,
    InconsistentHistory,
    History(HistoryCacheError),
}

impl From<HistoryCacheError> for MirrorError {
    fn from(value: HistoryCacheError) -> Self {
        Self::History(value)
    }
}

pub(crate) struct SessionView {
    pub session: MirrorSessionState,
    pub baseline: Arc<HistorySnapshot>,
}

pub(crate) struct SessionMirror {
    epoch: String,
    next_operation: u64,
    sessions: HashMap<String, MirrorSessionState>,
    history: HistoryCache,
    overlay_bytes: usize,
    // Process-local output attached to materialized history, outside the frequently
    // cloned runtime/control metadata and the Agent's original tool payloads.
    retained_terminals: HashMap<SessionKey, BTreeMap<String, Value>>,
    retained_terminal_bytes: usize,
}

impl SessionMirror {
    pub(crate) fn new(epoch: impl Into<String>) -> Self {
        let epoch = epoch.into();
        Self {
            history: HistoryCache::new(epoch.clone()),
            epoch,
            next_operation: 0,
            sessions: HashMap::new(),
            overlay_bytes: 0,
            retained_terminals: HashMap::new(),
            retained_terminal_bytes: 0,
        }
    }

    pub(crate) fn register_cold(&mut self, session_id: impl Into<String>, incarnation: u64) {
        let session_id = session_id.into();
        self.remove_existing_session(&session_id);
        self.history
            .install_empty(SessionKey::new(session_id.clone(), incarnation));
        self.sessions.insert(
            session_id.clone(),
            MirrorSessionState {
                session_id,
                incarnation,
                view_revision: 1,
                history_revision: None,
                phase: MirrorPhase::Cold,
                active_turn: None,
                turn_outcomes: Vec::new(),
                sync_error: None,
                history_notice: None,
                active_payload_digest: None,
                recent_consumptions: VecDeque::new(),
                consumed_intents: ConsumedIntentFilter::default(),
                load_attempt: None,
                active_overlay_bytes: 0,
            },
        );
    }

    pub(crate) fn register_new(&mut self, session_id: impl Into<String>, incarnation: u64) {
        let session_id = session_id.into();
        self.remove_existing_session(&session_id);
        let key = SessionKey::new(session_id.clone(), incarnation);
        let snapshot = self.history.install_empty(key);
        self.sessions.insert(
            session_id.clone(),
            MirrorSessionState {
                session_id,
                incarnation,
                view_revision: 1,
                history_revision: Some(snapshot.revision().to_string()),
                phase: MirrorPhase::Ready,
                active_turn: None,
                turn_outcomes: Vec::new(),
                sync_error: None,
                history_notice: None,
                active_payload_digest: None,
                recent_consumptions: VecDeque::new(),
                consumed_intents: ConsumedIntentFilter::default(),
                load_attempt: None,
                active_overlay_bytes: 0,
            },
        );
    }

    pub(crate) fn begin_load(
        &mut self,
        session_id: &str,
        incarnation: u64,
        attempt_id: impl Into<String>,
    ) -> Result<(), MirrorError> {
        let attempt_id = attempt_id.into();
        let session = self.require_session_mut(session_id, incarnation)?;
        if !matches!(
            session.phase,
            MirrorPhase::Cold
                | MirrorPhase::Loading
                | MirrorPhase::Ready
                | MirrorPhase::Reconciling
        ) || session.load_attempt.is_some()
        {
            return Err(MirrorError::WrongPhase);
        }
        let key = SessionKey::new(session_id, incarnation);
        self.history.begin_candidate(key, attempt_id.clone())?;
        let session = self
            .sessions
            .get_mut(session_id)
            .expect("session was validated above");
        if matches!(session.phase, MirrorPhase::Cold | MirrorPhase::Ready) {
            session.phase = MirrorPhase::Loading;
        }
        session.load_attempt = Some(attempt_id);
        session.sync_error = None;
        session.view_revision = next_revision(session.view_revision);
        Ok(())
    }

    pub(crate) fn append_load_update(
        &mut self,
        session_id: &str,
        incarnation: u64,
        attempt_id: &str,
        update: Value,
    ) -> Result<(), MirrorError> {
        let session = self.require_session(session_id, incarnation)?;
        if session.load_attempt.as_deref() != Some(attempt_id)
            || !matches!(
                session.phase,
                MirrorPhase::Loading | MirrorPhase::Reconciling
            )
        {
            return Err(MirrorError::OperationMismatch);
        }
        self.history.append_candidate(
            &SessionKey::new(session_id, incarnation),
            attempt_id,
            update,
        )?;
        Ok(())
    }

    pub(crate) fn invalidate_load(
        &mut self,
        session_id: &str,
        incarnation: u64,
        attempt_id: &str,
    ) -> Result<(), MirrorError> {
        let session = self.require_session(session_id, incarnation)?;
        if session.load_attempt.as_deref() != Some(attempt_id)
            || !matches!(
                session.phase,
                MirrorPhase::Loading | MirrorPhase::Reconciling
            )
        {
            return Err(MirrorError::OperationMismatch);
        }
        self.history.poison_candidate(
            &SessionKey::new(session_id, incarnation),
            attempt_id,
            HistoryCacheError::InvalidReplay,
        )?;
        Ok(())
    }

    pub(crate) fn commit_load(
        &mut self,
        session_id: &str,
        incarnation: u64,
        attempt_id: &str,
    ) -> Result<Arc<HistorySnapshot>, MirrorError> {
        let session = self.require_session(session_id, incarnation)?;
        if session.load_attempt.as_deref() != Some(attempt_id)
            || !matches!(
                session.phase,
                MirrorPhase::Loading | MirrorPhase::Reconciling
            )
        {
            return Err(MirrorError::OperationMismatch);
        }
        let load_phase = session.phase;
        let key = SessionKey::new(session_id, incarnation);
        if session.phase == MirrorPhase::Reconciling {
            let prior = self
                .history
                .peek(&key)
                .ok_or(MirrorError::InconsistentHistory)?;
            let candidate = self.history.candidate_updates(&key, attempt_id)?;
            let Some(prefix_end) = history_prefix_end(candidate, prior.updates()) else {
                return Err(MirrorError::InconsistentHistory);
            };
            let turn = session
                .active_turn
                .as_ref()
                .ok_or(MirrorError::InconsistentHistory)?;
            if prefix_end >= candidate.len()
                || !replay_suffix_starts_with_prompt(&candidate[prefix_end..], &turn.prompt)
            {
                return Err(MirrorError::InconsistentHistory);
            }
            if !replay_contains_completed_turn(&candidate[prefix_end..], &turn.updates) {
                return Err(MirrorError::InconsistentHistory);
            }
        }
        let previous_revision = session.history_revision.clone();
        let active = session.active_turn.as_ref().map(|turn| {
            (
                turn.client_intent_id.clone(),
                session
                    .active_payload_digest
                    .expect("running turn has digest"),
                turn.operation_id.clone(),
            )
        });
        let completed_outcome = session.active_turn.as_ref().and_then(|turn| {
            prompt_response(turn.terminal.as_ref()?).map(|response| CompletedTurnOutcome {
                operation_id: turn.operation_id.clone(),
                after_update: 0,
                response: response.clone(),
            })
        });
        let snapshot = self.history.commit_candidate(&key, attempt_id)?;
        let after_update = snapshot.updates().len();
        let session = self
            .sessions
            .get_mut(session_id)
            .expect("session was validated above");
        if load_phase == MirrorPhase::Loading {
            // A fresh authoritative replay has no historical PromptResponse data.
            // Do not retain outcome offsets from the baseline it replaced.
            session.turn_outcomes.clear();
            session.history_notice = None;
        } else if let Some(mut outcome) = completed_outcome {
            outcome.after_update = after_update;
            session.turn_outcomes.push(outcome);
        }
        if let (Some(consumed_revision), Some((client_intent_id, payload_digest, operation_id))) =
            (previous_revision, active)
        {
            session.consumed_intents.insert(&client_intent_id);
            session.recent_consumptions.push_back(LastConsumption {
                operation_id,
                client_intent_id,
                payload_digest,
                consumed_revision,
                successor_revision: snapshot.revision().to_string(),
            });
            while session.recent_consumptions.len() > RECENT_CONSUMPTION_LIMIT {
                session.recent_consumptions.pop_front();
            }
        }
        session.history_revision = Some(snapshot.revision().to_string());
        self.overlay_bytes = self
            .overlay_bytes
            .saturating_sub(session.active_overlay_bytes);
        session.active_overlay_bytes = 0;
        session.phase = MirrorPhase::Ready;
        session.active_turn = None;
        session.active_payload_digest = None;
        session.load_attempt = None;
        session.sync_error = None;
        session.view_revision = next_revision(session.view_revision);
        self.prune_retained_terminals(&key, snapshot.updates());
        Ok(snapshot)
    }

    // Resume notifications are useful context, but do not promise a full replay.
    pub(crate) fn commit_attachment_cache(
        &mut self,
        session_id: &str,
        incarnation: u64,
        attempt_id: &str,
    ) -> Result<Arc<HistorySnapshot>, MirrorError> {
        let snapshot = self.commit_load(session_id, incarnation, attempt_id)?;
        self.require_session_mut(session_id, incarnation)?.phase = MirrorPhase::Cold;
        Ok(snapshot)
    }

    pub(crate) fn use_cached_history(
        &mut self,
        session_id: &str,
        incarnation: u64,
        updates: &[Value],
        notice: String,
    ) -> Result<(), MirrorError> {
        let session = self.require_session_mut(session_id, incarnation)?;
        if session.active_turn.is_some() || session.load_attempt.is_some() {
            return Err(MirrorError::WrongPhase);
        }
        session.phase = MirrorPhase::Cold;
        let attempt = "bridge-cache-fallback";
        self.begin_load(session_id, incarnation, attempt)?;
        for update in updates {
            self.append_load_update(session_id, incarnation, attempt, update.clone())?;
        }
        self.commit_load(session_id, incarnation, attempt)?;
        self.require_session_mut(session_id, incarnation)?
            .history_notice = Some(notice);
        Ok(())
    }

    pub(crate) fn fail_load(
        &mut self,
        session_id: &str,
        incarnation: u64,
        attempt_id: &str,
        message: impl Into<String>,
        retryable: bool,
    ) -> Result<(), MirrorError> {
        let phase = self.require_session(session_id, incarnation)?.phase;
        let key = SessionKey::new(session_id, incarnation);
        self.history.abort_candidate(&key, attempt_id)?;
        let session = self
            .sessions
            .get_mut(session_id)
            .expect("session was validated above");
        if session.load_attempt.as_deref() != Some(attempt_id) {
            return Err(MirrorError::OperationMismatch);
        }
        session.load_attempt = None;
        session.sync_error = Some(message.into());
        session.phase = if retryable {
            match phase {
                MirrorPhase::Loading if session.history_revision.is_some() => MirrorPhase::Ready,
                _ => phase,
            }
        } else {
            MirrorPhase::Blocked
        };
        session.view_revision = next_revision(session.view_revision);
        Ok(())
    }

    pub(crate) fn start_turn(
        &mut self,
        session_id: &str,
        incarnation: u64,
        expected_history_revision: &str,
        client_intent_id: &str,
        prompt: Vec<Value>,
    ) -> Result<TurnAdmission, MirrorError> {
        let digest = payload_digest(&prompt);
        let existing = self.require_session(session_id, incarnation)?;
        if let Some(turn) = &existing.active_turn
            && turn.client_intent_id == client_intent_id
        {
            return if existing.active_payload_digest == Some(digest) {
                Ok(TurnAdmission::Duplicate {
                    operation_id: turn.operation_id.clone(),
                })
            } else {
                Err(MirrorError::IdempotencyConflict)
            };
        }
        if let Some(last) = existing
            .recent_consumptions
            .iter()
            .find(|last| last.client_intent_id == client_intent_id)
        {
            return if last.payload_digest == digest
                && (last.consumed_revision == expected_history_revision
                    || last.successor_revision == expected_history_revision)
            {
                Ok(TurnAdmission::Duplicate {
                    operation_id: last.operation_id.clone(),
                })
            } else {
                Err(MirrorError::IdempotencyConflict)
            };
        }
        if existing.consumed_intents.contains(client_intent_id) {
            // Exact response metadata is deliberately bounded, but the fixed-size
            // filter has no false negatives. A very old intent is rejected rather
            // than risking a second Agent dispatch. A Bloom false-positive merely
            // asks the caller to mint a fresh intent ID.
            return Err(MirrorError::IdempotencyConflict);
        }
        if existing.phase != MirrorPhase::Ready {
            return Err(MirrorError::WrongPhase);
        }
        if existing.history_revision.as_deref() != Some(expected_history_revision) {
            return Err(MirrorError::StaleHistory);
        }
        let overlay = TurnOverlay {
            operation_id: String::new(),
            client_intent_id: client_intent_id.to_string(),
            prompt,
            updates: Vec::new(),
            terminal: None,
        };
        self.next_operation = self.next_operation.wrapping_add(1).max(1);
        let operation_id = format!("{}:{}", self.epoch, self.next_operation);
        let mut overlay = overlay;
        overlay.operation_id = operation_id.clone();
        let overlay_bytes = serialized_len(&overlay);
        let session = self
            .sessions
            .get_mut(session_id)
            .expect("session was validated above");
        session.phase = MirrorPhase::Running;
        session.active_payload_digest = Some(digest);
        session.active_turn = Some(overlay);
        session.active_overlay_bytes = overlay_bytes;
        self.overlay_bytes = self.overlay_bytes.saturating_add(overlay_bytes);
        session.sync_error = None;
        session.view_revision = next_revision(session.view_revision);
        Ok(TurnAdmission::Accepted { operation_id })
    }

    pub(crate) fn append_turn_update(
        &mut self,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        update: Value,
    ) -> Result<(), MirrorError> {
        let session = self.require_session(session_id, incarnation)?;
        if session.phase != MirrorPhase::Running {
            return Err(MirrorError::WrongPhase);
        }
        let turn = session
            .active_turn
            .as_ref()
            .ok_or(MirrorError::OperationMismatch)?;
        if turn.operation_id != operation_id || turn.terminal.is_some() {
            return Err(MirrorError::OperationMismatch);
        }
        let old_bytes = session.active_overlay_bytes;
        let mut candidate = turn.clone();
        candidate.updates = fold_active_turn_update(&turn.updates, &update)
            .map_err(|_| MirrorError::InconsistentHistory)?;
        let candidate_bytes = serialized_len(&candidate);
        let session = self
            .sessions
            .get_mut(session_id)
            .expect("session was validated above");
        session.active_turn = Some(candidate);
        session.active_overlay_bytes = candidate_bytes;
        self.overlay_bytes = self
            .overlay_bytes
            .saturating_sub(old_bytes)
            .saturating_add(candidate_bytes);
        session.view_revision = next_revision(session.view_revision);
        Ok(())
    }

    pub(crate) fn append_session_update(
        &mut self,
        session_id: &str,
        incarnation: u64,
        update: Value,
    ) -> Result<(), MirrorError> {
        let session = self.require_session(session_id, incarnation)?;
        if session.load_attempt.is_some() {
            return Err(MirrorError::WrongPhase);
        }
        if session.phase == MirrorPhase::Reconciling {
            let turn = session
                .active_turn
                .as_ref()
                .ok_or(MirrorError::OperationMismatch)?;
            let old_bytes = session.active_overlay_bytes;
            let mut candidate = turn.clone();
            candidate.updates = fold_active_turn_update(&turn.updates, &update)
                .map_err(|_| MirrorError::InconsistentHistory)?;
            let candidate_bytes = serialized_len(&candidate);
            let session = self
                .sessions
                .get_mut(session_id)
                .expect("validated session");
            session.active_turn = Some(candidate);
            session.active_overlay_bytes = candidate_bytes;
            session.view_revision = next_revision(session.view_revision);
            self.overlay_bytes = self
                .overlay_bytes
                .saturating_sub(old_bytes)
                .saturating_add(candidate_bytes);
            return Ok(());
        }
        if session.phase != MirrorPhase::Ready {
            return Err(MirrorError::WrongPhase);
        }
        let snapshot = self
            .history
            .append_committed_updates(&SessionKey::new(session_id, incarnation), &[update])?;
        let session = self
            .sessions
            .get_mut(session_id)
            .expect("validated session");
        session.history_revision = Some(snapshot.revision().to_string());
        session.view_revision = next_revision(session.view_revision);
        Ok(())
    }

    pub(crate) fn retain_terminal_output(
        &mut self,
        session_id: &str,
        incarnation: u64,
        terminal_id: &str,
        terminal: &Value,
    ) -> Result<bool, MirrorError> {
        let session = self.require_session(session_id, incarnation)?;
        if terminal.get("released").and_then(Value::as_bool) != Some(true)
            || terminal.get("outputAppend").and_then(Value::as_bool) == Some(true)
        {
            return Ok(false);
        }
        let key = SessionKey::new(session_id, incarnation);
        if self
            .retained_terminals
            .get(&key)
            .is_some_and(|terminals| terminals.contains_key(terminal_id))
        {
            return Ok(false);
        }
        let referenced = session.active_turn.as_ref().is_some_and(|turn| {
            turn.updates
                .iter()
                .any(|update| references_terminal(update, terminal_id))
        }) || self.history.peek(&key).is_some_and(|history| {
            history
                .updates()
                .iter()
                .any(|update| references_terminal(update, terminal_id))
        });
        if !referenced {
            return Ok(false);
        }
        let retained = serde_json::json!({
            "sessionId": session_id,
            "terminalId": terminal_id,
            "output": terminal.get("output").cloned().unwrap_or(Value::String(String::new())),
            "truncated": terminal.get("truncated").cloned().unwrap_or(Value::Bool(false)),
            "exitStatus": terminal.get("exitStatus").cloned().unwrap_or(Value::Null),
            "released": true,
        });
        self.retained_terminal_bytes = self
            .retained_terminal_bytes
            .saturating_add(serialized_len(&retained));
        self.retained_terminals
            .entry(key)
            .or_default()
            .insert(terminal_id.to_string(), retained);
        let session = self
            .sessions
            .get_mut(session_id)
            .expect("session was validated above");
        session.view_revision = next_revision(session.view_revision);
        Ok(true)
    }

    pub(crate) fn complete_turn(
        &mut self,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        terminal: Value,
    ) -> Result<(), MirrorError> {
        let session = self.require_session(session_id, incarnation)?;
        if session.phase != MirrorPhase::Running {
            return Err(MirrorError::WrongPhase);
        }
        let turn = session
            .active_turn
            .as_ref()
            .ok_or(MirrorError::OperationMismatch)?;
        if turn.operation_id != operation_id || turn.terminal.is_some() {
            return Err(MirrorError::OperationMismatch);
        }
        let old_bytes = session.active_overlay_bytes;
        let mut candidate = turn.clone();
        candidate.terminal = Some(terminal);
        let candidate_bytes = serialized_len(&candidate);
        let session = self
            .sessions
            .get_mut(session_id)
            .expect("session was validated above");
        session.active_turn = Some(candidate);
        session.active_overlay_bytes = candidate_bytes;
        self.overlay_bytes = self
            .overlay_bytes
            .saturating_sub(old_bytes)
            .saturating_add(candidate_bytes);
        session.phase = MirrorPhase::Reconciling;
        session.view_revision = next_revision(session.view_revision);
        Ok(())
    }

    pub(crate) fn commit_completed_turn_from_memory(
        &mut self,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
    ) -> Result<Arc<HistorySnapshot>, MirrorError> {
        let session = self.require_session(session_id, incarnation)?;
        if session.phase != MirrorPhase::Reconciling {
            return Err(MirrorError::WrongPhase);
        }
        let turn = session
            .active_turn
            .as_ref()
            .filter(|turn| turn.operation_id == operation_id && turn.terminal.is_some())
            .ok_or(MirrorError::OperationMismatch)?
            .clone();
        let previous_revision = session
            .history_revision
            .clone()
            .ok_or(MirrorError::InconsistentHistory)?;
        let payload_digest = session
            .active_payload_digest
            .ok_or(MirrorError::InconsistentHistory)?;
        let active_overlay_bytes = session.active_overlay_bytes;
        let completed_outcome = turn
            .terminal
            .as_ref()
            .and_then(prompt_response)
            .map(|response| CompletedTurnOutcome {
                operation_id: turn.operation_id.clone(),
                after_update: 0,
                response: response.clone(),
            });
        let mut suffix = if replay_suffix_starts_with_prompt(&turn.updates, &turn.prompt) {
            // Prefer the Agent's echo, including message IDs and metadata. This
            // comparison is scoped to this accepted prompt, never earlier turns.
            Vec::new()
        } else {
            turn.prompt
                .iter()
                .cloned()
                .map(|content| {
                    serde_json::json!({
                        "sessionUpdate": "user_message_chunk",
                        "content": content,
                        "_meta": {
                            "attyd": {
                                "turnOperationId": turn.operation_id,
                            }
                        },
                    })
                })
                .collect::<Vec<_>>()
        };
        suffix.extend(turn.updates.iter().cloned());
        let snapshot = self
            .history
            .append_committed_updates(&SessionKey::new(session_id, incarnation), &suffix)?;
        let after_update = snapshot.updates().len();

        let session = self
            .sessions
            .get_mut(session_id)
            .expect("session was validated above");
        if let Some(mut outcome) = completed_outcome {
            outcome.after_update = after_update;
            session.turn_outcomes.push(outcome);
        }
        session.consumed_intents.insert(&turn.client_intent_id);
        session.recent_consumptions.push_back(LastConsumption {
            operation_id: turn.operation_id,
            client_intent_id: turn.client_intent_id,
            payload_digest,
            consumed_revision: previous_revision,
            successor_revision: snapshot.revision().to_string(),
        });
        while session.recent_consumptions.len() > RECENT_CONSUMPTION_LIMIT {
            session.recent_consumptions.pop_front();
        }
        session.history_revision = Some(snapshot.revision().to_string());
        self.overlay_bytes = self.overlay_bytes.saturating_sub(active_overlay_bytes);
        session.active_overlay_bytes = 0;
        session.phase = MirrorPhase::Ready;
        session.active_turn = None;
        session.active_payload_digest = None;
        session.load_attempt = None;
        session.sync_error = None;
        session.view_revision = next_revision(session.view_revision);
        Ok(snapshot)
    }

    pub(crate) fn view(
        &mut self,
        session_id: &str,
        incarnation: u64,
    ) -> Result<SessionView, MirrorError> {
        let session = self.require_session(session_id, incarnation)?.clone();
        let baseline = self
            .history
            .snapshot(&SessionKey::new(session_id, incarnation))
            .ok_or(MirrorError::WrongPhase)?;
        Ok(SessionView { session, baseline })
    }

    pub(crate) fn state(&self, session_id: &str) -> Option<&MirrorSessionState> {
        self.sessions.get(session_id)
    }

    pub(crate) fn load_attempt(&self, session_id: &str, incarnation: u64) -> Option<&str> {
        let session = self.require_session(session_id, incarnation).ok()?;
        session.load_attempt.as_deref()
    }

    pub(crate) fn active_operation_id(&self, session_id: &str, incarnation: u64) -> Option<&str> {
        let session = self.require_session(session_id, incarnation).ok()?;
        session
            .active_turn
            .as_ref()
            .map(|turn| turn.operation_id.as_str())
    }

    pub(crate) fn touch(&mut self, session_id: &str, incarnation: u64) -> Result<u64, MirrorError> {
        let session = self.require_session_mut(session_id, incarnation)?;
        session.view_revision = next_revision(session.view_revision);
        Ok(session.view_revision)
    }

    pub(crate) fn abort_turn(
        &mut self,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
    ) -> Result<(), MirrorError> {
        let old_bytes = {
            let session = self.require_session(session_id, incarnation)?;
            if session.phase != MirrorPhase::Running
                || session
                    .active_turn
                    .as_ref()
                    .is_none_or(|turn| turn.operation_id != operation_id)
            {
                return Err(MirrorError::OperationMismatch);
            }
            session.active_overlay_bytes
        };
        self.overlay_bytes = self.overlay_bytes.saturating_sub(old_bytes);
        let session = self
            .sessions
            .get_mut(session_id)
            .expect("session was validated above");
        session.phase = MirrorPhase::Ready;
        session.active_overlay_bytes = 0;
        session.active_turn = None;
        session.active_payload_digest = None;
        session.view_revision = next_revision(session.view_revision);
        Ok(())
    }

    pub(crate) fn block_turn(
        &mut self,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        message: impl Into<String>,
    ) -> Result<(), MirrorError> {
        let session = self.require_session_mut(session_id, incarnation)?;
        if !matches!(
            session.phase,
            MirrorPhase::Running | MirrorPhase::Reconciling
        ) || session
            .active_turn
            .as_ref()
            .is_none_or(|turn| turn.operation_id != operation_id)
        {
            return Err(MirrorError::OperationMismatch);
        }
        session.phase = MirrorPhase::Blocked;
        session.sync_error = Some(message.into());
        session.load_attempt = None;
        session.view_revision = next_revision(session.view_revision);
        Ok(())
    }

    pub(crate) fn view_value(
        &mut self,
        session_id: &str,
        incarnation: u64,
    ) -> Result<Value, MirrorError> {
        let view = self.view(session_id, incarnation)?;
        let digest = view
            .baseline
            .digest()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok(serde_json::json!({
            "bridgeEpoch": self.epoch,
            "session": view.session,
            "terminals": self.retained_terminals.get(&SessionKey::new(session_id, incarnation)).cloned().unwrap_or_default(),
            "baseline": {
                "revision": view.baseline.revision(),
                "updates": view.baseline.updates(),
                "bytes": view.baseline.bytes(),
                "digest": digest,
            }
        }))
    }

    pub(crate) fn remove(&mut self, session_id: &str, incarnation: u64) {
        if self
            .sessions
            .get(session_id)
            .is_none_or(|session| session.incarnation != incarnation)
        {
            return;
        }
        if let Some(session) = self.sessions.remove(session_id) {
            self.overlay_bytes = self
                .overlay_bytes
                .saturating_sub(session.active_overlay_bytes);
        }
        self.remove_retained_terminals(&SessionKey::new(session_id, incarnation));
        self.history
            .remove(&SessionKey::new(session_id, incarnation));
    }

    fn remove_retained_terminals(&mut self, key: &SessionKey) {
        if let Some(terminals) = self.retained_terminals.remove(key) {
            for terminal in terminals.values() {
                self.retained_terminal_bytes = self
                    .retained_terminal_bytes
                    .saturating_sub(serialized_len(terminal));
            }
        }
    }

    fn prune_retained_terminals(&mut self, key: &SessionKey, updates: &[Value]) {
        if let Some(terminals) = self.retained_terminals.get_mut(key) {
            terminals.retain(|terminal_id, terminal| {
                if updates
                    .iter()
                    .any(|update| references_terminal(update, terminal_id))
                {
                    true
                } else {
                    self.retained_terminal_bytes = self
                        .retained_terminal_bytes
                        .saturating_sub(serialized_len(terminal));
                    false
                }
            });
        }
    }

    fn remove_existing_session(&mut self, session_id: &str) {
        if let Some(previous) = self.sessions.remove(session_id) {
            self.overlay_bytes = self
                .overlay_bytes
                .saturating_sub(previous.active_overlay_bytes);
            let key = SessionKey::new(previous.session_id, previous.incarnation);
            self.remove_retained_terminals(&key);
            self.history.remove(&key);
        }
    }

    fn require_session(
        &self,
        session_id: &str,
        incarnation: u64,
    ) -> Result<&MirrorSessionState, MirrorError> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or(MirrorError::UnknownSession)?;
        if session.incarnation != incarnation {
            return Err(MirrorError::StaleIncarnation);
        }
        Ok(session)
    }

    fn require_session_mut(
        &mut self,
        session_id: &str,
        incarnation: u64,
    ) -> Result<&mut MirrorSessionState, MirrorError> {
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or(MirrorError::UnknownSession)?;
        if session.incarnation != incarnation {
            return Err(MirrorError::StaleIncarnation);
        }
        Ok(session)
    }
}

fn payload_digest(prompt: &[Value]) -> [u8; 32] {
    Sha256::digest(serde_json::to_vec(prompt).expect("prompt values serialize")).into()
}

fn serialized_len(value: &impl Serialize) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |value| value.len())
}

fn prompt_response(value: &Value) -> Option<&Value> {
    value
        .get("stopReason")
        .and_then(Value::as_str)
        .map(|_| value)
}

fn next_revision(current: u64) -> u64 {
    current.wrapping_add(1).max(1)
}

fn references_terminal(update: &Value, terminal_id: &str) -> bool {
    update
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|content| {
            content.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("terminal")
                    && item.get("terminalId").and_then(Value::as_str) == Some(terminal_id)
            })
        })
}

fn replay_suffix_starts_with_prompt(updates: &[Value], prompt: &[Value]) -> bool {
    let replay_prompt = updates
        .iter()
        .take_while(|update| {
            update.get("sessionUpdate").and_then(Value::as_str) == Some("user_message_chunk")
        })
        .filter_map(|update| update.get("content").cloned())
        .collect::<Vec<_>>();
    normalize_prompt_blocks(&replay_prompt) == normalize_prompt_blocks(prompt)
}

fn history_prefix_end(candidate: &[Value], prior: &[Value]) -> Option<usize> {
    // IDs/chunk boundaries may change on load. Normalize only this comparison;
    // never discard identities from the snapshot installed for the browser.
    let prior = normalize_history_updates(
        &prior
            .iter()
            .map(history_update_identity)
            .collect::<Vec<_>>(),
    )
    .ok()?;
    if prior.is_empty() {
        return Some(0);
    }
    let mut prefix = Vec::new();
    for (index, update) in candidate.iter().enumerate() {
        prefix = fold_history_update(&prefix, &history_update_identity(update)).ok()?;
        if prefix == prior {
            return Some(index + 1);
        }
    }
    None
}

fn replay_contains_completed_turn(replay: &[Value], live_updates: &[Value]) -> bool {
    fn durable(update: &&Value) -> bool {
        matches!(
            update.get("sessionUpdate").and_then(Value::as_str),
            Some("agent_message_chunk" | "tool_call" | "tool_call_update")
        )
    }

    let replay = replay
        .iter()
        .filter(durable)
        .map(history_update_identity)
        .collect::<Vec<_>>();
    let live = live_updates
        .iter()
        .filter(durable)
        .map(history_update_identity)
        .collect::<Vec<_>>();
    let Ok(replay) = normalize_history_updates(&replay) else {
        return false;
    };
    let Ok(live) = normalize_history_updates(&live) else {
        return false;
    };
    let replay = replay
        .iter()
        .map(history_update_identity)
        .collect::<Vec<_>>();
    let live = live.iter().map(history_update_identity).collect::<Vec<_>>();
    let mut replay = replay.iter();
    live.iter()
        .all(|expected| replay.by_ref().any(|candidate| candidate == expected))
}

fn history_update_identity(update: &Value) -> Value {
    let mut update = update.clone();
    if let Some(update) = update.as_object_mut() {
        // ACP message IDs correlate streamed chunks within one connection. They are
        // optional and are not a durable history identity across session/load.
        update.remove("messageId");
    }
    update
}

fn normalize_prompt_blocks(blocks: &[Value]) -> Vec<Value> {
    let mut normalized: Vec<Value> = Vec::with_capacity(blocks.len());
    for block in blocks {
        let Some(text) = block
            .get("text")
            .and_then(Value::as_str)
            .filter(|_| block.get("type").and_then(Value::as_str) == Some("text"))
        else {
            normalized.push(block.clone());
            continue;
        };
        if let Some(previous) = normalized.last_mut()
            && previous.get("type").and_then(Value::as_str) == Some("text")
            && {
                let mut shape = block.clone();
                shape["text"] = Value::Null;
                let mut previous_shape = previous.clone();
                previous_shape["text"] = Value::Null;
                shape == previous_shape
            }
        {
            let previous_text = previous.get("text").and_then(Value::as_str).unwrap_or("");
            previous["text"] = Value::String(format!("{previous_text}{text}"));
        } else {
            normalized.push(block.clone());
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_updates_preserve_reconciling_load_and_incarnation_boundaries() {
        let mut mirror = mirror();
        mirror.register_new("session", 1);
        let revision = mirror
            .view("session", 1)
            .unwrap()
            .baseline
            .revision()
            .to_string();
        let TurnAdmission::Accepted { operation_id } = mirror
            .start_turn("session", 1, &revision, "intent", text_prompt("Run"))
            .unwrap()
        else {
            panic!("new intent")
        };
        mirror.append_turn_update("session", 1, &operation_id, json!({ "sessionUpdate": "tool_call", "toolCallId": "tool", "title": "Run", "status": "in_progress" })).unwrap();
        mirror
            .complete_turn(
                "session",
                1,
                &operation_id,
                json!({ "stopReason": "cancelled" }),
            )
            .unwrap();
        mirror.append_session_update("session", 1, json!({ "sessionUpdate": "tool_call_update", "toolCallId": "tool", "status": "completed" })).unwrap();
        let reconciling = mirror.view("session", 1).unwrap();
        assert!(reconciling.baseline.updates().is_empty());
        assert_eq!(
            reconciling.session.active_turn.unwrap().terminal,
            Some(json!({ "stopReason": "cancelled" }))
        );
        let baseline = mirror
            .commit_completed_turn_from_memory("session", 1, &operation_id)
            .unwrap();
        assert_eq!(baseline.updates()[1]["status"], "completed");
        mirror.begin_load("session", 1, "load").unwrap();
        assert_eq!(
            mirror.append_session_update("session", 1, json!({})),
            Err(MirrorError::WrongPhase)
        );
        assert_eq!(
            mirror.append_session_update("session", 2, json!({})),
            Err(MirrorError::StaleIncarnation)
        );
        assert_eq!(
            mirror.view("session", 1).unwrap().baseline.revision(),
            baseline.revision()
        );
    }

    #[test]
    fn completed_turn_uses_agent_prompt_echo_once_and_keeps_its_identity() {
        let mut mirror = mirror();
        mirror.register_new("session", 1);
        for intent in ["first", "second"] {
            let revision = mirror
                .view("session", 1)
                .unwrap()
                .baseline
                .revision()
                .to_string();
            let prompt = vec![
                json!({ "type": "text", "text": "repeat" }),
                json!({ "type": "resource_link", "uri": "file:///file", "name": "file" }),
            ];
            let TurnAdmission::Accepted { operation_id } = mirror
                .start_turn("session", 1, &revision, intent, prompt.clone())
                .unwrap()
            else {
                panic!("new intent")
            };
            for content in prompt {
                mirror.append_turn_update("session", 1, &operation_id, json!({ "sessionUpdate": "user_message_chunk", "messageId": intent, "content": content })).unwrap();
            }
            mirror.append_turn_update("session", 1, &operation_id, json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "done" } })).unwrap();
            mirror
                .complete_turn(
                    "session",
                    1,
                    &operation_id,
                    json!({ "stopReason": "end_turn" }),
                )
                .unwrap();
            mirror
                .commit_completed_turn_from_memory("session", 1, &operation_id)
                .unwrap();
        }
        let view = mirror.view("session", 1).unwrap();
        assert_eq!(view.baseline.updates().len(), 6);
        assert_eq!(view.baseline.updates()[0]["messageId"], "first");
        assert_eq!(view.baseline.updates()[3]["messageId"], "second");
        assert_eq!(
            view.session
                .turn_outcomes
                .iter()
                .map(|outcome| outcome.after_update)
                .collect::<Vec<_>>(),
            vec![3, 6]
        );
    }

    fn mirror() -> SessionMirror {
        SessionMirror::new("epoch")
    }

    fn load_initial(mirror: &mut SessionMirror, session_id: &str, incarnation: u64) {
        mirror.register_cold(session_id, incarnation);
        mirror
            .begin_load(session_id, incarnation, "initial")
            .unwrap();
        mirror
            .append_load_update(
                session_id,
                incarnation,
                "initial",
                json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "old" } }),
            )
            .unwrap();
        mirror
            .commit_load(session_id, incarnation, "initial")
            .unwrap();
    }

    fn text_prompt(text: &str) -> Vec<Value> {
        vec![json!({ "type": "text", "text": text })]
    }

    fn user_update(text: &str) -> Value {
        json!({
            "sessionUpdate": "user_message_chunk",
            "content": { "type": "text", "text": text }
        })
    }

    #[test]
    fn prompt_terminal_retains_overlay_until_authoritative_load_commits() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session", 1);
        let revision = mirror
            .state("session")
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        let operation = match mirror
            .start_turn(
                "session",
                1,
                &revision,
                "intent",
                vec![json!({ "type": "text", "text": "next" })],
            )
            .unwrap()
        {
            TurnAdmission::Accepted { operation_id } => operation_id,
            _ => unreachable!(),
        };
        mirror
            .append_turn_update(
                "session",
                1,
                &operation,
                json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "answer" } }),
            )
            .unwrap();
        mirror
            .complete_turn(
                "session",
                1,
                &operation,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();

        let state = mirror.state("session").unwrap();
        assert_eq!(state.phase, MirrorPhase::Reconciling);
        assert_eq!(state.active_turn.as_ref().unwrap().updates.len(), 1);
        assert_eq!(
            state.active_turn.as_ref().unwrap().terminal,
            Some(json!({ "stopReason": "end_turn" }))
        );
    }

    #[test]
    fn observer_during_running_or_reconciling_reads_same_baseline_plus_overlay() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session", 1);
        let revision = mirror
            .state("session")
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        let operation = match mirror
            .start_turn("session", 1, &revision, "intent", text_prompt("prompt"))
            .unwrap()
        {
            TurnAdmission::Accepted { operation_id } => operation_id,
            _ => unreachable!(),
        };
        let running = mirror.view("session", 1).unwrap();
        mirror
            .complete_turn(
                "session",
                1,
                &operation,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        let reconciling = mirror.view("session", 1).unwrap();

        assert!(Arc::ptr_eq(&running.baseline, &reconciling.baseline));
        assert!(running.session.active_turn.is_some());
        assert!(reconciling.session.active_turn.is_some());
    }

    #[test]
    fn successful_reconcile_atomically_advances_baseline_and_drops_overlay() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session", 1);
        let old = mirror.view("session", 1).unwrap().baseline;
        let revision = old.revision().to_string();
        let operation = match mirror
            .start_turn("session", 1, &revision, "intent", text_prompt("next"))
            .unwrap()
        {
            TurnAdmission::Accepted { operation_id } => operation_id,
            _ => unreachable!(),
        };
        mirror
            .append_turn_update(
                "session",
                1,
                &operation,
                json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "new" } }),
            )
            .unwrap();
        mirror
            .complete_turn(
                "session",
                1,
                &operation,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        mirror.begin_load("session", 1, "reconcile").unwrap();
        for update in old.updates() {
            mirror
                .append_load_update("session", 1, "reconcile", update.clone())
                .unwrap();
        }
        mirror
            .append_load_update("session", 1, "reconcile", user_update("next"))
            .unwrap();
        mirror
            .append_load_update(
                "session",
                1,
                "reconcile",
                json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "new" } }),
            )
            .unwrap();
        let next = mirror.commit_load("session", 1, "reconcile").unwrap();

        let state = mirror.state("session").unwrap();
        assert_eq!(state.phase, MirrorPhase::Ready);
        assert!(state.active_turn.is_none());
        assert_ne!(next.revision(), revision);
        assert!(!Arc::ptr_eq(&old, &next));
    }

    #[test]
    fn completed_turn_can_atomically_advance_the_in_memory_baseline_without_load() {
        let mut mirror = mirror();
        mirror.register_new("session", 1);
        let old = mirror.view("session", 1).unwrap().baseline;
        let revision = old.revision().to_string();
        let prompt = text_prompt("next");
        let operation = match mirror
            .start_turn("session", 1, &revision, "intent", prompt.clone())
            .unwrap()
        {
            TurnAdmission::Accepted { operation_id } => operation_id,
            _ => unreachable!(),
        };
        for text in ["new ", "answer"] {
            mirror
                .append_turn_update(
                    "session",
                    1,
                    &operation,
                    json!({
                        "sessionUpdate": "agent_message_chunk",
                        "messageId": "answer",
                        "content": { "type": "text", "text": text }
                    }),
                )
                .unwrap();
        }
        mirror
            .complete_turn(
                "session",
                1,
                &operation,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();

        let next = mirror
            .commit_completed_turn_from_memory("session", 1, &operation)
            .unwrap();

        assert_eq!(
            next.updates(),
            &[
                json!({
                    "sessionUpdate": "user_message_chunk",
                    "content": { "type": "text", "text": "next" },
                    "_meta": {
                        "attyd": {
                            "turnOperationId": operation,
                        }
                    },
                }),
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "answer",
                    "content": { "type": "text", "text": "new answer" }
                }),
            ]
        );
        assert_ne!(next.revision(), revision);
        let state = mirror.state("session").unwrap();
        assert_eq!(state.phase, MirrorPhase::Ready);
        assert!(state.active_turn.is_none());
        assert_eq!(state.history_revision.as_deref(), Some(next.revision()));
        assert_eq!(state.turn_outcomes.len(), 1);
        assert_eq!(state.turn_outcomes[0].operation_id, operation);
        assert_eq!(state.turn_outcomes[0].after_update, 2);
        assert_eq!(
            state.turn_outcomes[0].response,
            json!({ "stopReason": "end_turn" })
        );
        assert!(matches!(
            mirror.start_turn("session", 1, next.revision(), "intent", prompt),
            Ok(TurnAdmission::Duplicate { operation_id }) if operation_id == operation
        ));

        let second_operation = match mirror
            .start_turn(
                "session",
                1,
                next.revision(),
                "second-intent",
                text_prompt("again"),
            )
            .unwrap()
        {
            TurnAdmission::Accepted { operation_id } => operation_id,
            _ => unreachable!(),
        };
        mirror
            .append_turn_update(
                "session",
                1,
                &second_operation,
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "second-answer",
                    "content": { "type": "text", "text": "done" }
                }),
            )
            .unwrap();
        mirror
            .complete_turn(
                "session",
                1,
                &second_operation,
                json!({ "stopReason": "max_tokens" }),
            )
            .unwrap();
        let final_snapshot = mirror
            .commit_completed_turn_from_memory("session", 1, &second_operation)
            .unwrap();
        let state = mirror.state("session").unwrap();
        assert_eq!(state.turn_outcomes.len(), 2);
        assert_eq!(state.turn_outcomes[0].after_update, 2);
        assert_eq!(state.turn_outcomes[1].after_update, 4);
        assert_eq!(final_snapshot.updates().len(), 4);
        let view = mirror.view_value("session", 1).unwrap();
        assert_eq!(view["session"]["turnOutcomes"].as_array().unwrap().len(), 2);

        mirror.begin_load("session", 1, "fresh-load").unwrap();
        for update in final_snapshot.updates() {
            mirror
                .append_load_update("session", 1, "fresh-load", update.clone())
                .unwrap();
        }
        mirror.commit_load("session", 1, "fresh-load").unwrap();
        assert!(mirror.state("session").unwrap().turn_outcomes.is_empty());
    }

    #[test]
    fn released_terminal_output_survives_commit_without_rewriting_agent_fields() {
        let mut mirror = mirror();
        mirror.register_new("session", 1);
        let revision = mirror
            .view("session", 1)
            .unwrap()
            .baseline
            .revision()
            .to_string();
        let operation = match mirror
            .start_turn("session", 1, &revision, "intent", text_prompt("run"))
            .unwrap()
        {
            TurnAdmission::Accepted { operation_id } => operation_id,
            _ => unreachable!(),
        };
        mirror
            .append_turn_update(
                "session",
                1,
                &operation,
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": "tool",
                    "title": "shell",
                    "status": "completed",
                    "content": [{ "type": "terminal", "terminalId": "terminal" }],
                    "rawOutput": { "agent": "original output" },
                }),
            )
            .unwrap();

        assert!(
            mirror
                .retain_terminal_output(
                    "session",
                    1,
                    "terminal",
                    &json!({
                        "output": "tool output",
                        "truncated": false,
                        "exitStatus": { "exitCode": 0 },
                        "released": true,
                    }),
                )
                .unwrap()
        );
        assert!(
            !mirror
                .retain_terminal_output(
                    "session",
                    1,
                    "terminal",
                    &json!({ "output": "must not overwrite Agent output" }),
                )
                .unwrap()
        );
        mirror
            .complete_turn(
                "session",
                1,
                &operation,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        let committed = mirror
            .commit_completed_turn_from_memory("session", 1, &operation)
            .unwrap();
        let tool = committed
            .updates()
            .iter()
            .find(|update| update["sessionUpdate"] == "tool_call")
            .unwrap();
        assert_eq!(tool["rawOutput"], json!({ "agent": "original output" }));
        let view = mirror.view_value("session", 1).unwrap();
        assert_eq!(view["terminals"]["terminal"]["output"], "tool output");
        assert_eq!(view["terminals"]["terminal"]["exitStatus"]["exitCode"], 0);
        assert_eq!(view["terminals"]["terminal"]["sessionId"], "session");
        assert_eq!(view["terminals"]["terminal"]["terminalId"], "terminal");
        assert!(view["session"].get("terminals").is_none());
        assert_eq!(mirror.overlay_bytes, 0);
    }

    #[test]
    fn terminal_released_after_its_turn_is_retained_only_for_matching_history() {
        let mut mirror = mirror();
        mirror.register_new("session", 1);
        mirror.begin_load("session", 1, "history").unwrap();
        mirror
            .append_load_update(
                "session",
                1,
                "history",
                json!({
                    "sessionUpdate": "tool_call", "toolCallId": "tool", "title": "run",
                    "content": [{ "type": "terminal", "terminalId": "terminal" }],
                }),
            )
            .unwrap();
        mirror.commit_load("session", 1, "history").unwrap();
        let output = json!({ "output": "retained", "released": true });
        assert!(
            !mirror
                .retain_terminal_output("session", 1, "unreferenced", &output)
                .unwrap()
        );
        assert!(
            !mirror
                .retain_terminal_output(
                    "session",
                    1,
                    "terminal",
                    &json!({
                        "output": "", "outputAppend": true, "released": true,
                    })
                )
                .unwrap()
        );
        assert!(
            mirror
                .retain_terminal_output("session", 1, "terminal", &output)
                .unwrap()
        );
        let view = mirror.view_value("session", 1).unwrap();
        assert_eq!(view["terminals"]["terminal"]["output"], "retained");
        assert!(view["baseline"]["updates"][0].get("rawOutput").is_none());
        assert_eq!(view["terminals"].as_object().unwrap().len(), 1);

        mirror.begin_load("session", 1, "replacement").unwrap();
        mirror.commit_load("session", 1, "replacement").unwrap();
        assert_eq!(
            mirror.view_value("session", 1).unwrap()["terminals"],
            json!({})
        );
        assert_eq!(mirror.retained_terminal_bytes, 0);
    }

    #[test]
    fn retained_terminal_output_is_accounted_and_removed_with_its_incarnation() {
        let mut mirror = mirror();
        mirror.register_new("session", 1);
        mirror.begin_load("session", 1, "history").unwrap();
        mirror
            .append_load_update(
                "session",
                1,
                "history",
                json!({
                    "sessionUpdate": "tool_call", "toolCallId": "tool", "title": "run",
                    "content": [{ "type": "terminal", "terminalId": "terminal" }],
                }),
            )
            .unwrap();
        mirror.commit_load("session", 1, "history").unwrap();
        mirror
            .retain_terminal_output(
                "session",
                1,
                "terminal",
                &json!({
                    "output": "retained", "released": true,
                }),
            )
            .unwrap();
        assert!(mirror.retained_terminal_bytes >= "retained".len());
        mirror.remove("session", 2);
        assert!(mirror.retained_terminal_bytes > 0);
        mirror.register_new("session", 2);
        assert_eq!(mirror.retained_terminal_bytes, 0);
        assert!(mirror.retained_terminals.is_empty());
        assert_eq!(
            mirror.view_value("session", 2).unwrap()["terminals"],
            json!({})
        );
        assert_eq!(
            mirror.retain_terminal_output("session", 1, "terminal", &json!({})),
            Err(MirrorError::StaleIncarnation)
        );
    }

    #[test]
    fn rejected_in_memory_commit_preserves_the_completed_overlay_and_old_baseline() {
        let mut mirror = mirror();
        mirror.register_new("session", 1);
        let old = mirror.view("session", 1).unwrap().baseline;
        let operation = match mirror
            .start_turn("session", 1, old.revision(), "intent", text_prompt("next"))
            .unwrap()
        {
            TurnAdmission::Accepted { operation_id } => operation_id,
            _ => unreachable!(),
        };
        mirror
            .complete_turn(
                "session",
                1,
                &operation,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();

        assert!(matches!(
            mirror.commit_completed_turn_from_memory("session", 1, "stale-operation"),
            Err(MirrorError::OperationMismatch)
        ));
        let retained = mirror.view("session", 1).unwrap();
        assert!(Arc::ptr_eq(&retained.baseline, &old));
        assert_eq!(retained.session.phase, MirrorPhase::Reconciling);
        assert_eq!(
            retained
                .session
                .active_turn
                .as_ref()
                .map(|turn| turn.operation_id.as_str()),
            Some(operation.as_str())
        );
    }

    #[test]
    fn reconciliation_cannot_drop_a_completed_agent_answer() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session", 1);
        let revision = mirror
            .state("session")
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        let operation = match mirror
            .start_turn("session", 1, &revision, "intent", text_prompt("next"))
            .unwrap()
        {
            TurnAdmission::Accepted { operation_id } => operation_id,
            _ => unreachable!(),
        };
        mirror
            .append_turn_update(
                "session",
                1,
                &operation,
                json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "answer" } }),
            )
            .unwrap();
        mirror
            .complete_turn(
                "session",
                1,
                &operation,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        mirror.begin_load("session", 1, "reconcile").unwrap();
        mirror
            .append_load_update("session", 1, "reconcile", user_update("next"))
            .unwrap();

        assert!(matches!(
            mirror.commit_load("session", 1, "reconcile"),
            Err(MirrorError::InconsistentHistory)
        ));
        assert!(mirror.state("session").unwrap().active_turn.is_some());
    }

    #[test]
    fn reconciliation_cannot_drop_a_completed_tool_result() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session", 1);
        let revision = mirror
            .state("session")
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        let operation = match mirror
            .start_turn("session", 1, &revision, "intent", text_prompt("next"))
            .unwrap()
        {
            TurnAdmission::Accepted { operation_id } => operation_id,
            _ => unreachable!(),
        };
        mirror
            .append_turn_update(
                "session",
                1,
                &operation,
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": "tool-1",
                    "title": "read",
                    "status": "completed",
                    "content": [{ "type": "content", "content": { "type": "text", "text": "result" } }]
                }),
            )
            .unwrap();
        mirror
            .complete_turn(
                "session",
                1,
                &operation,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        mirror.begin_load("session", 1, "reconcile").unwrap();
        mirror
            .append_load_update("session", 1, "reconcile", user_update("next"))
            .unwrap();

        assert!(matches!(
            mirror.commit_load("session", 1, "reconcile"),
            Err(MirrorError::InconsistentHistory)
        ));
    }

    #[test]
    fn reconcile_failure_keeps_old_baseline_and_completed_overlay() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session", 1);
        let old = mirror.view("session", 1).unwrap().baseline;
        let revision = old.revision().to_string();
        let operation = match mirror
            .start_turn("session", 1, &revision, "intent", text_prompt("next"))
            .unwrap()
        {
            TurnAdmission::Accepted { operation_id } => operation_id,
            _ => unreachable!(),
        };
        mirror
            .complete_turn(
                "session",
                1,
                &operation,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        mirror.begin_load("session", 1, "failed").unwrap();
        mirror
            .append_load_update("session", 1, "failed", json!({ "partial": true }))
            .unwrap();
        mirror
            .fail_load("session", 1, "failed", "temporary", true)
            .unwrap();

        let view = mirror.view("session", 1).unwrap();
        assert!(Arc::ptr_eq(&old, &view.baseline));
        assert_eq!(view.session.phase, MirrorPhase::Reconciling);
        assert!(view.session.active_turn.is_some());
        assert_eq!(view.session.sync_error.as_deref(), Some("temporary"));
    }

    #[test]
    fn stale_base_and_second_turn_while_running_are_rejected() {
        let mut mirror = mirror();
        mirror.register_new("session", 1);
        let revision = mirror
            .state("session")
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        mirror
            .start_turn("session", 1, &revision, "first", vec![json!("one")])
            .unwrap();

        assert_eq!(
            mirror.start_turn("session", 1, &revision, "second", vec![json!("two")]),
            Err(MirrorError::WrongPhase)
        );
        assert_eq!(
            mirror.start_turn("session", 1, "old", "third", vec![json!("three")]),
            Err(MirrorError::WrongPhase)
        );
    }

    #[test]
    fn duplicate_intent_never_dispatches_twice_before_or_after_fast_reconcile() {
        let mut mirror = mirror();
        mirror.register_new("session", 1);
        let revision = mirror
            .state("session")
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        let prompt = vec![json!({ "type": "text", "text": "same" })];
        let operation = match mirror
            .start_turn("session", 1, &revision, "intent", prompt.clone())
            .unwrap()
        {
            TurnAdmission::Accepted { operation_id } => operation_id,
            _ => unreachable!(),
        };
        assert_eq!(
            mirror.start_turn("session", 1, &revision, "intent", prompt.clone()),
            Ok(TurnAdmission::Duplicate {
                operation_id: operation.clone()
            })
        );
        assert_eq!(
            mirror.start_turn("session", 1, &revision, "intent", text_prompt("changed")),
            Err(MirrorError::IdempotencyConflict)
        );
        mirror
            .complete_turn(
                "session",
                1,
                &operation,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        mirror.begin_load("session", 1, "load").unwrap();
        mirror
            .append_load_update("session", 1, "load", user_update("same"))
            .unwrap();
        mirror
            .append_load_update(
                "session",
                1,
                "load",
                json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "done" } }),
            )
            .unwrap();
        let successor = mirror.commit_load("session", 1, "load").unwrap();

        assert!(matches!(
            mirror.start_turn("session", 1, successor.revision(), "intent", prompt),
            Ok(TurnAdmission::Duplicate { .. })
        ));
        assert_eq!(
            mirror.start_turn(
                "session",
                1,
                successor.revision(),
                "intent",
                text_prompt("changed")
            ),
            Err(MirrorError::IdempotencyConflict)
        );
    }

    #[test]
    fn consumed_intent_remains_non_dispatchable_after_exact_metadata_is_evicted() {
        let mut mirror = mirror();
        mirror.register_new("session", 1);
        let revision = mirror
            .state("session")
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        let prompt = text_prompt("same");
        let operation = match mirror
            .start_turn("session", 1, &revision, "old-intent", prompt.clone())
            .unwrap()
        {
            TurnAdmission::Accepted { operation_id } => operation_id,
            _ => unreachable!(),
        };
        mirror
            .complete_turn(
                "session",
                1,
                &operation,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        mirror.begin_load("session", 1, "load").unwrap();
        mirror
            .append_load_update("session", 1, "load", user_update("same"))
            .unwrap();
        let successor = mirror.commit_load("session", 1, "load").unwrap();

        mirror
            .sessions
            .get_mut("session")
            .unwrap()
            .recent_consumptions
            .clear();
        assert_eq!(
            mirror.start_turn("session", 1, successor.revision(), "old-intent", prompt,),
            Err(MirrorError::IdempotencyConflict)
        );
    }

    #[test]
    fn candidate_missing_the_previous_baseline_never_commits() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session", 1);
        let revision = mirror
            .state("session")
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        let operation = match mirror
            .start_turn("session", 1, &revision, "intent", text_prompt("prompt"))
            .unwrap()
        {
            TurnAdmission::Accepted { operation_id } => operation_id,
            _ => unreachable!(),
        };
        mirror
            .complete_turn(
                "session",
                1,
                &operation,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        mirror.begin_load("session", 1, "stale").unwrap();
        mirror
            .append_load_update("session", 1, "stale", json!({ "different": true }))
            .unwrap();

        assert!(matches!(
            mirror.commit_load("session", 1, "stale"),
            Err(MirrorError::InconsistentHistory)
        ));
        assert_eq!(
            mirror.state("session").unwrap().phase,
            MirrorPhase::Reconciling
        );
    }

    #[test]
    fn retryable_initial_load_can_start_a_new_single_flight_attempt() {
        let mut mirror = mirror();
        mirror.register_cold("session", 1);
        mirror.begin_load("session", 1, "first").unwrap();
        mirror
            .append_load_update("session", 1, "first", user_update("partial"))
            .unwrap();
        mirror
            .fail_load("session", 1, "first", "temporary", true)
            .unwrap();

        mirror.begin_load("session", 1, "second").unwrap();
        mirror
            .append_load_update("session", 1, "second", user_update("complete"))
            .unwrap();
        let snapshot = mirror.commit_load("session", 1, "second").unwrap();

        assert_eq!(snapshot.updates(), &[user_update("complete")]);
        assert_eq!(mirror.state("session").unwrap().phase, MirrorPhase::Ready);
    }

    #[test]
    fn invalidated_load_attempt_cannot_commit_a_truncated_replay() {
        let mut mirror = mirror();
        mirror.register_cold("session", 1);
        mirror.begin_load("session", 1, "invalid").unwrap();
        mirror
            .append_load_update("session", 1, "invalid", user_update("retained"))
            .unwrap();
        mirror.invalidate_load("session", 1, "invalid").unwrap();

        assert!(matches!(
            mirror.append_load_update("session", 1, "invalid", user_update("ignored")),
            Err(MirrorError::History(HistoryCacheError::InvalidReplay))
        ));
        assert!(matches!(
            mirror.commit_load("session", 1, "invalid"),
            Err(MirrorError::History(HistoryCacheError::InvalidReplay))
        ));
        mirror
            .fail_load("session", 1, "invalid", "invalid replay", false)
            .unwrap();
        assert_eq!(mirror.state("session").unwrap().phase, MirrorPhase::Blocked);
    }

    #[test]
    fn reconciliation_compares_normalized_history_not_text_chunk_boundaries() {
        let mut mirror = mirror();
        mirror.register_cold("session", 1);
        mirror.begin_load("session", 1, "initial").unwrap();
        for text in ["old ", "answer"] {
            mirror
                .append_load_update(
                    "session",
                    1,
                    "initial",
                    json!({
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": text }
                    }),
                )
                .unwrap();
        }
        let old = mirror.commit_load("session", 1, "initial").unwrap();
        assert_eq!(old.updates().len(), 1);
        let operation = match mirror
            .start_turn("session", 1, old.revision(), "intent", text_prompt("next"))
            .unwrap()
        {
            TurnAdmission::Accepted { operation_id } => operation_id,
            _ => unreachable!(),
        };
        mirror
            .complete_turn(
                "session",
                1,
                &operation,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        mirror.begin_load("session", 1, "reconcile").unwrap();
        mirror
            .append_load_update(
                "session",
                1,
                "reconcile",
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "old answer" }
                }),
            )
            .unwrap();
        mirror
            .append_load_update("session", 1, "reconcile", user_update("next"))
            .unwrap();

        assert!(mirror.commit_load("session", 1, "reconcile").is_ok());
    }

    #[test]
    fn reconciliation_does_not_require_optional_message_ids_to_survive_reload() {
        let mut mirror = mirror();
        mirror.register_cold("session", 1);
        mirror.begin_load("session", 1, "initial").unwrap();
        mirror
            .append_load_update(
                "session",
                1,
                "initial",
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "connection-one",
                    "content": { "type": "text", "text": "old answer" }
                }),
            )
            .unwrap();
        let old = mirror.commit_load("session", 1, "initial").unwrap();
        let operation = match mirror
            .start_turn("session", 1, old.revision(), "intent", text_prompt("next"))
            .unwrap()
        {
            TurnAdmission::Accepted { operation_id } => operation_id,
            _ => unreachable!(),
        };
        mirror
            .complete_turn(
                "session",
                1,
                &operation,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        mirror.begin_load("session", 1, "reconcile").unwrap();
        mirror
            .append_load_update(
                "session",
                1,
                "reconcile",
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "connection-two",
                    "content": { "type": "text", "text": "old answer" }
                }),
            )
            .unwrap();
        mirror
            .append_load_update("session", 1, "reconcile", user_update("next"))
            .unwrap();

        assert!(mirror.commit_load("session", 1, "reconcile").is_ok());
    }

    #[test]
    fn reconciliation_allows_repartitioned_text_with_reassigned_message_ids() {
        let mut mirror = mirror();
        mirror.register_cold("session", 1);
        mirror.begin_load("session", 1, "initial").unwrap();
        for (message_id, text) in [("old-a", "old "), ("old-b", "answer")] {
            mirror
                .append_load_update(
                    "session",
                    1,
                    "initial",
                    json!({
                        "sessionUpdate": "agent_message_chunk",
                        "messageId": message_id,
                        "content": { "type": "text", "text": text }
                    }),
                )
                .unwrap();
        }
        let old = mirror.commit_load("session", 1, "initial").unwrap();
        assert_eq!(old.updates().len(), 2);
        assert_eq!(old.updates()[0]["messageId"], "old-a");
        assert_eq!(old.updates()[1]["messageId"], "old-b");
        let operation = match mirror
            .start_turn("session", 1, old.revision(), "intent", text_prompt("next"))
            .unwrap()
        {
            TurnAdmission::Accepted { operation_id } => operation_id,
            _ => unreachable!(),
        };
        mirror
            .complete_turn(
                "session",
                1,
                &operation,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        mirror.begin_load("session", 1, "reconcile").unwrap();
        mirror
            .append_load_update(
                "session",
                1,
                "reconcile",
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "new-id",
                    "content": { "type": "text", "text": "old answer" }
                }),
            )
            .unwrap();
        mirror
            .append_load_update("session", 1, "reconcile", user_update("next"))
            .unwrap();

        assert!(mirror.commit_load("session", 1, "reconcile").is_ok());
    }

    #[test]
    fn overlay_accounting_does_not_reject_a_large_valid_turn() {
        let mut mirror = mirror();
        mirror.register_new("session", 1);
        let revision = mirror
            .state("session")
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        let operation = match mirror
            .start_turn("session", 1, &revision, "intent", text_prompt("prompt"))
            .unwrap()
        {
            TurnAdmission::Accepted { operation_id } => operation_id,
            _ => unreachable!(),
        };
        mirror
            .append_turn_update(
                "session",
                1,
                &operation,
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "x".repeat(9_000) }
                }),
            )
            .unwrap();
        assert!(mirror.overlay_bytes > 8_192);
        assert_eq!(
            mirror
                .state("session")
                .unwrap()
                .active_turn
                .as_ref()
                .unwrap()
                .updates
                .len(),
            1
        );

        mirror.abort_turn("session", 1, &operation).unwrap();
        assert_eq!(mirror.overlay_bytes, 0);
    }

    #[test]
    fn stale_incarnation_removal_cannot_drop_the_current_session() {
        let mut mirror = mirror();
        mirror.register_new("session", 2);
        let revision = mirror
            .state("session")
            .unwrap()
            .history_revision
            .clone()
            .unwrap();

        mirror.remove("session", 1);

        assert_eq!(
            mirror.state("session").unwrap().history_revision.as_deref(),
            Some(revision.as_str())
        );
        assert!(mirror.view("session", 2).is_ok());
    }
}
