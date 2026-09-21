//! M02/M03 constrain redundant conversation retention, while M10/M12 protect
//! the current resources that still belong to the legacy projection. Counts
//! below are retained serialized payload bytes, not allocator or process RSS.

use super::*;
use crate::runtime_state::fold_active_turn_update;

const SESSION: &str = "memory-session";
const PROMPT: &str = "memory-prompt";

fn ingest(cache: &mut ActiveRuntimeProjection, event: Value) -> Value {
    serde_json::from_str(&cache.update_and_normalize(&event.to_string())).unwrap()
}

fn conversation(update: Value) -> Value {
    json!({
        "type": "acp/session_update",
        "notification": {"sessionId": SESSION, "update": update},
    })
}

fn running_cache() -> ActiveRuntimeProjection {
    let mut cache = ActiveRuntimeProjection::default();
    ingest(
        &mut cache,
        json!({
            "type": "acp/session_created", "cwd": "/workspace",
            "response": {"sessionId": SESSION, "modes": {
                "currentModeId": "build", "availableModes": [{"id": "build", "name": "Build"}],
            }},
        }),
    );
    ingest(
        &mut cache,
        json!({
            "type": "acp/prompt_started", "sessionId": SESSION, "requestId": PROMPT,
            "prompt": [{"type": "text", "text": "Retain current state"}],
        }),
    );
    cache
}

fn is_conversation_event(event: &str) -> bool {
    let event: Value = serde_json::from_str(event).unwrap();
    event["type"] == "acp/session_update"
        && matches!(
            event["notification"]["update"]["sessionUpdate"].as_str(),
            Some(
                "user_message_chunk"
                    | "agent_message_chunk"
                    | "agent_thought_chunk"
                    | "tool_call"
                    | "tool_call_update"
                    | "plan"
                    | "plan_update"
            )
        )
}

pub(super) fn raw_conversation_bytes(cache: &ActiveRuntimeProjection) -> usize {
    cache
        .sessions
        .values()
        .flat_map(|session| session.events.iter())
        .chain(cache.pending_session_events.values().flatten())
        .filter(|event| is_conversation_event(event))
        .map(String::len)
        .sum()
}

fn raw_event_bytes(cache: &ActiveRuntimeProjection) -> usize {
    let actual = cache
        .sessions
        .values()
        .flat_map(|session| session.events.iter())
        .map(String::len)
        .sum::<usize>();
    assert_eq!(
        actual,
        cache
            .sessions
            .values()
            .map(|session| session.event_bytes)
            .sum::<usize>()
    );
    actual
        + cache
            .pending_session_events
            .values()
            .flatten()
            .map(String::len)
            .sum::<usize>()
}

fn text_version(version: usize, bytes: usize, final_version: bool) -> String {
    let prefix = if final_version {
        "final:".to_string()
    } else {
        format!("version-{version}:")
    };
    assert!(bytes >= prefix.len());
    format!("{prefix}{}", "x".repeat(bytes - prefix.len()))
}

fn replace_tool_content(
    cache: &mut ActiveRuntimeProjection,
    versions: usize,
    final_bytes: usize,
    growing: bool,
) -> Vec<Value> {
    let first = ingest(
        cache,
        conversation(json!({
            "sessionUpdate": "tool_call", "toolCallId": "memory-tool", "title": "Output",
            "kind": "execute", "status": "in_progress", "content": [],
        })),
    );
    let mut folded = fold_active_turn_update(&[], &first["notification"]["update"]).unwrap();
    for version in 1..=versions {
        let bytes = if growing {
            final_bytes * version / versions
        } else {
            final_bytes
        };
        let update = json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "memory-tool",
            "status": "in_progress", "content": [{
                "type": "content", "content": {
                    "type": "text", "text": text_version(version, bytes, version == versions),
                },
            }],
        });
        let delivered = ingest(cache, conversation(update.clone()));
        assert_eq!(delivered["notification"]["update"], update);
        // The same delivered update must still reconstruct the complete current
        // conversation, independently of whether legacy raw replay retains it.
        folded = fold_active_turn_update(&folded, &delivered["notification"]["update"]).unwrap();
    }
    assert_eq!(folded.len(), 1);
    assert_eq!(folded[0]["sessionUpdate"], "tool_call");
    assert_eq!(folded[0]["toolCallId"], "memory-tool");
    assert_eq!(
        folded[0]["content"][0]["content"]["text"],
        text_version(versions, final_bytes, true)
    );
    folded
}

