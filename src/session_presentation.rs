//! A wire projection of completed history. The authoritative history stays intact;
//! hidden process bodies are only copied into the page explicitly requested.
use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

pub(crate) const PROCESS_PAGE_SIZE: usize = 10;

#[derive(Debug, PartialEq)]
pub(crate) enum PageError {
    StaleHistory,
    TurnNotFound,
    InvalidOffset,
}

struct Turn<'a> {
    id: String,
    entries: Vec<Entry<'a>>,
    outcomes: Vec<&'a Value>,
}

struct Entry<'a> {
    kind: &'a str,
    updates: Vec<&'a Value>,
}

impl Entry<'_> {
    fn is_prompt(&self) -> bool {
        self.kind == "user_message_chunk"
    }

    fn materialize(&self) -> Vec<Value> {
        match self.kind {
            "tool_call" | "tool_call_update" => {
                // Match the browser's recovery card for an update whose original
                // tool call is missing, then apply later patches to that card.
                let mut merged = if kind(self.updates[0]) == "tool_call_update" {
                    json!({
                        "sessionUpdate": "tool_call", "toolCallId": self.updates[0]["toolCallId"],
                        "title": "Tool call not found", "kind": "other", "status": "failed",
                        "content": [{"type": "content", "content": {"type": "text",
                            "text": "The Agent updated a tool call that was not introduced earlier."}}],
                    })
                } else {
                    self.updates[0].clone()
                };
                for update in &self.updates[1..] {
                    if let (Some(target), Some(patch)) =
                        (merged.as_object_mut(), update.as_object())
                    {
                        for (key, value) in patch {
                            if key != "sessionUpdate" && !value.is_null() {
                                target.insert(key.clone(), value.clone());
                            }
                        }
                    }
                }
                vec![merged]
            }
            "plan" | "plan_update" | "plan_removed" => {
                // A removed identified plan needs its introduction so the reducer
                // renders a removed plan, rather than an unknown protocol update.
                let last = self.updates.last().expect("entry is nonempty");
                if kind(last) == "plan_removed"
                    && let Some(introduction) = self
                        .updates
                        .iter()
                        .rev()
                        .find(|update| kind(update) == "plan_update")
                {
                    vec![(*introduction).clone(), (*last).clone()]
                } else {
                    vec![(*last).clone()]
                }
            }
            "compaction_update" | "compaction_summary_chunk" => {
                let mut result = json!({
                    "sessionUpdate": "compaction_update",
                    "compactionId": self.updates[0]["compactionId"],
                    "status": "in_progress",
                    "summary": [],
                });
                for update in &self.updates {
                    if kind(update) == "compaction_summary_chunk" {
                        append_block(
                            result["summary"].as_array_mut().unwrap(),
                            &update["content"],
                        );
                    } else {
                        for field in ["status", "summary", "error"] {
                            if let Some(value) = update.get(field) {
                                result[field] = if field == "summary" && value.is_null() {
                                    json!([])
                                } else {
                                    value.clone()
                                };
                            }
                        }
                    }
                }
                vec![result]
            }
            _ => self
                .updates
                .iter()
                .map(|update| (*update).clone())
                .collect(),
        }
    }
}

fn append_block(blocks: &mut Vec<Value>, incoming: &Value) {
    if let Some(previous) = blocks.last_mut()
        && previous["type"] == "text"
        && incoming["type"] == "text"
    {
        let mut old_shape = previous.clone();
        let mut new_shape = incoming.clone();
        old_shape["text"] = json!("");
        new_shape["text"] = json!("");
        if old_shape == new_shape
            && let (Some(old), Some(new)) = (previous["text"].as_str(), incoming["text"].as_str())
        {
            previous["text"] = json!(format!("{old}{new}"));
            return;
        }
    }
    blocks.push(incoming.clone());
}

fn kind(update: &Value) -> &str {
    update["sessionUpdate"].as_str().unwrap_or("")
}

fn prompt_operation(update: &Value) -> Option<&str> {
    update
        .pointer("/_meta/attyd/turnOperationId")
        .and_then(Value::as_str)
}

fn control(kind: &str) -> bool {
    matches!(
        kind,
        "current_mode_update"
            | "config_option_update"
            | "available_commands_update"
            | "session_info_update"
            | "usage_update"
    )
}

