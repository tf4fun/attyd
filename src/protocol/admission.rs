//! Admission of operation writes (protocol §6.4).
//!
//! A write is checked against the published documents, which are the bridge
//! state as of its last commit: the bridge checks it inside the guard that
//! installs it, and without an Agent connection the server checks it the same
//! way before it answers `unavailable`. The checks run in §6.4's order:
//! `expect`, then status and occupation, then capability and arguments. A
//! refusal changes nothing.
use serde_json::{Value, json};

use super::canon::AttachmentRef;
use super::documents::{ErrorCode, OperationKind, ProtocolError, Scope};
use super::publish::Publisher;

/// `POST /api/v2/scopes/{scope}/ops` (protocol §6.1).
#[derive(Debug, Clone)]
pub(crate) struct OpWrite {
    pub expect: String,
    pub kind: String,
    pub args: Value,
}

/// What an admitted write does.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Admitted {
    /// Starts an operation of `kind` with `input` (its `args`). A prompt's
    /// `input` is its blocks with every attachment reference resolved.
    Start { kind: OperationKind, input: Value },
    /// `cancel` or `answer` of the running operation `op`.
    Running { op: String, kind: String },
}

fn refuse(code: ErrorCode, message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(code, message)
}

fn invalid(message: impl Into<String>) -> ProtocolError {
    refuse(ErrorCode::InvalidRequest, message)
}

/// The kinds a client writes in `scope` (protocol §6.2).
fn started_kind(scope: &Scope, kind: &str) -> Option<OperationKind> {
    let kind = match (scope, kind) {
        (Scope::Session(_), "prompt") => OperationKind::Prompt,
        (Scope::Session(_), "set_mode") => OperationKind::SetMode,
        (Scope::Session(_), "set_config") => OperationKind::SetConfig,
        (Scope::Session(_), "fork") => OperationKind::Fork,
        (Scope::Global, "create") => OperationKind::Create,
        (Scope::Global, "delete") => OperationKind::Delete,
        (Scope::Global, "authenticate") => OperationKind::Authenticate,
        (Scope::Global, "logout") => OperationKind::Logout,
        (Scope::Global, "terminal_login") => OperationKind::TerminalLogin,
        (Scope::Catalog, "list") => OperationKind::List,
        _ => return None,
    };
    Some(kind)
}

/// Checks `write` in `scope` against the published documents.
pub(crate) fn admit(
    publisher: &Publisher,
    scope: &Scope,
    write: &OpWrite,
) -> Result<Admitted, ProtocolError> {
    let started = started_kind(scope, &write.kind);
    if started.is_none() && !matches!(write.kind.as_str(), "cancel" | "answer") {
        return Err(invalid(format!(
            "{} is not an operation clients write in {scope}",
            write.kind
        )));
    }
    let live = publisher
        .live(scope)
        .ok_or_else(|| refuse(ErrorCode::NotFound, format!("{scope} is not held")))?;
    let op = &live["op"];
    if op["id"].as_str() != Some(write.expect.as_str()) {
        return Err(refuse(
            ErrorCode::Stale,
            format!("{} is not the current operation of {scope}", write.expect),
        ));
    }
    let running = op["status"] == "running";
    let Some(kind) = started else {
        // `cancel` and `answer` are wired by the next segment of step 6.
        if !running {
            return Err(refuse(
                ErrorCode::NotAllowed,
                format!("the operation of {scope} has ended"),
            ));
        }
        return Err(refuse(
            ErrorCode::Unsupported,
            format!("{} is not served yet", write.kind),
        ));
    };
    if running {
        return Err(refuse(
            ErrorCode::NotAllowed,
            format!("the operation of {scope} is running"),
        ));
    }
    if kind == OperationKind::Delete
        && let Some(session_id) = write.args["sessionId"].as_str()
        && publisher
            .live(&Scope::Session(session_id.to_string()))
            .is_some_and(|session| session["op"]["status"] == "running")
    {
        return Err(refuse(
            ErrorCode::NotAllowed,
            format!("session/{session_id} is not free"),
        ));
    }
    let initialize = publisher
        .live(&Scope::Global)
        .map(|global| &global["agent"]["initialize"])
        .filter(|initialize| !initialize.is_null());
    capability(initialize, kind)?;
    let input = arguments(publisher, initialize, scope, live, kind, &write.args)?;
    Ok(Admitted::Start { kind, input })
}

