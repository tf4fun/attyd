//! The session timeline ([bridge memory model](../../docs/bridge-memory-model.md) §3–§5).
//!
//! One flat list of entries, built by one reducer. A `session/load` replay and the
//! updates of live prompts both go through [`Timeline::reduce`], so neither path
//! can produce a shape the other cannot. What only the live path observes, the
//! prompt the bridge sent and its outcome, is added to the same structure
//! ([`Timeline::begin_prompt`], [`Timeline::end_prompt`]). Turns are not stored;
//! [`Timeline::turns`] derives them.
//!
//! Entries are shared structurally (`Arc<Entry>`): cloning a timeline copies
//! pointers, and an update copies only the entry it changes, so a held view of the
//! timeline never makes an update allocate in proportion to unrelated content.
//!
//! Content blocks are kept as received. Replacing byte-carrying blocks with
//! attachment references is a wire concern of the projection, not of this memory.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Memory model §3.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Entry {
    Message(MessageEntry),
    Tool(ToolEntry),
    Plan(PlanEntry),
    Compaction(CompactionEntry),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Role {
    User,
    Agent,
    Thought,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MessageEntry {
    pub role: Role,
    /// The first `messageId` one of its chunks carried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    pub chunks: Vec<Chunk>,
}

/// One received chunk, its own fields kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Chunk {
    pub content: Value,
    /// `ContentChunk._meta`.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
    /// `SessionNotification._meta` of the notification that produced the chunk (§3.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolEntry {
    /// ACP's sparse `ToolCallUpdate`, reduced: a field is absent until the Agent
    /// provides it.
    pub call: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlanEntry {
    #[serde(flatten)]
    pub state: PlanState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanState {
    /// A legacy `plan` has no ID: a turn's snapshot (§3.4).
    Legacy(Value),
    /// By `planId`: a `plan_update`, or a `plan_removed`.
    Update(Value),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompactionEntry {
    pub compaction_id: String,
    pub status: Value,
    pub summary: Vec<Chunk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_meta: Option<Value>,
}

/// A prompt the bridge sent (memory model §4.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Prompt {
    /// Index of the user entry it sent.
    pub entry: usize,
    /// Absent while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<PromptEnd>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PromptEnd {
    /// `PromptResponse`, verbatim.
    Response(Value),
    /// The Agent answered with an error.
    Error(Value),
    /// No answer: the connection ended. Always `true`.
    Interrupted(bool),
}

/// A derived turn: entries `start..end`, and the prompt that started it, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Turn {
    pub start: usize,
    pub end: usize,
    pub prompt: Option<usize>,
}

/// An entity with an ID unique in the session: it has one entry (rule 1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Entity {
    Tool(String),
    Plan(String),
    Compaction(String),
}

/// The prompt the bridge sent and the blocks an echo may still match (§4.1).
#[derive(Debug, Clone)]
struct Sent {
    entry: usize,
    blocks: Vec<Chunk>,
    matched: usize,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Timeline {
    entries: Vec<Arc<Entry>>,
    prompts: Vec<Prompt>,
    places: HashMap<Entity, usize>,
    /// The user entry that started the current turn, if any (§5).
    opener: Option<usize>,
    /// The current turn's legacy plan entry (§3.4).
    legacy_plan: Option<usize>,
    sent: Option<Sent>,
}

impl Timeline {
    /// A load candidate: the replay reduced by the same reducer as live updates.
    pub(crate) fn from_replay<'a>(
        updates: impl IntoIterator<Item = (&'a Value, Option<&'a Value>)>,
    ) -> Self {
        let mut timeline = Self::default();
        for (update, envelope) in updates {
            timeline.reduce(update, envelope);
        }
        timeline
    }

    pub(crate) fn entries(&self) -> &[Arc<Entry>] {
        &self.entries
    }

    pub(crate) fn prompts(&self) -> &[Prompt] {
        &self.prompts
    }

    /// Reduces one `session/update` (`update` carries its `sessionUpdate` tag;
    /// `envelope` is the notification's `_meta`). Control updates and updates the
    /// bridge cannot reduce change nothing.
    pub(crate) fn reduce(&mut self, update: &Value, envelope: Option<&Value>) {
        let Some(kind) = update.get("sessionUpdate").and_then(Value::as_str) else {
            return;
        };
        let envelope = envelope.filter(|meta| !meta.is_null());
        match kind {
            "user_message_chunk" => self.chunk(Role::User, update, envelope),
            "agent_message_chunk" => self.chunk(Role::Agent, update, envelope),
            "agent_thought_chunk" => self.chunk(Role::Thought, update, envelope),
            "tool_call" => self.tool(update, true, envelope),
            "tool_call_update" => self.tool(update, false, envelope),
            "plan" => self.legacy_plan(update, envelope),
            "plan_update" => {
                if let Some(plan_id) = update["plan"]["planId"].as_str() {
                    self.plan(plan_id, update, envelope);
                }
            }
            "plan_removed" => {
                if let Some(plan_id) = update["planId"].as_str() {
                    self.plan(plan_id, update, envelope);
                }
            }
            "compaction_update" => self.compaction(update, envelope),
            "compaction_summary_chunk" => self.summary_chunk(update, envelope),
            _ => {}
        }
    }

