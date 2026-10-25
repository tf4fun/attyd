//! Direct cases for published entries and attachments (docs/protocol-r4-design.md
//! §4). Expected attachment IDs were computed by an independent
//! implementation (Python `hashlib` over the exact form), not by this module.

use serde_json::{Value, json};

use super::super::canon::canonical_json;
use super::super::timeline::Timeline;
use super::*;

const IMAGE_ID: &str = "caf76d0a16892b770bf06da2da02838e5294dd0d4be88c85ff2efb98a0591f79";
const BIG_1_ID: &str = "d15636b2347f6832956094024569ce82a7dc8e2c9fbb8f6f5d7ab16fbfc354ee";
const BIG_2_ID: &str = "a4b59695f415f0f1f993826bc78e1ec5576c5c97127c2b38c9b310201c423a03";
const FLOAT_ID: &str = "0356203878c57afc909e8c9913689ee9c8e709915f05a841035edc787f878754";
const RESOURCE_ID: &str = "61c2aa83e3b13970a9ae42beb9aa3a56ca4ef82aa21f32ed246a851e226d4359";
const AUDIO_ID: &str = "c971a664295fe73ee3dd28e481f29f7864dd2607e44bf9b7af96efae4e2a52ce";

fn text(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn image() -> Value {
    json!({ "type": "image", "data": "AAAA", "mimeType": "image/png" })
}

fn timeline(updates: &[Value]) -> Timeline {
    Timeline::from_replay(updates.iter().map(|update| (update, None)))
}

// --- Exact form and attachment identity ---------------------------------------------

#[test]
fn the_exact_form_orders_keys_and_escapes_as_jcs_does() {
    let block = json!({ "b": "é\n\"", "a": [1, true, null], "type": "x" });
    assert_eq!(
        String::from_utf8(exact_form(&block)).unwrap(),
        "{\"a\":[1,true,null],\"b\":\"é\\n\\\"\",\"type\":\"x\"}"
    );
}

#[test]
fn an_attachment_id_is_the_hash_of_the_exact_form() {
    assert_eq!(attachment_id(&image()), IMAGE_ID);
    assert_eq!(
        attachment_id(&json!({ "type": "audio", "data": "BBBB", "mimeType": "audio/wav" })),
        AUDIO_ID
    );
    assert_eq!(
        attachment_id(&json!({ "type": "image", "data": "AAAA", "mimeType": "image/png",
                               "annotations": { "priority": 0.1 } })),
        FLOAT_ID,
        "a float is written as its parsed f64"
    );
}

#[test]
fn integers_beyond_two_to_the_fifty_three_keep_attachments_apart() {
    let one: Value = serde_json::from_str(
        r#"{"type":"image","data":"AAAA","mimeType":"image/png","_meta":{"n":9007199254740992}}"#,
    )
    .unwrap();
    let two: Value = serde_json::from_str(
        r#"{"type":"image","data":"AAAA","mimeType":"image/png","_meta":{"n":9007199254740993}}"#,
    )
    .unwrap();
    assert_eq!(attachment_id(&one), BIG_1_ID);
    assert_eq!(attachment_id(&two), BIG_2_ID);
    // The document digest works over doubles and rounds both to the same text.
    assert_eq!(canonical_json(&one), canonical_json(&two));
}

// --- Publishing blocks ---------------------------------------------------------------

#[test]
fn text_and_resource_links_stay_inline_with_their_metadata() {
    let mut index = AttachmentIndex::default();
    let blocks = [
        json!({ "type": "text", "text": "hi", "_meta": { "k": 1 },
                "annotations": { "audience": ["user"] } }),
        json!({ "type": "resource_link", "uri": "file:///a", "name": "a" }),
    ];
    for block in &blocks {
        assert_eq!(&publish_block(block, &mut index), block);
    }
    assert_eq!(index.len(), 0);
}

#[test]
fn byte_blocks_become_references_and_the_index_keeps_them_as_received() {
    let mut index = AttachmentIndex::default();
    let resource = json!({ "type": "resource", "resource": {
        "uri": "file:///a.txt", "mimeType": "text/plain", "text": "hello" } });
    let audio = json!({ "type": "audio", "data": "BBBB", "mimeType": "audio/wav" });
    assert_eq!(
        publish_block(&image(), &mut index),
        json!({ "type": "attachment", "id": IMAGE_ID, "block": "image", "mimeType": "image/png" })
    );
    assert_eq!(
        publish_block(&audio, &mut index),
        json!({ "type": "attachment", "id": AUDIO_ID, "block": "audio", "mimeType": "audio/wav" })
    );
    assert_eq!(
        publish_block(&resource, &mut index),
        json!({ "type": "attachment", "id": RESOURCE_ID, "block": "resource",
                "mimeType": "text/plain", "uri": "file:///a.txt" })
    );
    assert_eq!(index.get(IMAGE_ID), Some(&image()));
    assert_eq!(index.get(AUDIO_ID), Some(&audio));
    assert_eq!(index.get(RESOURCE_ID), Some(&resource));
    assert_eq!(index.get("unknown"), None);
}

// --- Publishing entries --------------------------------------------------------------

#[test]
fn every_position_that_carries_a_block_is_published_as_a_reference() {
    let timeline = timeline(&[
        json!({ "sessionUpdate": "agent_message_chunk", "messageId": "m", "content": image(),
                "_meta": { "chunk": 1 } }),
        json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "Run",
                "content": [
                    { "type": "content", "content": image() },
                    { "type": "terminal", "terminalId": "term" },
                ] }),
        json!({ "sessionUpdate": "compaction_update", "compactionId": "c", "status": "completed",
                "summary": [image(), text("kept")] }),
        json!({ "sessionUpdate": "plan_update", "plan": { "planId": "p", "entries": [] } }),
    ]);
    let mut index = AttachmentIndex::default();
    let reference = json!({ "type": "attachment", "id": IMAGE_ID, "block": "image",
                            "mimeType": "image/png" });
    let entries: Vec<_> = timeline.entries().iter().map(|entry| publish_entry(entry, &mut index)).collect();

    assert_eq!(entries[0]["chunks"][0]["content"], reference);
    assert_eq!(entries[0]["chunks"][0]["_meta"], json!({ "chunk": 1 }), "chunk metadata stays");
    assert_eq!(entries[1]["call"]["content"][0]["content"], reference);
    assert_eq!(entries[1]["call"]["content"][1], json!({ "type": "terminal", "terminalId": "term" }));
    assert_eq!(entries[2]["summary"][0]["content"], reference);
    assert_eq!(entries[2]["summary"][1]["content"], text("kept"));
    assert_eq!(entries[3]["kind"], "plan");
    assert_eq!(index.len(), 1, "one image shared by three positions is kept once");
}