fn entries(updates: &[Value]) -> Vec<Entry<'_>> {
    let mut entries: Vec<Entry<'_>> = Vec::new();
    let mut named: HashMap<(&str, &str), usize> = HashMap::new();
    for update in updates {
        let kind = kind(update);
        if control(kind) {
            continue;
        }
        let key = match kind {
            "tool_call" | "tool_call_update" => {
                update["toolCallId"].as_str().map(|id| ("tool", id))
            }
            "plan" => Some(("plan", "")),
            "plan_update" => update
                .pointer("/plan/planId")
                .and_then(Value::as_str)
                .map(|id| ("plan", id)),
            "plan_removed" => update["planId"].as_str().map(|id| ("plan", id)),
            "compaction_update" | "compaction_summary_chunk" => {
                update["compactionId"].as_str().map(|id| ("compaction", id))
            }
            "user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk" => {
                update["messageId"].as_str().map(|id| (kind, id))
            }
            _ => None,
        };
        let existing = key.and_then(|key| named.get(&key).copied()).or_else(|| {
            if matches!(
                kind,
                "user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk"
            ) {
                entries
                    .last()
                    .filter(|entry| {
                        entry.kind == kind && {
                            let old = entry
                                .updates
                                .iter()
                                .find_map(|update| update["messageId"].as_str());
                            let new = update["messageId"].as_str();
                            old.is_none() || new.is_none() || old == new
                        }
                    })
                    .map(|_| entries.len() - 1)
            } else {
                None
            }
        });
        let index = if let Some(index) = existing {
            entries[index].updates.push(update);
            index
        } else {
            entries.push(Entry {
                kind,
                updates: vec![update],
            });
            entries.len() - 1
        };
        if let Some(key) = key {
            named.insert(key, index);
        }
    }
    entries.retain(|entry| {
        !(entry.kind == "plan"
            && entry
                .updates
                .last()
                .is_some_and(|update| update["entries"].as_array().is_some_and(Vec::is_empty)))
    });
    entries
}

fn turns(view: &Value) -> Vec<Turn<'_>> {
    let updates = view["timeline"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let outcomes = view["turnOutcomes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut result = Vec::new();
    let mut outcomes_by_offset: HashMap<usize, Vec<&Value>> = HashMap::new();
    for outcome in outcomes {
        if let Some(offset) = outcome["afterUpdate"]
            .as_u64()
            .and_then(|offset| usize::try_from(offset).ok())
        {
            outcomes_by_offset.entry(offset).or_default().push(outcome);
        }
    }
    let mut start = 0;
    let mut prompt: Option<&Value> = None;
    let mut non_prompt = false;
    for offset in 0..=updates.len() {
        for outcome in outcomes_by_offset.remove(&offset).unwrap_or_default() {
            result.push(Turn {
                id: format!("history-{start}-{}", result.len()),
                entries: entries(&updates[start..offset]),
                outcomes: vec![outcome],
            });
            start = offset;
            prompt = None;
            non_prompt = false;
        }
        let Some(update) = updates.get(offset) else {
            break;
        };
        if kind(update) == "user_message_chunk" {
            let same_prompt = prompt.is_some_and(|previous| {
                (prompt_operation(previous).is_some()
                    && prompt_operation(previous) == prompt_operation(update))
                    || (previous["messageId"].as_str().is_some()
                        && previous["messageId"] == update["messageId"])
                    || (!non_prompt
                        && prompt_operation(previous) == prompt_operation(update)
                        && (previous["messageId"].is_null()
                            || update["messageId"].is_null()
                            || previous["messageId"] == update["messageId"]))
            });
            if offset > start && !same_prompt {
                let entries = entries(&updates[start..offset]);
                if !entries.is_empty() {
                    result.push(Turn {
                        id: format!("history-{start}-{}", result.len()),
                        entries,
                        outcomes: Vec::new(),
                    });
                }
                start = offset;
                non_prompt = false;
            }
            prompt = Some(update);
        } else if !control(kind(update)) {
            non_prompt = true;
        }
    }
    if start < updates.len() {
        let entries = entries(&updates[start..]);
        if !entries.is_empty() {
            result.push(Turn {
                id: format!("history-{start}-{}", result.len()),
                entries,
                outcomes: Vec::new(),
            });
        }
    }
    result
}

fn output_index(turn: &Turn<'_>) -> Option<usize> {
    turn.entries
        .iter()
        .rposition(|entry| entry.kind == "agent_message_chunk")
}

fn turn_matches_operation(turn: &Turn<'_>, matches: impl Fn(&str) -> bool) -> bool {
    turn.outcomes
        .iter()
        .filter_map(|outcome| outcome["operationId"].as_str())
        .any(&matches)
        || turn
            .entries
            .iter()
            .filter(|entry| entry.is_prompt())
            .flat_map(|entry| &entry.updates)
            .filter_map(|update| prompt_operation(update))
            .any(matches)
}

fn turn_operation<'a>(turn: &Turn<'a>) -> Option<&'a str> {
    turn.outcomes
        .iter()
        .rev()
        .find_map(|outcome| outcome["operationId"].as_str())
        .or_else(|| {
            turn.entries
                .iter()
                .filter(|entry| entry.is_prompt())
                .flat_map(|entry| &entry.updates)
                .find_map(|update| prompt_operation(update))
        })
}