    /// A permission request's `toolCall` is a `ToolCallUpdate`, reduced like one
    /// (§3.2), also without a preceding tool notification.
    pub(crate) fn reduce_permission_tool_call(&mut self, tool_call: &Value) {
        self.tool(tool_call, false, None);
    }

    /// The bridge sends `session/prompt`: its blocks become the user entry the
    /// next echo may complete (§4.1), and the prompt is recorded as running.
    pub(crate) fn begin_prompt(&mut self, blocks: &[Value]) {
        let blocks: Vec<Chunk> = blocks
            .iter()
            .filter_map(block)
            .map(|content| Chunk {
                content,
                meta: None,
                notification_meta: None,
            })
            .collect();
        let mut chunks = Vec::new();
        for chunk in blocks.iter().cloned() {
            join(&mut chunks, chunk);
        }
        let index = self.push_message(Role::User, None, chunks);
        self.sent = Some(Sent {
            entry: index,
            blocks,
            matched: 0,
        });
        self.prompts.push(Prompt {
            entry: index,
            end: None,
        });
    }

    /// The running prompt ended with `end`; its content stays whatever the outcome.
    pub(crate) fn end_prompt(&mut self, end: PromptEnd) {
        if let Some(prompt) = self.prompts.iter_mut().rev().find(|p| p.end.is_none()) {
            prompt.end = Some(end);
        }
    }

    /// The turns of the entries, derived (memory model §5): a turn starts at each
    /// user message entry, except one whose present `messageId` equals that of the
    /// entry that started the current turn. Entries before the first user entry
    /// form a first turn.
    pub(crate) fn turns(&self) -> Vec<Turn> {
        let mut starts: Vec<usize> = Vec::new();
        let mut opener: Option<usize> = None;
        for (index, entry) in self.entries.iter().enumerate() {
            if !matches!(&**entry, Entry::Message(m) if m.role == Role::User) {
                continue;
            }
            if self.continues_turn(opener, index) {
                continue;
            }
            opener = Some(index);
            starts.push(index);
        }
        if starts.first() != Some(&0) && !self.entries.is_empty() {
            starts.insert(0, 0);
        }
        let prompt_of = |start: usize| self.prompts.iter().position(|p| p.entry == start);
        starts
            .iter()
            .enumerate()
            .map(|(n, &start)| Turn {
                start,
                end: starts.get(n + 1).copied().unwrap_or(self.entries.len()),
                prompt: prompt_of(start),
            })
            .collect()
    }

    /// Whether the user entry at `index` continues the turn `opener` started.
    fn continues_turn(&self, opener: Option<usize>, index: usize) -> bool {
        let id = |i: usize| match &*self.entries[i] {
            Entry::Message(message) => message.message_id.as_deref(),
            _ => None,
        };
        matches!((opener.and_then(id), id(index)), (Some(a), Some(b)) if a == b)
    }

    fn push(&mut self, entry: Entry) -> usize {
        let index = self.entries.len();
        self.entries.push(Arc::new(entry));
        if self.sent.as_ref().is_some_and(|sent| sent.entry != index) {
            self.sent = None;
        }
        index
    }

    fn push_message(&mut self, role: Role, message_id: Option<String>, chunks: Vec<Chunk>) -> usize {
        let index = self.push(Entry::Message(MessageEntry {
            role,
            message_id,
            chunks,
        }));
        if role == Role::User && !self.continues_turn(self.opener, index) {
            self.opener = Some(index);
            self.legacy_plan = None;
        }
        index
    }

