//! A load's private data candidate. Wire replay folds here without scheduling a
//! live session operation per record. Only the load response can publish it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;

use crate::history_cache::HistoryCacheError;
use crate::protocol::timeline::Timeline;
use crate::runtime_state::{RuntimeStateError, fold_active_turn_update, fold_control_update};
use crate::semantic::{SessionUpdateSemanticState, validate_history_update};

/// Incremental history folding. Only the changed slot is cloned/serialized;
/// completed turns are never copied for each newly received record.
#[derive(Default)]
struct HistoryBuilder {
    updates: Vec<Value>,
    turn_start: usize,
    tools: HashMap<String, usize>,
}

impl HistoryBuilder {
    fn append(&mut self, update: Value) -> Result<(), RuntimeStateError> {
        let kind = update.get("sessionUpdate").and_then(Value::as_str);
        let position = match kind {
            Some("user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk") => {
                self.updates.len().checked_sub(1)
            }
            Some("tool_call" | "tool_call_update") => update
                .get("toolCallId")
                .and_then(Value::as_str)
                .and_then(|id| self.tools.get(id).copied())
                .filter(|position| {
                    kind == Some("tool_call_update") || *position >= self.turn_start
                }),
            Some("plan" | "plan_update" | "plan_removed") => self.updates[self.turn_start..]
                .iter()
                .position(|candidate| match kind {
                    Some("plan") => candidate["sessionUpdate"] == "plan",
                    _ => {
                        candidate["sessionUpdate"] == "plan_update"
                            && &candidate["plan"]["planId"]
                                == if kind == Some("plan_removed") {
                                    &update["planId"]
                                } else {
                                    &update["plan"]["planId"]
                                }
                    }
                })
                .map(|position| position + self.turn_start),
            _ => None,
        };
        let retained = position
            .map(|position| std::slice::from_ref(&self.updates[position]))
            .unwrap_or(&[]);
        let mut folded = fold_active_turn_update(retained, &update)?;
        if let Some(position) = position {
            if folded.is_empty() {
                self.updates.remove(position);
                for slot in self.tools.values_mut() {
                    if *slot > position {
                        *slot -= 1;
                    }
                }
            } else {
                self.updates[position] = folded.remove(0);
            }
        }
        for entry in folded {
            let position = self.updates.len();
            if matches!(
                entry["sessionUpdate"].as_str(),
                Some("tool_call" | "tool_call_update")
            ) {
                if let Some(id) = entry["toolCallId"].as_str() {
                    self.tools.insert(id.to_owned(), position);
                }
            }
            if entry["sessionUpdate"] == "user_message_chunk" {
                self.turn_start = position;
            }
            self.updates.push(entry);
        }
        Ok(())
    }
}

/// A completed replay: the folded updates the turn log is built from, and the
/// session timeline reduced from the same updates as received (bridge memory
/// model §6.2).
#[derive(Debug)]
pub(crate) struct Replay {
    pub(crate) updates: Vec<Value>,
    pub(crate) timeline: Timeline,
}

#[derive(Default)]
pub(crate) struct ReplayData {
    history: HistoryBuilder,
    /// The candidate session timeline, private to this load attempt.
    timeline: Timeline,
    validation: SessionUpdateSemanticState,
    controls: Vec<Value>,
    invalid: Option<HistoryCacheError>,
    sealed: bool,
}

impl ReplayData {
    pub(crate) fn updates(&self) -> Result<&[Value], HistoryCacheError> {
        if let Some(error) = &self.invalid {
            return Err(error.clone());
        }
        Ok(&self.history.updates)
    }
}

#[derive(Clone, Default)]
pub(crate) struct ReplayCandidate(Arc<Mutex<ReplayData>>);

