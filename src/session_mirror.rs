use std::sync::Arc;

use serde_json::Value;

use crate::history_cache::{HistoryCacheError, SessionKey};

use crate::protocol::canon::TurnLog;
use crate::protocol::registry::SessionLog;
use crate::runtime_state::SessionOperationKind;
#[cfg(test)]
pub(crate) use crate::session_registry::SessionRegistry as SessionMirror;
use crate::session_registry::{SessionEntry, SessionRegistry};
use crate::session_state::SessionAdmission;
pub(crate) use crate::session_state::{
    MirrorError, MirrorPhase, SessionState as MirrorSessionState, TurnOverlay,
};

impl SessionRegistry {
    pub(crate) fn register_cold(&mut self, session_id: impl Into<String>, incarnation: u64) {
        self.register_history(session_id.into(), incarnation, false);
    }

    pub(crate) fn register_new(&mut self, session_id: impl Into<String>, incarnation: u64) {
        self.register_history(session_id.into(), incarnation, true);
    }

    fn register_history(&mut self, session_id: String, incarnation: u64, ready: bool) {
        self.next_incarnation = self.next_incarnation.max(incarnation);
        if let Some(previous) = self
            .sessions
            .get(&session_id)
            .map(|entry| entry.state.incarnation)
        {
            self.clear_history_for_replacement(&session_id, previous);
            if previous != incarnation {
                self.retire_entry(&session_id, previous);
            }
        }
        let mut state = if ready {
            MirrorSessionState::ready(session_id.clone(), incarnation)
        } else {
            MirrorSessionState::cold(session_id.clone(), incarnation)
        };
        if let Some(entry) = self.sessions.get_mut(&session_id) {
            state.live = entry.state.live.take();
            state.operation = entry.state.operation.take();
            entry.state = state;
        } else {
            self.sessions.insert(session_id, SessionEntry::new(state));
        }
    }

    pub(crate) fn begin_load(
        &mut self,
        session_id: &str,
        incarnation: u64,
        attempt_id: impl Into<String>,
    ) -> Result<(), MirrorError> {
        let attempt_id = attempt_id.into();
        let session = self.require_state_mut(session_id, incarnation)?;
        if !matches!(
            session.phase,
            MirrorPhase::Cold
                | MirrorPhase::Loading
                | MirrorPhase::Ready
                | MirrorPhase::Reconciling
                | MirrorPhase::Blocked
        ) || session.load_attempt.is_some()
            || session
                .active_turn
                .as_ref()
                .is_some_and(|turn| turn.terminal.is_none())
            || session.operation.as_ref().is_some_and(|operation| {
                operation.kind.admission() != SessionAdmission::Attachment
                    || operation.operation_id != attempt_id
            })
        {
            return Err(MirrorError::WrongPhase);
        }
        let key = SessionKey::new(session_id, incarnation);
        self.history.begin_candidate(key, attempt_id.clone())?;
        let entry = self
            .sessions
            .get_mut(session_id)
            .expect("session was validated above");
        // A load attempt is live work: cancel the idle countdown now so a
        // stale deadline cannot survive past the load's completion.
        entry.resources.observers.stop_absence();
        let session = &mut entry.state;
        session.load_origin = Some(session.phase);
        if session.phase == MirrorPhase::Blocked {
            session.phase = if session.active_turn.is_some() {
                MirrorPhase::Reconciling
            } else {
                MirrorPhase::Loading
            };
        }
        if matches!(session.phase, MirrorPhase::Cold | MirrorPhase::Ready) {
            session.phase = MirrorPhase::Loading;
        }
        session.load_attempt = Some(attempt_id);
        session.sync_error = None;
        Ok(())
    }

    pub(crate) fn load_candidate(
        &self,
        session_id: &str,
        incarnation: u64,
        attempt_id: &str,
    ) -> Result<crate::history_replay::ReplayCandidate, MirrorError> {
        let session = self.require_state(session_id, incarnation)?;
        if session.load_attempt.as_deref() != Some(attempt_id) {
            return Err(MirrorError::OperationMismatch);
        }
        Ok(self
            .history
            .candidate(&SessionKey::new(session_id, incarnation), attempt_id)?)
    }