    fn chunk(&mut self, role: Role, update: &Value, envelope: Option<&Value>) {
        let Some(content) = block(update.get("content").unwrap_or(&Value::Null)) else {
            return;
        };
        let message_id = update
            .get("messageId")
            .and_then(Value::as_str)
            .map(String::from);
        let chunk = Chunk {
            content,
            meta: meta(update),
            notification_meta: envelope.cloned(),
        };
        let mut start_new = false;
        if role == Role::User && self.sent.is_some() {
            if self.echo(&message_id, &chunk) {
                return;
            }
            // Not an echo: it never joins the sent text (§4.1).
            self.sent = None;
            start_new = true;
        }
        let joins = !start_new
            && matches!(
                self.entries.last().map(|entry| &**entry),
                Some(Entry::Message(last))
                    if last.role == role
                        && compatible(last.message_id.as_deref(), message_id.as_deref())
            );
        if joins {
            if let Some(Entry::Message(last)) = self.entries.last_mut().map(Arc::make_mut) {
                if last.message_id.is_none() {
                    last.message_id = message_id;
                }
                join(&mut last.chunks, chunk);
            }
            return;
        }
        self.push_message(role, message_id, vec![chunk]);
    }

    /// Whether `chunk` echoes the next sent block (§4.1). An echo adds no chunk:
    /// it completes the matched block and the entry's `messageId`.
    fn echo(&mut self, message_id: &Option<String>, chunk: &Chunk) -> bool {
        let Some(sent) = &mut self.sent else {
            return false;
        };
        let Some(expected) = sent.blocks.get_mut(sent.matched) else {
            return false;
        };
        let id_fits = match self.entries.get(sent.entry).map(|entry| &**entry) {
            Some(Entry::Message(entry)) => {
                compatible(entry.message_id.as_deref(), message_id.as_deref())
            }
            _ => return false,
        };
        if expected.content != chunk.content || !id_fits {
            return false;
        }
        expected.meta = chunk.meta.clone();
        expected.notification_meta = chunk.notification_meta.clone();
        let Some(Entry::Message(entry)) = self.entries.get_mut(sent.entry).map(Arc::make_mut)
        else {
            return false;
        };
        if entry.message_id.is_none() {
            entry.message_id = message_id.clone();
        }
        sent.matched += 1;
        entry.chunks = Vec::new();
        for block in sent.blocks.iter().cloned() {
            join(&mut entry.chunks, block);
        }
        if sent.matched == sent.blocks.len() {
            self.sent = None;
        }
        true
    }

    fn tool(&mut self, update: &Value, announce: bool, envelope: Option<&Value>) {
        let Some(tool_call_id) = update.get("toolCallId").and_then(Value::as_str) else {
            return;
        };
        let patch = tool_patch(update, announce);
        let entity = Entity::Tool(tool_call_id.to_owned());
        match self.places.get(&entity).copied() {
            Some(index) => {
                if let Entry::Tool(tool) = Arc::make_mut(&mut self.entries[index]) {
                    if let Some(call) = tool.call.as_object_mut() {
                        call.extend(patch);
                    }
                    if let Some(envelope) = envelope {
                        tool.notification_meta = Some(envelope.clone());
                    }
                }
            }
            None => {
                let index = self.push(Entry::Tool(ToolEntry {
                    call: Value::Object(patch),
                    notification_meta: envelope.cloned(),
                }));
                self.places.insert(entity, index);
            }
        }
    }

    /// A legacy `plan` replaces the legacy plan entry of the current turn, or
    /// starts one (§3.4).
    fn legacy_plan(&mut self, update: &Value, envelope: Option<&Value>) {
        let state = PlanState::Legacy(untagged(update));
        match self.legacy_plan {
            Some(index) => {
                if let Entry::Plan(plan) = Arc::make_mut(&mut self.entries[index]) {
                    plan.state = state;
                    if let Some(envelope) = envelope {
                        plan.notification_meta = Some(envelope.clone());
                    }
                }
            }
            None => {
                let index = self.push(Entry::Plan(PlanEntry {
                    state,
                    notification_meta: envelope.cloned(),
                }));
                self.legacy_plan = Some(index);
            }
        }
    }

    fn plan(&mut self, plan_id: &str, update: &Value, envelope: Option<&Value>) {
        let state = PlanState::Update(untagged(update));
        let entity = Entity::Plan(plan_id.to_owned());
        match self.places.get(&entity).copied() {
            Some(index) => {
                if let Entry::Plan(plan) = Arc::make_mut(&mut self.entries[index]) {
                    plan.state = state;
                    if let Some(envelope) = envelope {
                        plan.notification_meta = Some(envelope.clone());
                    }
                }
            }
            None => {
                let index = self.push(Entry::Plan(PlanEntry {
                    state,
                    notification_meta: envelope.cloned(),
                }));
                self.places.insert(entity, index);
            }
        }
    }

