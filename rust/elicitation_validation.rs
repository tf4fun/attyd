use std::collections::HashSet;

use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value};
use url::Url;

use crate::semantic::{ValidationResult, serialized_bytes};

const MAX_ELICITATION_FIELDS: usize = 64;
const MAX_ELICITATION_CHOICES: usize = 256;
const MAX_PATTERN_LENGTH: usize = 512;
const MAX_ELICITATION_BYTES: usize = 2_000_000;
const MAX_ELICITATION_MESSAGE_LENGTH: usize = 16_384;
const MAX_ELICITATION_FIELD_NAME_LENGTH: usize = 256;
const MAX_ELICITATION_RESPONSE_BYTES: usize = 2_000_000;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

pub fn validate_elicitation_request(request: &impl Serialize) -> ValidationResult<Value> {
    let request = serde_json::to_value(request)
        .map_err(|error| format!("failed to serialize elicitation request: {error}"))?;
    validate_elicitation_request_value(&request)?;
    Ok(request)
}

pub fn validate_elicitation_request_value(request: &Value) -> ValidationResult {
    if serialized_bytes(request)? > MAX_ELICITATION_BYTES {
        return Err(format!(
            "Elicitation request exceeds {MAX_ELICITATION_BYTES} bytes"
        ));
    }
    let message = required_string(request, "message", "Elicitation")?;
    if message.is_empty() || js_len(message) > MAX_ELICITATION_MESSAGE_LENGTH {
        return Err("Elicitation message is empty or too long".to_string());
    }
    validate_scope(request)?;
    match request.get("mode").and_then(Value::as_str) {
        Some("url") => {
            let raw_url = required_string(request, "url", "URL elicitation")?;
            let url = Url::parse(raw_url)
                .map_err(|_| "URL elicitation contains an invalid URL".to_string())?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err("URL elicitation must use HTTP or HTTPS".to_string());
            }
        }
        Some("form") => {
            let schema = request
                .get("requestedSchema")
                .ok_or_else(|| "Form elicitation is missing requestedSchema".to_string())?;
            validate_schema(schema)?;
        }
        Some(mode) => {
            return Err(format!(
                "Agent requested an unadvertised elicitation mode: {mode}"
            ));
        }
        None => return Err("Agent requested an unadvertised elicitation mode".to_string()),
    }
    Ok(())
}

pub fn validate_elicitation_response_value(request: &Value, response: &Value) -> ValidationResult {
    if serialized_bytes(response)? > MAX_ELICITATION_RESPONSE_BYTES {
        return Err(format!(
            "Elicitation response exceeds {MAX_ELICITATION_RESPONSE_BYTES} bytes"
        ));
    }
    let action = required_string(response, "action", "Elicitation response")?;
    if !matches!(action, "accept" | "decline" | "cancel") {
        return Err(format!("Unsupported elicitation response action: {action}"));
    }
    let content = response.get("content").filter(|value| !value.is_null());
    if action != "accept" {
        if content.is_some() {
            return Err("Only accepted elicitation responses may contain content".to_string());
        }
        return Ok(());
    }
    if request.get("mode").and_then(Value::as_str) == Some("url") {
        if content.is_some() {
            return Err("URL elicitation responses must not contain content".to_string());
        }
        return Ok(());
    }
    let schema = request
        .get("requestedSchema")
        .ok_or_else(|| "Form elicitation is missing requestedSchema".to_string())?;
    let empty = Map::new();
    let properties = match schema.get("properties").filter(|value| !value.is_null()) {
        None => &empty,
        Some(value) => value
            .as_object()
            .ok_or_else(|| "Form elicitation schema properties must be an object".to_string())?,
    };
    let content = match content {
        None => &empty,
        Some(value) => value
            .as_object()
            .ok_or_else(|| "Form elicitation content must be an object".to_string())?,
    };
    for name in string_array(schema.get("required"), "Elicitation schema required")? {
        if !content.contains_key(name) {
            return Err(format!(
                "Elicitation response is missing required field: {name}"
            ));
        }
    }
    for (name, value) in content {
        let field_schema = properties
            .get(name)
            .ok_or_else(|| format!("Unknown elicitation response field: {name}"))?;
        validate_field(name, value, field_schema)?;
    }
    Ok(())
}

