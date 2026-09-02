use std::collections::{HashMap, HashSet};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::Serialize;
use serde_json::Value;

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_CONTENT_BINARY_BYTES: usize = 3 * 1024 * 1024;
const MAX_URI_LENGTH: usize = 16_384;
const MAX_MIME_TYPE_LENGTH: usize = 255;
const MAX_RESOURCE_LABEL_LENGTH: usize = 16_384;
const MAX_ANNOTATION_TIMESTAMP_LENGTH: usize = 16_384;

const MAX_PROMPT_RESPONSE_BYTES: usize = 1_000_000;
const MAX_SESSION_CONTROLS: usize = 256;
const MAX_SELECT_VALUES: usize = 512;
const MAX_CONTROL_BYTES: usize = 1_000_000;
const MAX_CONTROL_IDENTIFIER_LENGTH: usize = 256;
const MAX_CONTROL_LABEL_LENGTH: usize = 4_096;

const MAX_UPDATE_BYTES: usize = 4_000_000;
const MAX_IDENTIFIER_LENGTH: usize = 1_024;
const MAX_AVAILABLE_COMMANDS: usize = 1_000;
const MAX_PLAN_ENTRIES: usize = 1_000;
const MAX_TOOL_COLLECTION_ITEMS: usize = 10_000;
const MAX_COMPACTIONS: usize = 1_000;
const MAX_TOOL_CALLS: usize = 10_000;
const MAX_MESSAGES: usize = 10_000;
const MAX_PLANS: usize = 1_000;
const MAX_COMPACTION_SUMMARY_BYTES: usize = 3_000_000;
const MAX_SESSION_UPDATES: usize = 100_000;
const MAX_SESSION_UPDATE_BYTES: usize = 128_000_000;
const MAX_SESSION_TITLE_LENGTH: usize = 16_384;
const MAX_TOOL_PATH_LENGTH: usize = 16_384;
const MAX_TOOL_LABEL_LENGTH: usize = 16_384;
const MAX_TOOL_LOCATION_LINE: u64 = u32::MAX as u64;
const MAX_PERMISSION_OPTIONS: usize = 100;
const MAX_PERMISSION_REQUEST_BYTES: usize = 1_000_000;
const MAX_PERMISSION_OPTION_NAME_LENGTH: usize = 4_096;

pub type ValidationResult<T = ()> = Result<T, String>;

#[derive(Clone, Default)]
pub struct SessionUpdateSemanticState {
    compactions: HashMap<String, TrackedCompaction>,
    tool_calls: HashMap<String, Option<String>>,
    messages: HashMap<String, String>,
    plans: HashMap<String, String>,
    pub update_count: usize,
    pub update_bytes: usize,
    pub current_mode_id: Option<String>,
    pub invalid_reason: Option<String>,
}

impl SessionUpdateSemanticState {
    /// Retire validation indexes that are meaningful only while a turn is live.
    /// Session-scoped control state remains available for validating later commands.
    pub fn retire_turn(&mut self) {
        self.compactions.clear();
        self.tool_calls.clear();
        self.messages.clear();
        self.plans.clear();
        self.update_count = 0;
        self.update_bytes = 0;
        self.invalid_reason = None;
    }
}

#[derive(Clone)]
struct TrackedCompaction {
    status: String,
    summary_bytes: usize,
}

pub fn serialized_bytes(value: &impl Serialize) -> ValidationResult<usize> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|error| format!("failed to serialize ACP value: {error}"))
}

pub fn validate_content_block(block: &Value, subject: &str) -> ValidationResult {
    let object = block
        .as_object()
        .ok_or_else(|| format!("{subject} must be an object"))?;
    validate_annotations(object.get("annotations"), &format!("{subject} annotations"))?;
    match object.get("type").and_then(Value::as_str) {
        Some("text") => Ok(()),
        Some("image") => {
            validate_base64(
                required_string(block, "data", subject)?,
                &format!("{subject} image data"),
            )?;
            validate_mime_type(
                required_string(block, "mimeType", subject)?,
                &format!("{subject} image MIME type"),
                Some("image"),
            )?;
            if let Some(uri) = optional_string(block, "uri", subject)? {
                validate_uri(uri, &format!("{subject} image URI"))?;
            }
            Ok(())
        }
        Some("audio") => {
            validate_base64(
                required_string(block, "data", subject)?,
                &format!("{subject} audio data"),
            )?;
            validate_mime_type(
                required_string(block, "mimeType", subject)?,
                &format!("{subject} audio MIME type"),
                Some("audio"),
            )
        }
        Some("resource_link") => {
            validate_uri(
                required_string(block, "uri", subject)?,
                &format!("{subject} resource URI"),
            )?;
            validate_label(
                required_string(block, "name", subject)?,
                &format!("{subject} resource name"),
            )?;
            for (field, label) in [
                ("title", "resource title"),
                ("description", "resource description"),
            ] {
                if let Some(value) = optional_string(block, field, subject)? {
                    validate_label(value, &format!("{subject} {label}"))?;
                }
            }
            if let Some(mime_type) = optional_string(block, "mimeType", subject)? {
                validate_mime_type(mime_type, &format!("{subject} resource MIME type"), None)?;
            }
            if let Some(size) = object.get("size").filter(|value| !value.is_null())
                && size.as_u64().is_none_or(|value| value > MAX_SAFE_INTEGER)
            {
                return Err(format!(
                    "{subject} resource size must be a non-negative safe integer"
                ));
            }
            Ok(())
        }
        Some("resource") => {
            let resource = object
                .get("resource")
                .and_then(Value::as_object)
                .ok_or_else(|| format!("{subject} embedded resource must be an object"))?;
            validate_uri(
                resource
                    .get("uri")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("{subject} embedded resource URI is missing"))?,
                &format!("{subject} embedded resource URI"),
            )?;
            if let Some(mime_type) = resource.get("mimeType").and_then(Value::as_str) {
                validate_mime_type(
                    mime_type,
                    &format!("{subject} embedded resource MIME type"),
                    None,
                )?;
            }
            if let Some(blob) = resource.get("blob").and_then(Value::as_str) {
                validate_base64(blob, &format!("{subject} embedded resource blob"))?;
            }
            Ok(())
        }
        Some(kind) => Err(format!("unsupported {subject} type: {kind}")),
        None => Err(format!("{subject} type is missing")),
    }
}

