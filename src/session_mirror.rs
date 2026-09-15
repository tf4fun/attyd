use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use crate::history_cache::{
    HistoryCacheError, HistorySnapshot, SessionKey, fold_history_update, normalize_history_updates,
};

use crate::runtime_state::SessionOperationKind;
#[cfg(test)]
pub(crate) use crate::session_registry::SessionRegistry as SessionMirror;
use crate::session_registry::{SessionEntry, SessionRegistry};
use crate::session_state::{CompletedTurnOutcome, SessionAdmission};
pub(crate) use crate::session_state::{
    MirrorError, MirrorPhase, SessionState as MirrorSessionState, TurnAdmission, TurnOverlay,
};

pub(crate) struct SessionView {
    pub session: SessionHistoryView,
    pub baseline: Arc<HistorySnapshot>,
}

/// The history view contains only published fields. In particular, taking a
/// history snapshot never clones live resource registries or admission state.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionHistoryView {
    pub session_id: String,
    pub incarnation: u64,
    pub view_revision: u64,
    pub history_revision: Option<String>,
    pub phase: MirrorPhase,
    pub active_turn: Option<TurnOverlay>,
    pub turn_outcomes: Vec<CompletedTurnOutcome>,
    pub sync_error: Option<String>,
    pub history_notice: Option<String>,
}