    fn compaction(&mut self, update: &Value, envelope: Option<&Value>) {
        let Some(compaction_id) = update.get("compactionId").and_then(Value::as_str) else {
            return;
        };
        // `summary`, `error` and `_meta`: absent keeps, `null` clears, a value
        // replaces. A summary becomes chunks without chunk `_meta`.
        let summary = match update.get("summary") {
            Some(Value::Array(blocks)) => Some(
                blocks
                    .iter()
                    .filter_map(block)
                    .map(|content| Chunk {
                        content,
                        meta: None,
                        notification_meta: envelope.cloned(),
                    })
                    .collect(),
            ),
            Some(Value::Null) => Some(Vec::new()),
            _ => None,
        };
        let entity = Entity::Compaction(compaction_id.to_owned());
        let index = match self.places.get(&entity).copied() {
            Some(index) => index,
            None => {
                let index = self.push(Entry::Compaction(CompactionEntry {
                    compaction_id: compaction_id.to_owned(),
                    status: update["status"].clone(),
                    summary: Vec::new(),
                    error: None,
                    meta: None,
                    notification_meta: None,
                }));
                self.places.insert(entity, index);
                index
            }
        };
        let Entry::Compaction(entry) = Arc::make_mut(&mut self.entries[index]) else {
            return;
        };
        if let Some(status) = update.get("status") {
            entry.status = status.clone();
        }
        if let Some(summary) = summary {
            entry.summary = summary;
        }
        if let Some(error) = update.get("error") {
            entry.error = error.as_str().map(String::from);
        }
        if let Some(value) = update.get("_meta") {
            entry.meta = (!value.is_null()).then(|| value.clone());
        }
        if let Some(envelope) = envelope {
            entry.notification_meta = Some(envelope.clone());
        }
    }

    fn summary_chunk(&mut self, update: &Value, envelope: Option<&Value>) {
        let Some(compaction_id) = update.get("compactionId").and_then(Value::as_str) else {
            return;
        };
        let Some(&index) = self
            .places
            .get(&Entity::Compaction(compaction_id.to_owned()))
        else {
            return;
        };
        let Some(content) = block(update.get("content").unwrap_or(&Value::Null)) else {
            return;
        };
        let chunk = Chunk {
            content,
            meta: meta(update),
            notification_meta: envelope.cloned(),
        };
        if let Entry::Compaction(entry) = Arc::make_mut(&mut self.entries[index]) {
            join(&mut entry.summary, chunk);
        }
    }
}

/// A content block: an object with a `type`.
fn block(value: &Value) -> Option<Value> {
    (value.get("type").and_then(Value::as_str).is_some()).then(|| value.clone())
}

/// The value's `_meta`; `null` is absent.
fn meta(value: &Value) -> Option<Value> {
    value.get("_meta").filter(|meta| !meta.is_null()).cloned()
}

/// Two `messageId`s differ only when both are present and unequal (rule 1).
fn compatible(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left == right,
        _ => true,
    }
}

/// An update's own fields, without its `sessionUpdate` tag.
fn untagged(update: &Value) -> Value {
    let mut state = update.clone();
    if let Some(fields) = state.as_object_mut() {
        fields.remove("sessionUpdate");
    }
    state
}

/// Whether two blocks differ at most in `text`.
fn same_but_text(left: &Value, right: &Value) -> bool {
    let (Some(left), Some(right)) = (left.as_object(), right.as_object()) else {
        return false;
    };
    left.len() == right.len()
        && left
            .iter()
            .all(|(key, value)| key == "text" || right.get(key) == Some(value))
}

/// Rule 3: a text chunk joins the previous text chunk when every other field of
/// both (annotations, block `_meta`, chunk `_meta`, `notificationMeta`) is equal;
/// otherwise it is a new chunk.
fn join(chunks: &mut Vec<Chunk>, chunk: Chunk) {
    if let Some(last) = chunks.last_mut()
        && last.meta == chunk.meta
        && last.notification_meta == chunk.notification_meta
        && last.content["type"] == "text"
        && chunk.content["type"] == "text"
        && same_but_text(&last.content, &chunk.content)
        && let Some(Value::String(text)) = last.content.get_mut("text")
    {
        text.push_str(chunk.content["text"].as_str().unwrap_or_default());
        return;
    }
    chunks.push(chunk);
}

/// A tool update as the patch rule applies it (rule 2): its fields but `null`,
/// and for a `tool_call` the schema's defaults of the fields it omits
/// (`ToolCallUpdate::from`).
fn tool_patch(update: &Value, announce: bool) -> Map<String, Value> {
    let mut patch: Map<String, Value> = update
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, value)| *key != "sessionUpdate" && !value.is_null())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if announce {
        for (field, default) in [
            ("kind", json!("other")),
            ("status", json!("pending")),
            ("content", json!([])),
            ("locations", json!([])),
        ] {
            patch.entry(field).or_insert(default);
        }
    }
    patch
}

#[cfg(test)]
mod memory_tests;
#[cfg(test)]
mod tests;