#[test]
fn memory_retention_m02_growing_tool_replacements_keep_only_current_canonical_content() {
    let retained = [32, 64, 128].map(|versions| {
        let mut cache = running_cache();
        replace_tool_content(&mut cache, versions, versions * 4096, true);
        assert!(cache.sessions[SESSION].active_prompt.is_some());
        assert_eq!(
            cache.prompt_sessions.get(PROMPT).map(String::as_str),
            Some(SESSION)
        );
        raw_conversation_bytes(&cache)
    });
    assert_eq!(
        retained,
        [0, 0, 0],
        "M02: canonical tools must not have another raw version archive"
    );
}

#[test]
fn memory_retention_m03_fixed_final_tool_has_update_count_independent_retention() {
    let retained = [8, 32, 128].map(|versions| {
        let mut cache = running_cache();
        replace_tool_content(&mut cache, versions, 64 * 1024, false);
        raw_event_bytes(&cache)
    });
    assert_eq!(
        retained, [retained[0]; 3],
        "M03: identical final content and resource identities must not retain more bytes after more replacements"
    );
}

#[test]
fn memory_retention_m02_message_chunks_remain_complete_outside_legacy_replay() {
    let mut cache = running_cache();
    let mut folded = Vec::new();
    let mut expected = String::new();
    for index in 0..512 {
        let text = format!("片段{index}🙂\n");
        expected.push_str(&text);
        let delivered = ingest(
            &mut cache,
            conversation(json!({
                "sessionUpdate": "agent_message_chunk", "messageId": "one-message",
                "content": {"type": "text", "text": text},
            })),
        );
        folded = fold_active_turn_update(&folded, &delivered["notification"]["update"]).unwrap();
    }
    assert_eq!(folded.len(), 1);
    assert_eq!(folded[0]["content"]["text"], expected);
    assert_eq!(
        raw_conversation_bytes(&cache),
        0,
        "full delivery must not create a second transcript"
    );
}

fn permission(id: &str) -> Value {
    json!({"type": "acp/permission_request", "permissionId": id, "request": {
        "sessionId": SESSION, "toolCall": {
            "toolCallId": "memory-tool", "title": "Confirm", "kind": "other", "status": "pending",
        }, "options": [{"optionId": "allow", "name": "Allow once", "kind": "allow_once"}],
    }})
}

fn form(id: &str) -> Value {
    json!({"type": "acp/elicitation_request", "elicitationId": id, "request": {
        "sessionId": SESSION, "mode": "form", "message": "Input",
        "requestedSchema": {"type": "object", "properties": {}},
    }})
}

fn url_request() -> Value {
    json!({"type": "acp/elicitation_request", "elicitationId": "bridge-url", "request": {
        "sessionId": SESSION, "mode": "url", "message": "Connect",
        "elicitationId": "agent-url", "url": "https://example.test/connect",
    }})
}

fn url_accepted() -> Value {
    json!({"type": "acp/elicitation_resolved", "elicitationId": "bridge-url",
        "requestId": "accept-url", "response": {"action": "accept"}})
}

fn operation() -> Value {
    json!({"type": "bridge/session_operation_started", "sessionId": SESSION,
        "requestId": "mode-operation", "operation": "mode"})
}

fn terminal_chunk(bytes: &[u8], retained_bytes: usize, released: bool) -> Value {
    json!({"type": "acp/terminal_state", "terminal": {
        "sessionId": SESSION, "terminalId": "memory-terminal", "output": "",
        "outputBytes": BASE64_STANDARD.encode(bytes), "outputAppend": true,
        "retainedBytes": retained_bytes, "truncated": false, "released": released,
    }})
}

fn install_resources(cache: &mut ActiveRuntimeProjection) {
    for event in [
        permission("permission"),
        form("form"),
        url_request(),
        url_accepted(),
        operation(),
    ] {
        ingest(cache, event);
    }
    ingest(cache, terminal_chunk(b"terminal output", 15, false));
}

fn replay_values(cache: &ActiveRuntimeProjection) -> Vec<Value> {
    cache
        .replay_session_events(SESSION)
        .iter()
        .map(|event| serde_json::from_str(event).unwrap())
        .collect()
}