fn terminal_references(value: &Value, ids: &mut HashSet<String>) {
    match value {
        Value::Array(values) => values
            .iter()
            .for_each(|value| terminal_references(value, ids)),
        Value::Object(fields) => {
            if let Some(id) = fields.get("terminalId").and_then(Value::as_str) {
                ids.insert(id.to_string());
            }
            fields
                .values()
                .for_each(|value| terminal_references(value, ids));
        }
        _ => {}
    }
}

pub(crate) fn compact_view(view: Value, live_terminals: Option<&Value>) -> Value {
    compact_view_from(view, live_terminals, None, &[])
}

pub(crate) fn compact_view_from(
    mut view: Value,
    live_terminals: Option<&Value>,
    include_process_from: Option<&str>,
    exclude_process_for: &[String],
) -> Value {
    let mut timeline = Vec::new();
    let mut descriptors = Vec::new();
    let mut outcomes = Vec::new();
    let excluded_operations = exclude_process_for
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut observed_suffix = false;
    for turn in turns(&view) {
        observed_suffix |= include_process_from.is_some_and(|operation_id| {
            turn_matches_operation(&turn, |candidate| candidate == operation_id)
        });
        // Exclusion is per turn: releasing the suffix anchor must not release
        // later observed turns, or allow a subsequent refresh to restore it.
        let process_included = observed_suffix
            && !turn_matches_operation(&turn, |operation_id| {
                excluded_operations.contains(operation_id)
            });
        let before = timeline.len();
        let final_index = output_index(&turn);
        let mut process_count = 0;
        let mut visible_ranges = Vec::new();
        for (index, entry) in turn.entries.iter().enumerate() {
            let process = !entry.is_prompt() && Some(index) != final_index;
            if process {
                process_count += 1;
            }
            if !process || process_included {
                let start = timeline.len();
                timeline.extend(entry.materialize());
                if !process {
                    visible_ranges.push(json!({"start": start, "end": timeline.len()}));
                }
            }
        }
        let turn_outcomes = turn
            .outcomes
            .iter()
            .map(|outcome| {
                let mut outcome = (*outcome).clone();
                outcome["afterUpdate"] = json!(timeline.len());
                outcome
            })
            .collect::<Vec<_>>();
        outcomes.extend(turn_outcomes.clone());
        let mut descriptor = json!({
            "turnId": turn.id, "beforeUpdate": before, "afterUpdate": timeline.len(),
            "processCount": process_count, "historyRevision": view["historyRevision"],
            "outcomes": turn_outcomes, "visibleRanges": visible_ranges,
        });
        if let Some(operation_id) = turn_operation(&turn) {
            descriptor["operationId"] = json!(operation_id);
        }
        if process_included {
            descriptor["processIncluded"] = json!(true);
        }
        descriptors.push(descriptor);
    }
    view["timeline"] = json!(timeline);
    view["collapsedTurns"] = json!(descriptors);
    view["turnOutcomes"] = json!(outcomes);
    let mut retained = HashSet::new();
    for field in ["timeline", "activeTurn", "interactions"] {
        terminal_references(&view[field], &mut retained);
    }
    if let Some(terminals) = live_terminals.and_then(Value::as_object) {
        retained.extend(terminals.keys().cloned());
    }
    if let Some(terminals) = view["terminals"].as_object_mut() {
        terminals.retain(|id, _| retained.contains(id));
    }
    view
}

