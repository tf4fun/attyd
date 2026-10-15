//! Resource actions in the bridge actor (protocol §7, migration step 6).
//!
//! An action takes no `expect` and installs no operation. Its resource is
//! found by the full scope and resource ID in the current state, and it is
//! checked and applied under one bridge guard; the commit publishes it.
use super::*;
use crate::protocol::documents::{ErrorCode, ProtocolError};

/// A resource action, its body parsed (protocol §7).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ResourceAction {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
    Dismiss,
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

/// Applies `action` to `resource` in `scope`.
pub(super) fn act(
    state: &mut BridgeState,
    sink: &EventSink,
    auth_terminal: &AuthTerminalManager,
    scope: &OpScope,
    resource: &str,
    action: ResourceAction,
) -> Result<(), ProtocolError> {
    if action == ResourceAction::Dismiss && dismiss_url_flow(state, sink, scope, resource) {
        return Ok(());
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
            Ok(())
        }
        _ if exited => Err(not_allowed(format!("login terminal {resource} has exited"))),
        // Written each time, in the order the bridge receives it (C3).
        ResourceAction::Input { data } => auth_terminal
            .write(resource, &data)
            .map_err(|error| not_allowed(error_message(error))),
        ResourceAction::Resize { cols, rows } => auth_terminal
            .resize(resource, cols, rows)
            .map_err(|error| not_allowed(error_message(error))),
    }
}

/// Dismisses the URL flow `scope` shows as `interaction_id`, if any. A flow
/// is named by the interaction it was accepted from, so an old flow's late
/// dismissal never reaches a new flow that reuses its `elicitationId`.
fn dismiss_url_flow(
    state: &mut BridgeState,
    sink: &EventSink,
    scope: &OpScope,
    interaction_id: &str,
) -> bool {
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
            .map(|owner| (owner.session_id.as_str(), owner.incarnation)),
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
    flush_runtime(state, sink);
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
