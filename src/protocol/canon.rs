//! The timeline reducer and the hash chain ([protocol §3](../../docs/protocol.md#3-log)).
//!
//! A live settle and a load's replay go through the same reducer, so the same
//! updates give the same items however they were chunked (acceptance 1).
//! Session updates and content blocks stay `serde_json::Value`, as in the bridge.
use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Entries (protocol §3.1)

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct Turn {
    /// The turn's timeline, in order (§3.2).
    pub items: Vec<Item>,
    /// How its prompt ended, when the bridge observed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub end: Option<TurnEnd>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Item {
    Message(MessageItem),
    Tool(ToolItem),
    Plan(PlanItem),
    Compaction(CompactionItem),
}

/// One contiguous run of one message's chunks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct MessageItem {
    pub role: Role,
    /// The chunks' `messageId`; absent when they had none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub message_id: Option<String>,
    pub chunks: Vec<Chunk>,
}

/// `user_message_chunk`, `agent_message_chunk`, `agent_thought_chunk`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum Role {
    User,
    Agent,
    Thought,
}

/// One received chunk, its own fields kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct Chunk {
    pub content: Block,
    /// The chunk's `_meta`, apart from the block's own.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional, type = "unknown"))]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct ToolItem {
    /// ACP's sparse `ToolCallUpdate`, reduced: a field is absent until the
    /// Agent provides it.
    #[cfg_attr(test, ts(type = "ToolCallState"))]
    pub call: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct PlanItem {
    /// A legacy `Plan` (this turn's snapshot), or by `planId` the plan's
    /// state: a `PlanUpdate` or a `PlanRemoved`.
    #[cfg_attr(test, ts(type = "Plan | PlanUpdate | PlanRemoved"))]
    pub update: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompactionItem {
    pub compaction_id: String,
    #[cfg_attr(test, ts(type = "CompactionStatus"))]
    pub status: Value,
    /// From `summary` (no chunk `_meta`) and summary chunks.
    pub summary: Vec<Chunk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub error: Option<String>,
    /// The compaction's `_meta`, patched.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional, type = "unknown"))]
    pub meta: Option<Value>,
}

/// How a turn's prompt ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct TurnEnd {
    /// ACP, verbatim.
    #[cfg_attr(test, ts(type = "PromptResponse"))]
    pub response: Value,
}

/// Content as the log carries it: a block that carries bytes is an
/// `AttachmentRef`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(untagged)]
pub(crate) enum Block {
    Attachment(AttachmentRef),
    /// A text or resource link block, verbatim.
    #[cfg_attr(
        test,
        ts(type = "Extract<ContentBlock, { type: \"text\" | \"resource_link\" }>")
    )]
    Content(Value),
}

/// What a prompt may contain (protocol §6.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(untagged)]
pub(crate) enum PromptBlock {
    Attachment(AttachmentRef),
    #[cfg_attr(test, ts(type = "ContentBlock"))]
    Content(Value),
}