pub fn validate_prompt_response(response: &impl Serialize) -> ValidationResult {
    let value = serde_json::to_value(response)
        .map_err(|error| format!("failed to serialize Agent prompt response: {error}"))?;
    if serialized_bytes(&value)? > MAX_PROMPT_RESPONSE_BYTES {
        return Err(format!(
            "Agent prompt response exceeds {MAX_PROMPT_RESPONSE_BYTES} bytes"
        ));
    }
    if let Some(usage) = value.get("usage").filter(|value| !value.is_null()) {
        validate_prompt_usage(usage)?;
    }
    Ok(())
}

fn validate_prompt_usage(usage: &Value) -> ValidationResult {
    let total = safe_non_negative_integer(usage.get("totalTokens"), "totalTokens")?
        .ok_or_else(|| "Agent prompt usage totalTokens is required".to_string())?;
    let input = safe_non_negative_integer(usage.get("inputTokens"), "inputTokens")?.unwrap_or(0);
    let output = safe_non_negative_integer(usage.get("outputTokens"), "outputTokens")?.unwrap_or(0);
    for name in [
        "inputTokens",
        "outputTokens",
        "thoughtTokens",
        "cachedReadTokens",
        "cachedWriteTokens",
    ] {
        if let Some(value) = safe_non_negative_integer(usage.get(name), name)?
            && value > total
        {
            return Err(format!("Agent prompt usage {name} exceeds totalTokens"));
        }
    }
    if input.saturating_add(output) > total {
        return Err(
            "Agent prompt usage inputTokens plus outputTokens exceeds totalTokens".to_string(),
        );
    }
    Ok(())
}

pub fn validate_session_controls(
    modes: Option<&Value>,
    config_options: Option<&Value>,
) -> ValidationResult {
    if let Some(modes) = modes.filter(|value| !value.is_null()) {
        validate_session_modes(modes)?;
    }
    if let Some(options) = config_options.filter(|value| !value.is_null()) {
        validate_session_config_options(options)?;
    }
    let combined = serde_json::json!({
        "modes": modes.cloned().unwrap_or(Value::Null),
        "configOptions": config_options.cloned().unwrap_or_else(|| Value::Array(Vec::new())),
    });
    if serialized_bytes(&combined)? > MAX_CONTROL_BYTES {
        return Err(format!(
            "Agent session controls exceed the {MAX_CONTROL_BYTES} byte limit"
        ));
    }
    Ok(())
}

pub fn validate_session_modes(modes: &Value) -> ValidationResult {
    let available = modes
        .get("availableModes")
        .and_then(Value::as_array)
        .ok_or_else(|| "Agent session modes are missing availableModes".to_string())?;
    if available.is_empty() {
        return Err("Agent session modes must contain at least one available mode".to_string());
    }
    if available.len() > MAX_SESSION_CONTROLS {
        return Err(format!(
            "Agent returned more than {MAX_SESSION_CONTROLS} session modes"
        ));
    }
    let mut ids = HashSet::new();
    for mode in available {
        let id = required_string(mode, "id", "session mode")?;
        let name = required_string(mode, "name", "session mode")?;
        validate_identifier(id, MAX_CONTROL_IDENTIFIER_LENGTH, "session mode ID")?;
        validate_non_empty_label(name, MAX_CONTROL_LABEL_LENGTH, "session mode name")?;
        if !ids.insert(id) {
            return Err(format!("Agent returned duplicate session mode ID: {id}"));
        }
    }
    let current = required_string(modes, "currentModeId", "session modes")?;
    if !ids.contains(current) {
        return Err(format!(
            "Agent current mode was not included in available modes: {current}"
        ));
    }
    Ok(())
}

pub fn validate_session_mode_reference(modes: Option<&Value>, mode_id: &str) -> ValidationResult {
    let offered = modes
        .and_then(|modes| modes.get("availableModes"))
        .and_then(Value::as_array)
        .is_some_and(|modes| {
            modes
                .iter()
                .any(|mode| mode.get("id").and_then(Value::as_str) == Some(mode_id))
        });
    if !offered {
        return Err(format!("Mode was not offered by the Agent: {mode_id}"));
    }
    Ok(())
}

pub fn validate_session_config_options(options: &Value) -> ValidationResult {
    let options = options
        .as_array()
        .ok_or_else(|| "Agent config options must be an array".to_string())?;
    if options.len() > MAX_SESSION_CONTROLS {
        return Err(format!(
            "Agent returned more than {MAX_SESSION_CONTROLS} config options"
        ));
    }
    let mut ids = HashSet::new();
    for option in options {
        let id = required_string(option, "id", "config option")?;
        let name = required_string(option, "name", "config option")?;
        validate_identifier(id, MAX_CONTROL_IDENTIFIER_LENGTH, "config option ID")?;
        validate_non_empty_label(name, MAX_CONTROL_LABEL_LENGTH, "config option name")?;
        if !ids.insert(id) {
            return Err(format!("Agent returned duplicate config option ID: {id}"));
        }
        if option.get("type").and_then(Value::as_str) == Some("select") {
            validate_select_option(option, id)?;
        }
    }
    Ok(())
}

pub fn validate_session_config_reference(
    options: &Value,
    config_id: &str,
    value: &Value,
) -> ValidationResult {
    let option = options
        .as_array()
        .and_then(|options| {
            options
                .iter()
                .find(|option| option.get("id").and_then(Value::as_str) == Some(config_id))
        })
        .ok_or_else(|| format!("Config option was not offered by the Agent: {config_id}"))?;
    match option.get("type").and_then(Value::as_str) {
        Some("boolean") => {
            if !value.is_boolean() {
                return Err(format!(
                    "Config option {config_id} requires a boolean value"
                ));
            }
        }
        Some("select") => {
            let selected = value.as_str().ok_or_else(|| {
                format!("Config option {config_id} value was not offered by the Agent")
            })?;
            let offered = option
                .get("options")
                .and_then(Value::as_array)
                .is_some_and(|options| {
                    options.iter().any(|item| {
                        item.get("value").and_then(Value::as_str) == Some(selected)
                            || item
                                .get("options")
                                .and_then(Value::as_array)
                                .is_some_and(|nested| {
                                    nested.iter().any(|item| {
                                        item.get("value").and_then(Value::as_str) == Some(selected)
                                    })
                                })
                    })
                });
            if !offered {
                return Err(format!(
                    "Config option {config_id} value was not offered by the Agent"
                ));
            }
        }
        _ => {
            return Err(format!(
                "Config option was not offered by the Agent: {config_id}"
            ));
        }
    }
    Ok(())
}

