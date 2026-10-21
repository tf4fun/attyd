use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::history_cache::HistoryCacheError;
use crate::protocol::registry::SessionLog;
use crate::protocol::timeline::{History, PromptEnd, Timeline};
use crate::runtime_state::{
    SessionLifecycle, SessionLiveState, SessionOperationKind, SessionOperationState,
    SharedTurnUpdates, fold_shared_active_turn_update,
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
    pub prompt: Arc<Vec<Value>>,
    pub updates: SharedTurnUpdates,
    pub terminal: Option<Value>,
    #[serde(skip)]
    pub(crate) execution: Option<TurnExecution>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnExecution {
    pub(crate) rpc_operation_id: String,
    pub(crate) cancel_requested: bool,
}

/// How a turn's terminal value ends its prompt (memory model §4.2): the Agent's
/// response, the Agent's error, or an answer the bridge could not accept, which
/// is recorded as it is and never as a success.
fn prompt_end(terminal: &Value) -> PromptEnd {
    if let Some(error) = terminal.get("error") {
        PromptEnd::Error(error.clone())
    } else if terminal.get("invalidPromptResponse").is_some() {
        PromptEnd::Error(terminal.clone())
    } else {
        PromptEnd::Response(terminal.clone())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionState {
    pub session_id: String,
    #[serde(skip)]
    pub(crate) live: Option<SessionLiveState>,
    pub phase: MirrorPhase,
    pub active_turn: Option<TurnOverlay>,
    pub sync_error: Option<String>,
    #[serde(skip)]
    pub(crate) operation: Option<SessionOperationState>,
    #[serde(skip)]
    pub(crate) load_attempt: Option<String>,
    #[serde(skip)]
    pub(crate) load_origin: Option<MirrorPhase>,
    /// The protocol v2 turn log: unknown until a load or creation.
    #[serde(skip)]
    pub(crate) log: SessionLog,
    /// The session timeline of the bridge memory model: unknown (`None`) until a
    /// load, a creation or a resume gives it. Held beside `log` until the wire
    /// is derived from it.
    #[serde(skip)]
    pub(crate) timeline: Option<Arc<Timeline>>,
    /// How much of the history the timeline holds; `Some` exactly when the
    /// timeline is known (memory model §6.1).
    #[serde(skip)]
    pub(crate) history: Option<History>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MirrorError {
    UnknownSession,
    WrongPhase,
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

    pub(crate) fn cold(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            live: None,
            phase: MirrorPhase::Cold,
            active_turn: None,
            sync_error: None,
            operation: None,
            load_attempt: None,
            load_origin: None,
            log: SessionLog::Unknown,
            timeline: None,
            history: None,
        }
    }

    pub(crate) fn ready(session_id: impl Into<String>) -> Self {
        let mut session = Self::cold(session_id);
        session.phase = MirrorPhase::Ready;
        session.log = SessionLog::empty();
        session.timeline = Some(Arc::default());
        session.history = Some(History::Complete);
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

    /// Installs an admitted prompt's turn; the operation and the prompt's
    /// RPC are its only correlation.
    pub(crate) fn admit_turn(
        &mut self,
        operation_id: String,
        client_intent_id: &str,
        prompt: Vec<Value>,
    ) -> Result<(), MirrorError> {
        self.can_begin(SessionAdmission::Prompt)?;
        self.phase = MirrorPhase::Running;
        // The prompt the bridge sends is the user entry the next echo may
        // complete; its outcome is recorded when the turn ends (memory model §4).
        if let Some(timeline) = self.timeline.as_mut() {
            Arc::make_mut(timeline).begin_prompt(&prompt);
        }
        self.active_turn = Some(TurnOverlay {
            operation_id,
            prompt: Arc::new(prompt),
            updates: Arc::new(Vec::new()),
            terminal: None,
            execution: Some(TurnExecution {
                rpc_operation_id: client_intent_id.to_string(),
                cancel_requested: false,
            }),
        });
        self.sync_error = None;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn append_turn_update(
        &mut self,
        operation_id: &str,
        update: Value,
    ) -> Result<(), MirrorError> {
        self.append_turn_update_with_meta(operation_id, update, None)
    }

    pub(crate) fn append_turn_update_with_meta(
        &mut self,
        operation_id: &str,
        update: Value,
        envelope: Option<&Value>,
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
        self.fold_overlay_update(update, envelope)
    }

    /// A conversation update while no prompt runs and none awaits its commit.
    /// The accepted update changes its entity, or continues the timeline,
    /// whether or not a prompt is running (memory model §3.2); the turn
    /// overlay and the log still ignore it.
    pub(crate) fn append_idle_update(&mut self, update: &Value, envelope: Option<&Value>) {
        if let Some(timeline) = self.timeline.as_mut() {
            Arc::make_mut(timeline).reduce(update, envelope);
        }
    }

    pub(crate) fn append_reconciling_update(
        &mut self,
        update: Value,
        envelope: Option<&Value>,
    ) -> Result<(), MirrorError> {
        if self.phase != MirrorPhase::Reconciling || self.load_attempt.is_some() {
            return Err(MirrorError::WrongPhase);
        }
        self.fold_overlay_update(update, envelope)
    }

    fn fold_overlay_update(
        &mut self,
        update: Value,
        envelope: Option<&Value>,
    ) -> Result<(), MirrorError> {
        let turn = self
            .active_turn
            .as_ref()
            .ok_or(MirrorError::OperationMismatch)?;
        let updates = fold_shared_active_turn_update(&turn.updates, &update)
            .map_err(|_| MirrorError::InconsistentHistory)?;
        self.active_turn
            .as_mut()
            .expect("the turn was checked above")
            .updates = Arc::new(updates);
        // Reduced only once the fold accepted the update, so a rejected one
        // leaves the timeline as it was.
        if let Some(timeline) = self.timeline.as_mut() {
            Arc::make_mut(timeline).reduce(&update, envelope);
        }
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
        if let Some(timeline) = self.timeline.as_mut() {
            Arc::make_mut(timeline).end_prompt(prompt_end(&terminal));
        }
        let turn = self
            .active_turn
            .as_mut()
            .expect("the turn was checked above");
        turn.execution = None;
        turn.terminal = Some(terminal);
        self.phase = MirrorPhase::Reconciling;
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
        self.active_turn = None;
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
        // No answer arrived: the outcome is unknown, never an invented stop
        // reason (memory model §4.2). A recorded response stays.
        if let Some(timeline) = self.timeline.as_mut() {
            Arc::make_mut(timeline).end_prompt(PromptEnd::Interrupted(true));
        }
        self.phase = MirrorPhase::Blocked;
        self.sync_error = Some(message.into());
        self.load_attempt = None;
        self.load_origin = None;
        Ok(())
    }

    /// A turn or a load ended and the session's log holds the result: the
    /// session is ready again.
    pub(crate) fn history_settled(&mut self) {
        self.phase = MirrorPhase::Ready;
        self.active_turn = None;
        self.load_attempt = None;
        self.load_origin = None;
        self.sync_error = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ready() -> SessionState {
        SessionState::ready("session")
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
            session.admit_turn("turn-1".into(), "intent-1", prompt("hello")),
            Ok(())
        );
    }

    #[test]
    fn an_occupied_session_refuses_a_prompt_without_changing() {
        let mut session = ready();
        session
            .begin_exclusive(SessionOperationKind::SetMode, "control")
            .unwrap();
        let before = session.clone();
        assert_eq!(
            session.admit_turn("turn-1".into(), "intent-1", prompt("hello")),
            Err(MirrorError::WrongPhase),
        );
        assert_eq!(session, before);
        session
            .settle_exclusive(SessionAdmission::Control, "control")
            .unwrap();
        start(&mut session);
        let before = session.clone();
        assert_eq!(
            session.admit_turn("another-operation".into(), "intent-2", prompt("hello")),
            Err(MirrorError::WrongPhase),
        );
        assert_eq!(session, before);
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
    fn a_settled_turn_keeps_a_concurrent_control() {
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
        session.history_settled();
        assert_eq!(session.phase, MirrorPhase::Ready);
        assert!(session.active_turn.is_none());
        assert_eq!(
            session.can_begin(SessionAdmission::Prompt),
            Err(MirrorError::WrongPhase)
        );
        session
            .settle_exclusive(SessionAdmission::Control, "control")
            .unwrap();
        assert_eq!(
            session.admit_turn("turn-2".into(), "intent-2", prompt("next")),
            Ok(())
        );
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
        let cold = SessionState::cold("cold");
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
        start(&mut session);
    }
}

#[cfg(test)]
mod memory_efficiency_tests;
