//! Attachment links are a browser projection of the existing session cache.
//! No attachment store, upload directory, or independent lifetime is introduced.
use std::io::Write;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::session_resources::SessionResourceOwner;

pub(crate) const REFERENCE_KEY: &str = "attyd/attachment";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AttachmentReference {
    pub id: String,
    pub session_id: String,
    pub bridge_epoch: String,
    pub session_incarnation: u64,
}

impl AttachmentReference {
    pub(crate) fn owner(&self) -> SessionResourceOwner {
        SessionResourceOwner::new(
            &self.bridge_epoch,
            &self.session_id,
            self.session_incarnation,
        )
    }
}

pub(crate) fn reference(block: &Value) -> Option<AttachmentReference> {
    if block["type"] != "resource_link" {
        return None;
    }
    serde_json::from_value(block.get("_meta")?.get(REFERENCE_KEY)?.clone()).ok()
}

fn resource(block: &Value) -> Option<&Value> {
    match block["type"].as_str()? {
        "image" | "audio" => Some(block),
        "resource" => block.get("resource"),
        _ => None,
    }
}

fn mime_type(resource: &Value) -> &str {
    resource["mimeType"]
        .as_str()
        .unwrap_or(if resource["text"].is_string() {
            "text/plain"
        } else {
            "application/octet-stream"
        })
}

