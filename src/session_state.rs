use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::history_cache::HistoryCacheError;
use crate::runtime_state::{
    SessionLifecycle, SessionLiveState, SessionOperationKind, SessionOperationState,
    fold_active_turn_update,
};

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
    pub prompt: Arc<Vec<Value>>,
    pub updates: Arc<Vec<Value>>,
    pub terminal: Option<Value>,
    #[serde(skip)]
    pub(crate) execution: Option<TurnExecution>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnExecution {
    pub(crate) rpc_operation_id: String,
    pub(crate) cancel_requested: bool,
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
pub(crate) struct SessionState {
    pub session_id: String,
    pub incarnation: u64,
    #[serde(skip)]
    pub(crate) live: Option<SessionLiveState>,
    pub view_revision: u64,
    pub history_revision: Option<String>,
    pub phase: MirrorPhase,
    pub active_turn: Option<TurnOverlay>,
    pub turn_outcomes: Vec<CompletedTurnOutcome>,
    pub sync_error: Option<String>,
    #[serde(default)]
    pub history_notice: Option<String>,
    #[serde(skip)]
    pub(crate) operation: Option<SessionOperationState>,
    #[serde(skip)]
    active_payload_digest: Option<[u8; 32]>,
    #[serde(skip)]
    consumed_intents: HashMap<String, LastConsumption>,
    #[serde(skip)]
    pub(crate) load_attempt: Option<String>,
    #[serde(skip)]
    pub(crate) load_origin: Option<MirrorPhase>,
    #[serde(skip)]
    pub(crate) active_overlay_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LastConsumption {
    operation_id: String,
    payload_digest: [u8; 32],
    consumed_revision: String,
    successor_revision: String,
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

/// Business intents checked by the session owner before dispatching Agent I/O.
/// Prompt occupies the active turn; the other intents occupy one exclusive slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionAdmission {
    Prompt,
    Control,
    Fork,
    Close,
    Delete,
    Attachment,
}

impl SessionState {
    /// Observation cannot resume after confirmed close, including the interval
    /// where Delete still owns cleanup. A direct delete without close remains
    /// observable until its Agent response confirms retirement.
    pub(crate) fn observation_retired(&self) -> bool {
        self.operation
            .as_ref()
            .is_some_and(|operation| operation.stage == "cleanup")
            || self.live.as_ref().is_some_and(|live| {
                live.lifecycle == SessionLifecycle::Closed
                    || (live.lifecycle == SessionLifecycle::Deleting
                        && self.operation.as_ref().is_some_and(|operation| {
                            operation.kind == SessionOperationKind::Delete
                                && operation.stage == "deleting"
                        }))
            })
    }

    pub(crate) fn turn_execution(&self) -> Option<&TurnExecution> {
        self.active_turn
            .as_ref()
            .and_then(|turn| turn.execution.as_ref())
    }

    pub(crate) fn cold(session_id: impl Into<String>, incarnation: u64) -> Self {
        Self {
            session_id: session_id.into(),
            incarnation,
            live: None,
            view_revision: 1,
            history_revision: None,
            phase: MirrorPhase::Cold,
            active_turn: None,
            turn_outcomes: Vec::new(),
            sync_error: None,
            history_notice: None,
            operation: None,
            active_payload_digest: None,
            consumed_intents: HashMap::new(),
            load_attempt: None,
            load_origin: None,
            active_overlay_bytes: 0,
        }
    }

    pub(crate) fn ready(
        session_id: impl Into<String>,
        incarnation: u64,
        history_revision: String,
    ) -> Self {
        let mut session = Self::cold(session_id, incarnation);
        session.phase = MirrorPhase::Ready;
        session.history_revision = Some(history_revision);
        session
    }

    pub(crate) fn can_begin(&self, admission: SessionAdmission) -> Result<(), MirrorError> {
        if self.operation.is_some() || self.load_attempt.is_some() {
            return Err(MirrorError::WrongPhase);
        }
        let allowed = match self.phase {
            MirrorPhase::Ready => self.active_turn.is_none(),
            MirrorPhase::Running => {
                self.active_turn
                    .as_ref()
                    .is_some_and(|turn| turn.execution.is_some() && turn.terminal.is_none())
                    && matches!(
                        admission,
                        SessionAdmission::Control | SessionAdmission::Close
                    )
            }
            MirrorPhase::Cold => {
                self.active_turn.is_none()
                    && matches!(
                        admission,
                        SessionAdmission::Attachment | SessionAdmission::Delete
                    )
            }
            MirrorPhase::Blocked => match admission {
                SessionAdmission::Close => true,
                SessionAdmission::Delete => self.active_turn.is_none(),
                // Explicit recovery may retain a completed overlay. The load transaction
                // must still verify that overlay before installing authoritative replay.
                SessionAdmission::Attachment => self
                    .active_turn
                    .as_ref()
                    .is_none_or(|turn| turn.terminal.is_some()),
                SessionAdmission::Prompt | SessionAdmission::Control | SessionAdmission::Fork => {
                    false
                }
            },
            MirrorPhase::Loading | MirrorPhase::Reconciling => false,
        };
        if allowed {
            Ok(())
        } else {
            Err(MirrorError::WrongPhase)
        }
    }

    pub(crate) fn can_retry_cold_attachment(&self) -> bool {
        self.phase == MirrorPhase::Loading
            && self.active_turn.is_none()
            && self.load_attempt.is_none()
            && self.operation.is_none()
            && self.sync_error.is_some()
    }

    pub(crate) fn begin_exclusive(
        &mut self,
        kind: SessionOperationKind,
        operation_id: impl Into<String>,
    ) -> Result<(), MirrorError> {
        self.can_begin(kind.admission())?;
        self.operation = Some(SessionOperationState {
            kind,
            operation_id: operation_id.into(),
            stage: "reserved".to_string(),
            uncertainty_reason: None,
        });
        Ok(())
    }

    pub(crate) fn settle_exclusive(
        &mut self,
        kind: SessionAdmission,
        operation_id: &str,
    ) -> Result<(), MirrorError> {
        if self.operation.as_ref().is_none_or(|operation| {
            operation.kind.admission() != kind || operation.operation_id != operation_id
        }) {
            return Err(MirrorError::OperationMismatch);
        }
        self.operation = None;
        Ok(())
    }

    pub(crate) fn check_turn_admission(
        &self,
        expected_history_revision: &str,
        client_intent_id: &str,
        prompt: &[Value],
    ) -> Result<Option<TurnAdmission>, MirrorError> {
        let digest = payload_digest(prompt);
        if let Some(turn) = &self.active_turn
            && turn.client_intent_id == client_intent_id
        {
            return if self.active_payload_digest == Some(digest) {
                Ok(Some(TurnAdmission::Duplicate {
                    operation_id: turn.operation_id.clone(),
                }))
            } else {
                Err(MirrorError::IdempotencyConflict)
            };
        }
        if let Some(last) = self.consumed_intents.get(client_intent_id) {
            return if last.payload_digest == digest
                && (last.consumed_revision == expected_history_revision
                    || last.successor_revision == expected_history_revision)
            {
                Ok(Some(TurnAdmission::Duplicate {
                    operation_id: last.operation_id.clone(),
                }))
            } else {
                Err(MirrorError::IdempotencyConflict)
            };
        }
        self.can_begin(SessionAdmission::Prompt)?;
        if self.history_revision.as_deref() != Some(expected_history_revision) {
            return Err(MirrorError::StaleHistory);
        }
        Ok(None)
    }

    pub(crate) fn admit_turn(
        &mut self,
        operation_id: String,
        expected_history_revision: &str,
        client_intent_id: &str,
        prompt: Vec<Value>,
    ) -> Result<TurnAdmission, MirrorError> {
        if let Some(duplicate) =
            self.check_turn_admission(expected_history_revision, client_intent_id, &prompt)?
        {
            return Ok(duplicate);
        }
        let digest = payload_digest(&prompt);
        let overlay = TurnOverlay {
            operation_id: operation_id.clone(),
            client_intent_id: client_intent_id.to_string(),
            prompt: Arc::new(prompt),
            updates: Arc::new(Vec::new()),
            terminal: None,
            execution: Some(TurnExecution {
                rpc_operation_id: client_intent_id.to_string(),
                cancel_requested: false,
            }),
        };
        self.active_overlay_bytes = serialized_len(&overlay);
        self.phase = MirrorPhase::Running;
        self.active_payload_digest = Some(digest);
        self.active_turn = Some(overlay);
        self.sync_error = None;
        self.advance_revision();
        Ok(TurnAdmission::Accepted { operation_id })
    }

    pub(crate) fn append_turn_update(
        &mut self,
        operation_id: &str,
        update: Value,
    ) -> Result<(), MirrorError> {
        if self.phase != MirrorPhase::Running {
            return Err(MirrorError::WrongPhase);
        }
        let turn = self
            .active_turn
            .as_ref()
            .ok_or(MirrorError::OperationMismatch)?;
        if turn.operation_id != operation_id || turn.terminal.is_some() {
            return Err(MirrorError::OperationMismatch);
        }
        self.fold_overlay_update(update)
    }

    pub(crate) fn append_reconciling_update(&mut self, update: Value) -> Result<(), MirrorError> {
        if self.phase != MirrorPhase::Reconciling || self.load_attempt.is_some() {
            return Err(MirrorError::WrongPhase);
        }
        self.fold_overlay_update(update)
    }

    fn fold_overlay_update(&mut self, update: Value) -> Result<(), MirrorError> {
        let turn = self
            .active_turn
            .as_ref()
            .ok_or(MirrorError::OperationMismatch)?;
        let mut candidate = turn.clone();
        candidate.updates = Arc::new(
            fold_active_turn_update(&turn.updates, &update)
                .map_err(|_| MirrorError::InconsistentHistory)?,
        );
        self.active_overlay_bytes = serialized_len(&candidate);
        self.active_turn = Some(candidate);
        self.advance_revision();
        Ok(())
    }

    pub(crate) fn complete_turn(
        &mut self,
        operation_id: &str,
        terminal: Value,
    ) -> Result<(), MirrorError> {
        if self.phase != MirrorPhase::Running {
            return Err(MirrorError::WrongPhase);
        }
        let turn = self
            .active_turn
            .as_ref()
            .ok_or(MirrorError::OperationMismatch)?;
        if turn.operation_id != operation_id || turn.terminal.is_some() {
            return Err(MirrorError::OperationMismatch);
        }
        // Live interaction retirement must happen before this history transition.
        // History-only owners have no responders and can retire the execution here.
        if self.live.is_some() && turn.execution.is_some() {
            return Err(MirrorError::OperationMismatch);
        }
        let mut candidate = turn.clone();
        candidate.execution = None;
        candidate.terminal = Some(terminal);
        self.active_overlay_bytes = serialized_len(&candidate);
        self.active_turn = Some(candidate);
        self.phase = MirrorPhase::Reconciling;
        self.advance_revision();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn abort_turn(&mut self, operation_id: &str) -> Result<(), MirrorError> {
        if self.phase != MirrorPhase::Running
            || self
                .active_turn
                .as_ref()
                .is_none_or(|turn| turn.operation_id != operation_id)
        {
            return Err(MirrorError::OperationMismatch);
        }
        self.phase = MirrorPhase::Ready;
        self.active_overlay_bytes = 0;
        self.active_turn = None;
        self.active_payload_digest = None;
        self.advance_revision();
        Ok(())
    }

    pub(crate) fn block_turn(
        &mut self,
        operation_id: &str,
        message: impl Into<String>,
    ) -> Result<(), MirrorError> {
        if !matches!(self.phase, MirrorPhase::Running | MirrorPhase::Reconciling)
            || self
                .active_turn
                .as_ref()
                .is_none_or(|turn| turn.operation_id != operation_id)
        {
            return Err(MirrorError::OperationMismatch);
        }
        self.phase = MirrorPhase::Blocked;
        self.sync_error = Some(message.into());
        self.load_attempt = None;
        self.load_origin = None;
        self.advance_revision();
        Ok(())
    }

    // Check before changing HistoryCache: publication must not fail after its baseline swap.
    pub(crate) fn validate_history_commit(&self) -> Result<(), MirrorError> {
        if self.active_turn.is_some() && self.active_payload_digest.is_none() {
            return Err(MirrorError::InconsistentHistory);
        }
        Ok(())
    }

    // Called only after a successful local history transaction, without an intervening await.
    pub(crate) fn history_committed(
        &mut self,
        revision: String,
        after_update: usize,
        replace_outcomes: bool,
    ) {
        if replace_outcomes {
            // Authoritative replay has no historical PromptResponse offsets.
            self.turn_outcomes.clear();
            self.history_notice = None;
        } else if let Some(turn) = &self.active_turn
            && let Some(response) = turn
                .terminal
                .as_ref()
                .filter(|response| response.get("stopReason").and_then(Value::as_str).is_some())
        {
            self.turn_outcomes.push(CompletedTurnOutcome {
                operation_id: turn.operation_id.clone(),
                after_update,
                response: response.clone(),
            });
        }
        if let Some(turn) = self.active_turn.take()
            && let Some(consumed_revision) = self.history_revision.as_ref()
        {
            self.consumed_intents.insert(
                turn.client_intent_id,
                LastConsumption {
                    operation_id: turn.operation_id,
                    payload_digest: self
                        .active_payload_digest
                        .expect("history commit was validated"),
                    consumed_revision: consumed_revision.clone(),
                    successor_revision: revision.clone(),
                },
            );
        }
        self.history_revision = Some(revision);
        self.active_overlay_bytes = 0;
        self.phase = MirrorPhase::Ready;
        self.active_turn = None;
        self.active_payload_digest = None;
        self.load_attempt = None;
        self.load_origin = None;
        self.sync_error = None;
        self.advance_revision();
    }

    fn advance_revision(&mut self) {
        self.view_revision = self.view_revision.wrapping_add(1).max(1);
    }
}

fn payload_digest(prompt: &[Value]) -> [u8; 32] {
    Sha256::digest(serde_json::to_vec(prompt).expect("prompt values serialize")).into()
}

fn serialized_len(value: &impl Serialize) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |value| value.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ready() -> SessionState {
        SessionState::ready("session", 1, "history-1".to_string())
    }

    fn prompt(text: &str) -> Vec<Value> {
        vec![json!({ "type": "text", "text": text })]
    }

    fn update(text: &str) -> Value {
        json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": text },
        })
    }

    fn start(session: &mut SessionState) {
        assert_eq!(
            session.admit_turn("turn-1".into(), "history-1", "intent-1", prompt("hello")),
            Ok(TurnAdmission::Accepted {
                operation_id: "turn-1".into()
            }),
        );
    }

    #[test]
    fn rejected_operation_and_stale_cas_do_not_consume_the_intent() {
        let mut session = ready();
        session
            .begin_exclusive(SessionOperationKind::SetMode, "control")
            .unwrap();
        let before = session.clone();
        assert_eq!(
            session.admit_turn("turn-1".into(), "history-1", "intent-1", prompt("hello")),
            Err(MirrorError::WrongPhase),
        );
        assert_eq!(session, before);
        session
            .settle_exclusive(SessionAdmission::Control, "control")
            .unwrap();
        let before = session.clone();
        assert_eq!(
            session.admit_turn(
                "turn-1".into(),
                "stale-history",
                "intent-1",
                prompt("hello")
            ),
            Err(MirrorError::StaleHistory),
        );
        assert_eq!(session, before);
        start(&mut session);
        assert_eq!(
            session.admit_turn(
                "another-operation".into(),
                "history-1",
                "intent-1",
                prompt("hello")
            ),
            Ok(TurnAdmission::Duplicate {
                operation_id: "turn-1".into()
            }),
        );
        assert_eq!(
            session.admit_turn(
                "another-operation".into(),
                "history-1",
                "intent-1",
                prompt("changed")
            ),
            Err(MirrorError::IdempotencyConflict),
        );
        assert_eq!(
            session.admit_turn(
                "another-operation".into(),
                "history-1",
                "different-intent",
                prompt("hello")
            ),
            Err(MirrorError::WrongPhase),
        );
    }

    #[test]
    fn running_turn_allows_control_or_close_but_preserves_exclusion() {
        let mut session = ready();
        start(&mut session);
        for admission in [
            SessionAdmission::Prompt,
            SessionAdmission::Fork,
            SessionAdmission::Delete,
            SessionAdmission::Attachment,
        ] {
            assert_eq!(session.can_begin(admission), Err(MirrorError::WrongPhase));
        }
        session
            .begin_exclusive(SessionOperationKind::SetMode, "control")
            .unwrap();
        let before = session.clone();
        assert_eq!(
            session.begin_exclusive(SessionOperationKind::Close, "close"),
            Err(MirrorError::WrongPhase)
        );
        assert_eq!(
            session.settle_exclusive(SessionAdmission::Control, "wrong-owner"),
            Err(MirrorError::OperationMismatch)
        );
        assert_eq!(
            session.settle_exclusive(SessionAdmission::Close, "control"),
            Err(MirrorError::OperationMismatch)
        );
        assert_eq!(session, before);
        session
            .append_turn_update("turn-1", update("still running"))
            .unwrap();
        session
            .settle_exclusive(SessionAdmission::Control, "control")
            .unwrap();
        session
            .begin_exclusive(SessionOperationKind::Close, "close")
            .unwrap();
        session
            .complete_turn("turn-1", json!({ "stopReason": "end_turn" }))
            .unwrap();
        assert_eq!(session.phase, MirrorPhase::Reconciling);
        assert_eq!(
            session.operation.as_ref().unwrap().kind,
            SessionOperationKind::Close
        );
        assert!(session.active_turn.as_ref().unwrap().terminal.is_some());
    }

    #[test]
    fn history_commit_keeps_concurrent_control_and_idempotent_retries() {
        let mut session = ready();
        start(&mut session);
        session
            .begin_exclusive(SessionOperationKind::SetMode, "control")
            .unwrap();
        session
            .append_turn_update("turn-1", update("answer"))
            .unwrap();
        let overlay = session.active_turn.as_ref().unwrap().updates.clone();
        session
            .complete_turn("turn-1", json!({ "stopReason": "end_turn" }))
            .unwrap();
        assert!(Arc::ptr_eq(
            &overlay,
            &session.active_turn.as_ref().unwrap().updates
        ));
        session.validate_history_commit().unwrap();
        session.history_committed("history-2".into(), 2, false);
        assert_eq!(session.phase, MirrorPhase::Ready);
        assert!(session.active_turn.is_none());
        assert_eq!(session.active_overlay_bytes, 0);
        assert_eq!(session.turn_outcomes[0].after_update, 2);
        assert_eq!(
            session.can_begin(SessionAdmission::Prompt),
            Err(MirrorError::WrongPhase)
        );
        for revision in ["history-1", "history-2"] {
            assert_eq!(
                session.admit_turn(
                    "new-operation".into(),
                    revision,
                    "intent-1",
                    prompt("hello")
                ),
                Ok(TurnAdmission::Duplicate {
                    operation_id: "turn-1".into()
                }),
            );
        }
        session
            .settle_exclusive(SessionAdmission::Control, "control")
            .unwrap();
        assert!(matches!(
            session.admit_turn("turn-2".into(), "history-2", "intent-2", prompt("next")),
            Ok(TurnAdmission::Accepted { .. }),
        ));
    }

    #[test]
    fn late_operation_completion_cannot_release_a_new_owner() {
        let mut session = ready();
        session
            .begin_exclusive(SessionOperationKind::Fork, "first")
            .unwrap();
        session
            .settle_exclusive(SessionAdmission::Fork, "first")
            .unwrap();
        session
            .begin_exclusive(SessionOperationKind::Fork, "second")
            .unwrap();
        assert_eq!(
            session.settle_exclusive(SessionAdmission::Fork, "first"),
            Err(MirrorError::OperationMismatch)
        );
        assert_eq!(session.operation.as_ref().unwrap().operation_id, "second");
        assert_eq!(
            session.can_begin(SessionAdmission::Prompt),
            Err(MirrorError::WrongPhase)
        );
    }

    #[test]
    fn synchronization_phases_and_in_flight_load_exclude_new_mutations() {
        for phase in [MirrorPhase::Loading, MirrorPhase::Reconciling] {
            let mut session = ready();
            session.phase = phase;
            for admission in [
                SessionAdmission::Prompt,
                SessionAdmission::Control,
                SessionAdmission::Fork,
                SessionAdmission::Close,
                SessionAdmission::Delete,
                SessionAdmission::Attachment,
            ] {
                assert_eq!(
                    session.can_begin(admission),
                    Err(MirrorError::WrongPhase),
                    "{phase:?} {admission:?}"
                );
            }
        }
        let mut session = ready();
        session.load_attempt = Some("in-flight".into());
        assert_eq!(
            session.can_begin(SessionAdmission::Prompt),
            Err(MirrorError::WrongPhase)
        );
        assert_eq!(
            session.can_begin(SessionAdmission::Attachment),
            Err(MirrorError::WrongPhase)
        );
        let cold = SessionState::cold("cold", 1);
        assert_eq!(cold.can_begin(SessionAdmission::Attachment), Ok(()));
        assert_eq!(
            cold.can_begin(SessionAdmission::Prompt),
            Err(MirrorError::WrongPhase)
        );
    }

    #[test]
    fn blocked_recovery_preserves_completed_overlay_without_reopening_prompt_admission() {
        let mut session = ready();
        start(&mut session);
        session
            .append_turn_update("turn-1", update("answer"))
            .unwrap();
        session
            .complete_turn("turn-1", json!({ "stopReason": "end_turn" }))
            .unwrap();
        session.block_turn("turn-1", "commit failed").unwrap();
        let retained = session.active_turn.clone();
        for admission in [
            SessionAdmission::Prompt,
            SessionAdmission::Control,
            SessionAdmission::Fork,
            SessionAdmission::Delete,
        ] {
            assert_eq!(session.can_begin(admission), Err(MirrorError::WrongPhase));
        }
        session
            .begin_exclusive(SessionOperationKind::Load, "explicit-load")
            .unwrap();
        assert_eq!(session.phase, MirrorPhase::Blocked);
        assert_eq!(session.active_turn, retained);
        session
            .settle_exclusive(SessionAdmission::Attachment, "explicit-load")
            .unwrap();
        assert_eq!(session.can_begin(SessionAdmission::Close), Ok(()));
        let mut uncertain = ready();
        start(&mut uncertain);
        uncertain
            .block_turn("turn-1", "transport uncertain")
            .unwrap();
        assert_eq!(
            uncertain.can_begin(SessionAdmission::Attachment),
            Err(MirrorError::WrongPhase)
        );
        assert_eq!(uncertain.can_begin(SessionAdmission::Close), Ok(()));
    }

    #[test]
    fn stale_turn_results_and_invalid_updates_leave_state_unchanged() {
        let mut session = ready();
        start(&mut session);
        session
            .append_turn_update("turn-1", update("answer"))
            .unwrap();
        let before = session.clone();
        assert_eq!(
            session.append_turn_update("wrong-turn", update("late")),
            Err(MirrorError::OperationMismatch)
        );
        assert_eq!(
            session.complete_turn("wrong-turn", json!({ "stopReason": "end_turn" })),
            Err(MirrorError::OperationMismatch)
        );
        assert_eq!(
            session.abort_turn("wrong-turn"),
            Err(MirrorError::OperationMismatch)
        );
        assert_eq!(
            session.block_turn("wrong-turn", "late error"),
            Err(MirrorError::OperationMismatch)
        );
        assert_eq!(
            session.append_turn_update("turn-1", json!({ "sessionUpdate": "plan", "entries": 7 })),
            Err(MirrorError::InconsistentHistory)
        );
        assert_eq!(session, before);
        session.abort_turn("turn-1").unwrap();
        assert_eq!(session.history_revision.as_deref(), Some("history-1"));
        start(&mut session);
    }

    #[test]
    fn completed_intents_remain_exactly_idempotent_after_many_turns() {
        let mut session = ready();
        start(&mut session);
        session
            .complete_turn("turn-1", json!({ "stopReason": "end_turn" }))
            .unwrap();
        session.validate_history_commit().unwrap();
        session.history_committed("history-2".into(), 1, false);
        for index in 2..200 {
            let operation = format!("turn-{index}");
            let current = format!("history-{index}");
            session
                .admit_turn(
                    operation.clone(),
                    &current,
                    &format!("intent-{index}"),
                    prompt("hello"),
                )
                .unwrap();
            session
                .complete_turn(&operation, json!({ "stopReason": "end_turn" }))
                .unwrap();
            session.validate_history_commit().unwrap();
            session.history_committed(format!("history-{}", index + 1), 1, false);
        }
        assert_eq!(
            session.admit_turn(
                "another-operation".into(),
                "history-2",
                "intent-1",
                prompt("hello")
            ),
            Ok(TurnAdmission::Duplicate {
                operation_id: "turn-1".into()
            }),
        );
        assert_eq!(session.phase, MirrorPhase::Ready);
        assert!(session.active_turn.is_none());
    }
}