fn validate_select_option(option: &Value, config_id: &str) -> ValidationResult {
    let options = option
        .get("options")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("Agent config option {config_id} is missing selectable values"))?;
    if options.is_empty() {
        return Err(format!(
            "Agent config option {config_id} has no selectable values"
        ));
    }
    let mut values = HashSet::new();
    let mut groups = HashSet::new();
    for item in options {
        if let Some(nested) = item.get("options").and_then(Value::as_array) {
            let group = required_string(item, "group", "config option group")?;
            let name = required_string(item, "name", "config option group")?;
            validate_identifier(
                group,
                MAX_CONTROL_IDENTIFIER_LENGTH,
                &format!("config option {config_id} group ID"),
            )?;
            validate_non_empty_label(
                name,
                MAX_CONTROL_LABEL_LENGTH,
                &format!("config option {config_id} group name"),
            )?;
            if !groups.insert(group) {
                return Err(format!(
                    "Agent config option {config_id} has duplicate group ID: {group}"
                ));
            }
            if nested.is_empty() {
                return Err(format!(
                    "Agent config option {config_id} contains an empty option group"
                ));
            }
            for value in nested {
                add_select_value(config_id, &mut values, value)?;
            }
        } else {
            add_select_value(config_id, &mut values, item)?;
        }
        if values.len() > MAX_SELECT_VALUES {
            return Err(format!(
                "Agent config option {config_id} has more than {MAX_SELECT_VALUES} selectable values"
            ));
        }
    }
    let current = required_string(option, "currentValue", "select config option")?;
    if !values.contains(current) {
        return Err(format!(
            "Agent config option {config_id} current value was not included in its selectable values"
        ));
    }
    Ok(())
}

fn add_select_value<'a>(
    config_id: &str,
    values: &mut HashSet<&'a str>,
    option: &'a Value,
) -> ValidationResult {
    let value = required_string(option, "value", "config option value")?;
    let name = required_string(option, "name", "config option value")?;
    validate_identifier(
        value,
        MAX_CONTROL_IDENTIFIER_LENGTH,
        &format!("config option {config_id} value"),
    )?;
    validate_non_empty_label(
        name,
        MAX_CONTROL_LABEL_LENGTH,
        &format!("config option {config_id} value name"),
    )?;
    if !values.insert(value) {
        return Err(format!(
            "Agent config option {config_id} has duplicate value: {value}"
        ));
    }
    Ok(())
}

pub fn validate_and_track_session_update(
    state: &mut SessionUpdateSemanticState,
    update: &Value,
) -> ValidationResult {
    let update_bytes = serialized_bytes(update)?;
    if update_bytes > MAX_UPDATE_BYTES {
        return Err(format!(
            "Agent session update exceeds {MAX_UPDATE_BYTES} bytes"
        ));
    }
    if state.update_count >= MAX_SESSION_UPDATES {
        return Err(format!(
            "Agent exceeded {MAX_SESSION_UPDATES} updates in one session"
        ));
    }
    if state.update_bytes.saturating_add(update_bytes) > MAX_SESSION_UPDATE_BYTES {
        return Err(format!(
            "Agent session updates exceed {MAX_SESSION_UPDATE_BYTES} cumulative bytes"
        ));
    }

    let mut next = state.clone();
    validate_session_update_payload(&mut next, update)?;
    next.update_count += 1;
    next.update_bytes += update_bytes;
    *state = next;
    Ok(())
}

pub fn validate_permission_request(
    state: &mut SessionUpdateSemanticState,
    request: &impl Serialize,
) -> ValidationResult<HashSet<String>> {
    let request = serde_json::to_value(request)
        .map_err(|error| format!("failed to serialize permission request: {error}"))?;
    if serialized_bytes(&request)? > MAX_PERMISSION_REQUEST_BYTES {
        return Err(format!(
            "Agent permission request exceeds {MAX_PERMISSION_REQUEST_BYTES} bytes"
        ));
    }
    let tool_call = request
        .get("toolCall")
        .ok_or_else(|| "Agent permission request is missing its tool call".to_string())?;
    validate_tool_update(state, tool_call)?;
    let options = request
        .get("options")
        .and_then(Value::as_array)
        .ok_or_else(|| "Agent permission options must be an array".to_string())?;
    if options.is_empty() || options.len() > MAX_PERMISSION_OPTIONS {
        return Err(format!(
            "Agent permission request must contain between 1 and {MAX_PERMISSION_OPTIONS} options"
        ));
    }
    let mut ids = HashSet::new();
    for option in options {
        let id = required_string(option, "optionId", "permission option")?;
        if id.is_empty() || js_len(id) > MAX_IDENTIFIER_LENGTH {
            return Err("Agent returned an invalid permission option ID".to_string());
        }
        if !ids.insert(id.to_string()) {
            return Err(format!(
                "Agent returned duplicate permission option ID: {id}"
            ));
        }
        let name = required_string(option, "name", "permission option")?;
        if name.is_empty() || js_len(name) > MAX_PERMISSION_OPTION_NAME_LENGTH {
            return Err(format!(
                "Agent returned an invalid permission option name: {id}"
            ));
        }
    }
    Ok(ids)
}