fn validate_scope(request: &Value) -> ValidationResult {
    let has_session = request
        .get("sessionId")
        .is_some_and(|value| !value.is_null());
    let has_request = request.get("requestId").is_some();
    if has_session == has_request {
        return Err("Elicitation must have exactly one session or request scope".to_string());
    }
    if has_session {
        let session_id = request
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("");
        if session_id.is_empty() || js_len(session_id) > 1_024 {
            return Err("Elicitation contains an invalid session scope".to_string());
        }
        return Ok(());
    }
    let request_id = request
        .get("requestId")
        .ok_or_else(|| "Elicitation is missing its request scope".to_string())?;
    let valid = request_id.is_null()
        || request_id
            .as_str()
            .is_some_and(|value| js_len(value) <= 1_024)
        || request_id.as_f64().is_some_and(|value| {
            value.is_finite() && value.fract() == 0.0 && value.abs() <= MAX_SAFE_INTEGER as f64
        });
    if !valid {
        return Err("Elicitation contains an invalid request scope".to_string());
    }
    Ok(())
}

fn validate_schema(schema: &Value) -> ValidationResult {
    let empty = Map::new();
    let properties = match schema.get("properties").filter(|value| !value.is_null()) {
        None => &empty,
        Some(value) => value
            .as_object()
            .ok_or_else(|| "Form elicitation schema properties must be an object".to_string())?,
    };
    if properties.len() > MAX_ELICITATION_FIELDS {
        return Err(format!(
            "Elicitation schema has more than {MAX_ELICITATION_FIELDS} fields"
        ));
    }
    let required = string_array(schema.get("required"), "Elicitation schema required")?;
    let mut unique = HashSet::new();
    for name in required {
        if !unique.insert(name) {
            return Err("Elicitation schema contains duplicate required fields".to_string());
        }
        if !properties.contains_key(name) {
            return Err(format!(
                "Elicitation schema requires an unknown field: {name}"
            ));
        }
    }
    for (name, field_schema) in properties {
        if name.is_empty() || js_len(name) > MAX_ELICITATION_FIELD_NAME_LENGTH {
            return Err("Elicitation schema contains an invalid field name".to_string());
        }
        validate_property_schema(name, field_schema)?;
    }
    Ok(())
}

fn validate_property_schema(name: &str, schema: &Value) -> ValidationResult {
    match schema.get("type").and_then(Value::as_str) {
        Some("string") => {
            validate_length_bounds(name, schema.get("minLength"), schema.get("maxLength"))?;
            if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
                validate_safe_pattern(name, pattern)?;
            }
            let enum_values = non_empty_string_choices(schema.get("enum"))?;
            let one_of = non_empty_object_choices(schema.get("oneOf"), "const")?;
            if enum_values.is_some() && one_of.is_some() {
                return Err(format!(
                    "Elicitation field {name} defines both enum and oneOf"
                ));
            }
            validate_choices(name, enum_values.as_ref().or(one_of.as_ref()))?;
        }
        Some("number") | Some("integer") => {
            let minimum = optional_finite_number(schema.get("minimum"), name, "minimum")?;
            let maximum = optional_finite_number(schema.get("maximum"), name, "maximum")?;
            if minimum
                .zip(maximum)
                .is_some_and(|(minimum, maximum)| minimum > maximum)
            {
                return Err(format!(
                    "Elicitation field {name} has inverted numeric bounds"
                ));
            }
        }
        Some("array") => {
            validate_length_bounds(name, schema.get("minItems"), schema.get("maxItems"))?;
            let items = schema
                .get("items")
                .ok_or_else(|| format!("Elicitation field {name} is missing items"))?;
            let enum_values = non_empty_string_choices(items.get("enum"))?;
            let any_of = non_empty_object_choices(items.get("anyOf"), "const")?;
            if enum_values.is_some() && any_of.is_some() {
                return Err(format!(
                    "Elicitation field {name} defines both enum and anyOf"
                ));
            }
            validate_choices(name, enum_values.as_ref().or(any_of.as_ref()))?;
        }
        Some("boolean") => {}
        Some(_) | None => return Ok(()),
    }
    if let Some(default) = schema.get("default").filter(|value| !value.is_null()) {
        validate_field(name, default, schema)?;
    }
    Ok(())
}