/// The bridge's reference to an image, audio or embedded resource block; not an
/// ACP `ContentBlock`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct AttachmentRef {
    #[cfg_attr(test, ts(type = "\"attachment\""))]
    pub r#type: AttachmentTag,
    /// Lowercase hex SHA-256 of the block's canonical JSON, `_meta` included.
    pub id: String,
    pub block: AttachmentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub mime_type: Option<String>,
    /// A resource's URI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub uri: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum AttachmentTag {
    #[serde(rename = "attachment")]
    Attachment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum AttachmentKind {
    Image,
    Audio,
    Resource,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct LogEntry {
    pub turn: Turn,
    /// `h_i` in its wire form (protocol §3.4).
    pub hash: String,
    #[serde(skip)]
    #[cfg_attr(test, ts(skip))]
    digest: ChainHash,
}

// ---------------------------------------------------------------------------
// Content

/// Attachment blocks by ID, as received; what attachment references name.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Attachments(HashMap<String, Arc<Value>>);

impl Attachments {
    pub(crate) fn get(&self, id: &str) -> Option<&Value> {
        self.0.get(id).map(|block| &**block)
    }

    /// A content block as the log carries it: text and resource links
    /// inline, a block that carries bytes kept here and referenced. `None`
    /// for a block that is no content the protocol carries.
    pub(crate) fn block(&mut self, block: &Value) -> Option<Block> {
        let kind = match block.get("type")?.as_str()? {
            "text" | "resource_link" => return Some(Block::Content(block.clone())),
            "image" => AttachmentKind::Image,
            "audio" => AttachmentKind::Audio,
            "resource" => AttachmentKind::Resource,
            _ => return None,
        };
        let source = match kind {
            AttachmentKind::Resource => &block["resource"],
            AttachmentKind::Image | AttachmentKind::Audio => block,
        };
        let mime_type = source["mimeType"].as_str().map(String::from);
        let uri = match kind {
            AttachmentKind::Resource => source["uri"].as_str().map(String::from),
            AttachmentKind::Image | AttachmentKind::Audio => None,
        };
        let id = Sha256::digest(canonical_json(block))
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        self.0
            .entry(id.clone())
            .or_insert_with(|| Arc::new(block.clone()));
        Some(Block::Attachment(AttachmentRef {
            r#type: AttachmentTag::Attachment,
            id,
            block: kind,
            mime_type,
            uri,
        }))
    }
}

/// A prompt names an attachment the session does not keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnknownAttachment(pub String);

/// Content blocks as the log carries them, one per block, keeping each
/// attachment's block in `attachments`.
pub(crate) fn normalize(blocks: &[Value], attachments: &mut Attachments) -> Vec<Block> {
    blocks
        .iter()
        .filter_map(|block| attachments.block(block))
        .collect()
}

/// Turns a prompt's attachment references back into the blocks they name, before
/// `session/prompt`.
pub(crate) fn resolve(
    prompt: &[PromptBlock],
    attachments: &Attachments,
) -> Result<Vec<Value>, UnknownAttachment> {
    prompt
        .iter()
        .map(|block| match block {
            PromptBlock::Attachment(reference) => attachments
                .get(&reference.id)
                .cloned()
                .ok_or_else(|| UnknownAttachment(reference.id.clone())),
            PromptBlock::Content(block) => Ok(block.clone()),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Reduction (protocol §3.2)

/// An entity with an ID unique in the session: it has one item (rule 1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Entity {
    Tool(String),
    Plan(String),
    Compaction(String),
}

/// Where an item is: the index of its turn, which is the log's length for the
/// turn being reduced, and its index in that turn.
type Place = (usize, usize);

/// The value's `_meta`; `null` is absent.
fn meta(value: &Value) -> Option<Value> {
    value.get("_meta").filter(|meta| !meta.is_null()).cloned()
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

/// Rule 3: a text chunk joins the previous text chunk when every other field
/// of both is equal; otherwise it is a new chunk.
fn join(chunks: &mut Vec<Chunk>, chunk: Chunk) {
    if let Some(last) = chunks.last_mut()
        && last.meta == chunk.meta
        && let (Block::Content(previous), Block::Content(next)) = (&mut last.content, &chunk.content)
        && previous["type"] == "text"
        && next["type"] == "text"
        && same_but_text(previous, next)
        && let Some(Value::String(text)) = previous.get_mut("text")
    {
        text.push_str(next["text"].as_str().unwrap_or_default());
        return;
    }
    chunks.push(chunk);
}

/// A tool update as the patch rule applies it (rule 2): its fields but
/// `null`, and for a `tool_call` the schema's defaults of the fields it
/// omits (`ToolCallUpdate::from`). Content blocks become log content.
fn tool_patch(update: &Value, announce: bool, attachments: &mut Attachments) -> Map<String, Value> {
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
    if let Some(contents) = patch.get_mut("content").and_then(Value::as_array_mut) {
        for content in contents {
            if content["type"] == "content"
                && let Some(block) = content.get_mut("content")
                && let Some(normalized) = attachments.block(block)
            {
                *block = serde_json::to_value(normalized).expect("blocks serialize");
            }
        }
    }
    patch
}

/// Reduces session updates into the turn being built and, for an entity
/// already in the log, into its entry.
struct Reducer<'a> {
    log: &'a mut TurnLog,
    /// The turn being reduced; it becomes entry `log.entries.len()`.
    items: Vec<Item>,
    /// The message item the next chunk may join.
    run: Option<usize>,
    /// The turn's legacy plan snapshot.
    legacy_plan: Option<usize>,
    /// A replay starts a turn at each user message (protocol §3.3).
    replay: bool,
    /// The `messageId` of the user message that started the current turn.
    opener: Option<Option<String>>,
    /// The first settled entry an update rewrote.
    rewritten: Option<usize>,
}

impl<'a> Reducer<'a> {
    fn new(log: &'a mut TurnLog, replay: bool) -> Self {
        Self {
            log,
            items: Vec::new(),
            run: None,
            legacy_plan: None,
            replay,
            opener: None,
            rewritten: None,
        }
    }

    fn push(&mut self, item: Item) -> Place {
        self.items.push(item);
        self.run = None;
        (self.log.entries.len(), self.items.len() - 1)
    }

    fn item_mut(&mut self, (turn, index): Place) -> &mut Item {
        if turn == self.log.entries.len() {
            return &mut self.items[index];
        }
        self.rewritten = Some(self.rewritten.map_or(turn, |first| first.min(turn)));
        &mut Arc::make_mut(&mut self.log.entries[turn]).turn.items[index]
    }

    fn reduce(&mut self, update: &Value) {
        let Some(kind) = update.get("sessionUpdate").and_then(Value::as_str) else {
            return;
        };
        match kind {
            "user_message_chunk" => self.chunk(Role::User, update),
            "agent_message_chunk" => self.chunk(Role::Agent, update),
            "agent_thought_chunk" => self.chunk(Role::Thought, update),
            "tool_call" => self.tool(update, true),
            "tool_call_update" => self.tool(update, false),
            "plan" => self.legacy_plan(update),
            "plan_update" => {
                if let Some(plan_id) = update["plan"]["planId"].as_str() {
                    self.plan(plan_id, update);
                }
            }
            "plan_removed" => {
                if let Some(plan_id) = update["planId"].as_str() {
                    self.plan(plan_id, update);
                }
            }
            "compaction_update" => self.compaction(update),
            "compaction_summary_chunk" => self.summary_chunk(update),
            // Control updates are not items (§4.5).
            _ => {}
        }
    }

    fn chunk(&mut self, role: Role, update: &Value) {
        let Some(content) = update
            .get("content")
            .and_then(|block| self.log.attachments.block(block))
        else {
            return;
        };
        let message_id = update
            .get("messageId")
            .and_then(Value::as_str)
            .map(String::from);
        let chunk = Chunk {
            content,
            meta: meta(update),
        };
        if let Some(index) = self.run
            && let Item::Message(message) = &mut self.items[index]
            && message.role == role
            && message.message_id == message_id
        {
            join(&mut message.chunks, chunk);
            return;
        }
        if self.replay && role == Role::User {
            let continues = matches!(
                (&self.opener, &message_id),
                (Some(Some(opener)), Some(id)) if opener == id
            );
            if !continues {
                self.flush();
                self.opener = Some(message_id.clone());
            }
        }
        let (_, index) = self.push(Item::Message(MessageItem {
            role,
            message_id,
            chunks: vec![chunk],
        }));
        self.run = Some(index);
    }

    fn tool(&mut self, update: &Value, announce: bool) {
        let Some(tool_call_id) = update.get("toolCallId").and_then(Value::as_str) else {
            return;
        };
        let patch = tool_patch(update, announce, &mut self.log.attachments);
        let entity = Entity::Tool(tool_call_id.to_owned());
        match self.log.places.get(&entity).copied() {
            Some(place) => {
                if let Item::Tool(tool) = self.item_mut(place)
                    && let Some(call) = tool.call.as_object_mut()
                {
                    call.extend(patch);
                }
            }
            None => {
                let place = self.push(Item::Tool(ToolItem {
                    call: Value::Object(patch),
                }));
                self.log.places.insert(entity, place);
            }
        }
    }

    fn legacy_plan(&mut self, update: &Value) {
        let state = untagged(update);
        match self.legacy_plan {
            Some(index) => {
                if let Item::Plan(plan) = &mut self.items[index] {
                    plan.update = state;
                }
            }
            None => {
                let (_, index) = self.push(Item::Plan(PlanItem { update: state }));
                self.legacy_plan = Some(index);
            }
        }
    }

    fn plan(&mut self, plan_id: &str, update: &Value) {
        let state = untagged(update);
        let entity = Entity::Plan(plan_id.to_owned());
        match self.log.places.get(&entity).copied() {
            Some(place) => {
                if let Item::Plan(plan) = self.item_mut(place) {
                    plan.update = state;
                }
            }
            None => {
                let place = self.push(Item::Plan(PlanItem { update: state }));
                self.log.places.insert(entity, place);
            }
        }
    }

    fn compaction(&mut self, update: &Value) {
        let Some(compaction_id) = update.get("compactionId").and_then(Value::as_str) else {
            return;
        };
        // `summary`, `error` and `_meta`: absent keeps, `null` clears, a value
        // replaces.
        let summary = match update.get("summary") {
            Some(Value::Array(blocks)) => Some(
                blocks
                    .iter()
                    .filter_map(|block| self.log.attachments.block(block))
                    .map(|content| Chunk {
                        content,
                        meta: None,
                    })
                    .collect(),
            ),
            Some(Value::Null) => Some(Vec::new()),
            _ => None,
        };
        let entity = Entity::Compaction(compaction_id.to_owned());
        let place = match self.log.places.get(&entity).copied() {
            Some(place) => place,
            None => {
                let place = self.push(Item::Compaction(CompactionItem {
                    compaction_id: compaction_id.to_owned(),
                    status: update["status"].clone(),
                    summary: Vec::new(),
                    error: None,
                    meta: None,
                }));
                self.log.places.insert(entity, place);
                place
            }
        };
        let Item::Compaction(item) = self.item_mut(place) else {
            return;
        };
        if let Some(status) = update.get("status") {
            item.status = status.clone();
        }
        if let Some(summary) = summary {
            item.summary = summary;
        }
        if let Some(error) = update.get("error") {
            item.error = error.as_str().map(String::from);
        }
        if let Some(value) = update.get("_meta") {
            item.meta = (!value.is_null()).then(|| value.clone());
        }
    }

    fn summary_chunk(&mut self, update: &Value) {
        let Some(compaction_id) = update.get("compactionId").and_then(Value::as_str) else {
            return;
        };
        let entity = Entity::Compaction(compaction_id.to_owned());
        let Some(place) = self.log.places.get(&entity).copied() else {
            return;
        };
        let Some(content) = update
            .get("content")
            .and_then(|block| self.log.attachments.block(block))
        else {
            return;
        };
        let chunk = Chunk {
            content,
            meta: meta(update),
        };
        if let Item::Compaction(item) = self.item_mut(place) {
            join(&mut item.summary, chunk);
        }
    }

    /// Replay: the current turn, if it has items, becomes an entry.
    fn flush(&mut self) {
        if !self.items.is_empty() {
            self.settle(None);
        }
    }

    /// The turn being reduced becomes the log's next entry; the chain is
    /// recomputed from the first entry that changed.
    fn settle(&mut self, end: Option<TurnEnd>) {
        let index = self.log.entries.len();
        self.log.entries.push(Arc::new(LogEntry {
            turn: Turn {
                items: std::mem::take(&mut self.items),
                end,
            },
            hash: String::new(),
            digest: ChainHash::ZERO,
        }));
        let from = self.rewritten.take().map_or(index, |first| first.min(index));
        self.log.rehash(from);
        self.run = None;
        self.legacy_plan = None;
    }
}

// ---------------------------------------------------------------------------
// JCS (RFC 8785)

/// RFC 8785 (JCS) serialization.
pub(crate) fn canonical_json(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(value) => {
            out.extend_from_slice(if *value { "true" } else { "false" }.as_bytes())
        }
        Value::Number(number) => out.extend_from_slice(canonical_number(number).as_bytes()),
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        Value::Object(map) => {
            // Keys are ordered by their UTF-16 code units.
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.encode_utf16().cmp(right.encode_utf16()));
            out.push(b'{');
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_string(key, out);
                out.push(b':');
                write_canonical(value, out);
            }
            out.push(b'}');
        }
    }
}

