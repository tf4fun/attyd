use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;
use std::sync::Arc;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::runtime_state::fold_active_turn_update;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SessionKey {
    pub session_id: String,
    pub incarnation: u64,
}

impl SessionKey {
    pub(crate) fn new(session_id: impl Into<String>, incarnation: u64) -> Self {
        Self {
            session_id: session_id.into(),
            incarnation,
        }
    }
}

#[derive(Debug)]
pub(crate) struct HistorySnapshot {
    revision: String,
    updates: Arc<[Value]>,
    bytes: usize,
    digest: [u8; 32],
}

impl HistorySnapshot {
    pub(crate) fn revision(&self) -> &str {
        &self.revision
    }

    pub(crate) fn updates(&self) -> &[Value] {
        &self.updates
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HistoryCacheError {
    CandidateExists,
    CandidateMissing,
    AttemptMismatch,
    InvalidReplay,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HistoryCacheStats {
    pub snapshots: usize,
    pub candidates: usize,
    pub snapshot_bytes: usize,
    pub candidate_bytes: usize,
}

struct HistoryEntry {
    snapshot: Arc<HistorySnapshot>,
    last_used: u64,
    #[cfg(test)]
    observers: usize,
}

struct LoadCandidate {
    attempt_id: String,
    updates: Vec<Value>,
    bytes: usize,
    invalid: Option<HistoryCacheError>,
}

pub(crate) struct HistoryCache {
    epoch: String,
    next_generation: u64,
    clock: u64,
    entries: HashMap<SessionKey, HistoryEntry>,
    candidates: HashMap<SessionKey, LoadCandidate>,
    snapshot_bytes: usize,
    candidate_bytes: usize,
}

impl HistoryCache {
    pub(crate) fn new(epoch: impl Into<String>) -> Self {
        Self {
            epoch: epoch.into(),
            next_generation: 0,
            clock: 0,
            entries: HashMap::new(),
            candidates: HashMap::new(),
            snapshot_bytes: 0,
            candidate_bytes: 0,
        }
    }

    pub(crate) fn install_empty(&mut self, key: SessionKey) -> Arc<HistorySnapshot> {
        self.install_snapshot(key, Vec::new(), 2)
    }

    pub(crate) fn snapshot(&mut self, key: &SessionKey) -> Option<Arc<HistorySnapshot>> {
        self.clock = self.clock.wrapping_add(1).max(1);
        let entry = self.entries.get_mut(key)?;
        entry.last_used = self.clock;
        Some(entry.snapshot.clone())
    }

    pub(crate) fn peek(&self, key: &SessionKey) -> Option<&Arc<HistorySnapshot>> {
        self.entries.get(key).map(|entry| &entry.snapshot)
    }

    pub(crate) fn candidate_updates(
        &self,
        key: &SessionKey,
        attempt_id: &str,
    ) -> Result<&[Value], HistoryCacheError> {
        let candidate = self
            .candidates
            .get(key)
            .ok_or(HistoryCacheError::CandidateMissing)?;
        if candidate.attempt_id != attempt_id {
            return Err(HistoryCacheError::AttemptMismatch);
        }
        if let Some(error) = &candidate.invalid {
            return Err(error.clone());
        }
        Ok(&candidate.updates)
    }

    pub(crate) fn begin_candidate(
        &mut self,
        key: SessionKey,
        attempt_id: impl Into<String>,
    ) -> Result<(), HistoryCacheError> {
        if self.candidates.contains_key(&key) {
            return Err(HistoryCacheError::CandidateExists);
        }
        self.candidates.insert(
            key,
            LoadCandidate {
                attempt_id: attempt_id.into(),
                updates: Vec::new(),
                bytes: 0,
                invalid: None,
            },
        );
        Ok(())
    }

    pub(crate) fn append_candidate(
        &mut self,
        key: &SessionKey,
        attempt_id: &str,
        update: Value,
    ) -> Result<(), HistoryCacheError> {
        let candidate = self
            .candidates
            .get(key)
            .ok_or(HistoryCacheError::CandidateMissing)?;
        if candidate.attempt_id != attempt_id {
            return Err(HistoryCacheError::AttemptMismatch);
        }
        if let Some(error) = &candidate.invalid {
            return Err(error.clone());
        }
        let updates = match fold_history_update(&candidate.updates, &update) {
            Ok(updates) => updates,
            Err(_) => {
                self.poison_candidate(key, attempt_id, HistoryCacheError::InvalidReplay)?;
                return Err(HistoryCacheError::InvalidReplay);
            }
        };
        let bytes = serde_json::to_vec(&updates)
            .map(|value| value.len())
            .unwrap_or(usize::MAX);
        let candidate = self
            .candidates
            .get_mut(key)
            .expect("candidate was validated above");
        self.candidate_bytes = self
            .candidate_bytes
            .saturating_sub(candidate.bytes)
            .saturating_add(bytes);
        candidate.bytes = bytes;
        candidate.updates = updates;
        Ok(())
    }

    pub(crate) fn poison_candidate(
        &mut self,
        key: &SessionKey,
        attempt_id: &str,
        error: HistoryCacheError,
    ) -> Result<(), HistoryCacheError> {
        let candidate = self
            .candidates
            .get_mut(key)
            .ok_or(HistoryCacheError::CandidateMissing)?;
        if candidate.attempt_id != attempt_id {
            return Err(HistoryCacheError::AttemptMismatch);
        }
        if candidate.invalid.is_none() {
            candidate.invalid = Some(error);
        }
        Ok(())
    }

    pub(crate) fn commit_candidate(
        &mut self,
        key: &SessionKey,
        attempt_id: &str,
    ) -> Result<Arc<HistorySnapshot>, HistoryCacheError> {
        let candidate = self
            .candidates
            .get(key)
            .ok_or(HistoryCacheError::CandidateMissing)?;
        if candidate.attempt_id != attempt_id {
            return Err(HistoryCacheError::AttemptMismatch);
        }
        if let Some(error) = &candidate.invalid {
            return Err(error.clone());
        }
        let candidate = self
            .candidates
            .remove(key)
            .expect("candidate was validated above");
        self.candidate_bytes = self.candidate_bytes.saturating_sub(candidate.bytes);
        Ok(self.install_snapshot(key.clone(), candidate.updates, candidate.bytes))
    }

    pub(crate) fn append_committed_updates(
        &mut self,
        key: &SessionKey,
        suffix: &[Value],
    ) -> Result<Arc<HistorySnapshot>, HistoryCacheError> {
        let previous = self
            .entries
            .get(key)
            .map(|entry| entry.snapshot.clone())
            .ok_or(HistoryCacheError::InvalidReplay)?;
        let updates = suffix
            .iter()
            .try_fold(previous.updates().to_vec(), |retained, update| {
                fold_history_update(&retained, update)
            })
            .map_err(|_| HistoryCacheError::InvalidReplay)?;
        let bytes = serde_json::to_vec(&updates)
            .map(|value| value.len())
            .unwrap_or(usize::MAX);
        Ok(self.install_snapshot(key.clone(), updates, bytes))
    }

    pub(crate) fn abort_candidate(
        &mut self,
        key: &SessionKey,
        attempt_id: &str,
    ) -> Result<(), HistoryCacheError> {
        let candidate = self
            .candidates
            .get(key)
            .ok_or(HistoryCacheError::CandidateMissing)?;
        if candidate.attempt_id != attempt_id {
            return Err(HistoryCacheError::AttemptMismatch);
        }
        let candidate = self
            .candidates
            .remove(key)
            .expect("candidate was validated above");
        self.candidate_bytes = self.candidate_bytes.saturating_sub(candidate.bytes);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn set_observed(&mut self, key: &SessionKey, observed: bool) {
        let Some(entry) = self.entries.get_mut(key) else {
            return;
        };
        if observed {
            entry.observers = entry.observers.saturating_add(1);
            self.clock = self.clock.wrapping_add(1).max(1);
            entry.last_used = self.clock;
        } else {
            entry.observers = entry.observers.saturating_sub(1);
        }
    }

    #[cfg(test)]
    pub(crate) fn evict_lru(&mut self, eligible: &HashSet<SessionKey>) -> Option<SessionKey> {
        let key = self
            .entries
            .iter()
            .filter(|(key, entry)| {
                eligible.contains(*key)
                    && entry.observers == 0
                    && !self.candidates.contains_key(*key)
            })
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| key.clone())?;
        self.remove(&key);
        Some(key)
    }

    pub(crate) fn remove(&mut self, key: &SessionKey) {
        if let Some(entry) = self.entries.remove(key) {
            self.snapshot_bytes = self.snapshot_bytes.saturating_sub(entry.snapshot.bytes);
        }
        if let Some(candidate) = self.candidates.remove(key) {
            self.candidate_bytes = self.candidate_bytes.saturating_sub(candidate.bytes);
        }
    }

    #[cfg(test)]
    pub(crate) fn stats(&self) -> HistoryCacheStats {
        HistoryCacheStats {
            snapshots: self.entries.len(),
            candidates: self.candidates.len(),
            snapshot_bytes: self.snapshot_bytes,
            candidate_bytes: self.candidate_bytes,
        }
    }

    fn install_snapshot(
        &mut self,
        key: SessionKey,
        updates: Vec<Value>,
        bytes: usize,
    ) -> Arc<HistorySnapshot> {
        if let Some(previous) = self.entries.remove(&key) {
            self.snapshot_bytes = self.snapshot_bytes.saturating_sub(previous.snapshot.bytes);
        }
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        self.clock = self.clock.wrapping_add(1).max(1);
        let digest: [u8; 32] = Sha256::digest(
            serde_json::to_vec(&updates).expect("validated history values always serialize"),
        )
        .into();
        let snapshot = Arc::new(HistorySnapshot {
            revision: format!(
                "{}:{}:{}",
                self.epoch, key.incarnation, self.next_generation
            ),
            updates: updates.into(),
            bytes,
            digest,
        });
        self.snapshot_bytes = self.snapshot_bytes.saturating_add(bytes);
        self.entries.insert(
            key,
            HistoryEntry {
                snapshot: snapshot.clone(),
                last_used: self.clock,
                #[cfg(test)]
                observers: 0,
            },
        );
        snapshot
    }
}

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
            folded[position] =
                fold_active_turn_update(&[retained[position].clone()], update)?.remove(0);
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
    folded.extend(fold_active_turn_update(&retained[turn_start..], update)?);
    Ok(folded)
}

pub(crate) fn normalize_history_updates(
    updates: &[Value],
) -> Result<Vec<Value>, crate::runtime_state::RuntimeStateError> {
    updates.iter().try_fold(Vec::new(), |retained, update| {
        fold_history_update(&retained, update)
    })
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
        let key = SessionKey::new("session", 1);
        let mut cache = HistoryCache::new("epoch");
        cache.begin_candidate(key.clone(), "load").unwrap();
        for update in &updates {
            cache
                .append_candidate(&key, "load", update.clone())
                .unwrap();
        }
        let snapshot = cache.commit_candidate(&key, "load").unwrap();
        assert_eq!(snapshot.updates().len(), 4);
        assert_eq!(snapshot.updates()[0], updates[0]);
        assert_eq!(snapshot.updates()[1], updates[1]);
        assert_eq!(snapshot.updates()[2]["status"], "completed");
        assert_eq!(snapshot.updates()[3], updates[3]);
    }

    #[test]
    fn valid_candidate_atomically_replaces_the_baseline_and_advances_revision() {
        let key = SessionKey::new("session", 7);
        let mut cache = HistoryCache::new("epoch");
        let old = cache.install_empty(key.clone());
        cache.begin_candidate(key.clone(), "load-1").unwrap();
        cache
            .append_candidate(&key, "load-1", json!({ "message": "one" }))
            .unwrap();

        assert!(cache.peek(&key).unwrap().updates().is_empty());
        let committed = cache.commit_candidate(&key, "load-1").unwrap();

        assert_eq!(committed.updates(), &[json!({ "message": "one" })]);
        assert_ne!(committed.revision(), old.revision());
        assert_eq!(cache.stats().candidates, 0);
    }

    #[test]
    fn failed_candidate_preserves_the_installed_baseline() {
        let key = SessionKey::new("session", 1);
        let mut cache = HistoryCache::new("epoch");
        cache.begin_candidate(key.clone(), "initial").unwrap();
        cache
            .append_candidate(&key, "initial", json!({ "message": "stable" }))
            .unwrap();
        let stable = cache.commit_candidate(&key, "initial").unwrap();
        cache.begin_candidate(key.clone(), "retry").unwrap();
        cache
            .append_candidate(&key, "retry", json!({ "message": "partial" }))
            .unwrap();
        cache.abort_candidate(&key, "retry").unwrap();

        assert!(Arc::ptr_eq(cache.peek(&key).unwrap(), &stable));
        assert_eq!(cache.stats().candidate_bytes, 0);
    }

    #[test]
    fn committed_suffix_is_not_rejected_by_accounting_watermarks() {
        let key = SessionKey::new("session", 1);
        let mut cache = HistoryCache::new("epoch");
        let stable = cache.install_empty(key.clone());

        let committed = cache
            .append_committed_updates(
                &key,
                &[json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "too large for the cache" }
                })],
            )
            .unwrap();
        assert!(!Arc::ptr_eq(&committed, &stable));
        assert!(committed.bytes() > 32);
        assert_eq!(cache.stats().snapshot_bytes, committed.bytes());
    }

    #[test]
    fn identical_replay_still_gets_a_new_opaque_revision() {
        let key = SessionKey::new("session", 2);
        let mut cache = HistoryCache::new("epoch");
        for attempt in ["first", "second"] {
            cache.begin_candidate(key.clone(), attempt).unwrap();
            cache
                .append_candidate(&key, attempt, json!({ "message": "same" }))
                .unwrap();
            let snapshot = cache.commit_candidate(&key, attempt).unwrap();
            if attempt == "first" {
                assert_eq!(snapshot.digest(), cache.peek(&key).unwrap().digest());
            }
        }
        let second = cache.peek(&key).unwrap().clone();
        assert!(second.revision().ends_with(":2"));
    }

    #[test]
    fn wrong_attempt_cannot_append_commit_or_abort_a_candidate() {
        let key = SessionKey::new("session", 1);
        let mut cache = HistoryCache::new("epoch");
        cache.begin_candidate(key.clone(), "current").unwrap();

        assert_eq!(
            cache.append_candidate(&key, "old", json!({})),
            Err(HistoryCacheError::AttemptMismatch)
        );
        assert!(matches!(
            cache.commit_candidate(&key, "old"),
            Err(HistoryCacheError::AttemptMismatch)
        ));
        assert_eq!(
            cache.abort_candidate(&key, "old"),
            Err(HistoryCacheError::AttemptMismatch)
        );
        assert_eq!(cache.stats().candidates, 1);
    }

    #[test]
    fn snapshots_are_shared_between_observers() {
        let key = SessionKey::new("session", 1);
        let mut cache = HistoryCache::new("epoch");
        cache.install_empty(key.clone());

        let left = cache.snapshot(&key).unwrap();
        let right = cache.snapshot(&key).unwrap();

        assert!(Arc::ptr_eq(&left, &right));
        assert_eq!(left.bytes(), 2, "an empty JSON replay still occupies []");
    }

    #[test]
    fn lru_evicts_only_eligible_unobserved_snapshots() {
        let a = SessionKey::new("a", 1);
        let b = SessionKey::new("b", 2);
        let mut cache = HistoryCache::new("epoch");
        cache.install_empty(a.clone());
        cache.install_empty(b.clone());
        cache.set_observed(&a, true);
        let eligible = HashSet::from([a.clone(), b.clone()]);

        assert_eq!(cache.evict_lru(&eligible), Some(b.clone()));
        assert!(cache.peek(&a).is_some());
        assert!(cache.peek(&b).is_none());
    }

    #[test]
    fn candidate_accounting_tracks_growth_without_becoming_an_admission_limit() {
        let key = SessionKey::new("session", 1);
        let mut cache = HistoryCache::new("epoch");
        cache.install_empty(key.clone());
        cache.begin_candidate(key.clone(), "load").unwrap();

        cache
            .append_candidate(&key, "load", json!({ "message": "candidate" }))
            .unwrap();
        assert!(cache.stats().candidate_bytes > 0);
        assert!(cache.peek(&key).unwrap().updates().is_empty());
    }
}
