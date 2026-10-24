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

/// The hash of a whole text.
pub(crate) fn text_hash(text: &str) -> Hash {
    finish(text.len() as u64, &chain_states(text).last().copied().unwrap_or(ZERO))
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

/// The `text` of a text block (never creates a member: indexing a `&mut Value` by a missing
/// key would insert `null`, changing what is hashed).
fn text_block(block: Option<&mut Value>) -> Option<&mut Value> {
    let block = block?;
    (block["type"] == "text" && block["text"].is_string()).then(|| &mut block["text"])
}

/// The text positions of `value` under `rule`.
fn texts<'a>(rule: TextRule, value: &'a mut Value) -> Vec<&'a mut Value> {
    let mut found = Vec::new();
    match rule {
        TextRule::None => {}
        TextRule::Terminal => {
            if value["output"].is_string() {
                found.push(&mut value["output"]);
            }
        }
        TextRule::OutputView => {
            if value["text"].is_string() {
                found.push(&mut value["text"]);
            }
        }
        TextRule::Entry => match value["kind"].as_str() {
            Some("message") => found.extend(
                value
                    .get_mut("chunks")
                    .and_then(Value::as_array_mut)
                    .into_iter()
                    .flatten()
                    .filter_map(|chunk| text_block(chunk.get_mut("content"))),
            ),
            Some("compaction") => found.extend(
                value
                    .get_mut("summary")
                    .and_then(Value::as_array_mut)
                    .into_iter()
                    .flatten()
                    .filter_map(|chunk| text_block(chunk.get_mut("content"))),
            ),
            Some("tool") => found.extend(
                value
                    .get_mut("call")
                    .and_then(|call| call.get_mut("content"))
                    .and_then(Value::as_array_mut)
                    .into_iter()
                    .flatten()
                    .filter(|item| item["type"] == "content")
                    .filter_map(|item| text_block(item.get_mut("content"))),
            ),
            _ => {}
        },
    }
    found
}

/// `h = H("v" ‖ canon(value'))`: `value'` is the value with each text position replaced by
/// `"#"` and the unpadded base64url of that text's hash.
pub(crate) fn value_hash(rule: TextRule, value: &Value) -> Hash {
    let mut replaced = value.clone();
    for text in texts(rule, &mut replaced) {
        let hash = text_hash(text.as_str().expect("a text position is a string"));
        *text = Value::String(format!("#{}", URL_SAFE_NO_PAD.encode(hash)));
    }
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