fn write_string(text: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for c in text.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\u{8}' => out.extend_from_slice(b"\\b"),
            '\u{c}' => out.extend_from_slice(b"\\f"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            c if c < ' ' => out.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes()),
            c => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
        }
    }
    out.push(b'"');
}

/// A number as ECMAScript's `Number.prototype.toString` writes the double.
fn canonical_number(number: &serde_json::Number) -> String {
    const EXACT: i64 = 1 << 53;
    if let Some(integer) = number.as_i64()
        && (-EXACT..=EXACT).contains(&integer)
    {
        return integer.to_string();
    }
    let value = number.as_f64().expect("JSON numbers are finite");
    if value == 0.0 {
        return "0".into();
    }
    let sign = if value < 0.0 { "-" } else { "" };
    let (digits, exponent) = ecmascript_digits(value.abs());
    let k = digits.len() as i32;
    let n = exponent + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        let (whole, fraction) = digits.split_at(n as usize);
        format!("{whole}.{fraction}")
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat(-n as usize))
    } else {
        let exponent = n - 1;
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        let (first, rest) = digits.split_at(1);
        let fraction = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        format!("{first}{fraction}e{exponent_sign}{}", exponent.abs())
    };
    format!("{sign}{body}")
}

/// ECMAScript's digits for a positive double, with the exponent of the first:
/// the fewest that round-trip and, among those, the closest to the exact value,
/// an exact tie going to the even digit.
fn ecmascript_digits(value: f64) -> (String, i32) {
    // Rust's shortest digits round-trip, but on a tie may not be the closest.
    let shortest = scientific_digits(&format!("{value:e}"));
    // The exact expansion; a double has at most 767 significant digits.
    let (exact, exponent) = scientific_digits(&format!("{value:.800e}"));
    let closest = round_half_even(&exact, exponent, shortest.0.len());
    let round_trips = format!("0.{}e{}", closest.0, closest.1 + 1).parse::<f64>() == Ok(value);
    if round_trips { closest } else { shortest }
}

