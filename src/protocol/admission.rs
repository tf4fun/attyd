//! Admission of operation writes (protocol §6.4).
//!
//! A write is checked against the published tree, which is the bridge
//! state as of its last commit: the bridge checks it inside the guard that
//! installs it, and without an Agent connection the server checks it the same
//! way before it answers `unavailable`. The checks run in §6.4's order:
//! `expect`, then status and occupation, then capability and arguments. A
//! refusal changes nothing.
use agent_client_protocol::schema::v1::{
    CreateElicitationResponse, RequestPermissionOutcome, RequestPermissionResponse,
};
use serde_json::{Value, json};

use super::canon::AttachmentRef;
use super::documents::{ErrorCode, OperationKind, ProtocolError, Scope};
use super::tree::{Node, Tree};

/// The block an attachment of a session names: `(session ID, attachment ID)`.
pub(crate) type AttachmentOf<'a> = &'a dyn Fn(&str, &str) -> Option<Value>;

/// The committed target, borrowed without rebuilding its entries or other children.
pub(crate) fn target<'a>(tree: &'a Tree, scope: &Scope) -> Option<&'a Node> {
    match scope {
        Scope::Global => tree.get("/global"),
        Scope::Catalog => tree.get("/catalog"),
        Scope::Session(id) => tree.get("/sessions")?.child(id),
    }
}

fn operation(target: &Node) -> &Value {
    target
        .child("op")
        .and_then(Node::as_value)
        .map(|(value, _)| value)
        .unwrap_or(&Value::Null)
}

/// `POST /api/v2/scopes/{scope}/ops` (protocol §6.1).
#[derive(Debug, Clone)]
pub(crate) struct OpWrite {
    /// The ID the client chose for the operation it starts (§6.5); the bridge
    /// names the operation when the write has none.
    pub id: Option<String>,
    pub expect: String,
    pub kind: String,
    pub args: Value,
}

/// What an admitted write does.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Admitted {
    /// Starts an operation of `kind` with `input` (its `args`). A prompt's
    /// `input` is its blocks with every attachment reference resolved.
    Start {
        kind: OperationKind,
        input: Value,
        /// The ID the client chose.
        id: Option<String>,
        /// The operation it replaces: the write's `expect`.
        after: String,
    },
    /// A write whose ID is the scope's current operation: the client repeats
    /// a write whose answer it did not get. Nothing starts (§6.6).
    Repeat { op: String },
    /// `cancel` of the running operation `op` of `kind`.
    Cancel { op: String, kind: OperationKind },
}

/// A validated answer to one pending interaction (protocol §6.2).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Answer {
    Permission(RequestPermissionOutcome),
    Elicitation(CreateElicitationResponse),
}