/// The Agent capability a kind needs (protocol §6.2). While the Agent never
/// initialized, nothing is known to be missing.
fn capability(initialize: Option<&Value>, kind: OperationKind) -> Result<(), ProtocolError> {
    let pointer = match kind {
        OperationKind::Fork => "/agentCapabilities/sessionCapabilities/fork",
        OperationKind::List => "/agentCapabilities/sessionCapabilities/list",
        OperationKind::Delete => "/agentCapabilities/sessionCapabilities/delete",
        OperationKind::Logout => "/agentCapabilities/auth/logout",
        _ => return Ok(()),
    };
    match initialize {
        Some(initialize) if initialize.pointer(pointer).is_none_or(Value::is_null) => Err(refuse(
            ErrorCode::Unsupported,
            format!("the Agent did not advertise {pointer}"),
        )),
        _ => Ok(()),
    }
}

fn string<'a>(args: &'a Value, field: &str) -> Result<&'a str, ProtocolError> {
    args[field]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid(format!("{field} must be a non-empty string")))
}

fn optional_string<'a>(args: &'a Value, field: &str) -> Result<Option<&'a str>, ProtocolError> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => string(args, field).map(Some),
    }
}

/// The advertised authentication method `id`, if any.
fn auth_method<'a>(initialize: Option<&'a Value>, id: &str) -> Option<&'a Value> {
    initialize?["authMethods"]
        .as_array()?
        .iter()
        .find(|method| method["id"] == id)
}