pub fn terminal_references(tool_update: &Value) -> Vec<String> {
    tool_update
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("terminal"))
        .filter_map(|item| item.get("terminalId").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

fn validate_session_update_payload(
    state: &mut SessionUpdateSemanticState,
    update: &Value,
) -> ValidationResult {
    let kind = required_string(update, "sessionUpdate", "session update")?;
    match kind {
        "user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk" => {
            validate_content_block(
                update
                    .get("content")
                    .ok_or_else(|| "Agent message content is missing".to_string())?,
                "Agent message content",
            )?;
            if let Some(message_id) = optional_string(update, "messageId", "session update")? {
                validate_identifier(message_id, MAX_IDENTIFIER_LENGTH, "Agent message ID")?;
                if !state.messages.contains_key(message_id) && state.messages.len() >= MAX_MESSAGES
                {
                    return Err(format!(
                        "Agent exceeded {MAX_MESSAGES} message IDs in one session"
                    ));
                }
                state
                    .messages
                    .insert(message_id.to_string(), kind.to_string());
            }
        }
        "tool_call" | "tool_call_update" => validate_tool_update(state, update)?,
        "plan" => validate_plan_entries(update.get("entries"))?,
        "plan_update" => {
            let plan = update
                .get("plan")
                .ok_or_else(|| "Agent plan update is missing plan".to_string())?;
            let plan_id = required_string(plan, "planId", "plan update")?;
            validate_identifier(plan_id, MAX_IDENTIFIER_LENGTH, "Agent plan ID")?;
            if plan.get("type").and_then(Value::as_str) == Some("items") {
                validate_plan_entries(plan.get("entries"))?;
            }
            track_named_state(&mut state.plans, plan_id, "active", MAX_PLANS, "plans")?;
        }
        "plan_removed" => {
            let plan_id = required_string(update, "planId", "plan removal")?;
            validate_identifier(plan_id, MAX_IDENTIFIER_LENGTH, "Agent plan ID")?;
            track_named_state(&mut state.plans, plan_id, "removed", MAX_PLANS, "plans")?;
        }
        "available_commands_update" => validate_available_commands(update)?,
        "current_mode_update" => {
            let mode_id = required_string(update, "currentModeId", "mode update")?;
            validate_identifier(mode_id, MAX_IDENTIFIER_LENGTH, "Agent mode ID")?;
            state.current_mode_id = Some(mode_id.to_string());
        }
        "config_option_update" => validate_session_controls(None, update.get("configOptions"))?,
        "session_info_update" => validate_session_metadata(
            update.get("title"),
            update.get("updatedAt"),
            "Agent session",
        )?,
        "usage_update" => validate_usage_update(update)?,
        "compaction_update" => validate_compaction_update(state, update)?,
        "compaction_summary_chunk" => validate_compaction_chunk(state, update)?,
        other => return Err(format!("unsupported ACP session update: {other}")),
    }
    Ok(())
}

fn validate_tool_update(
    state: &mut SessionUpdateSemanticState,
    update: &Value,
) -> ValidationResult {
    let tool_id = required_string(update, "toolCallId", "tool update")?;
    validate_identifier(tool_id, MAX_IDENTIFIER_LENGTH, "Agent tool call ID")?;
    for (field, label) in [("title", "title"), ("name", "name")] {
        if let Some(value) = optional_string(update, field, "tool update")?
            && js_len(value) > MAX_TOOL_LABEL_LENGTH
        {
            return Err(format!(
                "Agent tool {label} exceeds {MAX_TOOL_LABEL_LENGTH} characters"
            ));
        }
    }
    if let Some(content) = update.get("content").filter(|value| !value.is_null()) {
        let content = content
            .as_array()
            .ok_or_else(|| "Agent tool content must be an array".to_string())?;
        if content.len() > MAX_TOOL_COLLECTION_ITEMS {
            return Err(format!(
                "Agent tool content exceeds {MAX_TOOL_COLLECTION_ITEMS} items"
            ));
        }
        for item in content {
            match item.get("type").and_then(Value::as_str) {
                Some("content") => validate_content_block(
                    item.get("content")
                        .ok_or_else(|| "Agent tool content block is missing".to_string())?,
                    "Agent tool content",
                )?,
                Some("diff") => validate_absolute_path(
                    required_string(item, "path", "tool diff")?,
                    "Agent tool diff path",
                )?,
                Some("terminal") => validate_identifier(
                    required_string(item, "terminalId", "tool terminal")?,
                    MAX_IDENTIFIER_LENGTH,
                    "Agent terminal reference ID",
                )?,
                Some(other) => return Err(format!("unsupported Agent tool content: {other}")),
                None => return Err("Agent tool content type is missing".to_string()),
            }
        }
    }
    if let Some(locations) = update.get("locations").filter(|value| !value.is_null()) {
        let locations = locations
            .as_array()
            .ok_or_else(|| "Agent tool locations must be an array".to_string())?;
        if locations.len() > MAX_TOOL_COLLECTION_ITEMS {
            return Err(format!(
                "Agent tool locations exceed {MAX_TOOL_COLLECTION_ITEMS} items"
            ));
        }
        for location in locations {
            validate_absolute_path(
                required_string(location, "path", "tool location")?,
                "Agent tool location path",
            )?;
            if let Some(line) = location.get("line").filter(|value| !value.is_null())
                && line
                    .as_u64()
                    .is_none_or(|line| line > MAX_TOOL_LOCATION_LINE)
            {
                return Err(format!(
                    "Agent tool location line must be an integer between 0 and {MAX_TOOL_LOCATION_LINE}"
                ));
            }
        }
    }
    if !state.tool_calls.contains_key(tool_id) && state.tool_calls.len() >= MAX_TOOL_CALLS {
        return Err(format!(
            "Agent exceeded {MAX_TOOL_CALLS} tool calls in one session"
        ));
    }
    let previous = state.tool_calls.get(tool_id).cloned().flatten();
    let status = optional_string(update, "status", "tool update")?
        .map(str::to_string)
        .or(previous);
    state.tool_calls.insert(tool_id.to_string(), status);
    Ok(())
}

fn validate_available_commands(update: &Value) -> ValidationResult {
    let commands = update
        .get("availableCommands")
        .and_then(Value::as_array)
        .ok_or_else(|| "Agent available commands must be an array".to_string())?;
    if commands.len() > MAX_AVAILABLE_COMMANDS {
        return Err(format!(
            "Agent returned more than {MAX_AVAILABLE_COMMANDS} available commands"
        ));
    }
    let mut names = HashSet::new();
    for command in commands {
        let name = required_string(command, "name", "available command")?;
        validate_identifier(name, MAX_IDENTIFIER_LENGTH, "Agent available command name")?;
        if !names.insert(name) {
            return Err(format!(
                "Agent returned duplicate available command: {name}"
            ));
        }
        if js_len(required_string(
            command,
            "description",
            "available command",
        )?) > 16_384
        {
            return Err(format!("Available command description is too long: {name}"));
        }
        if let Some(hint) = command
            .get("input")
            .and_then(|input| input.get("hint"))
            .and_then(Value::as_str)
            && js_len(hint) > 4_096
        {
            return Err(format!("Available command input hint is too long: {name}"));
        }
    }
    Ok(())
}

fn validate_plan_entries(entries: Option<&Value>) -> ValidationResult {
    let entries = entries
        .and_then(Value::as_array)
        .ok_or_else(|| "Agent plan entries must be an array".to_string())?;
    if entries.len() > MAX_PLAN_ENTRIES {
        return Err(format!("Agent plan exceeds {MAX_PLAN_ENTRIES} entries"));
    }
    Ok(())
}

fn validate_usage_update(update: &Value) -> ValidationResult {
    for field in ["used", "size"] {
        let valid = update
            .get(field)
            .and_then(Value::as_f64)
            .is_some_and(|value| value.is_finite() && value >= 0.0);
        if !valid {
            return Err("Agent returned invalid context usage values".to_string());
        }
    }
    if let Some(cost) = update.get("cost").filter(|value| !value.is_null()) {
        if !cost
            .get("amount")
            .and_then(Value::as_f64)
            .is_some_and(|value| value.is_finite() && value >= 0.0)
        {
            return Err("Agent returned an invalid cumulative cost".to_string());
        }
        let currency = cost.get("currency").and_then(Value::as_str).unwrap_or("");
        if currency.is_empty() || js_len(currency) > 32 {
            return Err("Agent returned an invalid cost currency label".to_string());
        }
    }
    Ok(())
}

pub fn validate_session_metadata(
    title: Option<&Value>,
    updated_at: Option<&Value>,
    subject: &str,
) -> ValidationResult {
    if let Some(title) = title.filter(|value| !value.is_null()) {
        let title = title
            .as_str()
            .ok_or_else(|| format!("{subject} title must be a string or null"))?;
        if js_len(title) > MAX_SESSION_TITLE_LENGTH {
            return Err(format!(
                "{subject} title exceeds {MAX_SESSION_TITLE_LENGTH} characters"
            ));
        }
    }
    if let Some(updated_at) = updated_at.filter(|value| !value.is_null()) {
        let updated_at = updated_at
            .as_str()
            .ok_or_else(|| format!("{subject} updatedAt must be a string or null"))?;
        if js_len(updated_at) > 256 {
            return Err(format!("{subject} updatedAt exceeds 256 characters"));
        }
    }
    Ok(())
}

fn validate_compaction_update(
    state: &mut SessionUpdateSemanticState,
    update: &Value,
) -> ValidationResult {
    let id = required_string(update, "compactionId", "compaction update")?;
    validate_identifier(id, MAX_IDENTIFIER_LENGTH, "Agent compaction ID")?;
    let status = required_string(update, "status", "compaction update")?;
    let summary = update.get("summary").filter(|value| !value.is_null());
    if summary
        .and_then(Value::as_array)
        .is_some_and(|summary| !summary.is_empty())
        && status != "completed"
    {
        return Err(
            "A non-empty compaction summary is only valid with completed status".to_string(),
        );
    }
    if update.get("error").is_some_and(|value| !value.is_null()) && status != "failed" {
        return Err("A compaction error is only valid with failed status".to_string());
    }
    let summary_bytes = if let Some(summary) = summary {
        let blocks = summary
            .as_array()
            .ok_or_else(|| "Agent compaction summary must be an array".to_string())?;
        let bytes = serialized_bytes(summary)?;
        if bytes > MAX_COMPACTION_SUMMARY_BYTES {
            return Err(format!(
                "Agent compaction summary exceeds {MAX_COMPACTION_SUMMARY_BYTES} bytes"
            ));
        }
        for block in blocks {
            validate_content_block(block, "Agent compaction summary content")?;
        }
        bytes
    } else {
        state
            .compactions
            .get(id)
            .map(|tracked| tracked.summary_bytes)
            .unwrap_or(0)
    };
    if state
        .compactions
        .get(id)
        .is_some_and(|tracked| is_terminal_compaction_status(&tracked.status))
    {
        return Err(format!("Compaction is already terminal: {id}"));
    }
    if !state.compactions.contains_key(id) && state.compactions.len() >= MAX_COMPACTIONS {
        return Err(format!(
            "Agent exceeded {MAX_COMPACTIONS} compactions in one session"
        ));
    }
    state.compactions.insert(
        id.to_string(),
        TrackedCompaction {
            status: status.to_string(),
            summary_bytes,
        },
    );
    Ok(())
}

fn validate_compaction_chunk(
    state: &mut SessionUpdateSemanticState,
    update: &Value,
) -> ValidationResult {
    let id = required_string(update, "compactionId", "compaction chunk")?;
    validate_identifier(id, MAX_IDENTIFIER_LENGTH, "Agent compaction ID")?;
    let content = update
        .get("content")
        .ok_or_else(|| "Agent compaction chunk content is missing".to_string())?;
    validate_content_block(content, "Agent compaction summary content")?;
    let tracked = state.compactions.get_mut(id).ok_or_else(|| {
        format!("Compaction summary chunks require an in-progress compaction: {id}")
    })?;
    if tracked.status != "in_progress" {
        return Err(format!(
            "Compaction summary chunks require an in-progress compaction: {id}"
        ));
    }
    let bytes = tracked
        .summary_bytes
        .saturating_add(serialized_bytes(content)?);
    if bytes > MAX_COMPACTION_SUMMARY_BYTES {
        return Err(format!(
            "Compaction summary exceeds {MAX_COMPACTION_SUMMARY_BYTES} bytes"
        ));
    }
    tracked.summary_bytes = bytes;
    Ok(())
}

fn is_terminal_compaction_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled")
}