    pub(crate) fn append_load_update(
        &mut self,
        session_id: &str,
        incarnation: u64,
        attempt_id: &str,
        update: Value,
    ) -> Result<(), MirrorError> {
        self.require_load(session_id, incarnation, attempt_id)?;
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
        self.require_load(session_id, incarnation, attempt_id)?;
        self.history.poison_candidate(
            &SessionKey::new(session_id, incarnation),
            attempt_id,
            HistoryCacheError::InvalidReplay,
        )?;
        Ok(())
    }

    /// Commits a replay: the session's log is rebuilt from it.
    pub(crate) fn commit_load(
        &mut self,
        session_id: &str,
        incarnation: u64,
        attempt_id: &str,
    ) -> Result<(), MirrorError> {
        let replay = self.commit_history(session_id, incarnation, attempt_id)?;
        self.replace_session_log(session_id, &replay);
        Ok(())
    }

    /// Ends a load with its replay; a failed or stale load leaves the log.
    fn commit_history(
        &mut self,
        session_id: &str,
        incarnation: u64,
        attempt_id: &str,
    ) -> Result<Vec<Value>, MirrorError> {
        self.require_load(session_id, incarnation, attempt_id)?;
        let replay = self
            .history
            .commit_candidate(&SessionKey::new(session_id, incarnation), attempt_id)?;
        self.require_state_mut(session_id, incarnation)?
            .history_settled();
        Ok(replay)
    }

    // Resume notifications are useful context, but do not promise a full replay:
    // they never replace a log the bridge already knows.
    pub(crate) fn commit_attachment_cache(
        &mut self,
        session_id: &str,
        incarnation: u64,
        attempt_id: &str,
    ) -> Result<(), MirrorError> {
        let replay = self.commit_history(session_id, incarnation, attempt_id)?;
        self.adopt_session_log(session_id, &replay);
        self.require_state_mut(session_id, incarnation)?.phase = MirrorPhase::Cold;
        Ok(())
    }

    /// The session goes on with the log it has, or with `log` if it knows
    /// none: the Agent gave no replay (protocol S1).
    pub(crate) fn use_cached_history(
        &mut self,
        session_id: &str,
        incarnation: u64,
        log: Option<Arc<TurnLog>>,
    ) -> Result<(), MirrorError> {
        let session = self.require_state_mut(session_id, incarnation)?;
        if session.active_turn.is_some() || session.load_attempt.is_some() {
            return Err(MirrorError::WrongPhase);
        }
        if matches!(session.log, SessionLog::Unknown) {
            session.log = SessionLog::Known(log.unwrap_or_default());
        }
        session.history_settled();
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
        let session = self.require_state(session_id, incarnation)?;
        if session.load_attempt.as_deref() != Some(attempt_id) {
            return Err(MirrorError::OperationMismatch);
        }
        let key = SessionKey::new(session_id, incarnation);
        self.history.abort_candidate(&key, attempt_id)?;
        let session = self.require_state_mut(session_id, incarnation)?;
        let origin = session.load_origin.take();
        session.load_attempt = None;
        session.sync_error = Some(message.into());
        session.phase = if retryable {
            match origin {
                Some(MirrorPhase::Ready) => MirrorPhase::Ready,
                Some(MirrorPhase::Blocked) => MirrorPhase::Blocked,
                Some(MirrorPhase::Reconciling) => MirrorPhase::Reconciling,
                _ => MirrorPhase::Loading,
            }
        } else {
            MirrorPhase::Blocked
        };
        Ok(())
    }

    /// Admits a prompt's turn; returns its operation.
    pub(crate) fn start_turn(
        &mut self,
        session_id: &str,
        incarnation: u64,
        client_intent_id: &str,
        prompt: Vec<Value>,
    ) -> Result<String, MirrorError> {
        let next_operation = self.next_operation.wrapping_add(1).max(1);
        let operation_id = format!("{}:{}", self.epoch, next_operation);
        self.require_state_mut(session_id, incarnation)?
            .admit_turn(operation_id.clone(), client_intent_id, prompt)?;
        self.next_operation = next_operation;
        Ok(operation_id)
    }

