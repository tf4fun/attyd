//! `canon` and the hash chain ([protocol §3](../../docs/protocol.md#3-log)).
//!
//! A live commit and a load's replay go through the same functions, so the same
//! messages give the same entry and hash however they were chunked (acceptance 1).
//! Session updates and content blocks stay `serde_json::Value`, as in the bridge.
use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Entries (protocol §3.1)

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct Turn {
    /// The user's message, normalized.
    pub prompt: Vec<Block>,
    /// The Agent's message content, in order, normalized.
    pub reply: Vec<Block>,
}

/// Normalized content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(untagged)]
pub(crate) enum Block {
    Attachment(AttachmentRef),
    /// A text or resource link block, verbatim without `_meta`.
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
    /// Lowercase hex SHA-256 of the block's canonical JSON.
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
    /// `h_i` in its wire form (protocol §3.3).
    pub hash: String,
    #[serde(skip)]
    #[cfg_attr(test, ts(skip))]
    digest: ChainHash,
}

// ---------------------------------------------------------------------------
// `canon` (protocol §3.2)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MessageKind {
    User,
    Agent,
}

/// A logical message: its chunks' content blocks, in order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Message {
    pub kind: MessageKind,
    pub content: Vec<Value>,
}

/// A turn's ACP content before normalization.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TurnContent {
    pub prompt: Vec<Value>,
    pub reply: Vec<Value>,
}

/// Attachment blocks by ID, without `_meta`; what attachment references name.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Attachments(HashMap<String, Arc<Value>>);

impl Attachments {
    pub(crate) fn get(&self, id: &str) -> Option<&Value> {
        self.0.get(id).map(|block| &**block)
    }
}

/// A prompt names an attachment the session does not keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnknownAttachment(pub String);

/// Step 1: reduces message chunks to logical messages. Every other update is
/// skipped and does not separate chunks.
pub(crate) fn reduce_messages(updates: &[Value]) -> Vec<Message> {
    let mut messages: Vec<Message> = Vec::new();
    let mut by_id = HashMap::new();
    // The message that a chunk without `messageId` may extend.
    let mut open = None;
    for update in updates {
        let Some(kind) = message_kind(update) else {
            continue;
        };
        let Some(content) = update.get("content") else {
            continue;
        };
        let index = match update.get("messageId").and_then(Value::as_str) {
            Some(id) => {
                open = None;
                *by_id
                    .entry((kind, id))
                    .or_insert_with(|| start_message(&mut messages, kind))
            }
            None => match open {
                Some(index) if messages[index].kind == kind => index,
                _ => *open.insert(start_message(&mut messages, kind)),
            },
        };
        messages[index].content.push(content.clone());
    }
    messages
}

fn message_kind(update: &Value) -> Option<MessageKind> {
    match update.get("sessionUpdate")?.as_str()? {
        "user_message_chunk" => Some(MessageKind::User),
        "agent_message_chunk" => Some(MessageKind::Agent),
        _ => None,
    }
}

fn start_message(messages: &mut Vec<Message>, kind: MessageKind) -> usize {
    messages.push(Message {
        kind,
        content: Vec::new(),
    });
    messages.len() - 1
}

/// Step 2: groups messages into turns. A user message starts a turn; Agent
/// messages before the first one form a turn with an empty prompt.
pub(crate) fn turns(messages: Vec<Message>) -> Vec<TurnContent> {
    let mut turns: Vec<TurnContent> = Vec::new();
    for message in messages {
        match (message.kind, turns.last_mut()) {
            (MessageKind::Agent, Some(turn)) => turn.reply.extend(message.content),
            (MessageKind::Agent, None) => turns.push(TurnContent {
                prompt: Vec::new(),
                reply: message.content,
            }),
            (MessageKind::User, _) => turns.push(TurnContent {
                prompt: message.content,
                reply: Vec::new(),
            }),
        }
    }
    turns
}

/// Step 3: normalizes content, keeping each attachment's block in `attachments`.
pub(crate) fn normalize(blocks: &[Value], attachments: &mut Attachments) -> Vec<Block> {
    let mut normalized: Vec<Block> = Vec::new();
    for block in blocks {
        let block = without_meta(block);
        let block_type = block["type"].as_str().unwrap_or_default().to_owned();
        let kind = match block_type.as_str() {
            "text" => {
                if let Some(Block::Content(previous)) = normalized.last_mut()
                    && previous["type"] == "text"
                    && let Some(Value::String(text)) = previous.get_mut("text")
                {
                    text.push_str(block["text"].as_str().unwrap_or_default());
                } else {
                    normalized.push(Block::Content(block));
                }
                continue;
            }
            "resource_link" => {
                normalized.push(Block::Content(block));
                continue;
            }
            "image" => AttachmentKind::Image,
            "audio" => AttachmentKind::Audio,
            "resource" => AttachmentKind::Resource,
            // Not content the protocol carries.
            _ => continue,
        };
        let source = match kind {
            AttachmentKind::Resource => &block["resource"],
            AttachmentKind::Image | AttachmentKind::Audio => &block,
        };
        let mime_type = source["mimeType"].as_str().map(String::from);
        let uri = match kind {
            AttachmentKind::Resource => source["uri"].as_str().map(String::from),
            AttachmentKind::Image | AttachmentKind::Audio => None,
        };
        let id = Sha256::digest(canonical_json(&block))
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        attachments
            .0
            .entry(id.clone())
            .or_insert_with(|| Arc::new(block));
        normalized.push(Block::Attachment(AttachmentRef {
            r#type: AttachmentTag::Attachment,
            id,
            block: kind,
            mime_type,
            uri,
        }));
    }
    normalized
}