/// Splits `d.ddde<exponent>` into its digits and exponent.
fn scientific_digits(scientific: &str) -> (String, i32) {
    let (mantissa, exponent) = scientific.split_once('e').expect("exponent form");
    let digits = mantissa.chars().filter(|c| *c != '.').collect();
    (digits, exponent.parse().expect("exponent"))
}

/// Rounds a digit string to `count` digits, half to even.
fn round_half_even(digits: &str, exponent: i32, count: usize) -> (String, i32) {
    let (head, tail) = digits.split_at(count.min(digits.len()));
    let mut head = head.as_bytes().to_vec();
    let tail = tail.trim_end_matches('0').as_bytes();
    let round_up = match tail {
        [] => false,
        [b'5'] => head.last().is_some_and(|digit| (digit - b'0') % 2 == 1),
        [first, ..] => *first >= b'5',
    };
    let mut exponent = exponent;
    if round_up {
        match head.iter().rposition(|digit| *digit != b'9') {
            Some(index) => {
                head[index] += 1;
                head[index + 1..].fill(b'0');
            }
            None => {
                // 99…9 becomes 10…0 with one more integer digit.
                head.fill(b'0');
                head[0] = b'1';
                exponent += 1;
            }
        }
    }
    (String::from_utf8(head).expect("ASCII digits"), exponent)
}

// ---------------------------------------------------------------------------
// Hash chain (protocol §3.4)

/// A full chain hash; only its first 16 bytes go on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ChainHash([u8; 32]);

impl ChainHash {
    /// `h_0`.
    pub(crate) const ZERO: Self = Self([0; 32]);

    pub(crate) fn wire(&self) -> String {
        URL_SAFE_NO_PAD.encode(&self.0[..16])
    }
}

/// `h_i = SHA-256(h_{i-1} ‖ JCS(turn_i))`.
pub(crate) fn chain(prev: &ChainHash, turn: &Turn) -> ChainHash {
    let turn = serde_json::to_value(turn).expect("turns serialize");
    let mut hasher = Sha256::new();
    hasher.update(prev.0);
    hasher.update(canonical_json(&turn));
    ChainHash(hasher.finalize().into())
}

// ---------------------------------------------------------------------------
// Turn log (docs/state-sync.md §2.1, §7.1)

/// A session's turns, appended by live settles and rebuilt from a load's
/// replay. It keeps the attachments its entries reference and the item of
/// each entity of the session.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TurnLog {
    entries: Vec<Arc<LogEntry>>,
    attachments: Attachments,
    places: HashMap<Entity, Place>,
}

impl TurnLog {
    /// Builds every turn of a `session/load` or `session/resume` replay.
    pub(crate) fn from_replay(updates: &[Value]) -> Self {
        let mut log = Self::default();
        let mut reducer = Reducer::new(&mut log, true);
        for update in updates {
            reducer.reduce(update);
        }
        reducer.flush();
        log
    }

    /// Settles a prompt's turn: `prompt` is what the bridge sent in
    /// `session/prompt` and is the turn's first item, `updates` are the
    /// turn's session updates and `response` the prompt's response. An update
    /// for an entity of an earlier entry rewrites that entry.
    pub(crate) fn commit(&mut self, prompt: &[Value], updates: &[Value], response: &Value) {
        let chunks = prompt
            .iter()
            .filter_map(|block| self.attachments.block(block))
            .map(|content| Chunk {
                content,
                meta: None,
            })
            .collect();
        let mut reducer = Reducer::new(self, false);
        reducer.push(Item::Message(MessageItem {
            role: Role::User,
            message_id: None,
            chunks,
        }));
        for update in updates {
            reducer.reduce(update);
        }
        reducer.settle(Some(TurnEnd {
            response: response.clone(),
        }));
    }

    /// Recomputes the chain from entry `from` on.
    fn rehash(&mut self, from: usize) {
        let mut prev = match from {
            0 => ChainHash::ZERO,
            from => self.entries[from - 1].digest,
        };
        for entry in &mut self.entries[from..] {
            let digest = chain(&prev, &entry.turn);
            if digest != entry.digest || entry.hash.is_empty() {
                let entry = Arc::make_mut(entry);
                entry.digest = digest;
                entry.hash = digest.wire();
            }
            prev = digest;
        }
    }

    pub(crate) fn entries(&self) -> &[Arc<LogEntry>] {
        &self.entries
    }

    /// The last entry's wire hash.
    pub(crate) fn head(&self) -> Option<&str> {
        self.entries.last().map(|entry| entry.hash.as_str())
    }