#[test]
fn memory_retention_m10_live_resources_survive_conversation_replacements() {
    let mut cache = running_cache();
    install_resources(&mut cache);
    replace_tool_content(&mut cache, 16, 16 * 1024, true);
    let session = &cache.sessions[SESSION];
    assert_eq!(session.pending_permissions.len(), 1);
    assert_eq!(session.pending_elicitations.len(), 1);
    assert_eq!(session.active_url_flows.len(), 1);
    assert_eq!(
        cache
            .permission_sessions
            .get("permission")
            .map(String::as_str),
        Some(SESSION)
    );
    assert_eq!(
        cache
            .elicitation_sessions
            .get("form")
            .and_then(Option::as_deref),
        Some(SESSION)
    );
    assert_eq!(
        cache
            .url_elicitation_sessions
            .get("agent-url")
            .and_then(Option::as_deref),
        Some(SESSION)
    );
    assert_eq!(
        cache
            .operation_sessions
            .get("mode-operation")
            .map(String::as_str),
        Some(SESSION)
    );
    let replay = replay_values(&cache);
    for required in [
        permission("permission"),
        form("form"),
        url_request(),
        url_accepted(),
        operation(),
    ] {
        assert_eq!(
            replay.iter().filter(|event| **event == required).count(),
            1,
            "current resource must be recoverable exactly once: {required}"
        );
    }
    let terminals = replay
        .iter()
        .filter(|event| event["type"] == "acp/terminal_state")
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0]["terminal"]["output"], "terminal output");
}

#[test]
fn memory_retention_m10_duplicate_resolution_preserves_unrelated_current_interactions() {
    let mut cache = running_cache();
    for event in [
        permission("done-permission"),
        permission("keep-permission"),
        form("done-form"),
        form("keep-form"),
        url_request(),
        url_accepted(),
    ] {
        ingest(&mut cache, event);
    }
    for _ in 0..2 {
        ingest(
            &mut cache,
            json!({"type": "acp/permission_resolved", "permissionId": "done-permission"}),
        );
        ingest(
            &mut cache,
            json!({"type": "acp/elicitation_resolved", "elicitationId": "done-form", "response": {"action": "cancel"}}),
        );
        ingest(
            &mut cache,
            json!({"type": "acp/elicitation_complete", "notification": {"elicitationId": "agent-url"}}),
        );
    }
    let session = &cache.sessions[SESSION];
    assert_eq!(
        session
            .pending_permissions
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["keep-permission"]
    );
    assert_eq!(
        session
            .pending_elicitations
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["keep-form"]
    );
    assert!(session.active_url_flows.is_empty());
    assert!(!cache.permission_sessions.contains_key("done-permission"));
    assert!(!cache.elicitation_sessions.contains_key("done-form"));
    assert!(!cache.elicitation_sessions.contains_key("bridge-url"));
    assert!(cache.url_elicitation_sessions.is_empty());
    let replay = replay_values(&cache);
    assert_eq!(
        replay
            .iter()
            .filter(|event| **event == permission("keep-permission"))
            .count(),
        1
    );
    assert_eq!(
        replay
            .iter()
            .filter(|event| **event == form("keep-form"))
            .count(),
        1
    );
}

#[test]
fn memory_retention_m10_terminal_append_exit_and_release_keep_exact_current_output() {
    let mut cache = running_cache();
    let first = ingest(&mut cache, terminal_chunk(b"hello", 5, false));
    let second = ingest(&mut cache, terminal_chunk("🙂".as_bytes(), 9, false));
    assert_eq!(first["terminal"]["output"], "hello");
    assert_eq!(second["terminal"]["output"], "🙂");
    let mut exit = terminal_chunk(&[], 9, false);
    exit["terminal"]["exitStatus"] = json!({"exitCode": 0});
    ingest(&mut cache, exit);
    let terminals = replay_values(&cache)
        .into_iter()
        .filter(|event| event["type"] == "acp/terminal_state")
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0]["terminal"]["output"], "hello🙂");
    assert_eq!(terminals[0]["terminal"]["exitStatus"]["exitCode"], 0);
    assert_eq!(
        cache
            .terminal_output_bytes
            .values()
            .map(Vec::len)
            .sum::<usize>(),
        9
    );
    let released = ingest(&mut cache, terminal_chunk(&[], 9, true));
    assert_eq!(released["terminal"]["output"], "");
    assert!(cache.terminal_output_bytes.is_empty());
    assert!(cache.sessions[SESSION].terminal_states.is_empty());
    assert!(
        !replay_values(&cache)
            .iter()
            .any(|event| event["type"] == "acp/terminal_state")
    );
}

#[test]
fn memory_retention_m10_late_old_operation_result_cannot_clear_the_current_operation() {
    let mut cache = running_cache();
    ingest(&mut cache, operation());
    let old_result = json!({"type": "acp/mode_changed", "sessionId": SESSION,
        "requestId": "mode-operation", "modeId": "build"});
    ingest(&mut cache, old_result.clone());
    let current = json!({"type": "bridge/session_operation_started", "sessionId": SESSION,
        "requestId": "config-operation", "operation": "config"});
    ingest(&mut cache, current.clone());
    replace_tool_content(&mut cache, 8, 8192, true);
    ingest(&mut cache, old_result);
    let retained: Value =
        serde_json::from_str(cache.sessions[SESSION].active_operation.as_deref().unwrap()).unwrap();
    assert_eq!(retained, current);
    assert_eq!(cache.operation_sessions.len(), 1);
    assert_eq!(
        cache
            .operation_sessions
            .get("config-operation")
            .map(String::as_str),
        Some(SESSION)
    );
    assert_eq!(
        replay_values(&cache)
            .iter()
            .filter(|event| **event == current)
            .count(),
        1
    );
    ingest(
        &mut cache,
        json!({"type": "acp/config_changed", "sessionId": SESSION,
        "requestId": "config-operation", "configId": "profile", "value": "fast"}),
    );
    assert!(cache.sessions[SESSION].active_operation.is_none());
    assert!(cache.operation_sessions.is_empty());
}