    pub(crate) fn append_turn_update(
        &mut self,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        update: Value,
    ) -> Result<(), MirrorError> {
        self.require_state_mut(session_id, incarnation)?
            .append_turn_update(operation_id, update)
    }

    /// A conversation update outside a turn: a completed turn being
    /// reconciled keeps it; otherwise no turn holds it, and the log is only
    /// appended by turns and rebuilt by loads.
    pub(crate) fn append_session_update(
        &mut self,
        session_id: &str,
        incarnation: u64,
        update: Value,
    ) -> Result<(), MirrorError> {
        let session = self.require_state_mut(session_id, incarnation)?;
        if session.load_attempt.is_some() {
            return Err(MirrorError::WrongPhase);
        }
        match session.phase {
            MirrorPhase::Reconciling => session.append_reconciling_update(update),
            MirrorPhase::Ready => Ok(()),
            _ => Err(MirrorError::WrongPhase),
        }
    }

    pub(crate) fn complete_turn(
        &mut self,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        terminal: Value,
    ) -> Result<(), MirrorError> {
        self.require_state_mut(session_id, incarnation)?
            .complete_turn(operation_id, terminal)
    }

    /// The completed turn is in the session's log: the session is ready.
    pub(crate) fn commit_completed_turn(
        &mut self,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
    ) -> Result<(), MirrorError> {
        let session = self.require_state_mut(session_id, incarnation)?;
        if session.phase != MirrorPhase::Reconciling {
            return Err(MirrorError::WrongPhase);
        }
        if session
            .active_turn
            .as_ref()
            .is_none_or(|turn| turn.operation_id != operation_id || turn.terminal.is_none())
        {
            return Err(MirrorError::OperationMismatch);
        }
        session.history_settled();
        Ok(())
    }

    pub(crate) fn state(&self, session_id: &str) -> Option<&MirrorSessionState> {
        self.sessions.get(session_id).map(|entry| &entry.state)
    }

    pub(crate) fn iter_states(&self) -> impl Iterator<Item = &MirrorSessionState> {
        self.sessions.values().map(|entry| &entry.state)
    }

    pub(crate) fn can_begin(
        &self,
        session_id: &str,
        incarnation: u64,
        admission: SessionAdmission,
    ) -> Result<(), MirrorError> {
        self.require_state(session_id, incarnation)?
            .can_begin(admission)
    }

    pub(crate) fn begin_exclusive(
        &mut self,
        session_id: &str,
        incarnation: u64,
        kind: SessionOperationKind,
        operation_id: impl Into<String>,
    ) -> Result<(), MirrorError> {
        let operation_id = operation_id.into();
        self.require_state(session_id, incarnation)?;
        if self.operation_in_use(&operation_id) {
            return Err(MirrorError::OperationMismatch);
        }
        self.require_state_mut(session_id, incarnation)?
            .begin_exclusive(kind, operation_id)
    }

    pub(crate) fn settle_exclusive(
        &mut self,
        session_id: &str,
        incarnation: u64,
        kind: SessionAdmission,
        operation_id: &str,
    ) -> Result<(), MirrorError> {
        self.require_state_mut(session_id, incarnation)?
            .settle_exclusive(kind, operation_id)
    }

    pub(crate) fn load_attempt(&self, session_id: &str, incarnation: u64) -> Option<&str> {
        let session = self.require_state(session_id, incarnation).ok()?;
        session.load_attempt.as_deref()
    }

    pub(crate) fn active_operation_id(&self, session_id: &str, incarnation: u64) -> Option<&str> {
        let session = self.require_state(session_id, incarnation).ok()?;
        session
            .active_turn
            .as_ref()
            .map(|turn| turn.operation_id.as_str())
    }