fn validate_annotations(annotations: Option<&Value>, subject: &str) -> ValidationResult {
    let Some(annotations) = annotations.filter(|value| !value.is_null()) else {
        return Ok(());
    };
    let annotations = annotations
        .as_object()
        .ok_or_else(|| format!("{subject} must be an object"))?;
    if annotations
        .get("lastModified")
        .and_then(Value::as_str)
        .is_some_and(|value| js_len(value) > MAX_ANNOTATION_TIMESTAMP_LENGTH)
    {
        return Err(format!(
            "{subject} last-modified timestamp exceeds {MAX_ANNOTATION_TIMESTAMP_LENGTH} characters"
        ));
    }
    if let Some(priority) = annotations.get("priority").filter(|value| !value.is_null())
        && !priority.as_f64().is_some_and(f64::is_finite)
    {
        return Err(format!("{subject} priority must be finite"));
    }
    Ok(())
}

fn validate_base64(value: &str, subject: &str) -> ValidationResult {
    let maximum_characters = MAX_CONTENT_BINARY_BYTES.div_ceil(3) * 4;
    if value.len() > maximum_characters {
        return Err(format!(
            "{subject} exceeds {MAX_CONTENT_BINARY_BYTES} decoded bytes"
        ));
    }
    if !value.len().is_multiple_of(4) {
        return Err(format!("{subject} must be canonical base64"));
    }
    let decoded = BASE64_STANDARD
        .decode(value)
        .map_err(|_| format!("{subject} must be canonical base64"))?;
    if decoded.len() > MAX_CONTENT_BINARY_BYTES {
        return Err(format!(
            "{subject} exceeds {MAX_CONTENT_BINARY_BYTES} decoded bytes"
        ));
    }
    if BASE64_STANDARD.encode(&decoded) != value {
        return Err(format!("{subject} must be canonical base64"));
    }
    Ok(())
}