fn settle_resources(cache: &mut ActiveRuntimeProjection) {
    for event in [
        json!({"type": "acp/permission_resolved", "permissionId": "permission"}),
        json!({"type": "acp/elicitation_resolved", "elicitationId": "form", "response": {"action": "cancel"}}),
        json!({"type": "acp/elicitation_aborted", "elicitationId": "agent-url", "sessionId": SESSION, "reason": "runtime_terminal"}),
        json!({"type": "acp/mode_changed", "sessionId": SESSION, "requestId": "mode-operation", "modeId": "build"}),
        terminal_chunk(&[], 15, true),
    ] {
        ingest(cache, event);
    }
}

fn assert_turn_resources_released(cache: &ActiveRuntimeProjection) {
    assert_eq!(raw_event_bytes(cache), 0);
    assert_eq!(raw_conversation_bytes(cache), 0);
    assert!(cache.pending_session_events.is_empty());
    assert!(cache.prompt_sessions.is_empty());
    assert!(cache.operation_sessions.is_empty());
    assert!(cache.permission_sessions.is_empty());
    assert!(cache.elicitation_sessions.is_empty());
    assert!(cache.url_elicitation_sessions.is_empty());
    assert!(cache.terminal_output_bytes.is_empty());
    for session in cache.sessions.values() {
        assert!(session.events.is_empty());
        assert!(session.active_prompt.is_none());
        assert!(session.active_operation.is_none());
        assert!(session.pending_permissions.is_empty());
        assert!(session.pending_elicitations.is_empty());
        assert!(session.active_url_flows.is_empty());
        assert!(session.terminal_states.is_empty());
    }
}

fn prompt_terminal_path(failed: bool, cancelled: bool) {
    let mut cache = running_cache();
    install_resources(&mut cache);
    replace_tool_content(&mut cache, 16, 16 * 1024, true);
    // The bridge emits these resource outcomes before the prompt terminal event;
    // legacy projection does not own or invoke the actual ACP responders.
    settle_resources(&mut cache);
    let terminal = if failed {
        json!({"type": "bridge/error", "operation": "session/prompt", "requestId": PROMPT,
            "message": "Agent prompt failed", "code": -32603})
    } else {
        json!({"type": "acp/prompt_complete", "sessionId": SESSION, "requestId": PROMPT,
            "response": {"stopReason": if cancelled { "cancelled" } else { "end_turn" }}})
    };
    ingest(&mut cache, terminal);
    assert_turn_resources_released(&cache);
    assert_eq!(
        cache.sessions.len(),
        1,
        "prompt completion must not retire the session shell"
    );
    assert_eq!(
        cache.sessions[SESSION].session["modes"]["currentModeId"],
        "build"
    );
}

#[test]
fn memory_retention_m12_completed_prompt_releases_raw_turn_and_settled_resources() {
    prompt_terminal_path(false, false);
}

#[test]
fn memory_retention_m12_failed_prompt_releases_raw_turn_and_settled_resources() {
    prompt_terminal_path(true, false);
}

#[test]
fn memory_retention_m12_cancelled_prompt_response_releases_raw_turn_and_settled_resources() {
    prompt_terminal_path(false, true);
}

#[test]
fn memory_retention_m12_session_teardown_releases_every_owned_legacy_payload() {
    for kind in [
        "acp/session_closed",
        "acp/session_deleted",
        "bridge/session_retired",
    ] {
        let mut cache = running_cache();
        install_resources(&mut cache);
        replace_tool_content(&mut cache, 16, 16 * 1024, true);
        ingest(
            &mut cache,
            json!({"type": kind, "sessionId": SESSION,
            "bridgeEpoch": "memory-epoch", "sessionIncarnation": 1, "reason": "closed"}),
        );
        assert_turn_resources_released(&cache);
        assert!(cache.sessions.is_empty());
        // A payload-free dead-letter identity is deliberately retained.
        assert!(cache.retired_sessions.contains(SESSION));
    }
}
