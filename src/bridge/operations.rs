//! Operation writes in the bridge actor (protocol §6, migration step 6).
//!
//! A write is admitted and its operation installed under one bridge guard;
//! the commit publishes it. The operation then runs as the internal command
//! of its kind, on the normal ACP path, with the operation ID as its request
//! ID, so its end settles that slot and no other.
use super::*;
use crate::protocol::admission::{self, Admitted, Answer, OpWrite};
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

/// An admitted write.
pub(super) enum Write {
    /// A started operation, and the command that runs it.
    Start(OperationAdmission, Value),
    /// A `cancel` or `answer` of the running operation, which the caller
    /// applies under the same guard (`cancel`, `answer`).
    Running(Admitted),
    /// A repeat of a write already admitted: accepted again, nothing starts.
    Repeat(OperationAdmission),
}

/// Admits `write` in `scope`; a write that starts an operation installs it.
pub(super) fn admit(
    state: &mut BridgeState,
    scope: &OpScope,
    write: &OpWrite,
) -> Result<Write, ProtocolError> {
    let admitted = {
        let published = state.protocol.publication.lock();
        admission::admit(
            &|scope| published.feed.live(scope),
            &|session, id| published.feed.attachment(session, id),
            scope,
            write,
        )?
    };
    if let Admitted::Repeat { op } = admitted {
        return Ok(Write::Repeat(OperationAdmission { op, end: None }));
    }
    let Admitted::Start {
        kind,
        input,
        id,
        after,
    } = admitted
    else {
        return Ok(Write::Running(admitted));
    };
    let installed = match (kind, scope) {
        (OpKind::Prompt, OpScope::Session(session_id)) => {
            let blocks = input["prompt"].as_array().cloned().unwrap_or_default();
            state
                .sessions
                .install_prompt_as(session_id, &blocks, id, Some(after))
        }
        _ => state
            .sessions
            .install_operation_as(scope, kind, input.clone(), id, Some(after)),
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
    Ok(Write::Start(OperationAdmission { op, end }, command))
}

fn not_allowed(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorCode::NotAllowed, message)
}

/// Reports what an admitted answer or cancel could not settle; nothing it
/// already delivered is undone.
fn report(sink: &EventSink, operation: &str, error: Error) {
    sink.acp_error(error, None, Some(operation));
}

/// Where a pending interaction waits, and what it is answered with.
pub(super) enum Delivery {
    Permission(SessionResourceOwner, RequestPermissionResponse),
    Elicitation {
        owner: Option<SessionResourceOwner>,
        response: CreateElicitationResponse,
        accepted_url_id: Option<String>,
    },
}

/// Finds the responder of every answered interaction by its ID, wherever it
/// waits (a session or the connection's request-scoped elicitations); one
/// that is gone refuses the whole answer before anything is delivered.
pub(super) fn deliveries(
    state: &BridgeState,
    answers: Vec<(String, Answer)>,
) -> Result<Vec<(String, Delivery)>, ProtocolError> {
    let gone = |id: &str| not_allowed(format!("interaction {id} is no longer pending"));
    answers
        .into_iter()
        .map(|(id, answer)| {
            let delivery = match answer {
                Answer::Permission(outcome) => {
                    let (owner, _) = state.sessions.permission(&id).ok_or_else(|| gone(&id))?;
                    Delivery::Permission(owner.clone(), RequestPermissionResponse::new(outcome))
                }
                Answer::Elicitation(response) => {
                    let (owner, pending) =
                        pending_elicitation(state, &id).ok_or_else(|| gone(&id))?;
                    let accepted_url_id = matches!(&response.action, ElicitationAction::Accept(_))
                        .then(|| pending.url_elicitation_id.clone())
                        .flatten();
                    if let Some(url_id) = &accepted_url_id
                        && (state.sessions.url_owners.contains_key(url_id)
                            || state.sessions.url_flow_in_use(url_id))
                    {
                        return Err(not_allowed(format!("URL elicitation {url_id} is in use")));
                    }
                    Delivery::Elicitation {
                        owner: owner.cloned(),
                        response,
                        accepted_url_id,
                    }
                }
            };
            Ok((id, delivery))
        })
        .collect()
}

