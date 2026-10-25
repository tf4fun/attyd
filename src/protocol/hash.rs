//! Content hashes of protocol revision 4 (docs/protocol-r4-design.md §4.6, §5).
//!
//! Text is hashed as a chain over fixed segments, so that a client can verify and continue a
//! window of a text it does not hold whole. Everything is `SHA-256`; node hashes are 32 bytes
//! and go on the wire as the unpadded base64url of their first 16 bytes (`digest::wire`).
#![cfg_attr(not(test), allow(dead_code))]

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::canon::canonical_json;
pub(crate) use super::digest::Hash;

/// The segment length of a text position, in UTF-8 bytes (§4.6). It is part of every text
/// hash: changing it changes the protocol.
pub(crate) const L: usize = 4096;

/// The chain state before the first segment.
pub(crate) const ZERO: Hash = [0; 32];

/// The segments of `text`: its UTF-8 bytes split greedily from the start into runs of at most
/// `L` bytes, each ending on a code point boundary. Appending to a text changes only its last
/// segment and adds new ones; earlier segments never change.
pub(crate) fn segments(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let mut end = rest.len().min(L);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (segment, tail) = rest.split_at(end);
        rest = tail;
        Some(segment)
    })
}

/// `c_i = H(c_{i-1} ‖ s_i)`: the chain state after `segment`.
pub(crate) fn chain_step(before: &Hash, segment: &str) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(before);
    hasher.update(segment.as_bytes());
    hasher.finalize().into()
}

/// The chain states after each segment of `text`.
pub(crate) fn chain_states(text: &str) -> Vec<Hash> {
    let mut state = ZERO;
    segments(text)
        .map(|segment| {
            state = chain_step(&state, segment);
            state
        })
        .collect()
}

/// `h = H("t" ‖ u64be(len) ‖ c_last)` from the chain state after the last segment (`ZERO` for
/// an empty text) and the text's length in UTF-8 bytes.
fn finish(length: u64, last: &Hash) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(b"t");
    hasher.update(length.to_be_bytes());
    hasher.update(last);
    hasher.finalize().into()
}

/// The hash from the chain state after the last segment (tests of a paged text).
#[cfg(test)]
pub(crate) fn finish_for_tests(length: u64, last: &Hash) -> Hash {
    finish(length, last)
}

/// The hash of a whole text.
pub(crate) fn text_hash(text: &str) -> Hash {
    finish(text.len() as u64, &chain_states(text).last().copied().unwrap_or(ZERO))
}

/// A computed chain, retained across proven appends. The final segment may grow, so only
/// segments before it are reused. No old text or old tree is retained by this cache.
#[derive(Debug, Clone, Default)]
pub(crate) struct TextChain {
    ends: Vec<usize>,
    states: Vec<Hash>,
}

impl TextChain {
    /// `prefix` is valid only when the caller proved the text has only been appended to.
    pub(crate) fn continued(text: &str, prefix: Option<&Self>) -> Self {
        let mut chain = prefix.cloned().unwrap_or_default();
        chain.ends.pop();
        chain.states.pop();
        let mut offset = chain.ends.last().copied().unwrap_or(0);
        let mut state = chain.states.last().copied().unwrap_or(ZERO);
        for segment in segments(&text[offset..]) {
            offset += segment.len();
            state = chain_step(&state, segment);
            chain.ends.push(offset);
            chain.states.push(state);
        }
        chain
    }

    pub(crate) fn hash(&self) -> Hash {
        finish(self.ends.last().copied().unwrap_or(0) as u64, self.states.last().unwrap_or(&ZERO))
    }

    /// The segment boundaries before `length` are also those of any appended version.
    pub(crate) fn window_offset(&self, length: usize, bytes: usize) -> usize {
        let boundary = self.ends.partition_point(|end| *end <= length.saturating_sub(bytes));
        boundary.checked_sub(1).map_or(0, |index| self.ends[index])
    }

    pub(crate) fn before(&self, offset: usize) -> Hash {
        if offset == 0 { ZERO } else { self.states[self.ends.binary_search(&offset).expect("a segment boundary")] }
    }

    pub(crate) fn segment(&self, index: usize) -> Option<(usize, usize, Hash)> {
        let end = *self.ends.get(index)?;
        let start = index.checked_sub(1).map_or(0, |previous| self.ends[previous]);
        Some((start, end, self.before(start)))
    }
}

/// The hash of a text of `length` bytes of which a client holds the tail `text`, starting at a
/// segment boundary whose chain state is `before`. It equals `text_hash` of the whole text.
pub(crate) fn window_hash(before: &Hash, length: u64, text: &str) -> Hash {
    let last = segments(text).fold(*before, |state, segment| chain_step(&state, segment));
    finish(length, &last)
}

/// Where a value node keeps its text positions (§4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextRule {
    /// A plain value: no text position.
    None,
    /// An entry: the `text` of the text blocks of its message chunks, tool call content items
    /// and compaction summary.
    Entry,
    /// A terminal: its `output`.
    Terminal,
    /// An output view: its `text`.
    OutputView,
}

/// Whether `value` is a text position's content: a string, or its window (design §6.4).
fn is_text(value: &Value) -> bool {
    value.is_string() || value.as_object().is_some_and(|window| window.get("length").is_some_and(Value::is_u64))
}

/// Whether `block` is a text block: `{ "type": "text", "text": <text> }`.
fn is_text_block(block: Option<&Value>) -> bool {
    block.is_some_and(|block| block["type"] == "text" && is_text(&block["text"]))
}