fn validate_field(name: &str, value: &Value, schema: &Value) -> ValidationResult {
    match schema.get("type").and_then(Value::as_str) {
        Some("string") => {
            let value = value.as_str().ok_or_else(|| invalid_type(name, "string"))?;
            if let Some(minimum) = optional_safe_length(schema.get("minLength"))?
                && js_len(value) < minimum
            {
                return Err(format!(
                    "Elicitation field {name} is shorter than minLength"
                ));
            }
            if let Some(maximum) = optional_safe_length(schema.get("maxLength"))?
                && js_len(value) > maximum
            {
                return Err(format!("Elicitation field {name} is longer than maxLength"));
            }
            if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
                if js_len(value) > 10_000 {
                    return Err(format!(
                        "Elicitation field {name} is too long for pattern validation"
                    ));
                }
                let regex = Regex::new(pattern).map_err(|_| {
                    format!("Elicitation field {name} has an invalid schema pattern")
                })?;
                if !regex.is_match(value) {
                    return Err(format!("Elicitation field {name} does not match pattern"));
                }
            }
            if let Some(format) = schema.get("format").and_then(Value::as_str)
                && !matches_format(value, format)
            {
                return Err(format!("Elicitation field {name} is not a valid {format}"));
            }
            let enum_values = non_empty_string_choices(schema.get("enum"))?;
            let one_of = non_empty_object_choices(schema.get("oneOf"), "const")?;
            if enum_values
                .as_ref()
                .or(one_of.as_ref())
                .is_some_and(|allowed| !allowed.iter().any(|item| item == value))
            {
                return Err(format!("Elicitation field {name} is not an allowed value"));
            }
        }
        Some("number") | Some("integer") => {
            let number = value
                .as_f64()
                .filter(|number| number.is_finite())
                .ok_or_else(|| invalid_type(name, schema["type"].as_str().unwrap_or("number")))?;
            if schema["type"] == "integer" && number.fract() != 0.0 {
                return Err(invalid_type(name, "integer"));
            }
            if optional_finite_number(schema.get("minimum"), name, "minimum")?
                .is_some_and(|minimum| number < minimum)
            {
                return Err(format!("Elicitation field {name} is below minimum"));
            }
            if optional_finite_number(schema.get("maximum"), name, "maximum")?
                .is_some_and(|maximum| number > maximum)
            {
                return Err(format!("Elicitation field {name} is above maximum"));
            }
        }
        Some("boolean") => {
            if !value.is_boolean() {
                return Err(invalid_type(name, "boolean"));
            }
        }
        Some("array") => {
            let values = value
                .as_array()
                .filter(|values| values.iter().all(Value::is_string))
                .ok_or_else(|| invalid_type(name, "string array"))?;
            let strings = values.iter().filter_map(Value::as_str).collect::<Vec<_>>();
            if strings.iter().copied().collect::<HashSet<_>>().len() != strings.len() {
                return Err(format!(
                    "Elicitation field {name} contains duplicate selections"
                ));
            }
            if optional_safe_length(schema.get("minItems"))?
                .is_some_and(|minimum| strings.len() < minimum)
            {
                return Err(format!("Elicitation field {name} has too few selections"));
            }
            if optional_safe_length(schema.get("maxItems"))?
                .is_some_and(|maximum| strings.len() > maximum)
            {
                return Err(format!("Elicitation field {name} has too many selections"));
            }
            let items = schema
                .get("items")
                .ok_or_else(|| format!("Elicitation field {name} is missing items"))?;
            let enum_values = non_empty_string_choices(items.get("enum"))?;
            let any_of = non_empty_object_choices(items.get("anyOf"), "const")?;
            if enum_values
                .as_ref()
                .or(any_of.as_ref())
                .is_some_and(|allowed| {
                    strings
                        .iter()
                        .any(|value| !allowed.iter().any(|item| item == value))
                })
            {
                return Err(format!(
                    "Elicitation field {name} contains an unknown selection"
                ));
            }
        }
        Some(kind) => {
            return Err(format!(
                "Unsupported elicitation field type for {name}: {kind}"
            ));
        }
        None => {
            return Err(format!(
                "Unsupported elicitation field type for {name}: missing"
            ));
        }
    }
    Ok(())
}

fn validate_length_bounds(
    name: &str,
    minimum: Option<&Value>,
    maximum: Option<&Value>,
) -> ValidationResult {
    let minimum = optional_safe_length(minimum)
        .map_err(|_| format!("Elicitation field {name} has an invalid minimum length"))?;
    let maximum = optional_safe_length(maximum)
        .map_err(|_| format!("Elicitation field {name} has an invalid maximum length"))?;
    if minimum
        .zip(maximum)
        .is_some_and(|(minimum, maximum)| minimum > maximum)
    {
        return Err(format!(
            "Elicitation field {name} has inverted length bounds"
        ));
    }
    Ok(())
}

