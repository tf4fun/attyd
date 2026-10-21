use std::sync::Arc;

use serde_json::Value;

use crate::history_cache::HistoryCacheError;
use crate::history_replay::Replay;
use crate::protocol::canon::TurnLog;
use crate::protocol::op::operation_id;
use crate::protocol::registry::SessionLog;
use crate::protocol::timeline::History;
use crate::runtime_state::SessionOperationKind;
#[cfg(test)]
pub(crate) use crate::session_registry::SessionRegistry as SessionMirror;
use crate::session_registry::{SessionEntry, SessionRegistry};
use crate::session_state::SessionAdmission;
pub(crate) use crate::session_state::{
    MirrorError, MirrorPhase, SessionState as MirrorSessionState,
};

impl SessionRegistry {
    pub(crate) fn register_cold(&mut self, session_id: impl Into<String>) {
        self.register_history(session_id.into(), false);
    }

    pub(crate) fn register_new(&mut self, session_id: impl Into<String>) {
        self.register_history(session_id.into(), true);
    }

    fn register_history(&mut self, session_id: String, ready: bool) {
        self.clear_history_for_replacement(&session_id);
        let mut state = if ready {
            MirrorSessionState::ready(session_id.clone())
        } else {
            MirrorSessionState::cold(session_id.clone())
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
        attempt_id: impl Into<String>,
    ) -> Result<(), MirrorError> {
        let attempt_id = attempt_id.into();
        let session = self.require_state_mut(session_id)?;
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
        self.history
            .begin_candidate(session_id, attempt_id.clone())?;
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
        attempt_id: &str,
    ) -> Result<crate::history_replay::ReplayCandidate, MirrorError> {
        let session = self.require_state(session_id)?;
        if session.load_attempt.as_deref() != Some(attempt_id) {
            return Err(MirrorError::OperationMismatch);
        }
        Ok(self.history.candidate(session_id, attempt_id)?)
    }

    #[cfg(test)]
    pub(crate) fn append_load_update(
        &mut self,
        session_id: &str,
        attempt_id: &str,
        update: Value,
    ) -> Result<(), MirrorError> {
        self.append_load_update_with_meta(session_id, attempt_id, update, None)
    }

    /// `envelope` is the notification's `_meta`, kept beside the update (§3.5).
    pub(crate) fn append_load_update_with_meta(
        &mut self,
        session_id: &str,
        attempt_id: &str,
        update: Value,
        envelope: Option<&Value>,
    ) -> Result<(), MirrorError> {
        self.require_load(session_id, attempt_id)?;
        self.history
            .append_candidate_with_meta(session_id, attempt_id, update, envelope)?;
        Ok(())
    }

    pub(crate) fn invalidate_load(
        &mut self,
        session_id: &str,
        attempt_id: &str,
    ) -> Result<(), MirrorError> {
        self.require_load(session_id, attempt_id)?;
        self.history
            .poison_candidate(session_id, attempt_id, HistoryCacheError::InvalidReplay)?;
        Ok(())
    }

    /// Commits a replay: the session's log is rebuilt from it.
    pub(crate) fn commit_load(
        &mut self,
        session_id: &str,
        attempt_id: &str,
    ) -> Result<(), MirrorError> {
        let replay = self.commit_history(session_id, attempt_id)?;
        self.replace_session_log(session_id, &replay.updates);
        self.replace_session_timeline(session_id, replay.timeline);
        Ok(())
    }

    /// Ends a load with its replay; a failed or stale load leaves the log.
    fn commit_history(
        &mut self,
        session_id: &str,
        attempt_id: &str,
    ) -> Result<Replay, MirrorError> {
        self.require_load(session_id, attempt_id)?;
        let replay = self.history.commit_candidate(session_id, attempt_id)?;
        self.require_state_mut(session_id)?.history_settled();
        Ok(replay)
    }

    // Resume notifications are useful context, but do not promise a full replay:
    // they never replace a log the bridge already knows.
    pub(crate) fn commit_attachment_cache(
        &mut self,
        session_id: &str,
        attempt_id: &str,
    ) -> Result<(), MirrorError> {
        let replay = self.commit_history(session_id, attempt_id)?;
        self.adopt_session_log(session_id, &replay.updates);
        self.adopt_session_timeline(session_id, replay.timeline);
        self.require_state_mut(session_id)?.phase = MirrorPhase::Cold;
        Ok(())
    }

    /// The session goes on with the log it has, or with `log` if it knows
    /// none: the Agent gave no replay (protocol S1).
    pub(crate) fn use_cached_history(
        &mut self,
        session_id: &str,
        log: Option<Arc<TurnLog>>,
    ) -> Result<(), MirrorError> {
        let session = self.require_state_mut(session_id)?;
        if session.active_turn.is_some() || session.load_attempt.is_some() {
            return Err(MirrorError::WrongPhase);
        }
        if matches!(session.log, SessionLog::Unknown) {
            session.log = SessionLog::Known(log.unwrap_or_default());
        }
        if session.timeline.is_none() {
            // The context the Agent gave is no replay: what came before is not
            // known to be there (memory model §6.1).
            session.timeline = Some(Arc::default());
            session.history = Some(History::Partial);
            session.timeline_epoch = Some(operation_id());
        }
        session.history_settled();
        Ok(())
    }

    pub(crate) fn fail_load(
        &mut self,
        session_id: &str,
        attempt_id: &str,
        message: impl Into<String>,
        retryable: bool,
    ) -> Result<(), MirrorError> {
        let session = self.require_state(session_id)?;
        if session.load_attempt.as_deref() != Some(attempt_id) {
            return Err(MirrorError::OperationMismatch);
        }
        self.history.abort_candidate(session_id, attempt_id)?;
        let session = self.require_state_mut(session_id)?;
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
        client_intent_id: &str,
        prompt: Vec<Value>,
    ) -> Result<String, MirrorError> {
        let next_operation = self.next_operation.wrapping_add(1).max(1);
        let operation_id = format!("{}:{}", self.epoch, next_operation);
        self.require_state_mut(session_id)?.admit_turn(
            operation_id.clone(),
            client_intent_id,
            prompt,
        )?;
        self.next_operation = next_operation;
        Ok(operation_id)
    }

    #[cfg(test)]
    pub(crate) fn append_turn_update(
        &mut self,
        session_id: &str,
        operation_id: &str,
        update: Value,
    ) -> Result<(), MirrorError> {
        self.append_turn_update_with_meta(session_id, operation_id, update, None)
    }

    pub(crate) fn append_turn_update_with_meta(
        &mut self,
        session_id: &str,
        operation_id: &str,
        update: Value,
        envelope: Option<&Value>,
    ) -> Result<(), MirrorError> {
        self.require_state_mut(session_id)?
            .append_turn_update_with_meta(operation_id, update, envelope)
    }

    /// A conversation update outside a turn. A completed turn being reconciled
    /// keeps it in its overlay. The timeline reduces it in both cases; the log
    /// is still only appended by turns and rebuilt by loads.
    #[cfg(test)]
    pub(crate) fn append_session_update(
        &mut self,
        session_id: &str,
        update: Value,
    ) -> Result<(), MirrorError> {
        self.append_session_update_with_meta(session_id, update, None)
    }

    pub(crate) fn append_session_update_with_meta(
        &mut self,
        session_id: &str,
        update: Value,
        envelope: Option<&Value>,
    ) -> Result<(), MirrorError> {
        let session = self.require_state_mut(session_id)?;
        if session.load_attempt.is_some() {
            return Err(MirrorError::WrongPhase);
        }
        match session.phase {
            MirrorPhase::Reconciling => session.append_reconciling_update(update, envelope),
            MirrorPhase::Ready => {
                session.append_idle_update(&update, envelope);
                Ok(())
            }
            _ => Err(MirrorError::WrongPhase),
        }
    }

    pub(crate) fn complete_turn(
        &mut self,
        session_id: &str,
        operation_id: &str,
        terminal: Value,
    ) -> Result<(), MirrorError> {
        self.require_state_mut(session_id)?
            .complete_turn(operation_id, terminal)
    }

    /// The completed turn is in the session's log: the session is ready.
    pub(crate) fn commit_completed_turn(
        &mut self,
        session_id: &str,
        operation_id: &str,
    ) -> Result<(), MirrorError> {
        let session = self.require_state_mut(session_id)?;
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
        admission: SessionAdmission,
    ) -> Result<(), MirrorError> {
        self.require_state(session_id)?.can_begin(admission)
    }

    pub(crate) fn begin_exclusive(
        &mut self,
        session_id: &str,
        kind: SessionOperationKind,
        operation_id: impl Into<String>,
    ) -> Result<(), MirrorError> {
        let operation_id = operation_id.into();
        self.require_state(session_id)?;
        if self.operation_in_use(&operation_id) {
            return Err(MirrorError::OperationMismatch);
        }
        self.require_state_mut(session_id)?
            .begin_exclusive(kind, operation_id)
    }

    pub(crate) fn settle_exclusive(
        &mut self,
        session_id: &str,
        kind: SessionAdmission,
        operation_id: &str,
    ) -> Result<(), MirrorError> {
        self.require_state_mut(session_id)?
            .settle_exclusive(kind, operation_id)
    }

    pub(crate) fn load_attempt(&self, session_id: &str) -> Option<&str> {
        let session = self.require_state(session_id).ok()?;
        session.load_attempt.as_deref()
    }

    pub(crate) fn active_operation_id(&self, session_id: &str) -> Option<&str> {
        let session = self.require_state(session_id).ok()?;
        session
            .active_turn
            .as_ref()
            .map(|turn| turn.operation_id.as_str())
    }

    #[cfg(test)]
    pub(crate) fn abort_turn(
        &mut self,
        session_id: &str,
        operation_id: &str,
    ) -> Result<(), MirrorError> {
        self.require_state_mut(session_id)?.abort_turn(operation_id)
    }

    pub(crate) fn block_turn(
        &mut self,
        session_id: &str,
        operation_id: &str,
        message: impl Into<String>,
    ) -> Result<(), MirrorError> {
        self.require_state_mut(session_id)?
            .block_turn(operation_id, message)
    }

    pub(crate) fn remove(&mut self, session_id: &str) {
        self.clear_history_for_replacement(session_id);
        if self.sessions.get(session_id).is_some_and(|entry| {
            let session = &entry.state;
            session.live.is_none()
                && session.operation.is_none()
                && entry.resources.materialization.is_none()
        }) {
            self.retire_entry(session_id);
        }
    }

    /// Releases the session's log, load candidate and turn. Live lifecycle
    /// and the operation owner remain available until their own transition
    /// completes. Runtime replacement calls this before installing its entry.
    pub(crate) fn clear_history_for_replacement(&mut self, session_id: &str) {
        let Some(session) = self
            .sessions
            .get_mut(session_id)
            .map(|entry| &mut entry.state)
        else {
            return;
        };
        let mut cleared = MirrorSessionState::cold(session_id);
        cleared.live = session.live.take();
        cleared.operation = session.operation.take();
        *session = cleared;
        self.history.remove(session_id);
    }

    /// The session's load `attempt_id` is in flight.
    fn require_load(&self, session_id: &str, attempt_id: &str) -> Result<(), MirrorError> {
        let session = self.require_state(session_id)?;
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

    fn require_state(&self, session_id: &str) -> Result<&MirrorSessionState, MirrorError> {
        self.sessions
            .get(session_id)
            .map(|entry| &entry.state)
            .ok_or(MirrorError::UnknownSession)
    }

    fn require_state_mut(
        &mut self,
        session_id: &str,
    ) -> Result<&mut MirrorSessionState, MirrorError> {
        self.sessions
            .get_mut(session_id)
            .map(|entry| &mut entry.state)
            .ok_or(MirrorError::UnknownSession)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::timeline::History;
    use crate::runtime_state::SessionOperationKind;
    use serde_json::json;

    #[test]
    fn history_registration_preserves_the_live_attachment_and_its_owner() {
        let mut registry = mirror();
        registry.register_cold("known");
        registry
            .start_attachment(
                "epoch",
                "session",
                "/workspace",
                "load",
                SessionOperationKind::Load,
            )
            .unwrap();
        let live = registry.session("session").unwrap().clone();

        for ready in [false, true] {
            if ready {
                registry.register_new("session");
            } else {
                registry.register_cold("session");
            }
            assert_eq!(registry.session("session"), Some(live.clone()));
            let state = registry.state("session").unwrap();
            assert_eq!(state.live.as_ref().unwrap().lifecycle, live.lifecycle);
            assert_eq!(state.operation.as_ref().unwrap().operation_id, "load");
            assert_eq!(
                registry.can_begin("session", SessionAdmission::Prompt),
                Err(MirrorError::WrongPhase)
            );
        }
        assert_eq!(registry.sessions.len(), 2);
        assert_eq!(registry.live_snapshot().sessions.len(), 1);
    }

    #[test]
    fn history_removal_preserves_live_state_and_the_cleanup_owner() {
        let mut registry = mirror();
        registry
            .open_new_with_replay(
                "epoch",
                "session",
                "/workspace",
                json!({ "title": "session" }),
                vec![],
            )
            .unwrap();
        registry.register_new("session");
        registry
            .begin_exclusive("session", SessionOperationKind::Close, "close")
            .unwrap();
        registry
            .start_operation(
                "epoch",
                "session",
                "close",
                SessionOperationKind::Close,
                "closing",
            )
            .unwrap();
        let live = registry.session("session").unwrap().clone();

        registry.remove("session");

        assert_eq!(registry.session("session"), Some(live.clone()));
        assert_eq!(registry.state("session").unwrap().log, SessionLog::Unknown);
        registry.close_session("epoch", "session", "close").unwrap();
        registry.remove("session");
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
            .finish_session_cleanup("session", SessionAdmission::Close, "close")
            .unwrap();
        registry.remove("session");
        assert!(registry.state("session").is_none());
        assert!(registry.live_snapshot().sessions.is_empty());
    }

    #[test]
    fn history_replacement_releases_the_turn_and_load_without_retiring_the_live_owner() {
        let mut registry = mirror();
        registry
            .open_new_with_replay("epoch", "session", "/workspace", json!({}), vec![])
            .unwrap();
        registry.register_new("session");
        let operation_id = registry
            .start_turn("session", "intent", text_prompt("next"))
            .unwrap();
        registry
            .complete_prompt(
                "epoch",
                "session",
                "intent",
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        registry
            .complete_turn(
                "session",
                &operation_id,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        registry.begin_load("session", "candidate").unwrap();
        registry
            .append_load_update("session", "candidate", user_update("candidate payload"))
            .unwrap();
        assert_eq!(registry.history.candidates(), 1);

        let live = registry.live("session").unwrap().clone();
        registry.clear_history_for_replacement("session");

        assert_eq!(registry.history.candidates(), 0);
        let state = registry.state("session").unwrap();
        assert_eq!(registry.live("session"), Some(&live));
        assert!(state.active_turn.is_none());
        assert!(state.load_attempt.is_none());
        assert!(state.load_origin.is_none());
        assert_eq!(state.log, SessionLog::Unknown);
        registry.clear_history_for_replacement("session");
        assert_eq!(registry.live("session"), Some(&live));
    }

    #[test]
    fn session_updates_preserve_reconciling_load_boundaries() {
        let mut mirror = mirror();
        mirror.register_new("session");
        let operation_id = mirror
            .start_turn("session", "intent", text_prompt("Run"))
            .unwrap();
        mirror.append_turn_update("session", &operation_id, json!({ "sessionUpdate": "tool_call", "toolCallId": "tool", "title": "Run", "status": "in_progress" })).unwrap();
        mirror
            .complete_turn(
                "session",
                &operation_id,
                json!({ "stopReason": "cancelled" }),
            )
            .unwrap();
        mirror.append_session_update("session", json!({ "sessionUpdate": "tool_call_update", "toolCallId": "tool", "status": "completed" })).unwrap();
        let turn = mirror
            .state("session")
            .unwrap()
            .active_turn
            .clone()
            .unwrap();
        assert_eq!(turn.terminal, Some(json!({ "stopReason": "cancelled" })));
        assert_eq!(turn.updates[0]["status"], "completed");
        mirror
            .commit_completed_turn("session", &operation_id)
            .unwrap();
        assert_eq!(mirror.state("session").unwrap().phase, MirrorPhase::Ready);
        mirror.begin_load("session", "load").unwrap();
        assert_eq!(
            mirror.append_session_update("session", json!({})),
            Err(MirrorError::WrongPhase)
        );
    }

    fn mirror() -> SessionMirror {
        SessionMirror::new("epoch")
    }

    fn load_initial(mirror: &mut SessionMirror, session_id: &str) {
        mirror.register_cold(session_id);
        mirror.begin_load(session_id, "initial").unwrap();
        mirror
            .append_load_update(session_id, "initial", user_update("old"))
            .unwrap();
        mirror
            .append_load_update(
                session_id,
                "initial",
                json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "old reply" } }),
            )
            .unwrap();
        mirror.commit_load(session_id, "initial").unwrap();
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

    fn timeline_entries(mirror: &SessionMirror) -> Value {
        let timeline = mirror
            .state("session")
            .unwrap()
            .timeline
            .as_ref()
            .expect("the timeline is known");
        serde_json::to_value(timeline.entries()).unwrap()
    }

    fn agent_update(text: &str, message_id: &str) -> Value {
        json!({
            "sessionUpdate": "agent_message_chunk", "messageId": message_id,
            "content": { "type": "text", "text": text }
        })
    }

    fn epoch(mirror: &SessionMirror, session_id: &str) -> Option<String> {
        mirror.state(session_id).unwrap().timeline_epoch.clone()
    }

    #[test]
    fn a_timeline_has_an_epoch_that_a_cold_session_lacks_and_each_new_session_has_its_own() {
        let mut mirror = mirror();
        mirror.register_cold("cold");
        mirror.register_new("a");
        mirror.register_new("b");
        assert_eq!(epoch(&mirror, "cold"), None);
        let (a, b) = (epoch(&mirror, "a").unwrap(), epoch(&mirror, "b").unwrap());
        assert_eq!(a.len(), 22, "128 random bits in unpadded base64url");
        assert_ne!(a, b);
    }

    #[test]
    fn live_updates_keep_the_epoch_and_a_load_or_a_resume_gives_the_timeline_a_new_one() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session");
        let loaded = epoch(&mirror, "session").unwrap();

        // Prompts, updates and idle updates grow the timeline within its epoch.
        let operation = mirror
            .start_turn("session", "intent", text_prompt("next"))
            .unwrap();
        mirror
            .append_turn_update("session", &operation, agent_update("a", "m"))
            .unwrap();
        mirror
            .complete_turn("session", &operation, json!({ "stopReason": "end_turn" }))
            .unwrap();
        mirror
            .append_session_update("session", agent_update("late", "m"))
            .unwrap();
        mirror.commit_completed_turn("session", &operation).unwrap();
        assert_eq!(epoch(&mirror, "session"), Some(loaded.clone()));

        // A failed load replaces nothing.
        mirror.begin_load("session", "failing").unwrap();
        mirror.fail_load("session", "failing", "boom", true).unwrap();
        assert_eq!(epoch(&mirror, "session"), Some(loaded.clone()));

        // A load that commits replaces the timeline: another epoch.
        mirror.begin_load("session", "again").unwrap();
        mirror.commit_load("session", "again").unwrap();
        let reloaded = epoch(&mirror, "session").unwrap();
        assert_ne!(reloaded, loaded);

        // A resume of a session whose history is unknown adopts a timeline.
        mirror.register_cold("resumed");
        mirror.begin_load("resumed", "resume").unwrap();
        mirror.commit_attachment_cache("resumed", "resume").unwrap();
        let adopted = epoch(&mirror, "resumed").unwrap();
        // A second resume keeps the timeline it already has, and so its epoch.
        mirror.begin_load("resumed", "again").unwrap();
        mirror.commit_attachment_cache("resumed", "again").unwrap();
        assert_eq!(epoch(&mirror, "resumed"), Some(adopted));
    }

    #[test]
    fn going_on_with_given_context_gives_an_epoch_and_a_known_timeline_keeps_its_own() {
        let mut mirror = mirror();
        mirror.register_cold("cold");
        mirror.use_cached_history("cold", None).unwrap();
        assert!(epoch(&mirror, "cold").is_some());

        mirror.register_new("new");
        let before = epoch(&mirror, "new");
        mirror.use_cached_history("new", None).unwrap();
        assert_eq!(epoch(&mirror, "new"), before);
    }

    #[test]
    fn a_new_session_has_an_empty_known_timeline_and_a_cold_one_has_none() {
        let mut mirror = mirror();
        mirror.register_cold("cold");
        mirror.register_new("new");
        assert!(mirror.state("cold").unwrap().timeline.is_none());
        let new = mirror.state("new").unwrap().timeline.as_ref().unwrap();
        assert!(new.entries().is_empty());
    }

    fn history(mirror: &SessionMirror, session_id: &str) -> Option<History> {
        mirror.state(session_id).unwrap().history
    }

    #[test]
    fn history_is_unknown_for_a_cold_session_and_complete_for_a_new_one() {
        let mut mirror = mirror();
        mirror.register_cold("cold");
        mirror.register_new("new");
        assert_eq!(history(&mirror, "cold"), None);
        assert_eq!(history(&mirror, "new"), Some(History::Complete));
    }

    #[test]
    fn a_successful_empty_load_is_complete_not_unknown() {
        let mut mirror = mirror();
        mirror.register_cold("session");
        mirror.begin_load("session", "load").unwrap();
        assert_eq!(history(&mirror, "session"), None);
        mirror.commit_load("session", "load").unwrap();
        assert_eq!(history(&mirror, "session"), Some(History::Complete));
        assert!(timeline_entries(&mirror).as_array().unwrap().is_empty());
    }

    #[test]
    fn a_resume_without_a_replay_gives_partial_history_and_a_load_makes_it_complete() {
        let mut mirror = mirror();
        mirror.register_cold("session");
        mirror.begin_load("session", "resume").unwrap();
        mirror
            .append_load_update("session", "resume", user_update("cached"))
            .unwrap();
        mirror.commit_attachment_cache("session", "resume").unwrap();
        assert_eq!(history(&mirror, "session"), Some(History::Partial));

        // A second resume keeps the status of the timeline it already has.
        mirror.begin_load("session", "again").unwrap();
        mirror.commit_attachment_cache("session", "again").unwrap();
        assert_eq!(history(&mirror, "session"), Some(History::Partial));

        mirror.begin_load("session", "load").unwrap();
        mirror.commit_load("session", "load").unwrap();
        assert_eq!(history(&mirror, "session"), Some(History::Complete));
    }

    #[test]
    fn going_on_with_the_context_the_agent_gave_is_partial_and_keeps_a_known_status() {
        let mut mirror = mirror();
        mirror.register_cold("cold");
        mirror.use_cached_history("cold", None).unwrap();
        assert_eq!(history(&mirror, "cold"), Some(History::Partial));
        assert!(mirror.state("cold").unwrap().timeline.is_some());

        mirror.register_new("new");
        mirror.use_cached_history("new", None).unwrap();
        assert_eq!(history(&mirror, "new"), Some(History::Complete));
    }

    #[test]
    fn a_load_reduces_its_replay_as_received_into_the_timeline() {
        let mut mirror = mirror();
        mirror.register_cold("session");
        mirror.begin_load("session", "load").unwrap();
        for update in [
            agent_update("first", "a"),
            agent_update("second", "b"),
            json!({ "sessionUpdate": "tool_call", "toolCallId": "tool", "title": "Run" }),
            user_update("next turn"),
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "tool", "status": "completed" }),
        ] {
            mirror.append_load_update("session", "load", update).unwrap();
        }
        // The timeline is the candidate's until the load commits.
        assert!(mirror.state("session").unwrap().timeline.is_none());
        mirror.commit_load("session", "load").unwrap();
        assert_eq!(
            timeline_entries(&mirror),
            json!([
                { "kind": "message", "role": "agent", "messageId": "a",
                  "chunks": [{ "content": { "type": "text", "text": "first" } }] },
                { "kind": "message", "role": "agent", "messageId": "b",
                  "chunks": [{ "content": { "type": "text", "text": "second" } }] },
                { "kind": "tool", "call": { "toolCallId": "tool", "title": "Run", "kind": "other",
                  "status": "completed", "content": [], "locations": [] } },
                { "kind": "message", "role": "user",
                  "chunks": [{ "content": { "type": "text", "text": "next turn" } }] },
            ])
        );
    }