/// Drops `_meta` at every level.
fn without_meta(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| *key != "_meta")
                .map(|(key, value)| (key.clone(), without_meta(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_meta).collect()),
        value => value.clone(),
    }
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
    // The shortest digits that round-trip, as `d.ddde<exponent>`.
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').expect("exponent form");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exponent.parse::<i32>().expect("exponent") + 1;
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

// ---------------------------------------------------------------------------
// Hash chain (protocol §3.3)

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
// Turn log (docs/state-sync.md §2.1)

/// A session's turn history, appended by live commits and rebuilt from a load's
/// replay. It keeps the attachments its entries reference.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TurnLog {
    entries: Vec<Arc<LogEntry>>,
    attachments: Attachments,
}

impl TurnLog {
    /// Builds every turn of a `session/load` or `session/resume` replay.
    pub(crate) fn from_replay(updates: &[Value]) -> Self {
        let mut log = Self::default();
        for turn in turns(reduce_messages(updates)) {
            log.push(turn);
        }
        log
    }

    /// Appends a completed live turn: `prompt` is what the bridge sent in
    /// `session/prompt`, `updates` are the turn's session updates. Only the
    /// Agent's messages among them form the reply.
    pub(crate) fn commit(&mut self, prompt: &[Value], updates: &[Value]) -> Arc<LogEntry> {
        let reply = reduce_messages(updates)
            .into_iter()
            .filter(|message| message.kind == MessageKind::Agent)
            .flat_map(|message| message.content)
            .collect();
        self.push(TurnContent {
            prompt: prompt.to_vec(),
            reply,
        })
    }