fn validate_choices(name: &str, choices: Option<&Vec<String>>) -> ValidationResult {
    let Some(choices) = choices else {
        return Ok(());
    };
    if choices.len() > MAX_ELICITATION_CHOICES {
        return Err(format!("Elicitation field {name} has too many choices"));
    }
    if choices.iter().collect::<HashSet<_>>().len() != choices.len() {
        return Err(format!("Elicitation field {name} has duplicate choices"));
    }
    Ok(())
}

fn validate_safe_pattern(name: &str, source: &str) -> ValidationResult {
    if js_len(source) > MAX_PATTERN_LENGTH {
        return Err(format!("Elicitation field {name} pattern is too long"));
    }
    Regex::new(source)
        .map_err(|_| format!("Elicitation field {name} has an invalid schema pattern"))?;
    if has_back_reference(source) || has_unsafe_quantified_group(source) {
        return Err(format!(
            "Elicitation field {name} has an unsafe schema pattern"
        ));
    }
    Ok(())
}

fn has_back_reference(source: &str) -> bool {
    let bytes = source.as_bytes();
    bytes
        .windows(2)
        .any(|pair| pair[0] == b'\\' && matches!(pair[1], b'1'..=b'9'))
}

fn has_unsafe_quantified_group(source: &str) -> bool {
    #[derive(Clone, Copy)]
    struct Group {
        complex: bool,
    }
    let chars = source.chars().collect::<Vec<_>>();
    let mut stack = vec![Group { complex: false }];
    let mut in_class = false;
    let mut escaped = false;
    let mut closed_group = None;
    for (index, character) in chars.iter().copied().enumerate() {
        if escaped {
            escaped = false;
            closed_group = None;
            continue;
        }
        if character == '\\' {
            escaped = true;
            closed_group = None;
            continue;
        }
        if character == '[' && !in_class {
            in_class = true;
            closed_group = None;
            continue;
        }
        if character == ']' && in_class {
            in_class = false;
            continue;
        }
        if in_class {
            continue;
        }
        if character == '('
            && chars.get(index + 1) == Some(&'?')
            && chars.get(index + 2) != Some(&':')
        {
            return true;
        }
        if character == '(' {
            stack.push(Group { complex: false });
            closed_group = None;
            continue;
        }
        if character == ')' && stack.len() > 1 {
            let group = stack.pop().unwrap_or(Group { complex: true });
            if group.complex
                && let Some(parent) = stack.last_mut()
            {
                parent.complex = true;
            }
            closed_group = Some(group);
            continue;
        }
        if matches!(character, '*' | '+' | '{') {
            if closed_group.is_some_and(|group| group.complex) {
                return true;
            }
            if let Some(group) = stack.last_mut() {
                group.complex = true;
            }
            closed_group = None;
            continue;
        }
        if character == '|'
            && let Some(group) = stack.last_mut()
        {
            group.complex = true;
        }
        if !character.is_whitespace() {
            closed_group = None;
        }
    }
    false
}

fn matches_format(value: &str, format: &str) -> bool {
    match format {
        "email" => {
            Regex::new(r"^[^\s@]+@[^\s@]+\.[^\s@]+$").is_ok_and(|regex| regex.is_match(value))
        }
        "uri" => Url::parse(value).is_ok_and(|url| !url.scheme().is_empty()),
        "date" => valid_date(value),
        "date-time" => valid_date_time(value),
        _ => false,
    }
}

fn valid_date(value: &str) -> bool {
    let parts = value.split('-').collect::<Vec<_>>();
    if parts.len() != 3 || parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) = (
        parts[0].parse::<u32>(),
        parts[1].parse::<u32>(),
        parts[2].parse::<u32>(),
    ) else {
        return false;
    };
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let maximum = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=maximum).contains(&day)
}

