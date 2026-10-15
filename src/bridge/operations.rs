//! Operation writes in the bridge actor (protocol §6, migration step 6).
//!
//! A write is admitted and its operation installed under one bridge guard;
//! the commit publishes it. The operation then runs as the internal command
//! of its kind, on the normal ACP path, with the operation ID as its request
//! ID, so its end settles that slot and no other.
use super::*;
use crate::protocol::admission::{self, Admitted, OpWrite};
use crate::protocol::documents::{ErrorCode, ProtocolError};
use crate::protocol::op::OpEnd;
use crate::protocol::registry::InstallError;

/// An admitted write: its operation, and for `create`, `fork` and `list` the
/// end its held response waits for (protocol §6.3).
pub(crate) struct OperationAdmission {
    pub op: String,
    pub end: Option<oneshot::Receiver<OpEnd>>,
}

/// The internal command that tags the operation it runs for.
pub(super) const OPERATION_FIELD: &str = "protocolOperation";

/// Admits `write` in `scope` and installs its operation. Returns the
/// admission and the command that runs it.
pub(super) fn admit(
    state: &mut BridgeState,
    scope: &OpScope,
    write: &OpWrite,
) -> Result<(OperationAdmission, Value), ProtocolError> {
    let admitted = {
        let published = state.protocol.publication.lock();
        admission::admit(&published.publisher, scope, write)?
    };
    let Admitted::Start { kind, input } = admitted else {
        unreachable!("cancel and answer are refused until they are served");
    };
    let installed = match (kind, scope) {
        (OpKind::Prompt, OpScope::Session(session_id)) => {
            let blocks = input["prompt"].as_array().cloned().unwrap_or_default();
            state.sessions.install_prompt(session_id, &blocks)
        }
        _ => state.sessions.install_operation(scope, kind, input.clone()),
    };
    let op = installed.map_err(|error| match error {
        InstallError::NotFound => {
            ProtocolError::new(ErrorCode::NotFound, format!("{scope} is not held"))
        }
        InstallError::NotFree => {
            ProtocolError::new(ErrorCode::NotAllowed, format!("{scope} is not free"))
        }
    })?;
    let end = matches!(kind, OpKind::Create | OpKind::Fork | OpKind::List).then(|| {
        let (sender, end) = oneshot::channel();
        state.sessions.hold_response(scope, &op, sender);
        end
    });
    let command = command(state, scope, kind, &op, input);
    Ok((OperationAdmission { op, end }, command))
}

/// The internal command of an admitted operation.
fn command(state: &BridgeState, scope: &OpScope, kind: OpKind, op: &str, input: Value) -> Value {
    let session_id = match scope {
        OpScope::Session(session_id) => Some(session_id.as_str()),
        _ => None,
    };
    let mut command = match kind {
        OpKind::Prompt => {
            let session_id = session_id.expect("a prompt is a session's");
            let revision = state
                .sessions
                .active_session(session_id)
                .and_then(|session| session.state.history_revision.clone());
            json!({
                "type": "session/prompt",
                "clientIntentId": op,
                "historyRevision": revision,
                "prompt": input["prompt"],
            })
        }
        OpKind::SetMode => json!({ "type": "session/set_mode", "modeId": input["modeId"] }),
        OpKind::SetConfig => json!({
            "type": "session/set_config_option",
            "configId": input["configId"],
            "value": input["value"],
        }),
        OpKind::Fork => {
            let mut command = json!({ "type": "session/fork" });
            if let Some(cwd) = input.get("cwd") {
                command["cwd"] = cwd.clone();
            }
            command
        }
        OpKind::Create => json!({ "type": "session/new", "cwd": input["cwd"] }),
        OpKind::Delete => json!({ "type": "session/delete", "sessionId": input["sessionId"] }),
        OpKind::Authenticate => {
            json!({ "type": "auth/authenticate", "methodId": input["methodId"] })
        }
        OpKind::Logout => json!({ "type": "auth/logout" }),
        OpKind::TerminalLogin => json!({
            "type": "auth/terminal_start",
            "methodId": input["methodId"],
            "cols": input["cols"],
            "rows": input["rows"],
        }),
        OpKind::List => {
            let mut command = json!({ "type": "session/list" });
            if let Some(cursor) = input.get("cursor") {
                command["cursor"] = cursor.clone();
            }
            // Admission checked the revision; the list still refuses a catalog
            // that changes before the Agent answers.
            if input.get("revision").is_some() {
                command["expectedCatalogRevision"] = json!(session_catalog_revision(state));
            }
            command
        }
        OpKind::Load | OpKind::Open | OpKind::None | OpKind::AgentRequest => {
            unreachable!("bridge-started kinds are never admitted")
        }
    };
    command["requestId"] = json!(op);
    command[OPERATION_FIELD] = json!(op);
    if let Some(session_id) = session_id {
        command["sessionId"] = json!(session_id);
    }
    command
}

/// The session a write's command acts on, for ordering.
pub(super) fn write_session<'a>(scope: &'a OpScope, write: &'a OpWrite) -> Option<&'a str> {
    match scope {
        OpScope::Session(session_id) => Some(session_id),
        OpScope::Global if write.kind == "delete" => write.args["sessionId"].as_str(),
        _ => None,
    }
}

/// Whether a write's command is ordinary traffic, like the v1 command it
/// becomes.
pub(super) fn ordinary(write: &OpWrite) -> bool {
    matches!(write.kind.as_str(), "prompt" | "fork" | "create")
}