fn name(resource: &Value) -> String {
    if let Some(uri) = resource["uri"].as_str()
        && let Some(segment) = uri
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .rsplit('/')
            .find(|part| !part.is_empty())
    {
        // Form decoding supplies percent decoding; protect literal form separators.
        let encoded = format!("name={}", segment.replace('+', "%2B").replace('&', "%26"));
        let decoded = url::form_urlencoded::parse(encoded.as_bytes())
            .next()
            .unwrap()
            .1;
        let name: String = decoded
            .chars()
            .map(|c| {
                if c.is_control() || matches!(c, '/' | '\\') {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        if !matches!(name.as_str(), "" | "." | "..") {
            return name;
        }
    }
    let subtype = mime_type(resource)
        .split(';')
        .next()
        .unwrap_or("")
        .split('/')
        .nth(1)
        .unwrap_or("");
    if !subtype.is_empty()
        && subtype != "octet-stream"
        && subtype.bytes().all(|c| c.is_ascii_alphanumeric())
    {
        format!("attachment.{subtype}")
    } else {
        "attachment".into()
    }
}

fn id(block: &Value) -> String {
    struct HashWriter(Sha256);
    impl Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, block).expect("JSON content block serializes");
    writer
        .0
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn project_block(block: &mut Value, owner: &SessionResourceOwner) {
    let Some(resource) = resource(block) else {
        return;
    };
    let bytes = resource["text"].as_str().map(str::len).unwrap_or_else(|| {
        let data = resource["data"]
            .as_str()
            .or_else(|| resource["blob"].as_str())
            .unwrap_or("");
        (data.len() / 4 * 3).saturating_sub(data.bytes().rev().take_while(|c| *c == b'=').count())
    });
    let reference = AttachmentReference {
        id: id(block),
        session_id: owner.session_id.clone(),
        bridge_epoch: owner.epoch.clone(),
        session_incarnation: owner.incarnation,
    };
    let mut projected = json!({
        "type": "resource_link", "name": name(resource), "size": bytes,
        "uri": resource["uri"].as_str().map(str::to_owned).unwrap_or_else(|| format!("attyd://attachment/{}", reference.id)),
        "mimeType": mime_type(resource), "_meta": { REFERENCE_KEY: reference },
    });
    if let Some(annotations) = block.get("annotations") {
        projected["annotations"] = annotations.clone();
    }
    *block = projected;
}

fn tool_blocks(call: &mut Value, visit: &mut impl FnMut(&mut Value)) {
    if let Some(contents) = call.get_mut("content").and_then(Value::as_array_mut) {
        for content in contents {
            if content["type"] == "content"
                && let Some(block) = content.get_mut("content")
            {
                visit(block);
            }
        }
    }
}

fn update_blocks(update: &mut Value, visit: &mut impl FnMut(&mut Value)) {
    match update["sessionUpdate"].as_str() {
        Some(
            "user_message_chunk"
            | "agent_message_chunk"
            | "agent_thought_chunk"
            | "compaction_summary_chunk",
        ) => {
            if let Some(block) = update.get_mut("content") {
                visit(block);
            }
        }
        Some("tool_call" | "tool_call_update") => tool_blocks(update, visit),
        _ => {}
    }
}

/// Only ACP content positions are visited; opaque tool input/output and extension data stay intact.
fn blocks(value: &mut Value, visit: &mut impl FnMut(&mut Value)) {
    for path in [
        "/timeline",
        "/activeTurn/updates",
        "/baseline/updates",
        "/session/activeTurn/updates",
    ] {
        if let Some(updates) = value.pointer_mut(path).and_then(Value::as_array_mut) {
            for update in updates {
                update_blocks(update, visit);
            }
        }
    }
    for path in [
        "/prompt",
        "/activeTurn/prompt",
        "/session/activeTurn/prompt",
    ] {
        if let Some(prompt) = value.pointer_mut(path).and_then(Value::as_array_mut) {
            for block in prompt {
                visit(block);
            }
        }
    }
    if let Some(items) = value.get_mut("items").and_then(Value::as_array_mut) {
        for item in items {
            if let Some(updates) = item.as_array_mut() {
                for update in updates {
                    update_blocks(update, visit);
                }
            }
        }
    }
    if let Some(update) = value.pointer_mut("/change/update") {
        update_blocks(update, visit);
    }
    for path in ["/interactions/permissions", "/live/permissions"] {
        if let Some(permissions) = value.pointer_mut(path).and_then(Value::as_object_mut) {
            for permission in permissions.values_mut() {
                if let Some(call) = permission.pointer_mut("/request/toolCall") {
                    tool_blocks(call, visit);
                }
            }
        }
    }
    if let Some(call) = value.pointer_mut("/change/interaction/request/toolCall") {
        tool_blocks(call, visit);
    }
}

pub(crate) fn project(value: &mut Value) {
    let (Some(epoch), Some(session), Some(incarnation)) = (
        value["bridgeEpoch"].as_str(),
        value["sessionId"].as_str(),
        value["sessionIncarnation"].as_u64(),
    ) else {
        return;
    };
    let owner = SessionResourceOwner::new(epoch, session, incarnation);
    blocks(value, &mut |block| project_block(block, &owner));
}

pub(crate) fn find(view: &mut Value, attachment_id: &str) -> Option<Value> {
    let mut found = None;
    blocks(view, &mut |block| {
        if found.is_none() && resource(block).is_some() && id(block) == attachment_id {
            found = Some(block.clone());
        }
    });
    found
}

pub(crate) struct AttachmentFile {
    pub name: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

pub(crate) fn file(block: &Value) -> Option<AttachmentFile> {
    let resource = resource(block)?;
    let mut mime = mime_type(resource).to_string();
    let bytes = if let Some(text) = resource["text"].as_str() {
        let (essence, parameters) = crate::semantic::parse_mime_type(&mime)?;
        let mut utf8_mime = essence.to_string();
        for (name, value) in parameters {
            if !name.eq_ignore_ascii_case("charset") {
                utf8_mime.push_str(&format!(";{name}={value}"));
            }
        }
        utf8_mime.push_str(";charset=utf-8");
        mime = utf8_mime;
        text.as_bytes().to_vec()
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(
                resource["data"]
                    .as_str()
                    .or_else(|| resource["blob"].as_str())?,
            )
            .ok()?
    };
    Some(AttachmentFile {
        name: name(resource),
        mime_type: mime,
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_keeps_only_metadata_and_resolves_exact_original_blocks() {
        let originals = vec![
            json!({"type":"resource","resource":{"uri":"attyd://attachment/%E4%B8%AD%E6%96%87%20a+b.md","mimeType":"text/markdown","text":"中文正文"},"annotations":{"priority":0.5}}),
            json!({"type":"image","mimeType":"image/png","data":"AQID"}),
            json!({"type":"audio","mimeType":"audio/wav","data":"AQID"}),
            json!({"type":"resource","resource":{"uri":"file:///report.pdf","mimeType":"application/pdf","blob":"AQID"}}),
        ];
        let mut original = json!({"bridgeEpoch":"epoch","sessionId":"session","sessionIncarnation":1,
            "activeTurn":{"prompt":originals,"updates":[]}});
        let mut projected = original.clone();
        project(&mut projected);
        assert!(!projected.to_string().contains("中文正文"));
        assert!(!projected.to_string().contains("AQID"));
        for (index, block) in projected["activeTurn"]["prompt"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let reference = reference(block).expect("host-owned reference");
            assert_eq!(
                find(&mut original, &reference.id).unwrap(),
                originals[index]
            );
            assert_eq!(block["type"], "resource_link");
            assert_eq!(
                reference.owner(),
                SessionResourceOwner::new("epoch", "session", 1)
            );
        }
        assert_eq!(projected["activeTurn"]["prompt"][0]["name"], "中文 a+b.md");
        assert_eq!(projected["activeTurn"]["prompt"][0]["size"], 12);
        assert_eq!(
            projected["activeTurn"]["prompt"][0]["annotations"]["priority"],
            0.5
        );
        assert!(find(&mut original, &"0".repeat(64)).is_none());
    }

    #[test]
    fn history_process_events_and_permissions_project_only_actual_content() {
        let block =
            json!({"type":"resource","resource":{"uri":"file:///note","text":"attachment-body"}});
        let call = json!({"sessionUpdate":"tool_call","toolCallId":"t","title":"tool",
            "rawInput":block,"rawOutput":block,"content":[{"type":"content","content":block}]});
        for mut payload in [
            json!({"timeline":[{"sessionUpdate":"user_message_chunk","content":block}]}),
            json!({"items":[[call.clone()]]}),
            json!({"change":{"update":call.clone()}}),
            json!({"interactions":{"permissions":{"p":{"request":{"toolCall":call.clone()}}}}}),
            json!({"prompt":[block.clone()]}),
        ] {
            payload["bridgeEpoch"] = json!("epoch");
            payload["sessionId"] = json!("session");
            payload["sessionIncarnation"] = json!(1);
            project(&mut payload);
            let mut count = 0;
            blocks(&mut payload, &mut |block| {
                assert!(reference(block).is_some());
                count += 1;
            });
            assert_eq!(count, 1);
        }
        let mut event = json!({"bridgeEpoch":"epoch","sessionId":"session","sessionIncarnation":1,"change":{"update":call}});
        project(&mut event);
        assert_eq!(event["change"]["update"]["rawInput"], block);
        assert_eq!(event["change"]["update"]["rawOutput"], block);
    }

    #[test]
    fn serves_unicode_text_as_utf8_and_binary_bytes_unchanged() {
        for (mime, expected) in [
            ("text/markdown", "text/markdown;charset=utf-8"),
            ("text/html;charset=iso-8859-1", "text/html;charset=utf-8"),
            ("text/plain;CHARSET=gb18030", "text/plain;charset=utf-8"),
            (
                "Text/Plain \t; CHARSET=\"gb18030\"",
                "Text/Plain;charset=utf-8",
            ),
            (
                "text/plain; charset=gb18030; CHARSET=\"latin1\"",
                "text/plain;charset=utf-8",
            ),
            (
                r#"text/plain; note="keep; charset=original"; CHARSET="gb18030""#,
                r#"text/plain;note="keep; charset=original";charset=utf-8"#,
            ),
            (
                r#"text/plain; note="keep\"; charset=original"; CHARSET="gb18030""#,
                r#"text/plain;note="keep\"; charset=original";charset=utf-8"#,
            ),
        ] {
            let file = file(&json!({"type":"resource","resource":{"uri":"file:///中文.txt","mimeType":mime,"text":"中文 café 📝"}})).unwrap();
            assert_eq!(file.mime_type, expected);
            assert_eq!(file.bytes, "中文 café 📝".as_bytes());
        }
        for mime in [
            "text/plain;charset=gb18030",
            "Text/Plain \t; CHARSET=\"gb18030\"; label=\"a;b\"",
        ] {
            let file = file(&json!({"type":"resource","resource":{"uri":"file:///note","mimeType":mime,"blob":"1tDOxA=="}})).unwrap();
            assert_eq!(file.mime_type, mime);
            assert_eq!(file.bytes, vec![0xd6, 0xd0, 0xce, 0xc4]);
        }
    }
}