    #[cfg(test)]
    pub(crate) fn abort_turn(
        &mut self,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
    ) -> Result<(), MirrorError> {
        self.require_state_mut(session_id, incarnation)?
            .abort_turn(operation_id)
    }

    pub(crate) fn block_turn(
        &mut self,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        message: impl Into<String>,
    ) -> Result<(), MirrorError> {
        self.require_state_mut(session_id, incarnation)?
            .block_turn(operation_id, message)
    }

    pub(crate) fn remove(&mut self, session_id: &str, incarnation: u64) {
        self.clear_history_for_replacement(session_id, incarnation);
        if self.sessions.get(session_id).is_some_and(|entry| {
            let session = &entry.state;
            session.incarnation == incarnation
                && session.live.is_none()
                && session.operation.is_none()
                && entry.resources.materialization.is_none()
        }) {
            self.retire_entry(session_id, incarnation);
        }
    }

    /// Releases this incarnation's log, load candidate and turn. Live
    /// lifecycle and the operation owner remain available until their own
    /// transition completes; a stale cleanup cannot touch a new incarnation.
    /// Runtime replacement calls this before installing its entry.
    pub(crate) fn clear_history_for_replacement(&mut self, session_id: &str, incarnation: u64) {
        let Some(session) = self
            .sessions
            .get_mut(session_id)
            .map(|entry| &mut entry.state)
            .filter(|session| session.incarnation == incarnation)
        else {
            return;
        };
        let mut cleared = MirrorSessionState::cold(session_id, incarnation);
        cleared.live = session.live.take();
        cleared.operation = session.operation.take();
        *session = cleared;
        self.history
            .remove(&SessionKey::new(session_id, incarnation));
    }

    /// The session's load `attempt_id` is in flight.
    fn require_load(
        &self,
        session_id: &str,
        incarnation: u64,
        attempt_id: &str,
    ) -> Result<(), MirrorError> {
        let session = self.require_state(session_id, incarnation)?;
        if session.load_attempt.as_deref() != Some(attempt_id)
            || !matches!(
                session.phase,
                MirrorPhase::Loading | MirrorPhase::Reconciling
            )
        {
            return Err(MirrorError::OperationMismatch);
        }
        Ok(())
    }

    fn require_state(
        &self,
        session_id: &str,
        incarnation: u64,
    ) -> Result<&MirrorSessionState, MirrorError> {
        let session = self
            .sessions
            .get(session_id)
            .map(|entry| &entry.state)
            .ok_or(MirrorError::UnknownSession)?;
        if session.incarnation != incarnation {
            return Err(MirrorError::StaleIncarnation);
        }
        Ok(session)
    }

