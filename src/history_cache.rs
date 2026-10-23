use std::collections::HashMap;

use serde_json::Value;

use crate::history_replay::{Replay, ReplayCandidate};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HistoryCacheError {
    CandidateExists,
    CandidateMissing,
    AttemptMismatch,
    InvalidReplay,
}

struct LoadCandidate {
    attempt_id: String,
    replay: ReplayCandidate,
}

/// The replay of each load in flight. A completed history lives only in the
/// session's timeline; a candidate exists from the load's start to its end.
#[derive(Default)]
pub(crate) struct HistoryCache {
    candidates: HashMap<String, LoadCandidate>,
}

impl HistoryCache {
    pub(crate) fn candidate(
        &self,
        session_id: &str,
        attempt_id: &str,
    ) -> Result<ReplayCandidate, HistoryCacheError> {
        let candidate = self
            .candidates
            .get(session_id)
            .ok_or(HistoryCacheError::CandidateMissing)?;
        if candidate.attempt_id != attempt_id {
            return Err(HistoryCacheError::AttemptMismatch);
        }
        Ok(candidate.replay.clone())
    }

    pub(crate) fn begin_candidate(
        &mut self,
        session_id: impl Into<String>,
        attempt_id: impl Into<String>,
    ) -> Result<(), HistoryCacheError> {
        let session_id = session_id.into();
        if self.candidates.contains_key(&session_id) {
            return Err(HistoryCacheError::CandidateExists);
        }
        self.candidates.insert(
            session_id,
            LoadCandidate {
                attempt_id: attempt_id.into(),
                replay: ReplayCandidate::default(),
            },
        );
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn append_candidate(
        &mut self,
        session_id: &str,
        attempt_id: &str,
        update: Value,
    ) -> Result<(), HistoryCacheError> {
        self.append_candidate_with_meta(session_id, attempt_id, update, None)
    }

    pub(crate) fn append_candidate_with_meta(
        &mut self,
        session_id: &str,
        attempt_id: &str,
        update: Value,
        envelope: Option<&Value>,
    ) -> Result<(), HistoryCacheError> {
        self.candidate(session_id, attempt_id)?
            .append_with_meta(update, envelope)
    }

    pub(crate) fn poison_candidate(
        &mut self,
        session_id: &str,
        attempt_id: &str,
        error: HistoryCacheError,
    ) -> Result<(), HistoryCacheError> {
        self.candidate(session_id, attempt_id)?.poison(error);
        Ok(())
    }

    /// Ends a load with its replay: the history the session's log and timeline
    /// are built from.
    pub(crate) fn commit_candidate(
        &mut self,
        session_id: &str,
        attempt_id: &str,
    ) -> Result<Replay, HistoryCacheError> {
        let replay = self.candidate(session_id, attempt_id)?.take_replay()?;
        self.candidates.remove(session_id);
        Ok(replay)
    }

    pub(crate) fn abort_candidate(
        &mut self,
        session_id: &str,
        attempt_id: &str,
    ) -> Result<(), HistoryCacheError> {
        let replay = self.candidate(session_id, attempt_id)?;
        replay.discard();
        self.candidates.remove(session_id);
        Ok(())
    }

    pub(crate) fn remove(&mut self, session_id: &str) {
        if let Some(candidate) = self.candidates.remove(session_id) {
            candidate.replay.discard();
        }
    }

    #[cfg(test)]
    pub(crate) fn candidates(&self) -> usize {
        self.candidates.len()
    }
}

/// The history fold the replay builder reproduces incrementally.
#[cfg(test)]
pub(crate) fn fold_history_update(
    retained: &[Value],
    update: &Value,
) -> Result<Vec<Value>, crate::runtime_state::RuntimeStateError> {
    if update.get("sessionUpdate").and_then(Value::as_str) == Some("tool_call_update") {
        // Tool IDs belong to the session: a background tool may finish after a
        // later prompt starts. Keep its original slot (and outcome offsets).
        let tool_id = update.get("toolCallId");
        if let Some(position) = retained.iter().rposition(|candidate| {
            matches!(
                candidate.get("sessionUpdate").and_then(Value::as_str),
                Some("tool_call" | "tool_call_update")
            ) && candidate.get("toolCallId") == tool_id
        }) {
            let mut folded = retained.to_vec();
            folded[position] = crate::runtime_state::fold_active_turn_update(
                &[retained[position].clone()],
                update,
            )?
            .remove(0);
            return Ok(folded);
        }
    }
    let turn_start = retained
        .iter()
        .rposition(|candidate| {
            candidate.get("sessionUpdate").and_then(Value::as_str) == Some("user_message_chunk")
        })
        .unwrap_or(0);
    let mut folded = retained[..turn_start].to_vec();
    folded.extend(crate::runtime_state::fold_active_turn_update(
        &retained[turn_start..],
        update,
    )?);
    Ok(folded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replay_preserves_distinct_message_ids_and_session_scoped_tool_updates() {
        let updates = vec![
            json!({ "sessionUpdate": "agent_message_chunk", "messageId": "a", "content": { "type": "text", "text": "first" } }),
            json!({ "sessionUpdate": "agent_message_chunk", "messageId": "b", "content": { "type": "text", "text": "second" } }),
            json!({ "sessionUpdate": "tool_call", "toolCallId": "tool", "title": "Run", "status": "in_progress" }),
            json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "next turn" } }),
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "tool", "status": "completed" }),
        ];
        let key = "session";
        let mut cache = HistoryCache::default();
        cache.begin_candidate(key, "load").unwrap();
        for update in &updates {
            cache.append_candidate(key, "load", update.clone()).unwrap();
        }
        let replay = cache.commit_candidate(key, "load").unwrap().updates;
        assert_eq!(replay.len(), 4);
        assert_eq!(replay[0], updates[0]);
        assert_eq!(replay[1], updates[1]);
        assert_eq!(replay[2]["status"], "completed");
        assert_eq!(replay[3], updates[3]);
        assert_eq!(cache.candidates(), 0);
    }

    #[test]
    fn wrong_attempt_cannot_append_commit_or_abort_a_candidate() {
        let key = "session";
        let mut cache = HistoryCache::default();
        cache.begin_candidate(key, "current").unwrap();

        assert_eq!(
            cache.append_candidate(key, "old", json!({})),
            Err(HistoryCacheError::AttemptMismatch)
        );
        assert!(matches!(
            cache.commit_candidate(key, "old"),
            Err(HistoryCacheError::AttemptMismatch)
        ));
        assert_eq!(
            cache.abort_candidate(key, "old"),
            Err(HistoryCacheError::AttemptMismatch)
        );
        assert_eq!(cache.candidates(), 1);
    }
}