fn validate_mime_type(
    value: &str,
    subject: &str,
    expected_family: Option<&str>,
) -> ValidationResult {
    let mut parts = value.split(';');
    let essence = parts.next().unwrap_or_default();
    let Some((family, subtype)) = essence.split_once('/') else {
        return Err(format!("{subject} is invalid"));
    };
    if value.is_empty()
        || js_len(value) > MAX_MIME_TYPE_LENGTH
        || !mime_token(family)
        || !mime_token(subtype)
        || parts.any(|parameter| {
            parameter
                .split_once('=')
                .is_none_or(|(name, value)| !mime_token(name) || !mime_token(value))
        })
    {
        return Err(format!("{subject} is invalid"));
    }
    if expected_family.is_some_and(|expected| !family.eq_ignore_ascii_case(expected)) {
        return Err(format!(
            "{subject} must use the {}/* family",
            expected_family.unwrap()
        ));
    }
    Ok(())
}

fn mime_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn validate_uri(value: &str, subject: &str) -> ValidationResult {
    if value.is_empty() || js_len(value) > MAX_URI_LENGTH {
        return Err(format!(
            "{subject} must contain between 1 and {MAX_URI_LENGTH} characters"
        ));
    }
    url::Url::parse(value)
        .map(|_| ())
        .map_err(|_| format!("{subject} is invalid"))
}

fn validate_label(value: &str, subject: &str) -> ValidationResult {
    if js_len(value) > MAX_RESOURCE_LABEL_LENGTH {
        return Err(format!(
            "{subject} exceeds {MAX_RESOURCE_LABEL_LENGTH} characters"
        ));
    }
    Ok(())
}

fn validate_identifier(value: &str, maximum: usize, label: &str) -> ValidationResult {
    if value.is_empty() || js_len(value) > maximum {
        return Err(format!(
            "{label} must contain between 1 and {maximum} characters"
        ));
    }
    Ok(())
}

fn validate_non_empty_label(value: &str, maximum: usize, label: &str) -> ValidationResult {
    validate_identifier(value, maximum, label)
}

fn validate_absolute_path(path: &str, subject: &str) -> ValidationResult {
    if path.is_empty()
        || js_len(path) > MAX_TOOL_PATH_LENGTH
        || path.contains('\0')
        || !is_portable_absolute_path(path)
    {
        return Err(format!(
            "{subject} must be an absolute path of at most {MAX_TOOL_PATH_LENGTH} characters"
        ));
    }
    Ok(())
}

fn is_portable_absolute_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    value.starts_with('/')
        || value.starts_with("\\\\")
        || value.starts_with("//")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
}

fn js_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn track_named_state(
    ids: &mut HashMap<String, String>,
    id: &str,
    value: &str,
    maximum: usize,
    label: &str,
) -> ValidationResult {
    if !ids.contains_key(id) && ids.len() >= maximum {
        return Err(format!("Agent exceeded {maximum} {label} in one session"));
    }
    ids.insert(id.to_string(), value.to_string());
    Ok(())
}

fn safe_non_negative_integer(value: Option<&Value>, name: &str) -> ValidationResult<Option<u64>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let Some(value) = value.as_u64().filter(|value| *value <= MAX_SAFE_INTEGER) else {
        return Err(format!(
            "Agent prompt usage {name} must be a non-negative safe integer"
        ));
    };
    Ok(Some(value))
}

fn required_string<'a>(value: &'a Value, field: &str, subject: &str) -> ValidationResult<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{subject} {field} must be a string"))
}