impl From<&MirrorSessionState> for SessionHistoryView {
    fn from(state: &MirrorSessionState) -> Self {
        Self {
            session_id: state.session_id.clone(),
            incarnation: state.incarnation,
            view_revision: state.view_revision,
            history_revision: state.history_revision.clone(),
            phase: state.phase,
            active_turn: state.active_turn.clone(),
            turn_outcomes: state.turn_outcomes.clone(),
            sync_error: state.sync_error.clone(),
            history_notice: state.history_notice.clone(),
        }
    }
}

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
                self.retire_entry(&session_id, previous, "session incarnation was replaced");
            }
        }
        let snapshot = self
            .history
            .install_empty(SessionKey::new(session_id.clone(), incarnation));
        let mut state = if ready {
            MirrorSessionState::ready(
                session_id.clone(),
                incarnation,
                snapshot.revision().to_string(),
            )
        } else {
            MirrorSessionState::cold(session_id.clone(), incarnation)
        };
        if let Some(entry) = self.sessions.get_mut(&session_id) {
            state.live = entry.state.live.take();
            state.operation = entry.state.operation.take();
            state.view_revision = entry.state.view_revision;
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
        let session = self
            .sessions
            .get_mut(session_id)
            .map(|entry| &mut entry.state)
            .expect("session was validated above");
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
        session.view_revision = next_revision(session.view_revision);
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
        let session = self.require_state(session_id, incarnation)?;
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
        let session = self.require_state(session_id, incarnation)?;
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
        let session = self.require_state(session_id, incarnation)?;
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
            let replay = self.history.candidate(&key, attempt_id)?;
            let data = replay.lock();
            let candidate = data.updates()?;
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
        session.validate_history_commit()?;
        let old_bytes = session.active_overlay_bytes;
        let snapshot = self.history.commit_candidate(&key, attempt_id)?;
        let session = self
            .sessions
            .get_mut(session_id)
            .map(|entry| &mut entry.state)
            .expect("session was validated above");
        session.history_committed(
            snapshot.revision().to_string(),
            snapshot.updates().len(),
            load_phase == MirrorPhase::Loading,
        );
        self.overlay_bytes = self.overlay_bytes.saturating_sub(old_bytes);
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
        self.require_state_mut(session_id, incarnation)?.phase = MirrorPhase::Cold;
        Ok(snapshot)
    }

    pub(crate) fn use_cached_history(
        &mut self,
        session_id: &str,
        incarnation: u64,
        updates: &[Value],
        notice: String,
    ) -> Result<(), MirrorError> {
        let session = self.require_state_mut(session_id, incarnation)?;
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
        self.require_state_mut(session_id, incarnation)?
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
        let session = self.require_state(session_id, incarnation)?;
        if session.load_attempt.as_deref() != Some(attempt_id) {
            return Err(MirrorError::OperationMismatch);
        }
        let key = SessionKey::new(session_id, incarnation);
        self.history.abort_candidate(&key, attempt_id)?;
        let session = self
            .sessions
            .get_mut(session_id)
            .map(|entry| &mut entry.state)
            .expect("session was validated above");
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
        let next_operation = self.next_operation.wrapping_add(1).max(1);
        let operation_id = format!("{}:{}", self.epoch, next_operation);
        let session = self.require_state_mut(session_id, incarnation)?;
        let old_bytes = session.active_overlay_bytes;
        let admission = session.admit_turn(
            operation_id,
            expected_history_revision,
            client_intent_id,
            prompt,
        )?;
        let new_bytes = session.active_overlay_bytes;
        if matches!(admission, TurnAdmission::Accepted { .. }) {
            self.next_operation = next_operation;
        }
        self.overlay_bytes = self
            .overlay_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        Ok(admission)
    }

    pub(crate) fn append_turn_update(
        &mut self,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
        update: Value,
    ) -> Result<(), MirrorError> {
        let session = self.require_state_mut(session_id, incarnation)?;
        let old_bytes = session.active_overlay_bytes;
        session.append_turn_update(operation_id, update)?;
        let new_bytes = session.active_overlay_bytes;
        self.overlay_bytes = self
            .overlay_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        Ok(())
    }

    pub(crate) fn append_session_update(
        &mut self,
        session_id: &str,
        incarnation: u64,
        update: Value,
    ) -> Result<(), MirrorError> {
        let session = self.require_state(session_id, incarnation)?;
        if session.load_attempt.is_some() {
            return Err(MirrorError::WrongPhase);
        }
        if session.phase == MirrorPhase::Reconciling {
            let old_bytes = session.active_overlay_bytes;
            let session = self.require_state_mut(session_id, incarnation)?;
            session.append_reconciling_update(update)?;
            let new_bytes = session.active_overlay_bytes;
            self.overlay_bytes = self
                .overlay_bytes
                .saturating_sub(old_bytes)
                .saturating_add(new_bytes);
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
            .map(|entry| &mut entry.state)
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
        let session = self.require_state(session_id, incarnation)?;
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
            .map(|entry| &mut entry.state)
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
        let session = self.require_state_mut(session_id, incarnation)?;
        let old_bytes = session.active_overlay_bytes;
        session.complete_turn(operation_id, terminal)?;
        let new_bytes = session.active_overlay_bytes;
        self.overlay_bytes = self
            .overlay_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        Ok(())
    }

    pub(crate) fn commit_completed_turn_from_memory(
        &mut self,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
    ) -> Result<Arc<HistorySnapshot>, MirrorError> {
        let session = self.require_state(session_id, incarnation)?;
        if session.phase != MirrorPhase::Reconciling {
            return Err(MirrorError::WrongPhase);
        }
        let turn = session
            .active_turn
            .as_ref()
            .filter(|turn| turn.operation_id == operation_id && turn.terminal.is_some())
            .ok_or(MirrorError::OperationMismatch)?
            .clone();
        session
            .history_revision
            .as_ref()
            .ok_or(MirrorError::InconsistentHistory)?;
        session.validate_history_commit()?;
        let active_overlay_bytes = session.active_overlay_bytes;
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
        let session = self
            .sessions
            .get_mut(session_id)
            .map(|entry| &mut entry.state)
            .expect("session was validated above");
        session.history_committed(
            snapshot.revision().to_string(),
            snapshot.updates().len(),
            false,
        );
        self.overlay_bytes = self.overlay_bytes.saturating_sub(active_overlay_bytes);
        Ok(snapshot)
    }

    pub(crate) fn view(
        &mut self,
        session_id: &str,
        incarnation: u64,
    ) -> Result<SessionView, MirrorError> {
        let session = SessionHistoryView::from(self.require_state(session_id, incarnation)?);
        let baseline = self
            .history
            .snapshot(&SessionKey::new(session_id, incarnation))
            .ok_or(MirrorError::WrongPhase)?;
        Ok(SessionView { session, baseline })
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

    pub(crate) fn touch(&mut self, session_id: &str, incarnation: u64) -> Result<u64, MirrorError> {
        let session = self.require_state_mut(session_id, incarnation)?;
        session.view_revision = next_revision(session.view_revision);
        Ok(session.view_revision)
    }

    pub(crate) fn abort_turn(
        &mut self,
        session_id: &str,
        incarnation: u64,
        operation_id: &str,
    ) -> Result<(), MirrorError> {
        let session = self.require_state_mut(session_id, incarnation)?;
        let old_bytes = session.active_overlay_bytes;
        session.abort_turn(operation_id)?;
        self.overlay_bytes = self.overlay_bytes.saturating_sub(old_bytes);
        Ok(())
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
        self.clear_history_for_replacement(session_id, incarnation);
        if self.sessions.get(session_id).is_some_and(|entry| {
            let session = &entry.state;
            session.incarnation == incarnation
                && session.live.is_none()
                && session.operation.is_none()
                && entry.resources.materialization.is_none()
        }) {
            self.retire_entry(session_id, incarnation, "session was removed");
        }
    }

    /// Releases this incarnation's baseline, load candidate, overlay, and retained
    /// terminal output. Live lifecycle and the operation owner remain available
    /// until their own transition completes; a stale cleanup cannot touch a new
    /// incarnation. Runtime replacement calls this before installing its entry.
    pub(crate) fn clear_history_for_replacement(&mut self, session_id: &str, incarnation: u64) {
        if self
            .sessions
            .get(session_id)
            .is_none_or(|entry| entry.state.incarnation != incarnation)
        {
            return;
        }
        let session = self
            .sessions
            .get_mut(session_id)
            .map(|entry| &mut entry.state)
            .expect("validated session");
        self.overlay_bytes = self
            .overlay_bytes
            .saturating_sub(session.active_overlay_bytes);
        let mut cleared = MirrorSessionState::cold(session_id, incarnation);
        cleared.live = session.live.take();
        cleared.operation = session.operation.take();
        cleared.view_revision = next_revision(session.view_revision);
        *session = cleared;
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

fn serialized_len(value: &impl Serialize) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |value| value.len())
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
            assert!(
                registry.view_value("session", incarnation).unwrap()["session"]
                    .get("live")
                    .is_none()
            );
        }
        assert_eq!(registry.sessions.len(), 2);
        assert_eq!(registry.snapshot().sessions.len(), 1);
        assert_eq!(registry.history.stats().snapshots, 2);
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
        assert!(
            registry
                .state("session")
                .unwrap()
                .history_revision
                .is_none()
        );
        assert!(
            registry
                .history
                .peek(&SessionKey::new("session", incarnation))
                .is_none()
        );
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
        assert!(registry.snapshot().sessions.is_empty());
    }

    #[test]
    fn history_replacement_releases_every_allocation_without_retiring_the_live_owner() {
        let mut registry = mirror();
        let incarnation = registry
            .open_new_with_replay("epoch", "session", "/workspace", json!({}), vec![])
            .unwrap();
        registry.register_new("session", incarnation);
        registry
            .append_session_update(
                "session",
                incarnation,
                json!({
                    "sessionUpdate": "tool_call", "toolCallId": "tool", "title": "run",
                    "content": [{ "type": "terminal", "terminalId": "terminal" }],
                }),
            )
            .unwrap();
        registry
            .retain_terminal_output(
                "session",
                incarnation,
                "terminal",
                &json!({
                    "output": "retained output", "released": true,
                }),
            )
            .unwrap();
        let revision = registry
            .state("session")
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        let TurnAdmission::Accepted { operation_id } = registry
            .start_turn(
                "session",
                incarnation,
                &revision,
                "intent",
                text_prompt("next"),
            )
            .unwrap()
        else {
            panic!("turn must be admitted");
        };
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
        let old_key = SessionKey::new("session", incarnation);
        assert!(registry.history.stats().snapshot_bytes > 0);
        assert!(registry.history.stats().candidate_bytes > 0);
        assert!(registry.overlay_bytes > 0);
        assert!(registry.retained_terminal_bytes > 0);

        let live = registry.live("session").unwrap().clone();
        registry.clear_history_for_replacement("session", incarnation);

        assert!(registry.history.peek(&old_key).is_none());
        assert_eq!(registry.history.stats().candidates, 0);
        assert_eq!(registry.history.stats().snapshot_bytes, 0);
        assert_eq!(registry.history.stats().candidate_bytes, 0);
        assert_eq!(registry.overlay_bytes, 0);
        assert_eq!(registry.retained_terminal_bytes, 0);
        assert!(registry.retained_terminals.is_empty());
        assert_eq!(registry.state("session").unwrap().incarnation, incarnation);
        assert_eq!(registry.live("session"), Some(&live));
        assert!(registry.state("session").unwrap().active_turn.is_none());
        assert!(registry.state("session").unwrap().load_attempt.is_none());
        assert!(registry.state("session").unwrap().load_origin.is_none());
        registry.clear_history_for_replacement("session", incarnation + 1);
        assert_eq!(registry.live("session"), Some(&live));
    }

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
    fn resumed_cache_does_not_enable_commands_between_automatic_load_attempts() {
        let mut mirror = mirror();
        mirror.register_cold("session", 1);
        mirror.begin_load("session", 1, "resume").unwrap();
        mirror
            .append_load_update("session", 1, "resume", user_update("cached"))
            .unwrap();
        let cached = mirror
            .commit_attachment_cache("session", 1, "resume")
            .unwrap();
        assert_eq!(mirror.state("session").unwrap().phase, MirrorPhase::Cold);

        for attempt in ["automatic-first", "automatic-retry"] {
            mirror.begin_load("session", 1, attempt).unwrap();
            mirror
                .fail_load("session", 1, attempt, "temporary", true)
                .unwrap();
            let view = mirror.view("session", 1).unwrap();
            assert!(Arc::ptr_eq(&cached, &view.baseline));
            assert!(view.session.history_revision.is_some());
            assert_eq!(view.session.phase, MirrorPhase::Loading);
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
    fn failed_explicit_load_restores_its_admission_phase() {
        for origin in [MirrorPhase::Ready, MirrorPhase::Blocked] {
            let mut mirror = mirror();
            load_initial(&mut mirror, "session", 1);
            let old = mirror.view("session", 1).unwrap().baseline;
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
            assert!(Arc::ptr_eq(
                &old,
                &mirror.view("session", 1).unwrap().baseline
            ));
            assert!(mirror.state("session").unwrap().load_origin.is_none());
        }
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
    fn explicit_blocked_recovery_preserves_the_completed_turn_until_full_replay_commits() {
        let mut mirror = mirror();
        load_initial(&mut mirror, "session", 1);
        let old = mirror.view("session", 1).unwrap().baseline;
        let operation = match mirror
            .start_turn("session", 1, old.revision(), "intent", text_prompt("next"))
            .unwrap()
        {
            TurnAdmission::Accepted { operation_id } => operation_id,
            _ => unreachable!(),
        };
        let answer = json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "answer" } });
        mirror
            .append_turn_update("session", 1, &operation, answer.clone())
            .unwrap();
        mirror
            .complete_turn(
                "session",
                1,
                &operation,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        mirror
            .block_turn("session", 1, &operation, "commit failed")
            .unwrap();
        let retained = mirror.state("session").unwrap().active_turn.clone();

        mirror
            .begin_exclusive("session", 1, SessionOperationKind::Load, "recover")
            .unwrap();
        assert_eq!(
            mirror.begin_load("session", 1, "wrong-attempt"),
            Err(MirrorError::WrongPhase)
        );
        mirror.begin_load("session", 1, "recover").unwrap();
        for update in old.updates() {
            mirror
                .append_load_update("session", 1, "recover", update.clone())
                .unwrap();
        }
        mirror
            .append_load_update("session", 1, "recover", user_update("next"))
            .unwrap();
        assert_eq!(
            mirror.commit_load("session", 1, "recover").unwrap_err(),
            MirrorError::InconsistentHistory
        );
        let incomplete = mirror.view("session", 1).unwrap();
        assert!(Arc::ptr_eq(&old, &incomplete.baseline));
        assert_eq!(incomplete.session.active_turn, retained);

        mirror
            .append_load_update("session", 1, "recover", answer)
            .unwrap();
        mirror.commit_load("session", 1, "recover").unwrap();
        assert_eq!(
            mirror.can_begin("session", 1, SessionAdmission::Prompt),
            Err(MirrorError::WrongPhase)
        );
        mirror
            .settle_exclusive("session", 1, SessionAdmission::Attachment, "recover")
            .unwrap();
        assert_eq!(
            mirror.can_begin("session", 1, SessionAdmission::Prompt),
            Ok(())
        );
        let recovered = mirror.view("session", 1).unwrap();
        assert_ne!(recovered.baseline.revision(), old.revision());
        assert!(recovered.session.active_turn.is_none());
        assert_eq!(recovered.baseline.updates().len(), 3);
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
    fn consumed_intent_returns_its_original_operation_after_reconcile() {
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

        assert_eq!(
            mirror.start_turn("session", 1, successor.revision(), "old-intent", prompt,),
            Ok(TurnAdmission::Duplicate {
                operation_id: operation
            })
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