    fn require_state_mut(
        &mut self,
        session_id: &str,
        incarnation: u64,
    ) -> Result<&mut MirrorSessionState, MirrorError> {
        let session = self
            .sessions
            .get_mut(session_id)
            .map(|entry| &mut entry.state)
            .ok_or(MirrorError::UnknownSession)?;
        if session.incarnation != incarnation {
            return Err(MirrorError::StaleIncarnation);
        }
        Ok(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_state::SessionOperationKind;
    use serde_json::json;

    #[test]
    fn history_registration_preserves_the_live_attachment_and_its_owner() {
        let mut registry = mirror();
        registry.register_cold("known", 40);
        let incarnation = registry
            .start_attachment(
                "epoch",
                "session",
                "/workspace",
                "load",
                SessionOperationKind::Load,
            )
            .unwrap();
        assert_eq!(incarnation, 41);
        let live = registry.session("session").unwrap().clone();

        for ready in [false, true] {
            if ready {
                registry.register_new("session", incarnation);
            } else {
                registry.register_cold("session", incarnation);
            }
            assert_eq!(registry.session("session"), Some(live.clone()));
            let state = registry.state("session").unwrap();
            assert_eq!(state.live.as_ref().unwrap().lifecycle, live.lifecycle);
            assert_eq!(state.operation.as_ref().unwrap().operation_id, "load");
            assert_eq!(
                registry.can_begin("session", incarnation, SessionAdmission::Prompt),
                Err(MirrorError::WrongPhase)
            );
        }
        assert_eq!(registry.sessions.len(), 2);
        assert_eq!(registry.live_snapshot().sessions.len(), 1);
    }

    #[test]
    fn history_removal_preserves_live_state_and_the_cleanup_owner() {
        let mut registry = mirror();
        let incarnation = registry
            .open_new_with_replay(
                "epoch",
                "session",
                "/workspace",
                json!({ "title": "session" }),
                vec![],
            )
            .unwrap();
        registry.register_new("session", incarnation);
        registry
            .begin_exclusive("session", incarnation, SessionOperationKind::Close, "close")
            .unwrap();
        registry
            .start_operation(
                "epoch",
                "session",
                incarnation,
                "close",
                SessionOperationKind::Close,
                "closing",
            )
            .unwrap();
        let live = registry.session("session").unwrap().clone();

        registry.remove("session", incarnation);

        assert_eq!(registry.session("session"), Some(live.clone()));
        assert_eq!(registry.state("session").unwrap().log, SessionLog::Unknown);
        registry
            .close_session("epoch", "session", incarnation, "close")
            .unwrap();
        registry.remove("session", incarnation);
        assert!(registry.session("session").is_none());
        assert_eq!(
            registry
                .state("session")
                .unwrap()
                .operation
                .as_ref()
                .unwrap()
                .operation_id,
            "close"
        );

        registry
            .finish_session_cleanup("session", incarnation, SessionAdmission::Close, "close")
            .unwrap();
        registry.remove("session", incarnation);
        assert!(registry.state("session").is_none());
        assert!(registry.live_snapshot().sessions.is_empty());
    }

    #[test]
    fn history_replacement_releases_the_turn_and_load_without_retiring_the_live_owner() {
        let mut registry = mirror();
        let incarnation = registry
            .open_new_with_replay("epoch", "session", "/workspace", json!({}), vec![])
            .unwrap();
        registry.register_new("session", incarnation);
        let operation_id = registry
            .start_turn("session", incarnation, "intent", text_prompt("next"))
            .unwrap();
        registry
            .complete_prompt(
                "epoch",
                "session",
                incarnation,
                "intent",
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        registry
            .complete_turn(
                "session",
                incarnation,
                &operation_id,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        registry
            .begin_load("session", incarnation, "candidate")
            .unwrap();
        registry
            .append_load_update(
                "session",
                incarnation,
                "candidate",
                user_update("candidate payload"),
            )
            .unwrap();
        assert_eq!(registry.history.candidates(), 1);

        let live = registry.live("session").unwrap().clone();
        registry.clear_history_for_replacement("session", incarnation);

        assert_eq!(registry.history.candidates(), 0);
        let state = registry.state("session").unwrap();
        assert_eq!(state.incarnation, incarnation);
        assert_eq!(registry.live("session"), Some(&live));
        assert!(state.active_turn.is_none());
        assert!(state.load_attempt.is_none());
        assert!(state.load_origin.is_none());
        assert_eq!(state.log, SessionLog::Unknown);
        registry.clear_history_for_replacement("session", incarnation + 1);
        assert_eq!(registry.live("session"), Some(&live));
    }

    #[test]
    fn session_updates_preserve_reconciling_load_and_incarnation_boundaries() {
        let mut mirror = mirror();
        mirror.register_new("session", 1);
        let operation_id = mirror
            .start_turn("session", 1, "intent", text_prompt("Run"))
            .unwrap();
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
        let turn = mirror
            .state("session")
            .unwrap()
            .active_turn
            .clone()
            .unwrap();
        assert_eq!(turn.terminal, Some(json!({ "stopReason": "cancelled" })));
        assert_eq!(turn.updates[0]["status"], "completed");
        mirror
            .commit_completed_turn("session", 1, &operation_id)
            .unwrap();
        assert_eq!(mirror.state("session").unwrap().phase, MirrorPhase::Ready);
        mirror.begin_load("session", 1, "load").unwrap();
        assert_eq!(
            mirror.append_session_update("session", 1, json!({})),
            Err(MirrorError::WrongPhase)
        );
        assert_eq!(
            mirror.append_session_update("session", 2, json!({})),
            Err(MirrorError::StaleIncarnation)
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
            .append_load_update(session_id, incarnation, "initial", user_update("old"))
            .unwrap();
        mirror
            .append_load_update(
                session_id,
                incarnation,
                "initial",
                json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "old reply" } }),
            )
            .unwrap();
        mirror
            .commit_load(session_id, incarnation, "initial")
            .unwrap();
    }

    fn log(mirror: &SessionMirror) -> SessionLog {
        mirror.state("session").unwrap().log.clone()
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
    fn a_load_rebuilds_the_log_from_its_replay() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session", 1);
        let SessionLog::Known(known) = log(&mirror) else {
            panic!("a committed load knows the log");
        };
        assert_eq!(known.entries().len(), 1);
        assert_eq!(mirror.state("session").unwrap().phase, MirrorPhase::Ready);
        assert_eq!(mirror.history.candidates(), 0);
    }

    #[test]
    fn prompt_terminal_retains_the_turn_until_its_commit() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session", 1);
        let operation = mirror
            .start_turn("session", 1, "intent", text_prompt("next"))
            .unwrap();
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
        assert_eq!(
            mirror.commit_completed_turn("session", 1, "stale-operation"),
            Err(MirrorError::OperationMismatch)
        );
        mirror
            .commit_completed_turn("session", 1, &operation)
            .unwrap();
        let state = mirror.state("session").unwrap();
        assert_eq!(state.phase, MirrorPhase::Ready);
        assert!(state.active_turn.is_none());
        assert_eq!(
            mirror.can_begin("session", 1, SessionAdmission::Prompt),
            Ok(())
        );
    }

    #[test]
    fn resumed_cache_does_not_enable_commands_between_automatic_load_attempts() {
        let mut mirror = mirror();
        mirror.register_cold("session", 1);
        mirror.begin_load("session", 1, "resume").unwrap();
        mirror
            .append_load_update("session", 1, "resume", user_update("cached"))
            .unwrap();
        mirror
            .commit_attachment_cache("session", 1, "resume")
            .unwrap();
        assert_eq!(mirror.state("session").unwrap().phase, MirrorPhase::Cold);
        let cached = log(&mirror);
        assert!(matches!(cached, SessionLog::Known(_)));

        for attempt in ["automatic-first", "automatic-retry"] {
            mirror.begin_load("session", 1, attempt).unwrap();
            mirror
                .fail_load("session", 1, attempt, "temporary", true)
                .unwrap();
            assert_eq!(log(&mirror), cached);
            assert_eq!(mirror.state("session").unwrap().phase, MirrorPhase::Loading);
            for admission in [
                SessionAdmission::Prompt,
                SessionAdmission::Control,
                SessionAdmission::Close,
                SessionAdmission::Delete,
                SessionAdmission::Attachment,
            ] {
                assert_eq!(
                    mirror.can_begin("session", 1, admission),
                    Err(MirrorError::WrongPhase)
                );
            }
        }
        mirror.begin_load("session", 1, "successful-retry").unwrap();
        mirror
            .append_load_update("session", 1, "successful-retry", user_update("complete"))
            .unwrap();
        mirror
            .commit_load("session", 1, "successful-retry")
            .unwrap();
        assert_eq!(
            mirror.can_begin("session", 1, SessionAdmission::Prompt),
            Ok(())
        );
    }

    #[test]
    fn cached_history_keeps_a_known_log_and_installs_one_only_when_unknown() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session", 1);
        let known = log(&mirror);
        mirror
            .use_cached_history("session", 1, Some(Arc::new(TurnLog::default())))
            .unwrap();
        assert_eq!(log(&mirror), known);

        mirror.register_cold("fork", 2);
        let SessionLog::Known(source) = known else {
            unreachable!()
        };
        mirror
            .use_cached_history("fork", 2, Some(source.clone()))
            .unwrap();
        let fork = mirror.state("fork").unwrap();
        assert_eq!(fork.phase, MirrorPhase::Ready);
        assert!(matches!(&fork.log, SessionLog::Known(log) if Arc::ptr_eq(log, &source)));
    }