fn valid_date_time(value: &str) -> bool {
    let Some((date, time)) = value.split_once('T') else {
        return false;
    };
    if !valid_date(date) {
        return false;
    }
    let (clock, zone) = if let Some(clock) = time.strip_suffix('Z') {
        (clock, "Z")
    } else if time.len() >= 6 {
        let split = time.len() - 6;
        let zone = &time[split..];
        if !matches!(zone.as_bytes().first(), Some(b'+') | Some(b'-')) {
            return false;
        }
        (&time[..split], zone)
    } else {
        return false;
    };
    let base = clock.split('.').next().unwrap_or("");
    let parts = base.split(':').collect::<Vec<_>>();
    if parts.len() != 3 || parts.iter().any(|part| part.len() != 2) {
        return false;
    }
    let Ok(hour) = parts[0].parse::<u32>() else {
        return false;
    };
    let Ok(minute) = parts[1].parse::<u32>() else {
        return false;
    };
    let Ok(second) = parts[2].parse::<u32>() else {
        return false;
    };
    if hour > 23 || minute > 59 || second > 59 {
        return false;
    }
    if clock.contains('.')
        && clock.rsplit_once('.').is_none_or(|(_, fraction)| {
            fraction.is_empty() || !fraction.chars().all(|character| character.is_ascii_digit())
        })
    {
        return false;
    }
    if zone != "Z" {
        let zone_parts = zone[1..].split(':').collect::<Vec<_>>();
        if zone_parts.len() != 2
            || zone_parts.iter().any(|part| part.len() != 2)
            || zone_parts[0].parse::<u32>().map_or(true, |hour| hour > 23)
            || zone_parts[1]
                .parse::<u32>()
                .map_or(true, |minute| minute > 59)
        {
            return false;
        }
    }
    true
}

fn optional_finite_number(
    value: Option<&Value>,
    name: &str,
    label: &str,
) -> ValidationResult<Option<f64>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    value
        .as_f64()
        .filter(|number| number.is_finite())
        .map(Some)
        .ok_or_else(|| format!("Elicitation field {name} has a non-finite {label}"))
}

fn optional_safe_length(value: Option<&Value>) -> ValidationResult<Option<usize>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    value
        .as_f64()
        .filter(|length| {
            length.is_finite()
                && *length >= 0.0
                && length.fract() == 0.0
                && *length <= MAX_SAFE_INTEGER as f64
        })
        .and_then(|length| usize::try_from(length as u64).ok())
        .map(Some)
        .ok_or_else(|| "invalid non-negative safe integer".to_string())
}

fn non_empty_string_choices(value: Option<&Value>) -> ValidationResult<Option<Vec<String>>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or_else(|| "Elicitation choices must be an array".to_string())?;
    if values.is_empty() {
        return Ok(None);
    }
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "Elicitation choices must be strings".to_string())
        })
        .collect::<ValidationResult<Vec<_>>>()
        .map(Some)
}

fn non_empty_object_choices(
    value: Option<&Value>,
    field: &str,
) -> ValidationResult<Option<Vec<String>>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or_else(|| "Elicitation choices must be an array".to_string())?;
    if values.is_empty() {
        return Ok(None);
    }
    values
        .iter()
        .map(|value| {
            value
                .get(field)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| "Elicitation choices must contain string constants".to_string())
        })
        .collect::<ValidationResult<Vec<_>>>()
        .map(Some)
}

fn string_array<'a>(value: Option<&'a Value>, subject: &str) -> ValidationResult<Vec<&'a str>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .ok_or_else(|| format!("{subject} must be an array"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| format!("{subject} must contain strings"))
        })
        .collect()
}

fn required_string<'a>(value: &'a Value, field: &str, subject: &str) -> ValidationResult<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{subject} is missing {field}"))
}

fn invalid_type(name: &str, expected: &str) -> String {
    format!("Elicitation field {name} must be {expected}")
}