/// Delivers every answer of an admitted `answer` under this guard, so the
/// commit publishes them, and an accepted URL elicitation's flow, at once.
pub(super) fn deliver(
    state: &mut BridgeState,
    sink: &EventSink,
    deliveries: Vec<(String, Delivery)>,
) {
    for (id, delivery) in deliveries {
        let epoch = state.sessions.epoch().to_string();
        let delivered = match delivery {
            Delivery::Permission(owner, response) => {
                deliver_permission(state, &owner, &id, response, |state| {
                    state
                        .sessions
                        .resolve_permission(&epoch, &owner.session_id, &id)
                        .map(drop)
                        .map_err(runtime_state_error)
                })
            }
            Delivery::Elicitation {
                owner,
                response,
                accepted_url_id,
            } => {
                let scope = owner.as_ref().map(|owner| owner.session_id.as_str());
                deliver_elicitation(
                    state,
                    &id,
                    owner.as_ref(),
                    response,
                    accepted_url_id,
                    |state, accepted_url_id| {
                        state
                            .sessions
                            .resolve_elicitation(&epoch, scope, &id, accepted_url_id)
                            .map(drop)
                            .map_err(runtime_state_error)
                    },
                )
            }
        };
        if let Err(error) = delivered {
            report(sink, "answer", error);
        }
    }
}

/// The cancel outcome of each pending interaction of the operation that
/// `scope` shows (protocol §6.2).
pub(super) fn cancel_outcomes(state: &BridgeState, scope: &OpScope) -> Vec<(String, Answer)> {
    let published = state.protocol.publication.lock();
    let Some(live) = published.feed.live(scope) else {
        return Vec::new();
    };
    live["interactions"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(id, interaction)| {
            let answer = if interaction["kind"] == "permission" {
                Answer::Permission(RequestPermissionOutcome::Cancelled)
            } else {
                Answer::Elicitation(CreateElicitationResponse::new(ElicitationAction::Cancel))
            };
            (id.clone(), answer)
        })
        .collect()
}

/// Applies an admitted `cancel` of the running operation `op` under the guard that admitted
/// it. It installs nothing: the operation runs
/// on and ends by its own path.
pub(super) fn apply(
    state: &mut BridgeState,
    sink: &EventSink,
    connection: &ConnectionTo<Agent>,
    auth_terminal: &AuthTerminalManager,
    scope: &OpScope,
    admitted: Admitted,
) -> Result<OperationAdmission, ProtocolError> {
    let op = match admitted {
        Admitted::Start { .. } | Admitted::Repeat { .. } => {
            unreachable!("a started operation is installed by admit")
        }
        Admitted::Cancel { op, kind } => {
            cancel(state, sink, connection, auth_terminal, scope, &op, kind);
            op
        }
    };
    Ok(OperationAdmission { op, end: None })
}

/// A cancel sets `cancelRequested` once and asks for the operation's end; a
/// repeat changes nothing. The operation keeps its scopes until it ends.
fn cancel(
    state: &mut BridgeState,
    sink: &EventSink,
    connection: &ConnectionTo<Agent>,
    auth_terminal: &AuthTerminalManager,
    scope: &OpScope,
    op: &str,
    kind: OpKind,
) {
    if !state.sessions.request_operation_cancel(scope, op) {
        return;
    }
    match (kind, scope) {
        (OpKind::Prompt, OpScope::Session(session_id)) => {
            // A prompt whose turn has not started yet is cancelled once it
            // is sent (the `session/prompt` command).
            let started = state
                .sessions
                .state(session_id)
                .and_then(|session| session.active_turn.as_ref())
                .and_then(|turn| turn.execution.as_ref())
                .is_some_and(|execution| execution.rpc_operation_id == op);
            if !live_session(state, session_id) {
                return;
            }
            if started {
                if let Err(error) = connection
                    .send_notification(CancelNotification::new(session_id.to_string()))
                    .and_then(|()| commit_session_cancel(state, sink, session_id))
                {
                    report(sink, "session/cancel", error);
                }
            }
        }
        // A login that has not started yet is not started (the
        // `auth/terminal_start` command); a running one is stopped, and its
        // operation ends when the process exits.
        (OpKind::TerminalLogin, _) => {
            let _ = auth_terminal.cancel(op);
        }
        _ => unreachable!("admission refuses a cancel of other kinds"),
    }
}

/// The internal command of an admitted operation.
fn command(state: &BridgeState, scope: &OpScope, kind: OpKind, op: &str, input: Value) -> Value {
    let session_id = match scope {
        OpScope::Session(session_id) => Some(session_id.as_str()),
        _ => None,
    };
    let mut command = match kind {
        OpKind::Prompt => {
            json!({
                "type": "session/prompt",
                "clientIntentId": op,
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
        OpKind::Load | OpKind::Open | OpKind::None => {
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