    #[test]
    fn failed_explicit_load_restores_its_admission_phase() {
        for origin in [MirrorPhase::Ready, MirrorPhase::Blocked] {
            let mut mirror = mirror();
            load_initial(&mut mirror, "session", 1);
            let old = log(&mirror);
            if origin == MirrorPhase::Blocked {
                mirror.begin_load("session", 1, "fatal").unwrap();
                mirror
                    .fail_load("session", 1, "fatal", "unrecoverable", false)
                    .unwrap();
            }
            mirror
                .begin_exclusive("session", 1, SessionOperationKind::Load, "explicit")
                .unwrap();
            mirror.begin_load("session", 1, "explicit").unwrap();
            mirror
                .append_load_update("session", 1, "explicit", user_update("partial"))
                .unwrap();
            mirror
                .fail_load("session", 1, "explicit", "temporary", true)
                .unwrap();
            assert_eq!(mirror.state("session").unwrap().phase, origin);
            assert_eq!(
                mirror.can_begin("session", 1, SessionAdmission::Attachment),
                Err(MirrorError::WrongPhase)
            );
            mirror
                .settle_exclusive("session", 1, SessionAdmission::Attachment, "explicit")
                .unwrap();
            assert_eq!(
                mirror.can_begin("session", 1, SessionAdmission::Attachment),
                Ok(())
            );
            assert_eq!(log(&mirror), old, "a failed load leaves the known log");
            assert!(mirror.state("session").unwrap().load_origin.is_none());
            assert_eq!(
                mirror.state("session").unwrap().sync_error.as_deref(),
                Some("temporary")
            );
        }
    }

