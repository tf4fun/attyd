use std::collections::HashMap;

use serde_json::Value;

use crate::history_replay::ReplayCandidate;

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
/// session's `TurnLog`; a candidate exists from the load's start to its end.
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

    pub(crate) fn append_candidate(
        &mut self,
        session_id: &str,
        attempt_id: &str,
        update: Value,
    ) -> Result<(), HistoryCacheError> {
        self.candidate(session_id, attempt_id)?.append(update)
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

    /// Ends a load with its replay: the history the session's log is built
    /// from.
    pub(crate) fn commit_candidate(
        &mut self,
        session_id: &str,
        attempt_id: &str,
    ) -> Result<Vec<Value>, HistoryCacheError> {
        let updates = self.candidate(session_id, attempt_id)?.take_history()?;
        self.candidates.remove(session_id);
        Ok(updates)
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
        let replay = cache.commit_candidate(key, "load").unwrap();
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
