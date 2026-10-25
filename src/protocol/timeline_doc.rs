//! Entry and attachment projection for the revision 4 tree (docs/protocol-r4-design.md §4).
//!
//! The memory model's entries are published as they are, except that a content
//! block carrying bytes (an image, audio or an embedded resource) is replaced
//! by an `AttachmentRef`. The reference's `id` names the block's *exact form*
//! (protocol §8), which keeps every integer exact, so two blocks that differ
//! only in an integer beyond 2^53 have different IDs; JCS hashing works over
//! doubles and cannot tell them apart.
use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::canon::{AttachmentKind, AttachmentRef, AttachmentTag, canonical_number, write_string};
use super::timeline::Entry;

/// The blocks the published entries reference, by attachment ID, as the bridge
/// retains them (protocol §8). Built while entries are published; a block is
/// served while a published entry still references it, counted per reference.
#[cfg_attr(test, derive(Clone))]
#[derive(Debug, Default, PartialEq)]
pub(crate) struct AttachmentIndex(HashMap<String, Held>);

#[cfg_attr(test, derive(Clone))]
#[derive(Debug, PartialEq)]
struct Held {
    block: Arc<Value>,
    references: usize,
}

impl AttachmentIndex {
    pub(crate) fn get(&self, id: &str) -> Option<&Value> {
        self.0.get(id).map(|held| &*held.block)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    fn reference(&mut self, id: &str, block: Value) {
        self.0
            .entry(id.to_string())
            .or_insert_with(|| Held {
                block: Arc::new(block),
                references: 0,
            })
            .references += 1;
    }

    /// Drops one reference to each of `ids`; a block nothing references is no longer served.
    pub(crate) fn release(&mut self, ids: &[String]) {
        for id in ids {
            if let Some(held) = self.0.get_mut(id) {
                held.references -= 1;
                if held.references == 0 {
                    self.0.remove(id);
                }
            }
        }
    }
}

/// The exact form of a block: JCS key order and string escaping, integers
/// (`i64`, `u64`) written exactly, every other number as its parsed finite
/// `f64` in JCS number form.
pub(crate) fn exact_form(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_exact(value, &mut out);
    out
}

fn write_exact(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(value) => {
            out.extend_from_slice(if *value { "true" } else { "false" }.as_bytes());
        }
        Value::Number(number) => {
            let text = if let Some(integer) = number.as_i64() {
                integer.to_string()
            } else if let Some(integer) = number.as_u64() {
                integer.to_string()
            } else {
                canonical_number(number)
            };
            out.extend_from_slice(text.as_bytes());
        }
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_exact(item, out);
            }
            out.push(b']');
        }
        Value::Object(map) => {
            // Keys are ordered by their UTF-16 code units.
            let mut members: Vec<_> = map.iter().collect();
            members.sort_by(|(left, _), (right, _)| left.encode_utf16().cmp(right.encode_utf16()));
            out.push(b'{');
            for (index, (key, value)) in members.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_string(key, out);
                out.push(b':');
                write_exact(value, out);
            }
            out.push(b'}');
        }
    }
}

/// An attachment's ID: the lowercase hex SHA-256 of the block's exact form.
pub(crate) fn attachment_id(block: &Value) -> String {
    Sha256::digest(exact_form(block))
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A byte-carrying block's kind, MIME type and (for a resource) URI.
fn byte_block(block: &Value) -> Option<(AttachmentKind, Option<String>, Option<String>)> {
    let text = |value: &Value| value.as_str().map(String::from);
    match block.get("type").and_then(Value::as_str)? {
        "image" => Some((AttachmentKind::Image, text(&block["mimeType"]), None)),
        "audio" => Some((AttachmentKind::Audio, text(&block["mimeType"]), None)),
        "resource" => {
            let resource = &block["resource"];
            Some((
                AttachmentKind::Resource,
                text(&resource["mimeType"]),
                text(&resource["uri"]),
            ))
        }
        _ => None,
    }
}

/// A content block as it is published: text and resource links stay inline with
/// their annotations and `_meta`; a byte-carrying block becomes a reference and
/// is kept in `index`.
#[cfg(test)]
pub(crate) fn publish_block(block: &Value, index: &mut AttachmentIndex) -> Value {
    publish_owned(block.clone(), index, &mut Vec::new())
}

/// `publish_block` of a block it may keep, adding the ID it references to `ids`.
fn publish_owned(block: Value, index: &mut AttachmentIndex, ids: &mut Vec<String>) -> Value {
    let Some((kind, mime_type, uri)) = byte_block(&block) else {
        return block;
    };
    let id = attachment_id(&block);
    index.reference(&id, block);
    ids.push(id.clone());
    serde_json::to_value(AttachmentRef {
        r#type: AttachmentTag::Attachment,
        id,
        block: kind,
        mime_type,
        uri,
    })
    .expect("a reference serializes")
}

fn publish_member(parent: &mut Value, key: &str, index: &mut AttachmentIndex, ids: &mut Vec<String>) {
    if let Some(block) = parent.get_mut(key).map(Value::take) {
        parent[key] = publish_owned(block, index, ids);
    }
}

/// An entry as it is published: its content blocks as `publish_block` writes them.
#[cfg(test)]
pub(crate) fn publish_entry(entry: &Entry, index: &mut AttachmentIndex) -> Value {
    publish_entry_referencing(entry, index).0
}

/// `publish_entry`, with the attachment IDs the published entry references, once per
/// reference: what `AttachmentIndex::release` is given when the entry is replaced.
pub(crate) fn publish_entry_referencing(entry: &Entry, index: &mut AttachmentIndex) -> (Value, Vec<String>) {
    let mut ids = Vec::new();
    let mut value = serde_json::to_value(entry).expect("entries serialize");
    match entry {
        Entry::Message(_) => {
            for chunk in value["chunks"].as_array_mut().into_iter().flatten() {
                publish_member(chunk, "content", index, &mut ids);
            }
        }
        Entry::Compaction(_) => {
            for chunk in value["summary"].as_array_mut().into_iter().flatten() {
                publish_member(chunk, "content", index, &mut ids);
            }
        }
        Entry::Tool(_) => {
            let content = value
                .get_mut("call")
                .and_then(|call| call.get_mut("content"))
                .and_then(Value::as_array_mut);
            for item in content.into_iter().flatten() {
                if item["type"] == "content" {
                    publish_member(item, "content", index, &mut ids);
                }
            }
        }
        Entry::Plan(_) => {}
    }
    (value, ids)
}

#[cfg(test)]
mod tests;