impl ReplayCandidate {
    pub(crate) fn lock(&self) -> MutexGuard<'_, ReplayData> {
        self.0.lock().expect("history candidate lock poisoned")
    }

    #[cfg(test)]
    pub(crate) fn append(&self, update: Value) -> Result<(), HistoryCacheError> {
        self.append_with_meta(update, None)
    }

    /// `envelope` is the notification's `_meta` (memory model §3.5).
    pub(crate) fn append_with_meta(
        &self,
        update: Value,
        envelope: Option<&Value>,
    ) -> Result<(), HistoryCacheError> {
        let mut data = self.lock();
        if data.sealed {
            return Err(HistoryCacheError::AttemptMismatch);
        }
        data.updates()?;
        data.timeline.reduce(&update, envelope);
        if data.history.append(update).is_err() {
            data.invalid = Some(HistoryCacheError::InvalidReplay);
            return Err(HistoryCacheError::InvalidReplay);
        }
        Ok(())
    }

    /// Validation is transactional at the replay level: any malformed record
    /// poisons the whole candidate, so no per-record copy of the entity map is needed.
    #[cfg(test)]
    pub(crate) fn ingest(&self, update: Value, conversation: bool) -> Result<(), String> {
        self.ingest_with_meta(update, conversation, None)
    }

    pub(crate) fn ingest_with_meta(
        &self,
        update: Value,
        conversation: bool,
        envelope: Option<&Value>,
    ) -> Result<(), String> {
        let mut data = self.lock();
        if data.sealed || data.invalid.is_some() {
            return Ok(());
        }
        let result: Result<(), String> = (|| {
            validate_history_update(&mut data.validation, &update)?;
            if conversation {
                data.timeline.reduce(&update, envelope);
                data.history
                    .append(update)
                    .map_err(|_| "Invalid history fold".to_owned())?;
            } else {
                let position = data
                    .controls
                    .iter()
                    .position(|previous| previous["sessionUpdate"] == update["sessionUpdate"]);
                let previous = position.map(|position| &data.controls[position]);
                let update = fold_control_update(previous, update);
                if let Some(position) = position {
                    data.controls[position] = update;
                } else {
                    data.controls.push(update);
                }
            }
            Ok(())
        })();
        if let Err(error) = &result {
            data.invalid = Some(HistoryCacheError::InvalidReplay);
            data.validation.invalid_reason = Some(error.clone());
        }
        result
    }

    pub(crate) fn poison(&self, error: HistoryCacheError) {
        self.lock().invalid.get_or_insert(error);
    }

    pub(crate) fn reject(&self, message: String) {
        let mut data = self.lock();
        data.invalid.get_or_insert(HistoryCacheError::InvalidReplay);
        data.validation.invalid_reason.get_or_insert(message);
    }

    pub(crate) fn seal(&self) {
        self.lock().sealed = true;
    }

    pub(crate) fn take_controls(&self) -> (SessionUpdateSemanticState, Vec<Value>) {
        let mut data = self.lock();
        (
            std::mem::take(&mut data.validation),
            std::mem::take(&mut data.controls),
        )
    }

    pub(crate) fn take_replay(&self) -> Result<Replay, HistoryCacheError> {
        let mut data = self.lock();
        data.updates()?;
        data.sealed = true;
        Ok(Replay {
            updates: std::mem::take(&mut data.history).updates,
            timeline: std::mem::take(&mut data.timeline),
        })
    }

    pub(crate) fn discard(&self) {
        let mut validation = SessionUpdateSemanticState::default();
        validation.invalid_reason = Some("History replay was discarded".to_owned());
        *self.lock() = ReplayData {
            sealed: true,
            invalid: Some(HistoryCacheError::InvalidReplay),
            validation,
            ..ReplayData::default()
        };
    }

    /// The updates and controls the candidate still holds.
    #[cfg(test)]
    pub(crate) fn retained(&self) -> usize {
        let data = self.lock();
        data.history.updates.len() + data.controls.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_cache::{HistoryCache, fold_history_update};
    use serde_json::json;

    #[test]
    fn incremental_fold_matches_history_semantics() {
        let updates = vec![
            json!({"sessionUpdate":"user_message_chunk","messageId":"u","content":{"type":"text","text":"start"}}),
            json!({"sessionUpdate":"agent_message_chunk","messageId":"a","content":{"type":"text","text":"hello"}}),
            json!({"sessionUpdate":"agent_message_chunk","messageId":"a","content":{"type":"text","text":" world"}}),
            json!({"sessionUpdate":"tool_call","toolCallId":"tool","title":"Run","status":"in_progress"}),
            json!({"sessionUpdate":"plan","entries":[]}),
            json!({"sessionUpdate":"plan","entries":[{"content":"step","status":"pending","priority":"medium"}]}),
            json!({"sessionUpdate":"plan_update","plan":{"planId":"p","type":"items","entries":[]}}),
            json!({"sessionUpdate":"plan_removed","planId":"p"}),
            json!({"sessionUpdate":"user_message_chunk","messageId":"u2","content":{"type":"text","text":"next turn"}}),
            json!({"sessionUpdate":"tool_call_update","toolCallId":"tool","status":"completed"}),
            json!({"sessionUpdate":"tool_call","toolCallId":"tool","title":"again","status":"in_progress"}),
            json!({"sessionUpdate":"tool_call_update","toolCallId":"tool","status":"completed"}),
        ];
        let mut expected = Vec::new();
        let mut actual = HistoryBuilder::default();
        for update in updates {
            expected = fold_history_update(&expected, &update).unwrap();
            actual.append(update).unwrap();
            assert_eq!(actual.updates, expected);
        }
    }

    #[test]
    fn a_replay_keeps_the_notification_envelope_beside_the_update() {
        let replay = ReplayCandidate::default();
        let envelope = json!({ "trace": "t1" });
        replay
            .ingest_with_meta(
                json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}),
                true,
                Some(&envelope),
            )
            .unwrap();
        replay
            .append_with_meta(
                json!({"sessionUpdate":"tool_call","toolCallId":"t","title":"Run"}),
                Some(&envelope),
            )
            .unwrap();
        replay.seal();
        let entries = serde_json::to_value(replay.take_replay().unwrap().timeline.entries()).unwrap();
        assert_eq!(entries[0]["chunks"][0]["notificationMeta"], envelope);
        assert_eq!(entries[1]["notificationMeta"], envelope);
    }

    #[test]
    fn invalid_replay_cannot_publish_a_valid_prefix() {
        let mut cache = HistoryCache::default();
        let key = "s";
        cache.begin_candidate(key, "load").unwrap();
        let replay = cache.candidate(key, "load").unwrap();
        replay.ingest(json!({"sessionUpdate":"tool_call","toolCallId":"tool","title":"Run","status":"completed"}), true).unwrap();
        assert!(replay.ingest(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"image","data":"AA==","mimeType":"text/html"}}), true).is_err());
        replay.seal();
        assert!(matches!(
            cache.commit_candidate(key, "load"),
            Err(HistoryCacheError::InvalidReplay)
        ));
        cache.abort_candidate(key, "load").unwrap();
        assert_eq!(replay.retained(), 0);
    }

    #[test]
    fn retired_candidate_cannot_contaminate_replacement_load() {
        let mut cache = HistoryCache::default();
        let key = "s";
        cache.begin_candidate(key, "old").unwrap();
        let old = cache.candidate(key, "old").unwrap();
        old.append(json!({"message":"old"})).unwrap();
        old.discard();
        assert!(matches!(
            cache.commit_candidate(key, "old"),
            Err(HistoryCacheError::InvalidReplay)
        ));
        cache.remove(key);
        assert_eq!(old.retained(), 0);
        cache.begin_candidate(key, "new").unwrap();
        assert!(old.append(json!({"message":"late"})).is_err());
        old.ingest(
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"late"}}),
            true,
        )
        .unwrap();
        assert!(cache.commit_candidate(key, "new").unwrap().updates.is_empty());
    }

    #[test]
    fn replay_control_patches_fold_without_retaining_the_stream() {
        let replay = ReplayCandidate::default();
        for index in 0..10_050 {
            replay
                .ingest(
                    json!({"sessionUpdate":"session_info_update","title":format!("title-{index}")}),
                    false,
                )
                .unwrap();
        }
        replay
            .ingest(
                json!({"sessionUpdate":"session_info_update","updatedAt":"2026-09-14T00:00:00Z"}),
                false,
            )
            .unwrap();
        replay.seal();
        let (_, controls) = replay.take_controls();
        assert_eq!(controls.len(), 1);
        assert_eq!(controls[0]["title"], "title-10049");
        assert_eq!(controls[0]["updatedAt"], "2026-09-14T00:00:00Z");
        assert_eq!(replay.retained(), 0);
    }
}