fn js_len(value: &str) -> usize {
    value.encode_utf16().count()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn form_request() -> Value {
        json!({
            "sessionId": "s",
            "mode": "form",
            "message": "Configure",
            "requestedSchema": {
                "type": "object",
                "required": ["name", "count"],
                "properties": {
                    "name": { "type": "string", "minLength": 2 },
                    "count": { "type": "integer", "minimum": 1, "maximum": 3 },
                    "tags": {
                        "type": "array",
                        "maxItems": 2,
                        "items": { "type": "string", "enum": ["a", "b"] }
                    }
                }
            }
        })
    }

    #[test]
    fn validates_modes_scopes_and_url_schemes() {
        validate_elicitation_request_value(&json!({
            "sessionId": "s", "mode": "url", "message": "Connect",
            "elicitationId": "flow", "url": "https://example.test/connect"
        }))
        .unwrap();
        assert!(
            validate_elicitation_request_value(&json!({
                "sessionId": "s", "mode": "url", "message": "Run",
                "elicitationId": "flow", "url": "javascript:alert(1)"
            }))
            .is_err()
        );
        assert!(
            validate_elicitation_request_value(&json!({
                "sessionId": "s", "requestId": 1, "mode": "form", "message": "bad",
                "requestedSchema": { "type": "object", "properties": {} }
            }))
            .is_err()
        );
        validate_elicitation_request_value(&json!({
            "requestId": null, "mode": "form", "message": "before session",
            "requestedSchema": { "type": "object", "properties": {} }
        }))
        .unwrap();
    }

    #[test]
    fn validates_form_content_defaults_and_response_actions() {
        let request = form_request();
        validate_elicitation_request_value(&request).unwrap();
        validate_elicitation_response_value(
            &request,
            &json!({
                "action": "accept", "content": { "name": "ok", "count": 2, "tags": ["a"] }
            }),
        )
        .unwrap();
        assert!(
            validate_elicitation_response_value(
                &request,
                &json!({
                    "action": "accept", "content": { "name": "ok" }
                })
            )
            .is_err()
        );
        assert!(
            validate_elicitation_response_value(
                &request,
                &json!({
                    "action": "accept", "content": { "name": "ok", "count": 2.5 }
                })
            )
            .is_err()
        );
        assert!(
            validate_elicitation_response_value(
                &request,
                &json!({
                    "action": "decline", "content": { "name": "leak" }
                })
            )
            .is_err()
        );
        assert!(
            validate_elicitation_response_value(
                &request,
                &json!({
                    "action": "invented"
                })
            )
            .is_err()
        );
    }

    #[test]
    fn validates_patterns_formats_choices_and_unique_selections() {
        let request = json!({
            "sessionId": "s", "mode": "form", "message": "Identity",
            "requestedSchema": { "type": "object", "properties": {
                "code": { "type": "string", "pattern": "^[A-Z]{2}$" },
                "email": { "type": "string", "format": "email" },
                "choice": { "type": "string", "oneOf": [], "enum": ["a", "b"] },
                "tags": { "type": "array", "items": { "type": "string", "enum": ["x", "y"] } }
            }}
        });
        validate_elicitation_request_value(&request).unwrap();
        validate_elicitation_response_value(
            &request,
            &json!({
                "action": "accept", "content": {
                    "code": "AB", "email": "a@example.test", "choice": "b", "tags": ["x"]
                }
            }),
        )
        .unwrap();
        for content in [
            json!({ "code": "ab" }),
            json!({ "email": "invalid" }),
            json!({ "choice": "other" }),
            json!({ "tags": ["x", "x"] }),
        ] {
            assert!(
                validate_elicitation_response_value(
                    &request,
                    &json!({ "action": "accept", "content": content })
                )
                .is_err()
            );
        }
    }

    #[test]
    fn rejects_unsafe_and_inconsistent_schemas() {
        for property in [
            json!({ "type": "string", "pattern": "(a+)+$" }),
            json!({ "type": "string", "enum": ["a"], "oneOf": [{ "const": "b" }] }),
            json!({ "type": "integer", "minimum": 3, "maximum": 1 }),
            json!({ "type": "array", "minItems": 2, "maxItems": 1, "items": { "type": "string" } }),
        ] {
            assert!(
                validate_elicitation_request_value(&json!({
                    "sessionId": "s", "mode": "form", "message": "Unsafe",
                    "requestedSchema": { "type": "object", "properties": { "value": property } }
                }))
                .is_err()
            );
        }
        assert!(
            validate_elicitation_request_value(&json!({
                "sessionId": "s", "mode": "form", "message": "Unsafe",
                "requestedSchema": { "type": "object", "required": ["missing"], "properties": {} }
            }))
            .is_err()
        );
    }

    #[test]
    fn validates_date_and_date_time_formats() {
        assert!(valid_date("2024-02-29"));
        assert!(!valid_date("2023-02-29"));
        assert!(valid_date_time("2026-08-30T16:00:00+08:00"));
        assert!(valid_date_time("2026-08-30T08:00:00.125Z"));
        assert!(!valid_date_time("2026-08-30T25:00:00Z"));
    }
}
