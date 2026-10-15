//! The content blocks of ACP updates: protocol projection visits them to
//! reference attachments.
use serde_json::Value;

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

pub(crate) fn update_blocks(update: &mut Value, visit: &mut impl FnMut(&mut Value)) {
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
        Some("compaction_update") => {
            if let Some(summary) = update.get_mut("summary").and_then(Value::as_array_mut) {
                summary.iter_mut().for_each(&mut *visit);
            }
        }
        _ => {}
    }
}