    #[test]
    fn a_second_turn_while_running_is_rejected() {
        let mut mirror = mirror();
        mirror.register_new("session", 1);
        mirror
            .start_turn("session", 1, "first", vec![json!("one")])
            .unwrap();

        assert_eq!(
            mirror.start_turn("session", 1, "second", vec![json!("two")]),
            Err(MirrorError::WrongPhase)
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
        assert_eq!(log(&mirror), SessionLog::Unknown);

        mirror.begin_load("session", 1, "second").unwrap();
        mirror
            .append_load_update("session", 1, "second", user_update("complete"))
            .unwrap();
        mirror.commit_load("session", 1, "second").unwrap();

        let SessionLog::Known(known) = log(&mirror) else {
            panic!("a committed load knows the log");
        };
        assert_eq!(known.entries().len(), 1);
        assert_eq!(
            known.entries()[0].turn.prompt,
            TurnLog::from_replay(&[user_update("complete")]).entries()[0]
                .turn
                .prompt
        );
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
        assert_eq!(log(&mirror), SessionLog::Unknown);
        mirror
            .fail_load("session", 1, "invalid", "invalid replay", false)
            .unwrap();
        assert_eq!(mirror.state("session").unwrap().phase, MirrorPhase::Blocked);
        assert_eq!(log(&mirror), SessionLog::Unknown);
    }

    #[test]
    fn a_large_valid_turn_is_kept_whole() {
        let mut mirror = mirror();
        mirror.register_new("session", 1);
        let operation = mirror
            .start_turn("session", 1, "intent", text_prompt("prompt"))
            .unwrap();
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
        assert!(mirror.state("session").unwrap().active_turn.is_none());
    }

    #[test]
    fn stale_incarnation_removal_cannot_drop_the_current_session() {
        let mut mirror = mirror();
        mirror.register_new("session", 2);

        mirror.remove("session", 1);

        let state = mirror.state("session").unwrap();
        assert_eq!(state.incarnation, 2);
        assert_eq!(state.phase, MirrorPhase::Ready);
        assert_eq!(state.log, SessionLog::empty());
    }
}