    #[test]
    fn a_second_load_replaces_the_timeline_and_a_failed_one_keeps_it() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session");
        let first = mirror.state("session").unwrap().timeline.clone().unwrap();

        mirror.begin_load("session", "failing").unwrap();
        mirror
            .append_load_update("session", "failing", agent_update("partial", "x"))
            .unwrap();
        mirror
            .fail_load("session", "failing", "boom", true)
            .unwrap();
        let kept = mirror.state("session").unwrap().timeline.clone().unwrap();
        assert!(Arc::ptr_eq(&first, &kept), "a failed load changes nothing");

        mirror.begin_load("session", "again").unwrap();
        mirror
            .append_load_update("session", "again", agent_update("new", "n"))
            .unwrap();
        mirror.commit_load("session", "again").unwrap();
        assert_eq!(
            timeline_entries(&mirror),
            json!([{ "kind": "message", "role": "agent", "messageId": "n",
                     "chunks": [{ "content": { "type": "text", "text": "new" } }] }])
        );
    }

    #[test]
    fn a_resume_gives_the_timeline_only_while_it_is_unknown() {
        let mut mirror = mirror();
        mirror.register_cold("session");
        mirror.begin_load("session", "resume").unwrap();
        mirror
            .append_load_update("session", "resume", user_update("cached"))
            .unwrap();
        mirror.commit_attachment_cache("session", "resume").unwrap();
        let adopted = mirror.state("session").unwrap().timeline.clone().unwrap();
        assert_eq!(adopted.entries().len(), 1);

        mirror.begin_load("session", "again").unwrap();
        mirror
            .append_load_update("session", "again", user_update("other"))
            .unwrap();
        mirror.commit_attachment_cache("session", "again").unwrap();
        let kept = mirror.state("session").unwrap().timeline.clone().unwrap();
        assert!(Arc::ptr_eq(&adopted, &kept), "a known timeline stays");
    }

    #[test]
    fn a_load_rebuilds_the_log_from_its_replay() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session");
        let SessionLog::Known(known) = log(&mirror) else {
            panic!("a committed load knows the log");
        };
        assert_eq!(known.entries().len(), 1);
        assert_eq!(mirror.state("session").unwrap().phase, MirrorPhase::Ready);
        assert_eq!(mirror.history.candidates(), 0);
    }

    fn text(text: &str) -> Value {
        json!({ "type": "text", "text": text })
    }

    /// The entries from the start of the prompt `load_initial` leaves behind.
    fn entries_from(mirror: &SessionMirror, from: usize) -> Value {
        Value::Array(timeline_entries(mirror).as_array().unwrap()[from..].to_vec())
    }

    fn prompts(mirror: &SessionMirror) -> Value {
        serde_json::to_value(
            mirror
                .state("session")
                .unwrap()
                .timeline
                .as_ref()
                .unwrap()
                .prompts(),
        )
        .unwrap()
    }

    #[test]
    fn a_started_prompt_is_a_user_entry_and_a_running_prompt() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session");
        mirror
            .start_turn("session", "intent", text_prompt("next"))
            .unwrap();
        assert_eq!(
            entries_from(&mirror, 2),
            json!([{ "kind": "message", "role": "user", "chunks": [{ "content": text("next") }] }])
        );
        assert_eq!(prompts(&mirror), json!([{ "entry": 2 }]));
    }

    #[test]
    fn live_updates_are_reduced_as_received_and_the_response_ends_the_prompt() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session");
        let operation = mirror
            .start_turn("session", "intent", text_prompt("next"))
            .unwrap();
        for update in [
            agent_update("Hel", "m1"),
            agent_update("lo", "m1"),
            json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "Run" }),
            agent_update(" again", "m1"),
        ] {
            mirror
                .append_turn_update("session", &operation, update)
                .unwrap();
        }
        mirror
            .complete_turn("session", &operation, json!({ "stopReason": "end_turn" }))
            .unwrap();
        assert_eq!(
            entries_from(&mirror, 2),
            json!([
                { "kind": "message", "role": "user", "chunks": [{ "content": text("next") }] },
                { "kind": "message", "role": "agent", "messageId": "m1",
                  "chunks": [{ "content": text("Hello") }] },
                { "kind": "tool", "call": { "toolCallId": "t", "title": "Run", "kind": "other",
                  "status": "pending", "content": [], "locations": [] } },
                { "kind": "message", "role": "agent", "messageId": "m1",
                  "chunks": [{ "content": text(" again") }] },
            ])
        );
        assert_eq!(
            prompts(&mirror),
            json!([{ "entry": 2, "end": { "response": { "stopReason": "end_turn" } } }])
        );
    }

    /// A prompt that streamed `partial` and then ended as `end` says.
    fn prompt_with_partial_output(mirror: &mut SessionMirror) -> String {
        load_initial(mirror, "session");
        let operation = mirror
            .start_turn("session", "intent", text_prompt("next"))
            .unwrap();
        mirror
            .append_turn_update("session", &operation, agent_update("partial", "m"))
            .unwrap();
        operation
    }

    fn assert_partial_output_kept(mirror: &SessionMirror) {
        assert_eq!(
            entries_from(mirror, 3),
            json!([{ "kind": "message", "role": "agent", "messageId": "m",
                     "chunks": [{ "content": text("partial") }] }]),
            "content the Agent sent is not withdrawn by how the prompt ended"
        );
    }

    #[test]
    fn a_cancelled_stop_reason_is_a_response_and_keeps_the_content() {
        let mut mirror = mirror();
        let operation = prompt_with_partial_output(&mut mirror);
        mirror
            .complete_turn("session", &operation, json!({ "stopReason": "cancelled" }))
            .unwrap();
        assert_partial_output_kept(&mirror);
        assert_eq!(
            prompts(&mirror),
            json!([{ "entry": 2, "end": { "response": { "stopReason": "cancelled" } } }])
        );
    }

    #[test]
    fn an_agent_error_is_the_prompts_end_and_keeps_the_content() {
        let mut mirror = mirror();
        let operation = prompt_with_partial_output(&mut mirror);
        mirror
            .complete_turn(
                "session",
                &operation,
                json!({ "error": { "code": -32603, "message": "boom" } }),
            )
            .unwrap();
        assert_partial_output_kept(&mirror);
        assert_eq!(
            prompts(&mirror),
            json!([{ "entry": 2, "end": { "error": { "code": -32603, "message": "boom" } } }])
        );
    }

    #[test]
    fn an_invalid_response_is_recorded_as_an_error_not_as_success() {
        let mut mirror = mirror();
        let operation = prompt_with_partial_output(&mut mirror);
        mirror
            .complete_turn(
                "session",
                &operation,
                json!({ "invalidPromptResponse": "stopReason is missing" }),
            )
            .unwrap();
        assert_partial_output_kept(&mirror);
        assert_eq!(
            prompts(&mirror),
            json!([{ "entry": 2, "end": { "error": { "invalidPromptResponse": "stopReason is missing" } } }])
        );
    }

    #[test]
    fn a_lost_answer_is_interrupted_never_an_invented_stop_reason() {
        let mut mirror = mirror();
        let operation = prompt_with_partial_output(&mut mirror);
        mirror
            .block_turn("session", &operation, "transport closed")
            .unwrap();
        assert_partial_output_kept(&mirror);
        assert_eq!(
            prompts(&mirror),
            json!([{ "entry": 2, "end": { "interrupted": true } }])
        );
    }

    #[test]
    fn blocking_after_the_response_does_not_replace_the_recorded_end() {
        let mut mirror = mirror();
        let operation = prompt_with_partial_output(&mut mirror);
        mirror
            .complete_turn("session", &operation, json!({ "stopReason": "end_turn" }))
            .unwrap();
        mirror
            .block_turn("session", &operation, "commit failed")
            .unwrap();
        assert_eq!(
            prompts(&mirror),
            json!([{ "entry": 2, "end": { "response": { "stopReason": "end_turn" } } }])
        );
    }

    #[test]
    fn an_update_between_prompts_is_reduced_into_the_timeline() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session");
        assert_eq!(mirror.state("session").unwrap().phase, MirrorPhase::Ready);
        // A thought is a different role from the last entry, so it starts one.
        mirror
            .append_session_update(
                "session",
                json!({ "sessionUpdate": "agent_thought_chunk", "messageId": "n",
                        "content": text("unprompted") }),
            )
            .unwrap();
        assert_eq!(
            entries_from(&mirror, 2),
            json!([{ "kind": "message", "role": "thought", "messageId": "n",
                     "chunks": [{ "content": text("unprompted") }] }])
        );
        // The session is still idle and no prompt was invented for the update.
        let state = mirror.state("session").unwrap();
        assert_eq!(state.phase, MirrorPhase::Ready);
        assert!(state.active_turn.is_none());
        assert_eq!(prompts(&mirror), json!([]));
    }

    #[test]
    fn a_late_tool_update_between_prompts_changes_its_entry_in_place() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session");
        let operation = mirror
            .start_turn("session", "intent", text_prompt("next"))
            .unwrap();
        mirror
            .append_turn_update(
                "session",
                &operation,
                json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "Run" }),
            )
            .unwrap();
        mirror
            .complete_turn("session", &operation, json!({ "stopReason": "end_turn" }))
            .unwrap();
        mirror.commit_completed_turn("session", &operation).unwrap();
        assert_eq!(mirror.state("session").unwrap().phase, MirrorPhase::Ready);

        mirror
            .append_session_update(
                "session",
                json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t", "status": "completed" }),
            )
            .unwrap();
        let entries = entries_from(&mirror, 2);
        assert_eq!(entries.as_array().unwrap().len(), 2, "no entry was added");
        assert_eq!(entries[1]["call"]["status"], json!("completed"));
    }

    #[test]
    fn an_idle_update_for_a_session_with_unknown_history_changes_nothing() {
        let mut mirror = mirror();
        mirror.register_cold("session");
        // A cold session is not Ready: the update is refused, as before.
        assert!(
            mirror
                .append_session_update("session", agent_update("early", "x"))
                .is_err()
        );
        assert!(mirror.state("session").unwrap().timeline.is_none());
    }

    #[test]
    fn the_notification_envelope_is_kept_on_replayed_live_and_idle_updates() {
        let mut mirror = mirror();
        mirror.register_cold("session");
        mirror.begin_load("session", "load").unwrap();
        mirror
            .append_load_update_with_meta(
                "session",
                "load",
                agent_update("replayed", "r"),
                Some(&json!({ "from": "replay" })),
            )
            .unwrap();
        mirror.commit_load("session", "load").unwrap();
        assert_eq!(
            timeline_entries(&mirror)[0]["chunks"][0]["notificationMeta"],
            json!({ "from": "replay" })
        );

        let operation = mirror
            .start_turn("session", "intent", text_prompt("next"))
            .unwrap();
        mirror
            .append_turn_update_with_meta(
                "session",
                &operation,
                json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "Run" }),
                Some(&json!({ "from": "live" })),
            )
            .unwrap();
        mirror
            .complete_turn("session", &operation, json!({ "stopReason": "end_turn" }))
            .unwrap();
        mirror
            .append_session_update_with_meta(
                "session",
                json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t", "status": "completed" }),
                Some(&json!({ "from": "late" })),
            )
            .unwrap();
        let entries = timeline_entries(&mirror);
        assert_eq!(entries[2]["notificationMeta"], json!({ "from": "late" }));
        assert_eq!(entries[2]["call"]["status"], json!("completed"));
    }

    #[test]
    fn an_update_after_the_response_changes_its_entry_in_place() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session");
        let operation = mirror
            .start_turn("session", "intent", text_prompt("next"))
            .unwrap();
        mirror
            .append_turn_update(
                "session",
                &operation,
                json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "Run" }),
            )
            .unwrap();
        mirror
            .complete_turn("session", &operation, json!({ "stopReason": "end_turn" }))
            .unwrap();
        mirror
            .append_session_update(
                "session",
                json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t", "status": "completed" }),
            )
            .unwrap();
        let entries = entries_from(&mirror, 2);
        assert_eq!(entries.as_array().unwrap().len(), 2, "no entry was added");
        assert_eq!(entries[1]["call"]["status"], json!("completed"));
    }

    #[test]
    fn a_rejected_update_leaves_the_timeline_unchanged() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session");
        let operation = mirror
            .start_turn("session", "intent", text_prompt("next"))
            .unwrap();
        let before = timeline_entries(&mirror);
        assert!(
            mirror
                .append_turn_update(
                    "session",
                    &operation,
                    json!({ "sessionUpdate": "plan", "entries": 7 }),
                )
                .is_err()
        );
        assert_eq!(timeline_entries(&mirror), before);
    }

    #[test]
    fn an_exact_echo_of_the_prompt_is_not_a_second_user_entry() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session");
        let operation = mirror
            .start_turn("session", "intent", text_prompt("next"))
            .unwrap();
        mirror
            .append_turn_update(
                "session",
                &operation,
                json!({ "sessionUpdate": "user_message_chunk", "messageId": "u1", "content": text("next") }),
            )
            .unwrap();
        assert_eq!(
            entries_from(&mirror, 2),
            json!([{ "kind": "message", "role": "user", "messageId": "u1",
                     "chunks": [{ "content": text("next") }] }])
        );
    }

    #[test]
    fn prompt_terminal_retains_the_turn_until_its_commit() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session");
        let operation = mirror
            .start_turn("session", "intent", text_prompt("next"))
            .unwrap();
        mirror
            .append_turn_update(
                "session",
                &operation,
                json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "answer" } }),
            )
            .unwrap();
        mirror
            .complete_turn("session", &operation, json!({ "stopReason": "end_turn" }))
            .unwrap();

        let state = mirror.state("session").unwrap();
        assert_eq!(state.phase, MirrorPhase::Reconciling);
        assert_eq!(state.active_turn.as_ref().unwrap().updates.len(), 1);
        assert_eq!(
            state.active_turn.as_ref().unwrap().terminal,
            Some(json!({ "stopReason": "end_turn" }))
        );
        assert_eq!(
            mirror.commit_completed_turn("session", "stale-operation"),
            Err(MirrorError::OperationMismatch)
        );
        mirror.commit_completed_turn("session", &operation).unwrap();
        let state = mirror.state("session").unwrap();
        assert_eq!(state.phase, MirrorPhase::Ready);
        assert!(state.active_turn.is_none());
        assert_eq!(
            mirror.can_begin("session", SessionAdmission::Prompt),
            Ok(())
        );
    }

    #[test]
    fn resumed_cache_does_not_enable_commands_between_automatic_load_attempts() {
        let mut mirror = mirror();
        mirror.register_cold("session");
        mirror.begin_load("session", "resume").unwrap();
        mirror
            .append_load_update("session", "resume", user_update("cached"))
            .unwrap();
        mirror.commit_attachment_cache("session", "resume").unwrap();
        assert_eq!(mirror.state("session").unwrap().phase, MirrorPhase::Cold);
        let cached = log(&mirror);
        assert!(matches!(cached, SessionLog::Known(_)));

        for attempt in ["automatic-first", "automatic-retry"] {
            mirror.begin_load("session", attempt).unwrap();
            mirror
                .fail_load("session", attempt, "temporary", true)
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
                    mirror.can_begin("session", admission),
                    Err(MirrorError::WrongPhase)
                );
            }
        }
        mirror.begin_load("session", "successful-retry").unwrap();
        mirror
            .append_load_update("session", "successful-retry", user_update("complete"))
            .unwrap();
        mirror.commit_load("session", "successful-retry").unwrap();
        assert_eq!(
            mirror.can_begin("session", SessionAdmission::Prompt),
            Ok(())
        );
    }

    #[test]
    fn cached_history_keeps_a_known_log_and_installs_one_only_when_unknown() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session");
        let known = log(&mirror);
        mirror
            .use_cached_history("session", Some(Arc::new(TurnLog::default())))
            .unwrap();
        assert_eq!(log(&mirror), known);

        mirror.register_cold("fork");
        let SessionLog::Known(source) = known else {
            unreachable!()
        };
        mirror
            .use_cached_history("fork", Some(source.clone()))
            .unwrap();
        let fork = mirror.state("fork").unwrap();
        assert_eq!(fork.phase, MirrorPhase::Ready);
        assert!(matches!(&fork.log, SessionLog::Known(log) if Arc::ptr_eq(log, &source)));
    }

    #[test]
    fn failed_explicit_load_restores_its_admission_phase() {
        for origin in [MirrorPhase::Ready, MirrorPhase::Blocked] {
            let mut mirror = mirror();
            load_initial(&mut mirror, "session");
            let old = log(&mirror);
            if origin == MirrorPhase::Blocked {
                mirror.begin_load("session", "fatal").unwrap();
                mirror
                    .fail_load("session", "fatal", "unrecoverable", false)
                    .unwrap();
            }
            mirror
                .begin_exclusive("session", SessionOperationKind::Load, "explicit")
                .unwrap();
            mirror.begin_load("session", "explicit").unwrap();
            mirror
                .append_load_update("session", "explicit", user_update("partial"))
                .unwrap();
            mirror
                .fail_load("session", "explicit", "temporary", true)
                .unwrap();
            assert_eq!(mirror.state("session").unwrap().phase, origin);
            assert_eq!(
                mirror.can_begin("session", SessionAdmission::Attachment),
                Err(MirrorError::WrongPhase)
            );
            mirror
                .settle_exclusive("session", SessionAdmission::Attachment, "explicit")
                .unwrap();
            assert_eq!(
                mirror.can_begin("session", SessionAdmission::Attachment),
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
        mirror.register_new("session");
        mirror
            .start_turn("session", "first", vec![json!("one")])
            .unwrap();

        assert_eq!(
            mirror.start_turn("session", "second", vec![json!("two")]),
            Err(MirrorError::WrongPhase)
        );
    }

    #[test]
    fn retryable_initial_load_can_start_a_new_single_flight_attempt() {
        let mut mirror = mirror();
        mirror.register_cold("session");
        mirror.begin_load("session", "first").unwrap();
        mirror
            .append_load_update("session", "first", user_update("partial"))
            .unwrap();
        mirror
            .fail_load("session", "first", "temporary", true)
            .unwrap();
        assert_eq!(log(&mirror), SessionLog::Unknown);

        mirror.begin_load("session", "second").unwrap();
        mirror
            .append_load_update("session", "second", user_update("complete"))
            .unwrap();
        mirror.commit_load("session", "second").unwrap();

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
        mirror.register_cold("session");
        mirror.begin_load("session", "invalid").unwrap();
        mirror
            .append_load_update("session", "invalid", user_update("retained"))
            .unwrap();
        mirror.invalidate_load("session", "invalid").unwrap();

        assert!(matches!(
            mirror.append_load_update("session", "invalid", user_update("ignored")),
            Err(MirrorError::History(HistoryCacheError::InvalidReplay))
        ));
        assert!(matches!(
            mirror.commit_load("session", "invalid"),
            Err(MirrorError::History(HistoryCacheError::InvalidReplay))
        ));
        assert_eq!(log(&mirror), SessionLog::Unknown);
        mirror
            .fail_load("session", "invalid", "invalid replay", false)
            .unwrap();
        assert_eq!(mirror.state("session").unwrap().phase, MirrorPhase::Blocked);
        assert_eq!(log(&mirror), SessionLog::Unknown);
    }

    #[test]
    fn a_large_valid_turn_is_kept_whole() {
        let mut mirror = mirror();
        mirror.register_new("session");
        let operation = mirror
            .start_turn("session", "intent", text_prompt("prompt"))
            .unwrap();
        mirror
            .append_turn_update(
                "session",
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

        mirror.abort_turn("session", &operation).unwrap();
        assert!(mirror.state("session").unwrap().active_turn.is_none());
    }
}