fn arguments(
    publisher: &Publisher,
    initialize: Option<&Value>,
    scope: &Scope,
    live: &Value,
    kind: OperationKind,
    args: &Value,
) -> Result<Value, ProtocolError> {
    match kind {
        OperationKind::Prompt => {
            let Scope::Session(session_id) = scope else {
                unreachable!("a prompt is a session's");
            };
            let blocks = args["prompt"]
                .as_array()
                .ok_or_else(|| invalid("prompt must be an array of blocks"))?;
            // The content's capability comes before its arguments (§6.4).
            prompt_capabilities(initialize, blocks)?;
            let prompt = blocks
                .iter()
                .map(|block| prompt_block(publisher, session_id, block))
                .collect::<Result<Vec<_>, _>>()?;
            prompt_capabilities(initialize, &prompt)?;
            Ok(json!({ "prompt": prompt }))
        }
        OperationKind::SetMode => {
            let mode_id = string(args, "modeId")?;
            let modes = &live["controls"]["modes"];
            crate::semantic::validate_session_mode_reference(
                (!modes.is_null()).then_some(modes),
                mode_id,
            )
            .map_err(invalid)?;
            Ok(json!({ "modeId": mode_id }))
        }
        OperationKind::SetConfig => {
            let config_id = string(args, "configId")?;
            let value = args
                .get("value")
                .ok_or_else(|| invalid("value is required"))?;
            crate::semantic::validate_session_config_reference(
                &live["controls"]["configOptions"],
                config_id,
                value,
            )
            .map_err(invalid)?;
            Ok(json!({ "configId": config_id, "value": value }))
        }
        OperationKind::Fork => Ok(match optional_string(args, "cwd")? {
            Some(cwd) => json!({ "cwd": session_path(cwd)? }),
            None => json!({}),
        }),
        OperationKind::Create => Ok(json!({ "cwd": session_path(string(args, "cwd")?)? })),
        OperationKind::Delete => Ok(json!({ "sessionId": string(args, "sessionId")? })),
        OperationKind::Authenticate => {
            let method_id = string(args, "methodId")?;
            match auth_method(initialize, method_id) {
                Some(method) if method["type"] != "terminal" => {}
                _ => {
                    return Err(invalid(format!(
                        "{method_id} is not an authentication method the Agent runs"
                    )));
                }
            }
            Ok(json!({ "methodId": method_id }))
        }
        OperationKind::Logout => Ok(json!({})),
        OperationKind::TerminalLogin => {
            let method_id = string(args, "methodId")?;
            if auth_method(initialize, method_id).is_none_or(|method| method["type"] != "terminal")
            {
                return Err(invalid(format!(
                    "{method_id} is not a terminal authentication method"
                )));
            }
            let size = |field: &str| {
                args[field]
                    .as_u64()
                    .filter(|size| (1..=u64::from(u16::MAX)).contains(size))
                    .ok_or_else(|| invalid(format!("{field} must be a positive size")))
            };
            Ok(json!({ "methodId": method_id, "cols": size("cols")?, "rows": size("rows")? }))
        }
        OperationKind::List => {
            let cursor = optional_string(args, "cursor")?;
            if let Some(revision) = optional_string(args, "revision")?
                && live["revision"].as_str() != Some(revision)
            {
                return Err(refuse(
                    ErrorCode::CatalogChanged,
                    format!("the catalog is no longer at revision {revision}"),
                ));
            }
            let mut input = json!({});
            if let Some(cursor) = cursor {
                input["cursor"] = json!(cursor);
            }
            if let Some(revision) = args.get("revision").filter(|revision| !revision.is_null()) {
                input["revision"] = revision.clone();
            }
            Ok(input)
        }
        OperationKind::Load
        | OperationKind::Open
        | OperationKind::None
        | OperationKind::AgentRequest => {
            unreachable!("bridge-started kinds are refused first")
        }
    }
}

/// A prompt block: an ACP content block, or a reference to an attachment of
/// the session, resolved to the block it names.
fn prompt_block(
    publisher: &Publisher,
    session_id: &str,
    block: &Value,
) -> Result<Value, ProtocolError> {
    if block["type"] == "attachment" {
        let reference: AttachmentRef = serde_json::from_value(block.clone())
            .map_err(|error| invalid(format!("invalid attachment reference: {error}")))?;
        return publisher
            .attachment(session_id, &reference.id)
            .ok_or_else(|| invalid(format!("the session has no attachment {}", reference.id)));
    }
    serde_json::from_value::<agent_client_protocol::schema::v1::ContentBlock>(block.clone())
        .map_err(|error| invalid(format!("invalid prompt block: {error}")))?;
    crate::semantic::validate_content_block(block, "Prompt content").map_err(invalid)?;
    Ok(block.clone())
}

/// A session's working directory: an absolute path without NUL, as the
/// bridge passes it to the Agent.
fn session_path(cwd: &str) -> Result<&str, ProtocolError> {
    if crate::bridge::valid_session_path(cwd) {
        Ok(cwd)
    } else {
        Err(invalid("cwd must be an absolute path"))
    }
}

/// The content a prompt carries needs the Agent's prompt capabilities.
fn prompt_capabilities(initialize: Option<&Value>, prompt: &[Value]) -> Result<(), ProtocolError> {
    let Some(initialize) = initialize else {
        return Ok(());
    };
    let capabilities = &initialize["agentCapabilities"]["promptCapabilities"];
    for block in prompt {
        let capability = match block["type"].as_str() {
            Some("image") => "image",
            Some("audio") => "audio",
            Some("resource") => "embeddedContext",
            _ => continue,
        };
        if capabilities[capability] != true {
            return Err(refuse(
                ErrorCode::Unsupported,
                format!("the Agent did not advertise {capability} prompt support"),
            ));
        }
    }
    Ok(())
}