    pub(crate) fn attachments(&self) -> &Attachments {
        &self.attachments
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const IMAGE_ID: &str = "c44aabab6f5fe238799641da5a1ac8d3df1e546078398e3b727c2e7bc6806319";
    /// `image()` with `with_meta`'s `_meta`: the ID covers the whole block.
    const META_IMAGE_ID: &str = "4d4ede7a023c5ac14fa5f3488386a56e709560cf8ab2b79a35465dad9625d3b6";

    fn text(text: &str) -> Value {
        json!({ "type": "text", "text": text })
    }

    fn image() -> Value {
        json!({ "type": "image", "mimeType": "image/png", "data": "aGk=" })
    }

    fn chunk(kind: &str, content: Value, message_id: Option<&str>) -> Value {
        let mut update = json!({ "sessionUpdate": kind, "content": content });
        if let Some(id) = message_id {
            update["messageId"] = json!(id);
        }
        update
    }

    fn user(content: Value) -> Value {
        chunk("user_message_chunk", content, None)
    }

    fn agent(content: Value) -> Value {
        chunk("agent_message_chunk", content, None)
    }

    fn agent_in(message_id: &str, content: Value) -> Value {
        chunk("agent_message_chunk", content, Some(message_id))
    }

    fn tool_call(id: &str) -> Value {
        json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": "read" })
    }

    fn tool_update(fields: Value) -> Value {
        let mut update = json!({ "sessionUpdate": "tool_call_update" });
        update
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        update
    }

    fn thought(text_: &str) -> Value {
        json!({ "sessionUpdate": "agent_thought_chunk", "content": text(text_) })
    }

    fn usage() -> Value {
        json!({ "sessionUpdate": "usage_update", "used": 1, "size": 10 })
    }

    fn with_meta(mut value: Value) -> Value {
        value["_meta"] = json!({ "attyd": { "chunk": 1 } });
        value
    }

    fn end_turn() -> Value {
        json!({ "stopReason": "end_turn" })
    }

    fn turn(log: &TurnLog, index: usize) -> Value {
        serde_json::to_value(&log.entries()[index].turn).unwrap()
    }

    fn items(log: &TurnLog, index: usize) -> Value {
        turn(log, index)["items"].clone()
    }

    fn message(role: &str, chunks: Value) -> Value {
        json!({ "type": "message", "role": role, "chunks": chunks })
    }

    fn message_in(role: &str, message_id: &str, chunks: Value) -> Value {
        json!({ "type": "message", "role": role, "messageId": message_id, "chunks": chunks })
    }

    fn texts(parts: &[&str]) -> Value {
        Value::Array(parts.iter().map(|part| json!({ "content": text(part) })).collect())
    }

    #[test]
    fn canonical_json_follows_rfc_8785() {
        let value = json!({
            "b": [1.0, 1e21, 1e-7, 0.000001, -0.0, 123.456, 10, -3],
            "a": "\u{1f}\u{2028}/\"\\\n",
            "\u{e000}": 1,
            "\u{1f600}": 2,
            "nested": { "z": null, "y": [true, false] },
        });
        assert_eq!(
            String::from_utf8(canonical_json(&value)).unwrap(),
            "{\"a\":\"\\u001f\u{2028}/\\\"\\\\\\n\",\
             \"b\":[1,1e+21,1e-7,0.000001,0,123.456,10,-3],\
             \"nested\":{\"y\":[true,false],\"z\":null},\
             \"\u{1f600}\":2,\"\u{e000}\":1}"
        );
    }

    // RFC 8785 appendix B (finite values), and a valid `annotations.priority`
    // where the shortest digits end on a tie.
    #[test]
    fn canonical_numbers_follow_ecmascript() {
        let vectors = [
            (0x0000000000000000, "0"),
            (0x8000000000000000, "0"),
            (0x0000000000000001, "5e-324"),
            (0x8000000000000001, "-5e-324"),
            (0x7fefffffffffffff, "1.7976931348623157e+308"),
            (0xffefffffffffffff, "-1.7976931348623157e+308"),
            (0x4340000000000000, "9007199254740992"),
            (0xc340000000000000, "-9007199254740992"),
            (0x4430000000000000, "295147905179352830000"),
            (0x44b52d02c7e14af5, "9.999999999999997e+22"),
            (0x44b52d02c7e14af6, "1e+23"),
            (0x44b52d02c7e14af7, "1.0000000000000001e+23"),
            (0x444b1ae4d6e2ef4e, "999999999999999700000"),
            (0x444b1ae4d6e2ef4f, "999999999999999900000"),
            (0x444b1ae4d6e2ef50, "1e+21"),
            (0x3eb0c6f7a0b5ed8c, "9.999999999999997e-7"),
            (0x3eb0c6f7a0b5ed8d, "0.000001"),
            (0x41b3de4355555553, "333333333.3333332"),
            (0x41b3de4355555554, "333333333.33333325"),
            (0x41b3de4355555555, "333333333.3333333"),
            (0x41b3de4355555556, "333333333.3333334"),
            (0x41b3de4355555557, "333333333.33333343"),
            (0xbecbf647612f3696, "-0.0000033333333333333333"),
            (0x43143ff3c1cb0959, "1424953923781206.2"),
        ];
        for (bits, expected) in vectors {
            let value = json!(f64::from_bits(bits));
            assert_eq!(
                String::from_utf8(canonical_json(&value)).unwrap(),
                expected,
                "{bits:#018x}"
            );
        }
        assert_eq!(
            String::from_utf8(canonical_json(&json!({
                "annotations": { "priority": f64::from_bits(0x3e60000000000000) }
            })))
            .unwrap(),
            "{\"annotations\":{\"priority\":2.9802322387695312e-8}}"
        );
    }

    #[test]
    fn hash_chain_starts_at_zero_and_uses_short_base64url() {
        let mut log = TurnLog::default();
        log.commit(&[text("hi")], &[], &end_turn());
        assert_eq!(log.head(), Some("ntJue2Uc12DDlAZjgJbrag"));
        log.commit(&[text("hi")], &[agent(text("hello"))], &end_turn());
        assert_eq!(log.head(), Some("XeXvyB_1rdBJA19ktw-M7Q"));
        assert_eq!(ChainHash::ZERO.wire(), "AAAAAAAAAAAAAAAAAAAAAA");
    }

    // Acceptance 1, 19: chunks of one message join while nothing comes
    // between them; an item between them, or another `messageId`, starts a
    // new item with its own ID. Control updates are no items.
    #[test]
    fn messages_keep_their_ids_and_boundaries() {
        let log = TurnLog::from_replay(&[
            user(text("q")),
            agent_in("m1", text("a")),
            usage(),
            agent_in("m1", text("b")),
            tool_call("t"),
            agent_in("m1", text("c")),
            agent_in("m2", text("d")),
            thought("hmm"),
            agent(text("e")),
            agent(text("f")),
        ]);
        assert_eq!(
            items(&log, 0),
            json!([
                message("user", texts(&["q"])),
                message_in("agent", "m1", texts(&["ab"])),
                { "type": "tool", "call": {
                    "toolCallId": "t", "title": "read", "kind": "other", "status": "pending",
                    "content": [], "locations": [],
                }},
                message_in("agent", "m1", texts(&["c"])),
                message_in("agent", "m2", texts(&["d"])),
                message("thought", texts(&["hmm"])),
                message("agent", texts(&["ef"])),
            ])
        );
    }

    // Acceptance 1, 25: text joins only when every other field agrees,
    // `_meta` of the chunk and of the block included; blocks keep their
    // annotations and `_meta`.
    #[test]
    fn chunks_join_only_when_their_other_fields_agree() {
        let annotated = |text: &str, audience: &str| json!({ "type": "text", "text": text, "annotations": { "audience": [audience] } });
        let log = TurnLog::from_replay(&[
            user(text("q")),
            agent(annotated("a", "user")),
            agent(annotated("b", "user")),
            agent(annotated("c", "assistant")),
            with_meta(agent(text("d"))),
            agent(text("e")),
            agent(with_meta(text("f"))),
            agent(json!({ "type": "resource_link", "uri": "file:///y", "name": "y" })),
        ]);
        assert_eq!(
            items(&log, 0)[1]["chunks"],
            json!([
                { "content": annotated("ab", "user") },
                { "content": annotated("c", "assistant") },
                { "content": text("d"), "_meta": { "attyd": { "chunk": 1 } } },
                { "content": text("e") },
                { "content": with_meta(text("f")) },
                { "content": { "type": "resource_link", "uri": "file:///y", "name": "y" } },
            ])
        );
    }

    // Acceptance 1: the same updates give the same items however they were
    // chunked.
    #[test]
    fn differently_chunked_updates_give_the_same_entries() {
        let whole = TurnLog::from_replay(&[
            chunk("user_message_chunk", text("hello world"), Some("u1")),
            agent_in("a1", text("foo bar")),
            tool_call("t1"),
            chunk("user_message_chunk", text("again"), Some("u2")),
            agent(text("done")),
        ]);
        let chunked = TurnLog::from_replay(&[
            chunk("user_message_chunk", text("hel"), Some("u1")),
            chunk("user_message_chunk", text("lo world"), Some("u1")),
            agent_in("a1", text("foo ")),
            usage(),
            agent_in("a1", text("bar")),
            tool_call("t1"),
            chunk("user_message_chunk", text("aga"), Some("u2")),
            chunk("user_message_chunk", text("in"), Some("u2")),
            agent(text("do")),
            agent(text("ne")),
        ]);
        assert_eq!(whole.entries().len(), 2);
        assert_eq!(chunked, whole);
    }

    // Acceptance 1: equal entries have equal hashes; a tool result, a
    // `messageId`, a boundary, a chunk's `_meta` or the presence of `end`
    // changes the hash.
    #[test]
    fn the_hash_covers_the_whole_entry() {
        let replay = |updates: &[Value]| {
            let mut all = vec![user(text("q"))];
            all.extend_from_slice(updates);
            TurnLog::from_replay(&all).head().unwrap().to_owned()
        };
        let base = replay(&[agent_in("m", text("ab")), tool_call("t")]);
        assert_eq!(
            base,
            replay(&[agent_in("m", text("a")), agent_in("m", text("b")), tool_call("t")])
        );
        for (what, other) in [
            (
                "a tool result",
                replay(&[
                    agent_in("m", text("ab")),
                    tool_call("t"),
                    tool_update(json!({ "toolCallId": "t", "rawOutput": 1 })),
                ]),
            ),
            (
                "a messageId",
                replay(&[agent_in("n", text("ab")), tool_call("t")]),
            ),
            (
                "a boundary",
                replay(&[agent_in("m", text("a")), tool_call("t"), agent_in("m", text("b"))]),
            ),
            (
                "a chunk's _meta",
                replay(&[with_meta(agent_in("m", text("ab"))), tool_call("t")]),
            ),
        ] {
            assert_ne!(base, other, "{what}");
        }
        let mut settled = TurnLog::default();
        settled.commit(&[text("q")], &[agent_in("m", text("ab")), tool_call("t")], &end_turn());
        assert_ne!(settled.head().unwrap(), base, "the presence of end");
        let mut again = TurnLog::default();
        again.commit(
            &[text("q")],
            &[agent_in("m", text("a")), agent_in("m", text("b")), tool_call("t")],
            &end_turn(),
        );
        assert_eq!(again, settled);
    }

    // Acceptance 20, 36: the tool state is a sparse `ToolCallUpdate` patched
    // by the field rule; a `tool_call` gives its fields with the schema's
    // defaults.
    #[test]
    fn tool_updates_patch_a_sparse_state() {
        let log = TurnLog::from_replay(&[
            user(text("q")),
            tool_update(json!({ "toolCallId": "u", "status": "pending" })),
            tool_update(json!({ "toolCallId": "u", "title": "Read", "kind": "read" })),
            tool_update(json!({
                "toolCallId": "u", "content": [{ "type": "content", "content": text("x") }],
                "locations": [{ "path": "/a" }], "rawInput": { "n": 1 }, "rawOutput": "out",
                "_meta": { "m": 1 },
            })),
            // `null` and absence keep.
            tool_update(json!({ "toolCallId": "u", "name": null, "content": null, "rawOutput": null })),
            // A collection replaces whole; `[]` clears.
            tool_update(json!({ "toolCallId": "u", "content": [], "locations": [{ "path": "/b" }] })),
            json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "Run", "status": "in_progress" }),
            tool_update(json!({ "toolCallId": "t", "status": "completed" })),
            // A later announcement gives its fields and its defaults.
            json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "Run again" }),
        ]);
        assert_eq!(
            items(&log, 0),
            json!([
                message("user", texts(&["q"])),
                { "type": "tool", "call": {
                    "toolCallId": "u", "status": "pending", "title": "Read", "kind": "read",
                    "content": [], "locations": [{ "path": "/b" }], "rawInput": { "n": 1 },
                    "rawOutput": "out", "_meta": { "m": 1 },
                }},
                { "type": "tool", "call": {
                    "toolCallId": "t", "title": "Run again", "kind": "other", "status": "pending",
                    "content": [], "locations": [],
                }},
            ])
        );
    }

    // Acceptance 20: `summary`, `error` and `_meta` of a compaction are
    // three-state; summary chunks keep their own `_meta`.
    #[test]
    fn compactions_patch_three_state_fields() {
        let update = |fields: Value| {
            let mut update = json!({ "sessionUpdate": "compaction_update", "compactionId": "c" });
            update
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            update
        };
        let summary_chunk = |text_: &str, meta: Value| {
            json!({ "sessionUpdate": "compaction_summary_chunk", "compactionId": "c", "content": text(text_), "_meta": meta })
        };
        let at = |updates: &[Value]| {
            let mut all = vec![user(text("q"))];
            all.extend_from_slice(updates);
            items(&TurnLog::from_replay(&all), 0)[1].clone()
        };
        let started = [
            update(json!({ "status": "in_progress", "_meta": { "c": 1 } })),
            summary_chunk("a", json!({ "s": 1 })),
            summary_chunk("b", json!({ "s": 1 })),
            summary_chunk("c", json!({ "s": 2 })),
        ];
        assert_eq!(
            at(&started),
            json!({
                "type": "compaction", "compactionId": "c", "status": "in_progress",
                "summary": [
                    { "content": text("ab"), "_meta": { "s": 1 } },
                    { "content": text("c"), "_meta": { "s": 2 } },
                ],
                "_meta": { "c": 1 },
            })
        );
        let mut kept = started.to_vec();
        kept.push(update(json!({ "status": "failed", "error": "boom" })));
        assert_eq!(
            at(&kept),
            json!({
                "type": "compaction", "compactionId": "c", "status": "failed",
                "summary": [
                    { "content": text("ab"), "_meta": { "s": 1 } },
                    { "content": text("c"), "_meta": { "s": 2 } },
                ],
                "error": "boom", "_meta": { "c": 1 },
            })
        );
        let mut cleared = kept.clone();
        cleared.push(update(json!({ "status": "completed", "summary": null, "error": null, "_meta": null })));
        assert_eq!(
            at(&cleared),
            json!({ "type": "compaction", "compactionId": "c", "status": "completed", "summary": [] })
        );
        let mut replaced = cleared.clone();
        replaced.push(update(json!({ "status": "completed", "summary": [text("x"), text("y")] })));
        assert_eq!(
            at(&replaced)["summary"],
            json!([{ "content": text("x") }, { "content": text("y") }])
        );
    }

    // Acceptance 30: a replay starts a turn at each user message, except one
    // that continues the message that started the turn.
    #[test]
    fn a_replay_starts_turns_at_user_messages() {
        let log = TurnLog::from_replay(&[
            agent(text("welcome")),
            chunk("user_message_chunk", text("look"), Some("u1")),
            chunk("user_message_chunk", image(), Some("u1")),
            tool_call("t"),
            chunk("user_message_chunk", text("more"), Some("u1")),
            chunk("user_message_chunk", text("q2"), Some("u2")),
            user(text("q3")),
            agent(text("a")),
            user(text("q4")),
        ]);
        let turns: Vec<_> = (0..log.entries().len()).map(|index| turn(&log, index)).collect();
        let picture = json!({ "type": "attachment", "id": IMAGE_ID, "block": "image", "mimeType": "image/png" });
        assert_eq!(
            turns,
            [
                json!({ "items": [message("agent", texts(&["welcome"]))] }),
                json!({ "items": [
                    message_in("user", "u1", json!([{ "content": text("look") }, { "content": picture }])),
                    { "type": "tool", "call": {
                        "toolCallId": "t", "title": "read", "kind": "other", "status": "pending",
                        "content": [], "locations": [],
                    }},
                    message_in("user", "u1", texts(&["more"])),
                ]}),
                json!({ "items": [message_in("user", "u2", texts(&["q2"]))] }),
                json!({ "items": [message("user", texts(&["q3"])), message("agent", texts(&["a"]))] }),
                json!({ "items": [message("user", texts(&["q4"]))] }),
            ]
        );
    }

    // Acceptance 26, 30: live, the prompt is the turn's first item, one chunk
    // per block; user chunks the Agent sends are items of the turn.
    #[test]
    fn a_live_turn_starts_with_the_prompt_and_keeps_user_chunks() {
        let mut log = TurnLog::default();
        log.commit(
            &[text("look"), text(" here"), image()],
            &[user(text("look")), agent(text("ok")), user(text("more"))],
            &json!({ "stopReason": "end_turn", "_meta": { "r": 1 } }),
        );
        assert_eq!(log.entries().len(), 1);
        let picture = json!({ "type": "attachment", "id": IMAGE_ID, "block": "image", "mimeType": "image/png" });
        assert_eq!(
            turn(&log, 0),
            json!({
                "items": [
                    message("user", json!([
                        { "content": text("look") }, { "content": text(" here") }, { "content": picture },
                    ])),
                    message("user", texts(&["look"])),
                    message("agent", texts(&["ok"])),
                    message("user", texts(&["more"])),
                ],
                "end": { "response": { "stopReason": "end_turn", "_meta": { "r": 1 } } },
            })
        );
        assert_eq!(log.attachments().get(IMAGE_ID), Some(&image()));
    }

    // Acceptance 1, 27: a replay of a live turn has no `end`, so it differs
    // from the live entry; it holds only what it provided.
    #[test]
    fn a_replay_differs_from_the_live_entry_by_its_end() {
        let mut live = TurnLog::default();
        live.commit(&[text("q")], &[agent(text("a")), tool_call("t")], &end_turn());
        let replayed = TurnLog::from_replay(&[user(text("q")), agent(text("a")), tool_call("t")]);
        assert_eq!(items(&replayed, 0), items(&live, 0));
        assert_eq!(turn(&replayed, 0).get("end"), None);
        assert_ne!(replayed.head(), live.head());
    }

    // Acceptance 31: one item per `planId` in the session, at its first
    // position; a legacy plan is a snapshot per turn.
    #[test]
    fn plans_keep_their_identity_in_the_session() {
        let plan = |id: &str, content: &str| {
            json!({ "sessionUpdate": "plan_update", "plan": {
                "type": "items", "planId": id,
                "entries": [{ "content": content, "priority": "low", "status": "pending" }],
            }})
        };
        let legacy = |content: &str| {
            json!({ "sessionUpdate": "plan", "entries": [{ "content": content, "priority": "low", "status": "pending" }] })
        };
        let updates = [
            user(text("a")),
            plan("P", "p1"),
            plan("Q", "q1"),
            legacy("l1"),
            legacy("l2"),
            user(text("b")),
            plan("P", "p2"),
            json!({ "sessionUpdate": "plan_removed", "planId": "P" }),
            legacy("l3"),
            json!({ "sessionUpdate": "plan_removed", "planId": "R" }),
        ];
        let state = |update: &Value| untagged(update);
        let replayed = TurnLog::from_replay(&updates);
        assert_eq!(
            items(&replayed, 0),
            json!([
                message("user", texts(&["a"])),
                { "type": "plan", "update": { "planId": "P" } },
                { "type": "plan", "update": state(&plan("Q", "q1")) },
                { "type": "plan", "update": state(&legacy("l2")) },
            ])
        );
        assert_eq!(
            items(&replayed, 1),
            json!([
                message("user", texts(&["b"])),
                { "type": "plan", "update": state(&legacy("l3")) },
                { "type": "plan", "update": { "planId": "R" } },
            ])
        );

        // Live, the second turn's updates rewrite the first entry.
        let mut live = TurnLog::default();
        live.commit(&[text("a")], &updates[1..5], &end_turn());
        let first = live.entries()[0].hash.clone();
        live.commit(&[text("b")], &updates[6..], &end_turn());
        assert_eq!(items(&live, 0), items(&replayed, 0));
        assert_eq!(items(&live, 1), items(&replayed, 1));
        assert_ne!(live.entries()[0].hash, first, "the first entry was rewritten");
        let mut chain = TurnLog::default();
        chain.entries = live.entries().to_vec();
        chain.rehash(0);
        assert_eq!(
            chain.entries().iter().map(|entry| &entry.hash).collect::<Vec<_>>(),
            live.entries().iter().map(|entry| &entry.hash).collect::<Vec<_>>(),
            "the chain is recomputed from the rewritten entry"
        );
    }

    // Acceptance 13, 25: an attachment's ID covers the whole block, and the
    // block is kept as received.
    #[test]
    fn attachments_keep_the_block_as_received() {
        let mut attachments = Attachments::default();
        let resource = json!({
            "type": "resource",
            "resource": { "uri": "file:///x.txt", "mimeType": "text/plain", "text": "t", "_meta": { "x": 1 } },
            "annotations": { "priority": 1 },
        });
        let blocks = normalize(&[image(), with_meta(image()), resource.clone(), json!({ "type": "unknown" })], &mut attachments);
        assert_eq!(blocks.len(), 3);
        let Block::Attachment(plain) = &blocks[0] else { panic!("an image is referenced") };
        let Block::Attachment(meta) = &blocks[1] else { panic!("an image is referenced") };
        let Block::Attachment(kept) = &blocks[2] else { panic!("a resource is referenced") };
        assert_eq!(plain.id, IMAGE_ID);
        assert_eq!(meta.id, META_IMAGE_ID);
        assert_eq!(attachments.get(META_IMAGE_ID), Some(&with_meta(image())));
        assert_eq!(attachments.get(&kept.id), Some(&resource));
        assert_eq!(kept.uri.as_deref(), Some("file:///x.txt"));
        assert_eq!(kept.mime_type.as_deref(), Some("text/plain"));
    }

    // Acceptance 1: prompts that reference attachments are resolved before the
    // bridge calls the Agent, to the blocks as received.
    #[test]
    fn prompt_references_resolve_to_the_blocks_they_name() {
        let mut log = TurnLog::default();
        log.commit(&[text("look"), with_meta(image())], &[agent(text("a cat"))], &end_turn());
        let reference = items(&log, 0)[0]["chunks"][1]["content"].clone();
        assert_eq!(
            reference,
            json!({ "type": "attachment", "id": META_IMAGE_ID, "block": "image", "mimeType": "image/png" })
        );

        let prompt: Vec<PromptBlock> =
            serde_json::from_value(json!([{ "type": "text", "text": "again" }, reference])).unwrap();
        let resolved = resolve(&prompt, log.attachments()).unwrap();
        assert_eq!(resolved, vec![text("again"), with_meta(image())]);

        let unknown: Vec<PromptBlock> = serde_json::from_value(json!([
            { "type": "attachment", "id": "0".repeat(64), "block": "image" },
        ]))
        .unwrap();
        assert_eq!(
            resolve(&unknown, log.attachments()),
            Err(UnknownAttachment("0".repeat(64)))
        );
    }

    // Mirrored by tests/fixtures/protocol-canon.ts, which checks it against the
    // generated types.
    #[test]
    fn entries_use_the_documented_wire_shape() {
        let mut log = TurnLog::default();
        log.commit(
            &[text("look"), image()],
            &[
                agent_in("m", text("I see")),
                agent_in("m", json!({ "type": "resource_link", "uri": "file:///a", "name": "a" })),
                tool_update(json!({ "toolCallId": "t", "status": "pending" })),
                json!({ "sessionUpdate": "plan_removed", "planId": "p" }),
                json!({ "sessionUpdate": "compaction_update", "compactionId": "c", "status": "completed" }),
            ],
            &end_turn(),
        );
        assert_eq!(
            serde_json::to_value(&*log.entries()[0]).unwrap(),
            json!({
                "turn": {
                    "items": [
                        { "type": "message", "role": "user", "chunks": [
                            { "content": { "type": "text", "text": "look" } },
                            { "content": { "type": "attachment", "id": IMAGE_ID, "block": "image", "mimeType": "image/png" } },
                        ]},
                        { "type": "message", "role": "agent", "messageId": "m", "chunks": [
                            { "content": { "type": "text", "text": "I see" } },
                            { "content": { "type": "resource_link", "uri": "file:///a", "name": "a" } },
                        ]},
                        { "type": "tool", "call": { "toolCallId": "t", "status": "pending" } },
                        { "type": "plan", "update": { "planId": "p" } },
                        { "type": "compaction", "compactionId": "c", "status": "completed", "summary": [] },
                    ],
                    "end": { "response": { "stopReason": "end_turn" } },
                },
                "hash": log.head().unwrap(),
            })
        );
        let block: Block = serde_json::from_value(
            json!({ "type": "attachment", "id": IMAGE_ID, "block": "image" }),
        )
        .unwrap();
        assert!(matches!(block, Block::Attachment(_)));
    }
}