/// Whether `id` has the form of an operation ID: 128 bits in unpadded
/// base64url, 22 characters (protocol §6.5).
pub(crate) fn valid_operation_id(id: &str) -> bool {
    id.len() == 22
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
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

/// Checks `write` in `scope` against the committed target's head and control fields.
pub(crate) fn admit(
    tree: &Tree,
    attachment_of: AttachmentOf,
    scope: &Scope,
    write: &OpWrite,
) -> Result<Admitted, ProtocolError> {
    let started = started_kind(scope, &write.kind);
    if started.is_none() && write.kind != "cancel" {
        return Err(invalid(format!(
            "{} is not an operation clients write in {scope}",
            write.kind
        )));
    }
    if let Some(id) = &write.id
        && !valid_operation_id(id)
    {
        return Err(invalid(format!(
            "an operation ID is 22 characters of base64url, not {id:?}"
        )));
    }
    let held = target(tree, scope)
        .ok_or_else(|| refuse(ErrorCode::NotFound, format!("{scope} is not held")))?;
    let op = operation(held);
    // A write whose ID is the current operation's repeats one whose answer was
    // lost: accepted again, before `expect` is judged (protocol §6.4, §6.6).
    if started.is_some()
        && let Some(id) = &write.id
        && op["id"].as_str() == Some(id.as_str())
    {
        return Ok(Admitted::Repeat { op: id.clone() });
    }
    if op["id"].as_str() != Some(write.expect.as_str()) {
        return Err(refuse(
            ErrorCode::Stale,
            format!("{} is not the current operation of {scope}", write.expect),
        ));
    }
    let running = op["status"] == "running";
    let Some(kind) = started else {
        if !running {
            return Err(refuse(
                ErrorCode::NotAllowed,
                format!("the operation of {scope} has ended"),
            ));
        }
        let id = write.expect.clone();
        if write.kind == "cancel" {
            let kind = serde_json::from_value::<OperationKind>(op["kind"].clone())
                .map_err(|error| invalid(format!("unknown operation kind: {error}")))?;
            if !matches!(
                kind,
                OperationKind::Prompt | OperationKind::TerminalLogin
            ) {
                return Err(refuse(
                    ErrorCode::NotAllowed,
                    format!("a {} refuses a cancel", op["kind"].as_str().unwrap_or("")),
                ));
            }
            return Ok(Admitted::Cancel { op: id, kind });
        }
        unreachable!("only a cancel is written for a running operation");
    };
    if running {
        return Err(refuse(
            ErrorCode::NotAllowed,
            format!("the operation of {scope} is running"),
        ));
    }
    // Close is an action: its pending response does not replace the ended public op.
    // Repeated identities have already returned above; only a new prompt is refused.
    if kind == OperationKind::Prompt && held.field("closing").and_then(Value::as_bool) == Some(true) {
        return Err(refuse(
            ErrorCode::NotAllowed,
            format!("{scope} is closing"),
        ));
    }
    if kind == OperationKind::Delete
        && let Some(session_id) = write.args["sessionId"].as_str()
        && tree
            .get("/sessions")
            .and_then(|sessions| sessions.child(session_id))
            .is_some_and(|session| operation(session)["status"] == "running")
    {
        return Err(refuse(
            ErrorCode::NotAllowed,
            format!("session/{session_id} is not free"),
        ));
    }
    // A fork is recorded only in its source session. Admission excludes both global writes
    // and other forks while it runs, so pre-installation messages have one owner (§9).
    let global = target(tree, &Scope::Global);
    if kind == OperationKind::Fork
        && global.is_some_and(|global| operation(global)["status"] == "running")
    {
        return Err(refuse(ErrorCode::NotAllowed, "global is not free"));
    }
    if (matches!(scope, Scope::Global) || kind == OperationKind::Fork)
        && global
            .and_then(|global| global.child("sessions"))
            .is_some_and(|sessions| {
                sessions.child_names().into_iter().any(|id| {
                    sessions
                        .child(id)
                        .and_then(Node::as_value)
                        .is_some_and(|(session, _)| {
                            session["op"]["kind"] == "fork" && session["op"]["status"] == "running"
                        })
                })
            })
    {
        return Err(refuse(
            ErrorCode::NotAllowed,
            "a fork is installing a session",
        ));
    }
    let initialize = global
        .and_then(|global| global.field("agent"))
        .and_then(|agent| agent.get("initialize"))
        .filter(|initialize| !initialize.is_null());
    capability(initialize, kind)?;
    let input = arguments(
        attachment_of,
        initialize,
        scope,
        held.field("revision").and_then(Value::as_str),
        kind,
        &write.args,
    )?;
    Ok(Admitted::Start {
        kind,
        input,
        id: write.id.clone(),
        after: write.expect.clone(),
    })
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

/// The answer to one pending `interaction` from `body`: `{ outcome }` for a
/// permission, `{ response }` for an elicitation (protocol §7).
pub(crate) fn answer_one(interaction: &Value, body: &Value) -> Result<Answer, ProtocolError> {
    let request = &interaction["request"];
    Ok(match interaction["kind"].as_str() {
        Some("permission") => Answer::Permission(permission_answer(request, body)?),
        _ => Answer::Elicitation(elicitation_answer(request, body)?),
    })
}

/// `{ outcome }`, selecting an option the request offers, if any.
fn permission_answer(
    request: &Value,
    answer: &Value,
) -> Result<RequestPermissionOutcome, ProtocolError> {
    let outcome = answer
        .get("outcome")
        .ok_or_else(|| invalid("a permission answer needs an outcome"))?;
    let response: RequestPermissionResponse = serde_json::from_value(json!({ "outcome": outcome }))
        .map_err(|error| invalid(format!("invalid permission outcome: {error}")))?;
    if let RequestPermissionOutcome::Selected(selected) = &response.outcome {
        let offered = request["options"].as_array().is_some_and(|options| {
            options
                .iter()
                .any(|option| option["optionId"] == selected.option_id.0.as_ref())
        });
        if !offered {
            return Err(invalid("the permission did not offer that option"));
        }
    }
    Ok(response.outcome)
}

/// `{ response }`, valid for the elicitation's request.
fn elicitation_answer(
    request: &Value,
    answer: &Value,
) -> Result<CreateElicitationResponse, ProtocolError> {
    let response = answer
        .get("response")
        .ok_or_else(|| invalid("an elicitation answer needs a response"))?;
    crate::elicitation_validation::validate_elicitation_response_value(request, response)
        .map_err(invalid)?;
    serde_json::from_value(response.clone())
        .map_err(|error| invalid(format!("invalid elicitation response: {error}")))
}

/// The advertised authentication method `id`, if any.
fn auth_method<'a>(initialize: Option<&'a Value>, id: &str) -> Option<&'a Value> {
    initialize?["authMethods"]
        .as_array()?
        .iter()
        .find(|method| method["id"] == id)
}

fn arguments(
    attachment_of: AttachmentOf,
    initialize: Option<&Value>,
    scope: &Scope,
    catalog_revision: Option<&str>,
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
                .map(|block| prompt_block(attachment_of, session_id, block))
                .collect::<Result<Vec<_>, _>>()?;
            prompt_capabilities(initialize, &prompt)?;
            Ok(json!({ "prompt": prompt }))
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
                && catalog_revision != Some(revision)
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
        OperationKind::Load | OperationKind::Open | OperationKind::None => {
            unreachable!("bridge-started kinds are refused first")
        }
    }
}

/// A prompt block: an ACP content block, or a reference to an attachment of
/// the session, resolved to the block it names.
fn prompt_block(
    attachment_of: AttachmentOf,
    session_id: &str,
    block: &Value,
) -> Result<Value, ProtocolError> {
    if block["type"] == "attachment" {
        let reference: AttachmentRef = serde_json::from_value(block.clone())
            .map_err(|error| invalid(format!("invalid attachment reference: {error}")))?;
        return attachment_of(session_id, &reference.id)
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

#[cfg(test)]
mod tests;