/// The JSON Pointers, relative to `value`, of its text positions under `rule` (§4.6), in
/// document order.
pub(crate) fn text_pointers(rule: TextRule, value: &Value) -> Vec<String> {
    let mut found = Vec::new();
    match rule {
        TextRule::None => {}
        TextRule::Terminal => {
            if is_text(&value["output"]) {
                found.push("/output".to_string());
            }
        }
        TextRule::OutputView => {
            if is_text(&value["text"]) {
                found.push("/text".to_string());
            }
        }
        TextRule::Entry => {
            let items = |key: &str| value.get(key).and_then(Value::as_array).into_iter().flatten().enumerate();
            match value["kind"].as_str() {
                Some("message") => {
                    for (index, chunk) in items("chunks") {
                        if is_text_block(chunk.get("content")) {
                            found.push(format!("/chunks/{index}/content/text"));
                        }
                    }
                }
                Some("compaction") => {
                    for (index, chunk) in items("summary") {
                        if is_text_block(chunk.get("content")) {
                            found.push(format!("/summary/{index}/content/text"));
                        }
                    }
                }
                Some("tool") => {
                    let content = value.get("call").and_then(|call| call.get("content")).and_then(Value::as_array);
                    for (index, item) in content.into_iter().flatten().enumerate() {
                        if item["type"] == "content" && is_text_block(item.get("content")) {
                            found.push(format!("/call/content/{index}/content/text"));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    found
}

/// `h = H("v" ‖ canon(value'))`: `value'` is the value with each text position replaced by
/// `"#"` and the unpadded base64url of that text's hash.
pub(crate) fn value_hash(rule: TextRule, value: &Value) -> Hash {
    value_hash_with(rule, value, |_, text| text_hash(text))
}

/// Clone only the JSON around text positions; their potentially large strings are replaced
/// before copying. This is shared by hashes and window snapshots.
pub(crate) fn map_text(rule: TextRule, value: &Value, mut replace: impl FnMut(&str, &str) -> Value) -> Value {
    fn copy(value: &Value, path: &str, replacements: &mut std::collections::BTreeMap<String, Value>) -> Value {
        if let Some(replaced) = replacements.remove(path) {
            return replaced;
        }
        if !replacements.keys().any(|key| key.strip_prefix(path).is_some_and(|rest| rest.starts_with('/'))) {
            return value.clone();
        }
        match value {
            Value::Array(items) => Value::Array(items.iter().enumerate().map(|(index, item)| {
                copy(item, &format!("{path}/{index}"), replacements)
            }).collect()),
            Value::Object(fields) => Value::Object(fields.iter().map(|(key, value)| {
                let pointer = super::diff::child(path, key);
                (key.clone(), copy(value, &pointer, replacements))
            }).collect()),
            _ => value.clone(),
        }
    }
    let mut replacements = text_pointers(rule, value).into_iter().map(|pointer| {
        let text = value.pointer(&pointer).and_then(Value::as_str).expect("a text position is a string");
        let replacement = replace(&pointer, text);
        (pointer, replacement)
    }).collect();
    copy(value, "", &mut replacements)
}

pub(crate) fn value_hash_with(rule: TextRule, value: &Value, mut hash_text: impl FnMut(&str, &str) -> Hash) -> Hash {
    let replaced = map_text(rule, value, |pointer, text| {
        Value::String(format!("#{}", URL_SAFE_NO_PAD.encode(hash_text(pointer, text))))
    });
    let mut hasher = Sha256::new();
    hasher.update(b"v");
    hasher.update(canonical_json(&replaced));
    hasher.finalize().into()
}

fn by_utf16(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

fn canon_name(name: &str) -> Vec<u8> {
    canonical_json(&Value::String(name.to_string()))
}

fn named(hasher: &mut Sha256, children: Vec<(&str, Hash)>) {
    let mut children = children;
    children.sort_by(|a, b| by_utf16(a.0, b.0));
    for (name, hash) in children {
        hasher.update(canon_name(name));
        hasher.update(hash);
    }
}

/// `h = H("o" ‖ canon(own) ‖ ∥ (canon(name) ‖ h(child)))`, children by name ascending in
/// UTF-16 code units, as JCS orders object keys.
pub(crate) fn object_hash<'a>(
    own: &Map<String, Value>,
    children: impl IntoIterator<Item = (&'a str, Hash)>,
) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(b"o");
    hasher.update(canonical_json(&Value::Object(own.clone())));
    named(&mut hasher, children.into_iter().collect());
    hasher.finalize().into()
}

/// `h = H("m" ‖ ∥ (canon(key) ‖ h(child)))`, keys ascending in UTF-16 code units.
pub(crate) fn map_hash<'a>(children: impl IntoIterator<Item = (&'a str, Hash)>) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(b"m");
    named(&mut hasher, children.into_iter().collect());
    hasher.finalize().into()
}

/// `h = H("l" ‖ u64be(n) ‖ h(child_0) ‖ … ‖ h(child_{n-1}))`.
pub(crate) fn list_hash<'a>(children: impl IntoIterator<Item = &'a Hash>) -> Hash {
    let children: Vec<&Hash> = children.into_iter().collect();
    let mut hasher = Sha256::new();
    hasher.update(b"l");
    hasher.update((children.len() as u64).to_be_bytes());
    for hash in children {
        hasher.update(hash);
    }
    hasher.finalize().into()
}

#[cfg(test)]
mod tests;
