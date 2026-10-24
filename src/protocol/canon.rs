//! `canon`: normalized content blocks, attachment references and JCS serialization.
//!
//! Session updates and content blocks stay `serde_json::Value`, as in the bridge.
use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Content blocks and attachment references

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

/// Normalizes content, keeping each attachment's block in `attachments`. Text and
/// resource links lose `_meta`; an attachment keeps its exact form, so its ID is the one
/// the published timeline gives the same block (protocol §3.1).
pub(crate) fn normalize(blocks: &[Value], attachments: &mut Attachments) -> Vec<Block> {
    let mut normalized: Vec<Block> = Vec::new();
    for original in blocks {
        let block = without_meta(original);
        let block_type = block["type"].as_str().unwrap_or_default().to_owned();
        let kind = match block_type.as_str() {
            "text" => {
                if let Some(Block::Content(previous)) = normalized.last_mut()
                    && same_but_text(previous, &block)
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
        let id = super::timeline_doc::attachment_id(original);
        attachments
            .0
            .entry(id.clone())
            .or_insert_with(|| Arc::new(original.clone()));
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

/// Whether two blocks differ at most in `text`, so they may merge.
fn same_but_text(left: &Value, right: &Value) -> bool {
    let (Some(left), Some(right)) = (left.as_object(), right.as_object()) else {
        return false;
    };
    left.len() == right.len()
        && left.contains_key("text") == right.contains_key("text")
        && left
            .iter()
            .all(|(key, value)| key == "text" || right.get(key) == Some(value))
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

pub(super) fn write_string(text: &str, out: &mut Vec<u8>) {
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
pub(super) fn canonical_number(number: &serde_json::Number) -> String {
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

    fn with_meta(mut value: Value) -> Value {
        value["_meta"] = json!({ "attyd": { "chunk": 1 } });
        value
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
    fn normalization_merges_text_references_attachments_and_drops_meta_outside_them() {
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
            json!({ "type": "text", "text": "b", "annotations": { "priority": 1 } }),
            with_meta(image()),
            with_meta(json!({ "type": "resource_link", "uri": "file:///y", "name": "y" })),
            resource,
            text("c"),
            text("d"),
        ];
        let image_id = crate::protocol::timeline_doc::attachment_id(&blocks[2]);
        let resource_id = crate::protocol::timeline_doc::attachment_id(&blocks[4]);
        let mut attachments = Attachments::default();
        let normalized = normalize(&blocks, &mut attachments);
        assert_eq!(
            serde_json::to_value(&normalized).unwrap(),
            json!([
                { "type": "text", "text": "ab", "annotations": { "priority": 1 } },
                { "type": "attachment", "id": image_id, "block": "image", "mimeType": "image/png" },
                { "type": "resource_link", "uri": "file:///y", "name": "y" },
                {
                    "type": "attachment",
                    "id": resource_id,
                    "block": "resource",
                    "mimeType": "text/plain",
                    "uri": "file:///x.txt",
                },
                { "type": "text", "text": "cd" },
            ])
        );
        // An attachment is kept exactly as it came, `_meta` included.
        assert_ne!(image_id, IMAGE_ID);
        assert_ne!(resource_id, RESOURCE_ID);
        assert_eq!(attachments.get(&image_id), Some(&blocks[2]));
        assert_eq!(attachments.get(&resource_id), Some(&blocks[4]));
    }

    // #182: a prompt's references and the published timeline name the same block
    // by the same ID, also when the block carries `_meta`, so a reference taken from
    // the operation's input reads and resolves.
    #[test]
    fn a_prompt_attachment_with_meta_has_the_published_timelines_id() {
        let block = with_meta(image());
        let mut attachments = Attachments::default();
        let Block::Attachment(reference) = normalize(&[block.clone()], &mut attachments).remove(0)
        else {
            panic!("the image is referenced");
        };
        let mut index = crate::protocol::timeline_doc::AttachmentIndex::default();
        let published = crate::protocol::timeline_doc::publish_block(&block, &mut index);
        assert_eq!(published["id"], json!(reference.id));
        assert_eq!(index.get(&reference.id), Some(&block));
        assert_eq!(attachments.get(&reference.id), Some(&block));
    }

    #[test]
    fn blocks_differing_in_an_integer_beyond_two_to_the_53_get_different_ids() {
        let with = |number: &str| {
            serde_json::from_str::<Value>(&format!(
                r#"{{"type":"image","mimeType":"image/png","data":"aGk=","count":{number}}}"#
            ))
            .unwrap()
        };
        let id = |block: Value| {
            let mut attachments = Attachments::default();
            match normalize(&[block], &mut attachments).remove(0) {
                Block::Attachment(reference) => reference.id,
                other => panic!("an image is referenced: {other:?}"),
            }
        };
        let (a, b) = (with("9007199254740993"), with("9007199254740992"));
        assert_ne!(id(a.clone()), id(b), "integers are written exactly, not rounded to a double");
        // The same definition as the published timeline's attachments.
        assert_eq!(id(a.clone()), crate::protocol::timeline_doc::attachment_id(&a));
    }

    #[test]
    fn adjacent_text_merges_only_when_its_other_fields_agree() {
        let annotated = |text: &str, audience: &str| json!({ "type": "text", "text": text, "annotations": { "audience": [audience] } });
        let mut attachments = Attachments::default();
        assert_eq!(
            serde_json::to_value(normalize(
                &[
                    annotated("a", "user"),
                    annotated("b", "assistant"),
                    text("c")
                ],
                &mut attachments,
            ))
            .unwrap(),
            json!([
                annotated("a", "user"),
                annotated("b", "assistant"),
                text("c")
            ])
        );

        // Chunks that differ only by `_meta` merge; a different annotation does not.
        let normalized = |blocks: &[Value]| {
            serde_json::to_value(normalize(blocks, &mut Attachments::default())).unwrap()
        };
        assert_eq!(
            normalized(&[
                annotated("hel", "user"),
                with_meta(annotated("lo", "user"))
            ]),
            normalized(&[annotated("hello", "user")])
        );
        assert_ne!(
            normalized(&[annotated("a", "user"), annotated("b", "assistant")]),
            normalized(&[annotated("a", "user"), annotated("b", "user")])
        );
    }

    // Acceptance 1: prompts that reference attachments are resolved before the
    // bridge calls the Agent.
    #[test]
    fn prompt_references_resolve_to_the_blocks_they_name() {
        let mut attachments = Attachments::default();
        let normalized = normalize(&[text("look"), image()], &mut attachments);
        let Block::Attachment(reference) = normalized[1].clone() else {
            panic!("the image is referenced");
        };

        let prompt: Vec<PromptBlock> = serde_json::from_value(json!([
            { "type": "text", "text": "again" },
            { "type": "attachment", "id": IMAGE_ID, "block": "image", "mimeType": "image/png" },
        ]))
        .unwrap();
        assert_eq!(prompt[1], PromptBlock::Attachment(reference));
        let resolved = resolve(&prompt, &attachments).unwrap();
        assert_eq!(resolved, vec![text("again"), image()]);

        let unknown: Vec<PromptBlock> = serde_json::from_value(json!([
            { "type": "attachment", "id": "0".repeat(64), "block": "image" },
        ]))
        .unwrap();
        assert_eq!(
            resolve(&unknown, &attachments),
            Err(UnknownAttachment("0".repeat(64)))
        );
    }
}
