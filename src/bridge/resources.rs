//! Resource actions in the bridge actor (protocol §7, migration step 6).
//!
//! An action takes no `expect` and installs no operation. Its resource is
//! found by the full scope and resource ID in the current state and checked
//! under one bridge guard; a dismissal is applied there too, and the commit
//! publishes it. Terminal I/O runs after the guard is released, in the order
//! the actor receives it, so a stdin under backpressure never holds the lock
//! the terminal's output needs.
use super::*;
use crate::protocol::documents::{ErrorCode, ProtocolError};

/// A resource action, its body parsed (protocol §7).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ResourceAction {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
    Dismiss,
}

/// The terminal I/O of a checked action, run after the guard is released.
pub(super) struct TerminalIo {
    terminal: String,
    action: ResourceAction,
}

fn not_found(scope: &OpScope, resource: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::NotFound,
        format!("{scope} has no resource {resource}"),
    )
}

fn not_allowed(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorCode::NotAllowed, message)
}

/// Checks `action` on `resource` in `scope` under the guard, and applies a
/// dismissal; the terminal I/O of an input or resize is returned to run
/// once the guard is released.
pub(super) fn act(
    state: &mut BridgeState,
    scope: &OpScope,
    resource: &str,
    action: ResourceAction,
) -> Result<Option<TerminalIo>, ProtocolError> {
    if action == ResourceAction::Dismiss && dismiss_url_flow(state, scope, resource) {
        return Ok(None);
    }
    // A login terminal is `global`'s, by its operation's ID (§4.6).
    let exited = match scope {
        OpScope::Global => state
            .sessions
            .scopes
            .login_terminals
            .get(resource)
            .map(|terminal| terminal.exit_status.is_some()),
        _ => None,
    }
    .ok_or_else(|| not_found(scope, resource))?;
    match action {
        ResourceAction::Dismiss if !exited => Err(not_allowed(format!(
            "login terminal {resource} is running; it is dismissed after it exits"
        ))),
        ResourceAction::Dismiss => {
            state.sessions.discard_login_terminal(resource);
            Ok(None)
        }
        _ if exited => Err(not_allowed(format!("login terminal {resource} has exited"))),
        action => Ok(Some(TerminalIo {
            terminal: resource.to_string(),
            action,
        })),
    }
}

/// Runs a checked action's terminal I/O, without the bridge guard, and
/// answers `reply` when it ends. Input is queued on the terminal's own
/// blocking writer in the order the actor receives it (C3), written each
/// time; the actor does not wait for the write.
pub(super) fn perform(
    auth_terminal: &AuthTerminalManager,
    io: TerminalIo,
    reply: oneshot::Sender<Result<(), ProtocolError>>,
) {
    let refused = |error: Error| not_allowed(error_message(error));
    match io.action {
        ResourceAction::Input { data } => match auth_terminal.queue_input(&io.terminal, data) {
            Err(error) => {
                let _ = reply.send(Err(refused(error)));
            }
            Ok(written) => {
                tokio::spawn(async move {
                    let result = match written.await {
                        Ok(result) => result.map_err(refused),
                        Err(_) => Err(not_allowed(format!(
                            "login terminal {} ended before its input was written",
                            io.terminal
                        ))),
                    };
                    let _ = reply.send(result);
                });
            }
        },
        ResourceAction::Resize { cols, rows } => {
            let _ = reply.send(
                auth_terminal
                    .resize(&io.terminal, cols, rows)
                    .map_err(refused),
            );
        }
        ResourceAction::Dismiss => unreachable!("a dismissal is applied under the guard"),
    }
}

/// Dismisses the URL flow `scope` shows as `interaction_id`, if any. A flow
/// is named by the interaction it was accepted from, so an old flow's late
/// dismissal never reaches a new flow that reuses its `elicitationId`.
fn dismiss_url_flow(state: &mut BridgeState, scope: &OpScope, interaction_id: &str) -> bool {
    let Some((elicitation_id, registration)) = state
        .sessions
        .url_owners
        .iter()
        .find(|(_, route)| route.registration_id == interaction_id)
        .map(|(id, route)| (id.clone(), route.clone()))
    else {
        return false;
    };
    // The scope whose live shows the flow, as the projection places it.
    let shown_in = match &registration.owner {
        Some(owner) => {
            if !state.sessions.owns_resources(owner)
                || !resolves_canonically(state, &owner.session_id)
            {
                return false;
            }
            OpScope::Session(owner.session_id.clone())
        }
        None => state
            .sessions
            .scopes
            .flow_scopes
            .get(interaction_id)
            .cloned()
            .unwrap_or(OpScope::Global),
    };
    if &shown_in != scope {
        return false;
    }
    let epoch = state.sessions.epoch().to_owned();
    let resolution = state.sessions.settle_url_registration(
        &epoch,
        registration
            .owner
            .as_ref()
            .map(|owner| owner.session_id.as_str()),
        &elicitation_id,
        &registration.registration_id,
        UrlFlowStatus::Cancelled,
    );
    if !matches!(resolution, Ok(UrlFlowResolution::Applied)) {
        return false;
    }
    state
        .sessions
        .take_url_route(&elicitation_id, &registration);
    state.sessions.scopes.flow_scopes.remove(interaction_id);
    refresh_observer_timers(state);
    true
}

/// The workspace of a held session's context reads (protocol §8): its own
/// `cwd` under the launch's roots, by the existing workspace checks. It holds,
/// loads and renews nothing.
pub(super) fn workspace(
    state: &BridgeState,
    filesystem: Option<&WorkspaceFileSystem>,
    session_id: &str,
) -> Result<WorkspaceFileSystem, ProtocolError> {
    let session = state.sessions.resource_session(session_id).ok_or_else(|| {
        ProtocolError::new(
            ErrorCode::NotFound,
            format!("session/{session_id} is not held"),
        )
    })?;
    let filesystem = filesystem.ok_or_else(|| {
        ProtocolError::new(
            ErrorCode::Unsupported,
            "workspace context is unavailable for remote transports",
        )
    })?;
    filesystem.for_workspace(&session.live.cwd).map_err(refusal)
}

/// The protocol error of a refused ACP-level check: a workspace context
/// check or a terminal size (protocol §9).
pub(crate) fn refusal(error: Error) -> ProtocolError {
    let code = match i32::from(error.code) {
        -32002 => ErrorCode::NotFound,
        -32600 | -32602 => ErrorCode::InvalidRequest,
        _ => ErrorCode::Unavailable,
    };
    ProtocolError::new(code, error_message(error))
}
