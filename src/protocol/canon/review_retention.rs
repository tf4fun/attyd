//! Regression constraints for attachment lifetime when a retained entity changes.

use super::{AttachmentRef, Attachments, Block, PromptBlock, TurnLog, resolve};
use serde_json::{Value, json};

fn image() -> Value {
    json!({ "type": "image", "data": "AQID", "mimeType": "image/png", "_meta": { "source": "test" } })
}

fn reference(block: &Value) -> AttachmentRef {
    let Block::Attachment(reference) = Attachments::default().block(block).unwrap() else {
        panic!("an image has an attachment reference")
    };
    reference
}

fn prompt(text: &str) -> Vec<Value> {
    vec![json!({ "type": "text", "text": text })]
}

fn tool_with_image(block: &Value) -> Value {
    json!({
        "sessionUpdate": "tool_call", "toolCallId": "t", "title": "Picture",
        "content": [{ "type": "content", "content": block }],
    })
}

fn clear_tool() -> Value {
    json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t", "content": [] })
}

#[test]
fn rewriting_a_tool_in_an_earlier_turn_releases_its_unreferenced_attachment() {
    let block = image();
    let reference = reference(&block);
    let mut log = TurnLog::default();
    log.commit(
        &prompt("first"),
        &[tool_with_image(&block)],
        &json!({ "stopReason": "end_turn" }),
    );
    assert_eq!(log.attachments().get(&reference.id), Some(&block));

    log.commit(
        &prompt("second"),
        &[clear_tool()],
        &json!({ "stopReason": "end_turn" }),
    );
    let first = serde_json::to_value(&log.entries()[0]).unwrap();
    assert_eq!(first["turn"]["items"][1]["call"]["content"], json!([]));
    assert_eq!(
        log.attachments().get(&reference.id),
        None,
        "the rewritten log no longer references this attachment"
    );
    assert!(resolve(&[PromptBlock::Attachment(reference)], log.attachments()).is_err());
}

#[test]
fn replaying_a_cleared_compaction_summary_releases_its_unreferenced_attachment() {
    let block = image();
    let reference = reference(&block);
    let log = TurnLog::from_replay(&[
        json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "q" } }),
        json!({ "sessionUpdate": "compaction_update", "compactionId": "c", "status": "in_progress", "summary": [block] }),
        json!({ "sessionUpdate": "compaction_update", "compactionId": "c", "status": "completed", "summary": null }),
    ]);
    let entry = serde_json::to_value(&log.entries()[0]).unwrap();
    assert_eq!(entry["turn"]["items"][1]["summary"], json!([]));
    assert_eq!(
        log.attachments().get(&reference.id),
        None,
        "a summary's replaced content is no longer retained content"
    );
}

#[test]
fn rewriting_a_tool_keeps_an_attachment_still_referenced_by_a_message() {
    let block = image();
    let reference = reference(&block);
    let mut log = TurnLog::default();
    log.commit(
        &prompt("first"),
        &[
            tool_with_image(&block),
            json!({ "sessionUpdate": "agent_message_chunk", "content": block }),
        ],
        &json!({ "stopReason": "end_turn" }),
    );
    log.commit(
        &prompt("second"),
        &[clear_tool()],
        &json!({ "stopReason": "end_turn" }),
    );
    let first = serde_json::to_value(&log.entries()[0]).unwrap();
    assert_eq!(first["turn"]["items"][1]["call"]["content"], json!([]));
    assert_eq!(
        first["turn"]["items"][2]["chunks"][0]["content"]["id"],
        reference.id
    );
    assert_eq!(log.attachments().get(&reference.id), Some(&block));
    assert_eq!(
        resolve(&[PromptBlock::Attachment(reference)], log.attachments()).unwrap(),
        vec![block]
    );
}