    fn push(&mut self, content: TurnContent) -> Arc<LogEntry> {
        let turn = Turn {
            prompt: normalize(&content.prompt, &mut self.attachments),
            reply: normalize(&content.reply, &mut self.attachments),
        };
        let prev = self
            .entries
            .last()
            .map_or(ChainHash::ZERO, |entry| entry.digest);
        let digest = chain(&prev, &turn);
        let entry = Arc::new(LogEntry {
            turn,
            hash: digest.wire(),
            digest,
        });
        self.entries.push(Arc::clone(&entry));
        entry
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
    const RESOURCE_ID: &str = "bf9b2c50a19b54de2dfbc4fcb6376b1c55ea1b12c27938596b1ef4f9f1cf2319";

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

    fn tool_call(id: &str) -> Value {
        json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": "read" })
    }

    fn tool_call_update(id: &str) -> Value {
        json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed" })
    }

    fn thought(text_: &str) -> Value {
        json!({ "sessionUpdate": "agent_thought_chunk", "content": text(text_) })
    }

    fn plan() -> Value {
        json!({ "sessionUpdate": "plan", "entries": [] })
    }

    fn usage() -> Value {
        json!({ "sessionUpdate": "usage_update", "used": 1, "size": 10 })
    }

    fn with_meta(mut value: Value) -> Value {
        value["_meta"] = json!({ "attyd": { "chunk": 1 } });
        value
    }

    fn turn(log: &TurnLog, index: usize) -> Value {
        serde_json::to_value(&log.entries()[index].turn).unwrap()
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

    #[test]
    fn hash_chain_starts_at_zero_and_uses_short_base64url() {
        let mut log = TurnLog::default();
        log.commit(&[], &[]);
        assert_eq!(log.head(), Some("_Or5f4gS54l-SnZvXDsdBg"));
        log.commit(&[text("hi")], &[agent(text("hello"))]);
        assert_eq!(log.head(), Some("nP13PpWLtVj2FARjeF18Bw"));
        assert_eq!(ChainHash::ZERO.wire(), "AAAAAAAAAAAAAAAAAAAAAA");
    }

    #[test]
    fn chunks_reduce_to_messages_by_message_id_or_adjacency() {
        let updates = [
            user(text("a")),
            tool_call("t"),
            usage(),
            user(text("b")),
            chunk("agent_message_chunk", text("x"), Some("m1")),
            chunk("agent_message_chunk", text("y"), Some("m2")),
            chunk("agent_message_chunk", text("z"), Some("m1")),
            user(text("c")),
            agent(text("p")),
            thought("hmm"),
            plan(),
            agent(text("q")),
            chunk("agent_message_chunk", text("r"), Some("m3")),
            agent(text("s")),
        ];
        let messages = reduce_messages(&updates);
        let expected = [
            (MessageKind::User, vec![text("a"), text("b")]),
            (MessageKind::Agent, vec![text("x"), text("z")]),
            (MessageKind::Agent, vec![text("y")]),
            (MessageKind::User, vec![text("c")]),
            (MessageKind::Agent, vec![text("p"), text("q")]),
            (MessageKind::Agent, vec![text("r")]),
            (MessageKind::Agent, vec![text("s")]),
        ]
        .map(|(kind, content)| Message { kind, content });
        assert_eq!(messages, expected);
    }

    #[test]
    fn messages_group_into_turns_from_each_user_message() {
        let messages = vec![
            Message {
                kind: MessageKind::Agent,
                content: vec![text("welcome")],
            },
            Message {
                kind: MessageKind::User,
                content: vec![text("q1")],
            },
            Message {
                kind: MessageKind::User,
                content: vec![text("q2")],
            },
            Message {
                kind: MessageKind::Agent,
                content: vec![text("a")],
            },
            Message {
                kind: MessageKind::Agent,
                content: vec![text("b")],
            },
        ];
        assert_eq!(
            turns(messages),
            vec![
                TurnContent {
                    prompt: vec![],
                    reply: vec![text("welcome")],
                },
                TurnContent {
                    prompt: vec![text("q1")],
                    reply: vec![],
                },
                TurnContent {
                    prompt: vec![text("q2")],
                    reply: vec![text("a"), text("b")],
                },
            ]
        );
    }

    #[test]
    fn normalization_merges_text_references_attachments_and_drops_meta() {
        let resource = json!({
            "type": "resource",
            "resource": {
                "uri": "file:///x.txt",
                "mimeType": "text/plain",
                "text": "t",
                "_meta": { "x": 1 },
            },
        });
        let blocks = [
            with_meta(json!({ "type": "text", "text": "a", "annotations": { "priority": 1 } })),
            text("b"),
            with_meta(image()),
            with_meta(json!({ "type": "resource_link", "uri": "file:///y", "name": "y" })),
            resource,
            text("c"),
            text("d"),
        ];
        let mut attachments = Attachments::default();
        let normalized = normalize(&blocks, &mut attachments);
        assert_eq!(
            serde_json::to_value(&normalized).unwrap(),
            json!([
                { "type": "text", "text": "ab", "annotations": { "priority": 1 } },
                { "type": "attachment", "id": IMAGE_ID, "block": "image", "mimeType": "image/png" },
                { "type": "resource_link", "uri": "file:///y", "name": "y" },
                {
                    "type": "attachment",
                    "id": RESOURCE_ID,
                    "block": "resource",
                    "mimeType": "text/plain",
                    "uri": "file:///x.txt",
                },
                { "type": "text", "text": "cd" },
            ])
        );
        assert_eq!(attachments.get(IMAGE_ID), Some(&image()));
        assert_eq!(
            attachments.get(RESOURCE_ID),
            Some(&json!({
                "type": "resource",
                "resource": { "uri": "file:///x.txt", "mimeType": "text/plain", "text": "t" },
            }))
        );
    }

    // Acceptance 1: the same messages give the same entry and hash however they
    // were chunked.
    #[test]
    fn differently_chunked_messages_give_the_same_entries() {
        let whole = TurnLog::from_replay(&[
            user(text("hello world")),
            agent(text("foo bar baz")),
            user(text("again")),
            agent(text("done")),
        ]);
        let by_message_id = TurnLog::from_replay(&[
            chunk("user_message_chunk", text("hel"), Some("u1")),
            chunk("user_message_chunk", text("lo world"), Some("u1")),
            tool_call("t1"),
            chunk("agent_message_chunk", text("foo "), Some("a1")),
            thought("thinking"),
            chunk("agent_message_chunk", text("bar baz"), Some("a1")),
            tool_call_update("t1"),
            chunk("user_message_chunk", text("again"), Some("u2")),
            chunk("agent_message_chunk", text("do"), Some("a2")),
            chunk("agent_message_chunk", text("ne"), Some("a3")),
        ]);
        let by_adjacency = TurnLog::from_replay(&[
            with_meta(user(with_meta(text("hello ")))),
            user(text("world")),
            agent(text("foo")),
            plan(),
            agent(text(" bar")),
            usage(),
            agent(with_meta(text(" baz"))),
            user(text("aga")),
            user(text("in")),
            agent(text("done")),
        ]);
        assert_eq!(whole.entries().len(), 2);
        assert_eq!(by_message_id, whole);
        assert_eq!(by_adjacency, whole);
    }

    // Acceptance 1: a user message with text and an image, live or replayed as
    // several chunks.
    #[test]
    fn a_text_and_image_prompt_gives_the_same_entry_live_and_replayed() {
        let mut live = TurnLog::default();
        live.commit(
            &[text("look"), with_meta(image())],
            &[agent(text("I see")), agent(text(" a cat"))],
        );
        let replayed = TurnLog::from_replay(&[
            user(text("lo")),
            user(text("ok")),
            with_meta(user(image())),
            agent(text("I see a cat")),
        ]);
        assert_eq!(replayed, live);
        assert_eq!(
            turn(&live, 0),
            json!({
                "prompt": [
                    { "type": "text", "text": "look" },
                    { "type": "attachment", "id": IMAGE_ID, "block": "image", "mimeType": "image/png" },
                ],
                "reply": [{ "type": "text", "text": "I see a cat" }],
            })
        );
        assert_eq!(live.attachments().get(IMAGE_ID), Some(&image()));
    }

    // Acceptance 1: committed live or replayed by `load`.
    #[test]
    fn live_commits_and_a_load_replay_build_the_same_log() {
        let mut live = TurnLog::default();
        live.commit(
            &[text("read the file")],
            &[
                // An Agent may echo the prompt; the live prompt is the input.
                user(text("read the file")),
                tool_call("t1"),
                tool_call_update("t1"),
                agent(text("It says ")),
                agent(text("hi.")),
            ],
        );
        live.commit(
            &[text("thanks")],
            &[thought("ok"), agent(text("You're welcome."))],
        );
        live.commit(&[text("stop")], &[]);

        let replayed = TurnLog::from_replay(&[
            user(text("read the file")),
            tool_call("t1"),
            tool_call_update("t1"),
            agent(text("It says hi.")),
            user(text("thanks")),
            agent(text("You're ")),
            agent(text("welcome.")),
            user(text("stop")),
        ]);
        assert_eq!(replayed, live);
        assert_eq!(replayed.head(), live.head());
        assert_eq!(live.entries().len(), 3);
    }

    #[test]
    fn a_replay_keeps_turns_without_prompt_or_reply() {
        let log = TurnLog::from_replay(&[
            agent(text("welcome")),
            chunk("user_message_chunk", text("q1"), Some("u1")),
            chunk("user_message_chunk", text("q2"), Some("u2")),
            agent(text("a")),
        ]);
        let turns: Vec<_> = (0..3).map(|index| turn(&log, index)).collect();
        assert_eq!(
            turns,
            [
                json!({ "prompt": [], "reply": [text("welcome")] }),
                json!({ "prompt": [text("q1")], "reply": [] }),
                json!({ "prompt": [text("q2")], "reply": [text("a")] }),
            ]
        );
    }

    // Acceptance 1: prompts that reference attachments are resolved before the
    // bridge calls the Agent.
    #[test]
    fn prompt_references_resolve_to_the_blocks_they_name() {
        let mut log = TurnLog::default();
        log.commit(&[text("look"), image()], &[agent(text("a cat"))]);
        let reference = log.entries()[0].turn.prompt[1].clone();
        let Block::Attachment(reference) = reference else {
            panic!("the image is referenced");
        };

        let prompt: Vec<PromptBlock> = serde_json::from_value(json!([
            { "type": "text", "text": "again" },
            { "type": "attachment", "id": IMAGE_ID, "block": "image", "mimeType": "image/png" },
        ]))
        .unwrap();
        assert_eq!(prompt[1], PromptBlock::Attachment(reference));
        let resolved = resolve(&prompt, log.attachments()).unwrap();
        assert_eq!(resolved, vec![text("again"), image()]);

        log.commit(&resolved, &[]);
        assert_eq!(
            turn(&log, 1)["prompt"],
            json!([
                { "type": "text", "text": "again" },
                { "type": "attachment", "id": IMAGE_ID, "block": "image", "mimeType": "image/png" },
            ])
        );

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
                agent(text("I see")),
                agent(json!({ "type": "resource_link", "uri": "file:///a", "name": "a" })),
            ],
        );
        assert_eq!(
            serde_json::to_value(&*log.entries()[0]).unwrap(),
            json!({
                "turn": {
                    "prompt": [
                        { "type": "text", "text": "look" },
                        { "type": "attachment", "id": IMAGE_ID, "block": "image", "mimeType": "image/png" },
                    ],
                    "reply": [
                        { "type": "text", "text": "I see" },
                        { "type": "resource_link", "uri": "file:///a", "name": "a" },
                    ],
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