pub(crate) fn process_page(
    view: &Value,
    turn_id: &str,
    revision: &str,
    offset: usize,
) -> Result<Value, PageError> {
    if view["historyRevision"].as_str() != Some(revision) {
        return Err(PageError::StaleHistory);
    }
    let turn = turns(view)
        .into_iter()
        .find(|turn| turn.id == turn_id)
        .ok_or(PageError::TurnNotFound)?;
    let final_index = output_index(&turn);
    let process = turn
        .entries
        .iter()
        .enumerate()
        .filter(|(index, entry)| !entry.is_prompt() && Some(*index) != final_index)
        .map(|(_, entry)| entry)
        .collect::<Vec<_>>();
    if offset > process.len() || offset % PROCESS_PAGE_SIZE != 0 {
        return Err(PageError::InvalidOffset);
    }
    let end = offset.saturating_add(PROCESS_PAGE_SIZE).min(process.len());
    let items = process[offset..end]
        .iter()
        .map(|entry| entry.materialize())
        .collect::<Vec<_>>();
    let mut terminal_ids = HashSet::new();
    let items = json!(items);
    terminal_references(&items, &mut terminal_ids);
    let terminals = view["terminals"]
        .as_object()
        .into_iter()
        .flat_map(|terminals| terminals.iter())
        .filter(|(id, _)| terminal_ids.contains(*id))
        .map(|(id, value)| (id.clone(), value.clone()))
        .collect::<serde_json::Map<_, _>>();
    Ok(json!({
        "bridgeEpoch": view["bridgeEpoch"], "sessionId": view["sessionId"], "sessionIncarnation": view["sessionIncarnation"],
        "turnId": turn_id, "historyRevision": revision, "offset": offset, "total": process.len(),
        "nextOffset": (end < process.len()).then_some(end), "items": items, "terminals": terminals,
        "response": turn.outcomes.last().and_then(|outcome| outcome.get("response")),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(kind: &str, text: &str) -> Value {
        json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}})
    }

    fn view(updates: Vec<Value>) -> Value {
        json!({"bridgeEpoch": "epoch", "sessionId": "session", "sessionIncarnation": 3,
            "historyRevision": "history", "timeline": updates, "turnOutcomes": [],
            "activeTurn": null, "interactions": {}, "terminals": {}})
    }

    #[test]
    fn compact_hides_process_and_pages_ten_logical_entries_with_latest_tool_state() {
        let mut updates = vec![message("user_message_chunk", "prompt")];
        for index in 0..23 {
            updates.push(json!({"sessionUpdate": "tool_call", "toolCallId": format!("tool-{index}"),
                "title": format!("private-process-{index}"), "status": "in_progress", "rawInput": {"keep": index}}));
            updates.push(
                json!({"sessionUpdate": "tool_call_update", "toolCallId": format!("tool-{index}"),
                "status": "completed", "rawOutput": {"latest": index}, "rawInput": null}),
            );
        }
        updates.push(message("agent_message_chunk", "final answer"));
        let full = view(updates);
        let compact = compact_view(full.clone(), None);
        assert!(!compact.to_string().contains("private-process"));
        assert_eq!(compact["timeline"].as_array().unwrap().len(), 2);
        assert_eq!(compact["timeline"][1]["content"]["text"], "final answer");
        let descriptor = &compact["collapsedTurns"][0];
        assert_eq!(descriptor["processCount"], 23);
        assert_eq!(descriptor["beforeUpdate"], 0);
        assert_eq!(descriptor["afterUpdate"], 2);
        let id = descriptor["turnId"].as_str().unwrap();
        for (offset, count, next) in [(0, 10, Some(10)), (10, 10, Some(20)), (20, 3, None)] {
            let page = process_page(&full, id, "history", offset).unwrap();
            assert_eq!(page["items"].as_array().unwrap().len(), count);
            assert_eq!(page["nextOffset"], json!(next));
            assert_eq!(page["total"], 23);
            for (index, group) in page["items"].as_array().unwrap().iter().enumerate() {
                assert_eq!(group.as_array().unwrap().len(), 1);
                assert_eq!(group[0]["status"], "completed");
                assert_eq!(group[0]["rawInput"]["keep"], offset + index);
                assert_eq!(group[0]["rawOutput"]["latest"], offset + index);
            }
        }
        assert_eq!(
            process_page(&full, id, "changed", 0),
            Err(PageError::StaleHistory)
        );
        assert_eq!(
            process_page(&full, id, "history", 1),
            Err(PageError::InvalidOffset)
        );
        assert_eq!(
            process_page(&full, id, "history", 30),
            Err(PageError::InvalidOffset)
        );
        assert_eq!(
            process_page(&full, "missing", "history", 0),
            Err(PageError::TurnNotFound)
        );
    }

    #[test]
    fn final_named_message_preserves_all_blocks_and_earlier_thoughts_stay_deferred() {
        let mut first = message("agent_message_chunk", "final prefix");
        first["messageId"] = json!("answer");
        let image = json!({"sessionUpdate": "agent_message_chunk", "messageId": "answer",
            "content": {"type": "image", "mimeType": "image/png", "data": "picture"}});
        let full = view(vec![
            message("user_message_chunk", "prompt"),
            message("agent_thought_chunk", "secret thought"),
            first.clone(),
            json!({"sessionUpdate": "tool_call", "toolCallId": "tool", "title": "secret tool"}),
            image.clone(),
        ]);
        let compact = compact_view(full.clone(), None);
        assert_eq!(
            compact["timeline"],
            json!([full["timeline"][0], first, image])
        );
        assert_eq!(compact["collapsedTurns"][0]["processCount"], 2);
        assert!(!compact.to_string().contains("secret"));
    }

    #[test]
    fn turns_preserve_multi_chunk_prompts_empty_output_and_remapped_outcomes() {
        let mut first = message("user_message_chunk", "one");
        first["_meta"] = json!({"attyd": {"turnOperationId": "op1"}});
        let mut second = message("user_message_chunk", "two");
        second["_meta"] = first["_meta"].clone();
        let mut full = view(vec![
            first,
            second,
            message("agent_thought_chunk", "hidden"),
            message("user_message_chunk", "next"),
            message("agent_message_chunk", "answer"),
        ]);
        full["turnOutcomes"] = json!([
            {"operationId": "op1", "afterUpdate": 3, "response": {"stopReason": "cancelled"}},
            {"operationId": "op2", "afterUpdate": 5, "response": {"stopReason": "end_turn"}},
            {"operationId": "empty", "afterUpdate": 5, "response": {"stopReason": "end_turn"}},
        ]);
        let compact = compact_view(full.clone(), None);
        let turns = compact["collapsedTurns"].as_array().unwrap();
        assert_eq!(turns.len(), 3);
        assert_eq!(
            (
                turns[0]["beforeUpdate"].as_u64(),
                turns[0]["afterUpdate"].as_u64()
            ),
            (Some(0), Some(2))
        );
        assert_eq!(turns[0]["processCount"], 1);
        assert_eq!(turns[1]["processCount"], 0);
        assert_eq!(turns[2]["beforeUpdate"], 4);
        assert_eq!(turns[2]["afterUpdate"], 4);
        assert_ne!(turns[1]["turnId"], turns[2]["turnId"]);
        assert_eq!(turns[2]["outcomes"][0]["operationId"], "empty");
        let page = process_page(&full, turns[0]["turnId"].as_str().unwrap(), "history", 0).unwrap();
        assert_eq!(page["response"]["stopReason"], "cancelled");
    }

    #[test]
    fn loaded_history_without_outcomes_uses_new_user_prompts_as_turn_boundaries() {
        let full = view(vec![
            message("user_message_chunk", "first"),
            message("agent_message_chunk", "one"),
            message("user_message_chunk", "second"),
            message("agent_message_chunk", "two"),
        ]);
        let compact = compact_view(full, None);
        assert_eq!(compact["collapsedTurns"].as_array().unwrap().len(), 2);
        assert_eq!(compact["timeline"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn control_only_prefixes_and_suffixes_do_not_create_empty_turns() {
        let mut full = view(vec![
            json!({"sessionUpdate": "usage_update", "used": 1, "size": 100}),
            message("user_message_chunk", "prompt"),
            message("agent_message_chunk", "answer"),
            json!({"sessionUpdate": "usage_update", "used": 2, "size": 100}),
        ]);
        full["turnOutcomes"] = json!([{ "operationId": "op", "afterUpdate": 3, "response": {"stopReason": "end_turn"}}]);
        let compact = compact_view(full, None);
        assert_eq!(compact["collapsedTurns"].as_array().unwrap().len(), 1);
        assert_eq!(compact["collapsedTurns"][0]["processCount"], 0);
        assert_eq!(compact["timeline"].as_array().unwrap().len(), 2);
        let empty = compact_view(
            view(vec![
                json!({"sessionUpdate": "usage_update", "used": 1, "size": 100}),
            ]),
            None,
        );
        assert_eq!(empty["collapsedTurns"], json!([]));
    }

    #[test]
    fn terminal_payloads_follow_visible_or_requested_entries_and_live_resources() {
        let mut full = view(vec![
            message("user_message_chunk", "prompt"),
            json!({"sessionUpdate": "tool_call", "toolCallId": "tool", "title": "run", "content": [{"type": "terminal", "terminalId": "historical"}]}),
            message("agent_message_chunk", "done"),
        ]);
        full["terminals"] = json!({"historical": {"output": "secret terminal output"}, "live": {"output": "live output"}, "unrelated": {"output": "unrelated"}});
        full["activeTurn"] =
            json!({"updates": [message("agent_thought_chunk", "active remains full")]});
        let compact = compact_view(full.clone(), Some(&json!({"live": {}})));
        assert_eq!(
            compact["terminals"],
            json!({"live": {"output": "live output"}})
        );
        assert_eq!(compact["activeTurn"], full["activeTurn"]);
        let page = process_page(
            &full,
            compact["collapsedTurns"][0]["turnId"].as_str().unwrap(),
            "history",
            0,
        )
        .unwrap();
        assert_eq!(
            page["terminals"],
            json!({"historical": {"output": "secret terminal output"}})
        );
    }

    #[test]
    fn plan_and_compaction_snapshots_count_once_and_keep_current_semantics() {
        let full = view(vec![
            message("user_message_chunk", "prompt"),
            json!({"sessionUpdate": "plan_update", "plan": {"planId": "p", "type": "items", "entries": []}}),
            json!({"sessionUpdate": "plan_update", "plan": {"planId": "p", "type": "items", "entries": [{"content": "latest"}]}}),
            json!({"sessionUpdate": "plan_removed", "planId": "p"}),
            json!({"sessionUpdate": "plan_removed", "planId": "p"}),
            json!({"sessionUpdate": "compaction_update", "compactionId": "c", "status": "in_progress", "summary": [{"type": "text", "text": "obsolete"}]}),
            json!({"sessionUpdate": "compaction_update", "compactionId": "c", "status": "completed", "summary": [{"type": "text", "text": "latest"}]}),
            json!({"sessionUpdate": "compaction_summary_chunk", "compactionId": "c", "content": {"type": "text", "text": " tail"}}),
            message("agent_message_chunk", "done"),
        ]);
        let compact = compact_view(full.clone(), None);
        assert_eq!(compact["collapsedTurns"][0]["processCount"], 2);
        let page = process_page(
            &full,
            compact["collapsedTurns"][0]["turnId"].as_str().unwrap(),
            "history",
            0,
        )
        .unwrap();
        assert_eq!(page["items"][0].as_array().unwrap().len(), 2);
        assert_eq!(
            page["items"][0][0]["plan"]["entries"][0]["content"],
            "latest"
        );
        assert_eq!(page["items"][0][1]["sessionUpdate"], "plan_removed");
        assert_eq!(page["items"][1][0]["status"], "completed");
        assert_eq!(page["items"][1][0]["summary"].as_array().unwrap().len(), 1);
        assert_eq!(page["items"][1][0]["summary"][0]["text"], "latest tail");
        assert!(!page.to_string().contains("obsolete"));
    }

    #[test]
    fn orphan_tool_updates_recover_the_same_card_before_later_creation() {
        let full = view(vec![
            message("user_message_chunk", "prompt"),
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "orphan", "title": "ignored-first-title", "status": "completed"}),
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "orphan", "rawOutput": {"latest": true}}),
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "recovered", "status": "completed"}),
            json!({"sessionUpdate": "tool_call", "toolCallId": "recovered", "title": "Now introduced", "status": "completed", "content": []}),
        ]);
        let compact = compact_view(full.clone(), None);
        let page = process_page(
            &full,
            compact["collapsedTurns"][0]["turnId"].as_str().unwrap(),
            "history",
            0,
        )
        .unwrap();
        assert_eq!(page["items"][0][0]["title"], "Tool call not found");
        assert_eq!(page["items"][0][0]["status"], "failed");
        assert_eq!(page["items"][0][0]["rawOutput"]["latest"], true);
        assert_eq!(page["items"][1][0]["title"], "Now introduced");
        assert_eq!(page["items"][1][0]["status"], "completed");
        assert_eq!(page["items"][1][0]["content"], json!([]));
    }

    #[test]
    fn observed_turn_and_completed_suffix_keep_process_without_expanding_older_history() {
        let mut full = view(Vec::new());
        let mut updates = Vec::new();
        let mut outcomes = Vec::new();
        for operation_id in ["older", "watched", "next"] {
            updates.push(message("user_message_chunk", operation_id));
            updates.push(message(
                "agent_thought_chunk",
                &format!("{operation_id}-private-process"),
            ));
            updates.push(message(
                "agent_message_chunk",
                &format!("{operation_id}-answer"),
            ));
            outcomes.push(json!({"operationId": operation_id, "afterUpdate": updates.len(), "response": {"stopReason": "end_turn"}}));
        }
        full["timeline"] = json!(updates);
        full["turnOutcomes"] = json!(outcomes);
        let compact = compact_view_from(full.clone(), None, Some("watched"), &[]);
        assert!(!compact.to_string().contains("older-private-process"));
        assert!(compact.to_string().contains("watched-private-process"));
        assert!(compact.to_string().contains("next-private-process"));
        let descriptors = compact["collapsedTurns"].as_array().unwrap();
        assert!(descriptors[0].get("processIncluded").is_none());
        assert_eq!(descriptors[1]["processIncluded"], true);
        assert_eq!(descriptors[2]["processIncluded"], true);
        assert_eq!(
            descriptors
                .iter()
                .map(|descriptor| descriptor["processCount"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![1, 1, 1]
        );
        assert_eq!(descriptors[1]["beforeUpdate"], 2);
        assert_eq!(descriptors[1]["afterUpdate"], 5);
        assert_eq!(descriptors[2]["beforeUpdate"], 5);
        assert_eq!(descriptors[2]["afterUpdate"], 8);
        assert_eq!(compact["turnOutcomes"][2]["afterUpdate"], 8);
        assert_eq!(
            compact_view_from(full.clone(), None, Some("missing"), &[]),
            compact_view(full, None)
        );
    }

    #[test]
    fn observed_process_anchor_can_match_prompt_metadata_without_outcomes() {
        let mut prompt = message("user_message_chunk", "prompt");
        prompt["_meta"] = json!({"attyd": {"turnOperationId": "watched"}});
        let full = view(vec![
            prompt,
            json!({"sessionUpdate": "tool_call", "toolCallId": "tool", "title": "watched tool", "content": [{"type": "terminal", "terminalId": "watched-terminal"}]}),
            message("agent_message_chunk", "answer"),
        ]);
        let mut full = full;
        full["terminals"] = json!({"watched-terminal": {"output": "already visible output"}});
        let compact = compact_view_from(full, None, Some("watched"), &[]);
        assert_eq!(compact["collapsedTurns"][0]["processIncluded"], true);
        assert_eq!(compact["timeline"].as_array().unwrap().len(), 3);
        assert_eq!(
            compact["terminals"]["watched-terminal"]["output"],
            "already visible output"
        );
    }

    #[test]
    fn released_process_is_a_selective_hole_in_the_observed_suffix() {
        let mut full = view(Vec::new());
        let mut updates = Vec::new();
        let mut outcomes = Vec::new();
        for operation_id in ["a", "b", "c"] {
            updates.push(message("user_message_chunk", operation_id));
            updates.push(
                json!({"sessionUpdate": "tool_call", "toolCallId": operation_id,
                "title": format!("{operation_id}-private-process"),
                "content": [{"type": "terminal", "terminalId": operation_id}]}),
            );
            updates.push(message(
                "agent_message_chunk",
                &format!("{operation_id}-answer"),
            ));
            outcomes.push(
                json!({"operationId": operation_id, "afterUpdate": updates.len(),
                "response": {"stopReason": "end_turn"}}),
            );
        }
        full["timeline"] = json!(updates);
        full["turnOutcomes"] = json!(outcomes);
        full["terminals"] = json!({"a": {"output": "a-output"}, "b": {"output": "b-output"}, "c": {"output": "c-output"}});
        let compact = compact_view_from(full.clone(), None, Some("a"), &["b".into()]);
        assert!(compact.to_string().contains("a-private-process"));
        assert!(!compact.to_string().contains("b-private-process"));
        assert!(compact.to_string().contains("c-private-process"));
        assert!(compact["terminals"].get("b").is_none());
        let descriptors = compact["collapsedTurns"].as_array().unwrap();
        assert_eq!(descriptors[0]["processIncluded"], true);
        assert!(descriptors[1].get("processIncluded").is_none());
        assert_eq!(descriptors[2]["processIncluded"], true);
        assert_eq!(descriptors[1]["operationId"], "b");
        assert_eq!(descriptors[1]["processCount"], 1);
        assert_eq!(descriptors[1]["beforeUpdate"], 3);
        assert_eq!(descriptors[1]["afterUpdate"], 5);
        assert_eq!(descriptors[2]["beforeUpdate"], 5);
        assert_eq!(descriptors[2]["afterUpdate"], 8);
        assert_eq!(compact["turnOutcomes"][2]["afterUpdate"], 8);
        assert_eq!(
            descriptors[2]["visibleRanges"],
            json!([{"start": 5, "end": 6}, {"start": 7, "end": 8}])
        );
        let page = process_page(
            &full,
            descriptors[1]["turnId"].as_str().unwrap(),
            "history",
            0,
        )
        .unwrap();
        assert_eq!(page["total"], 1);
        assert_eq!(page["items"][0][0]["title"], "b-private-process");

        // Releasing the anchor itself must not hide later observed turns.
        let compact = compact_view_from(full, None, Some("a"), &["a".into()]);
        assert!(!compact.to_string().contains("a-private-process"));
        assert!(compact.to_string().contains("b-private-process"));
        assert!(compact.to_string().contains("c-private-process"));
    }

    #[test]
    fn visible_ranges_keep_all_prompt_and_final_message_blocks_without_process() {
        let mut prompt = message("user_message_chunk", "prompt");
        prompt["messageId"] = json!("prompt");
        prompt["_meta"] = json!({"attyd": {"turnOperationId": "watched"}});
        let full = view(vec![
            prompt,
            json!({"sessionUpdate": "user_message_chunk", "messageId": "prompt", "content": {"type": "image", "data": "prompt-image", "mimeType": "image/png"}}),
            message("agent_thought_chunk", "hidden-thought"),
            json!({"sessionUpdate": "agent_message_chunk", "messageId": "final", "content": {"type": "text", "text": "answer"}}),
            json!({"sessionUpdate": "tool_call", "toolCallId": "tool", "title": "hidden-tool"}),
            json!({"sessionUpdate": "agent_message_chunk", "messageId": "final", "content": {"type": "image", "data": "answer-image", "mimeType": "image/png"}}),
            json!({"sessionUpdate": "agent_message_chunk", "messageId": "final", "content": {"type": "text", "text": "ending"}}),
        ]);
        let included = compact_view_from(full.clone(), None, Some("watched"), &[]);
        let descriptor = &included["collapsedTurns"][0];
        assert_eq!(descriptor["operationId"], "watched");
        assert_eq!(descriptor["processCount"], 2);
        assert_eq!(
            descriptor["visibleRanges"],
            json!([{"start": 0, "end": 2}, {"start": 3, "end": 6}])
        );
        let ranges = descriptor["visibleRanges"].as_array().unwrap();
        let visible = ranges
            .iter()
            .flat_map(|range| {
                let start = range["start"].as_u64().unwrap() as usize;
                let end = range["end"].as_u64().unwrap() as usize;
                included["timeline"].as_array().unwrap()[start..end]
                    .iter()
                    .cloned()
            })
            .collect::<Vec<_>>();
        let compact = compact_view_from(full, None, Some("watched"), &["watched".into()]);
        assert_eq!(json!(visible), compact["timeline"]);
        assert_eq!(visible.len(), 5);
        assert_eq!(
            compact["collapsedTurns"][0]["visibleRanges"],
            json!([{"start": 0, "end": 2}, {"start": 2, "end": 5}])
        );
        assert!(
            compact["collapsedTurns"][0]
                .get("processIncluded")
                .is_none()
        );
    }
}