fn optional_string<'a>(
    value: &'a Value,
    field: &str,
    subject: &str,
) -> ValidationResult<Option<&'a str>> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(format!("{subject} {field} must be a string or null")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_standard_content_blocks_and_annotations() {
        for block in [
            json!({
                "type": "image",
                "data": "iVBORw==",
                "mimeType": "image/png",
                "uri": "attyd://attachment/screenshot.png"
            }),
            json!({ "type": "audio", "data": "AA==", "mimeType": "audio/mpeg" }),
            json!({
                "type": "resource_link",
                "name": "workspace file",
                "uri": "file:///workspace/readme.md",
                "mimeType": "text/markdown;charset=utf-8",
                "size": 0
            }),
            json!({
                "type": "resource",
                "resource": { "uri": "urn:fixture:text", "mimeType": "text/plain", "text": "hello" }
            }),
            json!({
                "type": "text",
                "text": "Annotated",
                "annotations": { "priority": 0.75, "lastModified": "2026-08-31T12:00:00Z" }
            }),
        ] {
            validate_content_block(&block, "ACP content block").unwrap();
        }
    }

    #[test]
    fn rejects_noncanonical_media_mime_uri_size_and_annotation_values() {
        for data in ["not base64", "AAA", "AA=A", "____"] {
            assert!(
                validate_content_block(
                    &json!({ "type": "image", "data": data, "mimeType": "image/png" }),
                    "ACP content block",
                )
                .unwrap_err()
                .contains("canonical base64")
            );
        }
        assert!(
            validate_content_block(
                &json!({ "type": "image", "data": "AA==", "mimeType": "text/html" }),
                "ACP content block",
            )
            .unwrap_err()
            .contains("image/* family")
        );
        assert!(
            validate_content_block(
                &json!({ "type": "resource_link", "name": "relative", "uri": "./relative" }),
                "ACP content block",
            )
            .unwrap_err()
            .contains("URI is invalid")
        );
        assert!(
            validate_content_block(
                &json!({
                    "type": "resource_link",
                    "name": "bad size",
                    "uri": "urn:fixture:size",
                    "size": -1
                }),
                "ACP content block",
            )
            .unwrap_err()
            .contains("non-negative safe integer")
        );
        assert!(
            validate_content_block(
                &json!({
                    "type": "text",
                    "text": "bad annotation",
                    "annotations": { "lastModified": "x".repeat(16_385) }
                }),
                "ACP content block",
            )
            .is_err()
        );
    }

    #[test]
    fn bounds_decoded_content_independently_from_the_envelope() {
        let oversized = "AAAA".repeat(MAX_CONTENT_BINARY_BYTES / 3 + 1);
        assert!(
            validate_content_block(
                &json!({
                    "type": "resource",
                    "resource": { "uri": "urn:fixture:large", "blob": oversized }
                }),
                "ACP content block",
            )
            .unwrap_err()
            .contains("decoded bytes")
        );
    }

    #[test]
    fn validates_prompt_usage_consistency_and_response_size() {
        validate_prompt_response(&json!({
            "stopReason": "end_turn",
            "usage": { "totalTokens": 10, "inputTokens": 4, "outputTokens": 6 }
        }))
        .unwrap();
        for usage in [
            json!({ "totalTokens": -1, "inputTokens": 0, "outputTokens": 0 }),
            json!({ "totalTokens": 10, "inputTokens": 11, "outputTokens": 0 }),
            json!({ "totalTokens": 10, "inputTokens": 6, "outputTokens": 5 }),
        ] {
            assert!(
                validate_prompt_response(&json!({ "stopReason": "end_turn", "usage": usage }))
                    .is_err()
            );
        }
        assert!(
            validate_prompt_response(&json!({
                "stopReason": "end_turn",
                "_meta": { "padding": "x".repeat(MAX_PROMPT_RESPONSE_BYTES) }
            }))
            .is_err()
        );
    }

    #[test]
    fn validates_modes_grouped_select_and_boolean_controls() {
        let modes = json!({
            "currentModeId": "build",
            "availableModes": [
                { "id": "build", "name": "Build" },
                { "id": "plan", "name": "Plan" }
            ]
        });
        let options = json!([
            { "type": "boolean", "id": "verbose", "name": "Verbose", "currentValue": true },
            {
                "type": "select",
                "id": "model",
                "name": "Model",
                "currentValue": "fast",
                "options": [{
                    "group": "local",
                    "name": "Local",
                    "options": [{ "value": "fast", "name": "Fast" }]
                }]
            }
        ]);
        validate_session_controls(Some(&modes), Some(&options)).unwrap();
    }

    #[test]
    fn rejects_contradictory_duplicate_and_unbounded_controls() {
        assert!(
            validate_session_modes(&json!({
                "currentModeId": "missing",
                "availableModes": [{ "id": "build", "name": "Build" }]
            }))
            .is_err()
        );
        assert!(
            validate_session_modes(&json!({
                "currentModeId": "build",
                "availableModes": [
                    { "id": "build", "name": "Build" },
                    { "id": "build", "name": "Again" }
                ]
            }))
            .is_err()
        );
        assert!(
            validate_session_config_options(&json!([
                { "type": "boolean", "id": "same", "name": "One", "currentValue": false },
                { "type": "boolean", "id": "same", "name": "Two", "currentValue": true }
            ]))
            .is_err()
        );
        assert!(
            validate_session_config_options(&json!([{
                "type": "select",
                "id": "model",
                "name": "Model",
                "currentValue": "missing",
                "options": [{ "value": "fast", "name": "Fast" }]
            }]))
            .is_err()
        );
        assert!(
            validate_session_modes(&json!({
                "currentModeId": "mode-0",
                "availableModes": (0..257).map(|index| json!({
                    "id": format!("mode-{index}"),
                    "name": format!("Mode {index}")
                })).collect::<Vec<_>>()
            }))
            .is_err()
        );
    }

    #[test]
    fn tracks_compaction_lifecycle_transactionally() {
        let mut state = SessionUpdateSemanticState::default();
        validate_and_track_session_update(
            &mut state,
            &json!({
                "sessionUpdate": "compaction_update",
                "compactionId": "compact-1",
                "status": "in_progress"
            }),
        )
        .unwrap();
        validate_and_track_session_update(
            &mut state,
            &json!({
                "sessionUpdate": "compaction_summary_chunk",
                "compactionId": "compact-1",
                "content": { "type": "text", "text": "summary" }
            }),
        )
        .unwrap();
        validate_and_track_session_update(
            &mut state,
            &json!({
                "sessionUpdate": "compaction_update",
                "compactionId": "compact-1",
                "status": "completed"
            }),
        )
        .unwrap();
        let accepted = state.update_count;
        assert!(
            validate_and_track_session_update(
                &mut state,
                &json!({
                    "sessionUpdate": "compaction_summary_chunk",
                    "compactionId": "compact-1",
                    "content": { "type": "text", "text": "too late" }
                })
            )
            .is_err()
        );
        assert_eq!(state.update_count, accepted);
    }

    #[test]
    fn validates_message_tool_plan_usage_metadata_and_dynamic_controls() {
        let mut state = SessionUpdateSemanticState::default();
        for update in [
            json!({
                "sessionUpdate": "agent_message_chunk",
                "messageId": "message-1",
                "content": { "type": "text", "text": "answer" }
            }),
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "tool-1",
                "title": "Inspect",
                "content": [{ "type": "diff", "path": "/workspace/file.ts", "newText": "next" }],
                "locations": [{ "path": "C:\\workspace\\file.ts", "line": 0 }]
            }),
            json!({
                "sessionUpdate": "plan_update",
                "plan": { "type": "markdown", "planId": "plan-1", "content": "Plan" }
            }),
            json!({ "sessionUpdate": "plan_removed", "planId": "plan-1" }),
            json!({ "sessionUpdate": "usage_update", "used": 11, "size": 10 }),
            json!({
                "sessionUpdate": "session_info_update",
                "title": null,
                "updatedAt": "not-a-timestamp"
            }),
            json!({ "sessionUpdate": "current_mode_update", "currentModeId": "plan" }),
            json!({
                "sessionUpdate": "config_option_update",
                "configOptions": [{
                    "type": "boolean",
                    "id": "verbose",
                    "name": "Verbose",
                    "currentValue": true
                }]
            }),
        ] {
            validate_and_track_session_update(&mut state, &update).unwrap();
        }
        assert_eq!(state.current_mode_id.as_deref(), Some("plan"));
    }

    #[test]
    fn rejects_invalid_updates_without_charging_session_budgets() {
        let mut state = SessionUpdateSemanticState::default();
        for update in [
            json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "image", "data": "not-base64", "mimeType": "image/png" }
            }),
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "relative",
                "content": [{ "type": "diff", "path": "relative.ts", "newText": "next" }]
            }),
            json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": [
                    { "name": "inspect", "description": "one" },
                    { "name": "inspect", "description": "two" }
                ]
            }),
            json!({ "sessionUpdate": "usage_update", "used": -1, "size": 10 }),
            json!({
                "sessionUpdate": "session_info_update",
                "title": "x".repeat(MAX_SESSION_TITLE_LENGTH + 1)
            }),
        ] {
            assert!(validate_and_track_session_update(&mut state, &update).is_err());
            assert_eq!(state.update_count, 0);
            assert_eq!(state.update_bytes, 0);
        }
    }

    #[test]
    fn enforces_cumulative_update_budgets() {
        let mut count_limited = SessionUpdateSemanticState {
            update_count: MAX_SESSION_UPDATES,
            ..SessionUpdateSemanticState::default()
        };
        assert!(
            validate_and_track_session_update(
                &mut count_limited,
                &json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "too many" }
                }),
            )
            .is_err()
        );
        let mut byte_limited = SessionUpdateSemanticState {
            update_bytes: MAX_SESSION_UPDATE_BYTES - 1,
            ..SessionUpdateSemanticState::default()
        };
        assert!(
            validate_and_track_session_update(
                &mut byte_limited,
                &json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "over budget" }
                }),
            )
            .is_err()
        );
    }

    #[test]
    fn retiring_a_turn_releases_semantic_indexes_but_keeps_session_controls() {
        let mut state = SessionUpdateSemanticState::default();
        for update in [
            json!({
                "sessionUpdate": "agent_message_chunk",
                "messageId": "reusable-message",
                "content": { "type": "text", "text": "answer" }
            }),
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "reusable-tool",
                "status": "completed"
            }),
            json!({
                "sessionUpdate": "current_mode_update",
                "currentModeId": "code"
            }),
        ] {
            validate_and_track_session_update(&mut state, &update).unwrap();
        }
        assert!(state.update_count > 0);
        assert!(state.update_bytes > 0);

        state.retire_turn();

        assert_eq!(state.update_count, 0);
        assert_eq!(state.update_bytes, 0);
        assert_eq!(state.current_mode_id.as_deref(), Some("code"));
        for update in [
            json!({
                "sessionUpdate": "agent_message_chunk",
                "messageId": "reusable-message",
                "content": { "type": "text", "text": "next answer" }
            }),
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "reusable-tool",
                "status": "pending"
            }),
        ] {
            validate_and_track_session_update(&mut state, &update).unwrap();
        }
    }

    #[test]
    fn accepts_tool_message_and_plan_upserts_like_zed() {
        let mut state = SessionUpdateSemanticState::default();
        for update in [
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "missing",
                "status": "in_progress"
            }),
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "tool-1",
                "status": "completed"
            }),
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "tool-1",
                "status": "pending"
            }),
            json!({
                "sessionUpdate": "agent_message_chunk",
                "messageId": "shared-message",
                "content": { "type": "text", "text": "answer" }
            }),
            json!({
                "sessionUpdate": "agent_thought_chunk",
                "messageId": "shared-message",
                "content": { "type": "text", "text": "thought" }
            }),
            json!({ "sessionUpdate": "plan_removed", "planId": "unknown" }),
            json!({
                "sessionUpdate": "plan_update",
                "plan": { "type": "markdown", "planId": "unknown", "content": "late" }
            }),
        ] {
            validate_and_track_session_update(&mut state, &update).unwrap();
        }
        assert_eq!(state.tool_calls.len(), 2);
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.plans.len(), 1);
        assert_eq!(state.plans["unknown"], "active");
    }

    #[test]
    fn validates_absolute_tool_paths_zero_lines_and_terminal_references() {
        let valid = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "edit-and-run",
            "content": [
                { "type": "diff", "path": "C:\\workspace\\file.ts", "newText": "new" },
                { "type": "terminal", "terminalId": "terminal-1" }
            ],
            "locations": [{ "path": "\\\\server\\share\\file.ts", "line": 0 }]
        });
        validate_and_track_session_update(&mut SessionUpdateSemanticState::default(), &valid)
            .unwrap();
        assert_eq!(terminal_references(&valid), vec!["terminal-1"]);
        for invalid in [
            json!({
                "sessionUpdate": "tool_call", "toolCallId": "relative-diff",
                "content": [{ "type": "diff", "path": "relative.ts", "newText": "next" }]
            }),
            json!({
                "sessionUpdate": "tool_call", "toolCallId": "relative-location",
                "locations": [{ "path": "relative.ts", "line": 0 }]
            }),
            json!({
                "sessionUpdate": "tool_call", "toolCallId": "negative-line",
                "locations": [{ "path": "/workspace/file.ts", "line": -1 }]
            }),
        ] {
            assert!(
                validate_and_track_session_update(
                    &mut SessionUpdateSemanticState::default(),
                    &invalid
                )
                .is_err()
            );
        }
    }

    #[test]
    fn validates_permission_tool_upserts_and_option_identity() {
        let mut state = SessionUpdateSemanticState::default();
        let ids = validate_permission_request(
            &mut state,
            &json!({
                "sessionId": "session",
                "toolCall": { "toolCallId": "missing" },
                "options": [{ "optionId": "yes", "name": "Allow", "kind": "allow_once" }]
            }),
        )
        .unwrap();
        assert!(ids.contains("yes"));
        assert!(state.tool_calls.contains_key("missing"));
        assert!(
            validate_permission_request(
                &mut state,
                &json!({
                    "sessionId": "session",
                    "toolCall": { "toolCallId": "missing", "status": "completed" },
                    "options": []
                })
            )
            .is_err()
        );
        assert!(
            validate_permission_request(
                &mut state,
                &json!({
                    "sessionId": "session",
                    "toolCall": { "toolCallId": "missing" },
                    "options": [
                        { "optionId": "same", "name": "Allow", "kind": "allow_once" },
                        { "optionId": "same", "name": "Reject", "kind": "reject_once" }
                    ]
                })
            )
            .is_err()
        );
    }

    #[test]
    fn requires_browser_controls_to_reference_offered_values() {
        let modes = json!({
            "currentModeId": "build",
            "availableModes": [{ "id": "build", "name": "Build" }]
        });
        validate_session_mode_reference(Some(&modes), "build").unwrap();
        assert!(validate_session_mode_reference(Some(&modes), "ghost").is_err());
        assert!(validate_session_mode_reference(None, "build").is_err());
        let options = json!([
            { "type": "boolean", "id": "verbose", "name": "Verbose", "currentValue": false },
            {
                "type": "select", "id": "model", "name": "Model", "currentValue": "fast",
                "options": [{
                    "group": "remote", "name": "Remote",
                    "options": [{ "value": "deep", "name": "Deep" }]
                }, { "value": "fast", "name": "Fast" }]
            }
        ]);
        validate_session_config_reference(&options, "verbose", &json!(true)).unwrap();
        validate_session_config_reference(&options, "model", &json!("deep")).unwrap();
        assert!(validate_session_config_reference(&options, "verbose", &json!("true")).is_err());
        assert!(validate_session_config_reference(&options, "model", &json!("missing")).is_err());
    }

    #[test]
    fn uses_javascript_utf16_lengths_for_protocol_character_limits() {
        let astral = "😀".repeat(MAX_CONTROL_IDENTIFIER_LENGTH);
        validate_session_modes(&json!({
            "currentModeId": astral,
            "availableModes": [{ "id": astral, "name": "Mode" }]
        }))
        .unwrap_err();
        let bmp = "界".repeat(MAX_CONTROL_IDENTIFIER_LENGTH);
        validate_session_modes(&json!({
            "currentModeId": bmp,
            "availableModes": [{ "id": bmp, "name": "模式" }]
        }))
        .unwrap();
    }
}
