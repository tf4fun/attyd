//! Control actions in the bridge actor (protocol §7): `set_mode` and
//! `set_config` set a value of a held session, also while a prompt runs. They
//! take no `expect` and install no operation; the response is held until the
//! Agent answers, and the change reaches every replica through `controls`.
use super::*;
use crate::protocol::documents::{ErrorCode, ProtocolError};

/// A control action, its body parsed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Control {
    SetMode { mode_id: String },
    SetConfig { config_id: String, value: Value },
    /// Ends the session at the Agent and releases everything the bridge held.
    Close,
}

/// What the Agent answered: nothing, or the error it answered with.
pub(crate) type ControlAnswer = oneshot::Receiver<Result<(), ProtocolError>>;

fn refusal(code: ErrorCode, message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(code, message)
}

/// Checks `control` against the session under the bridge guard and, if it
/// can be sent, returns the command that sends it and the answer to wait for.
/// A refusal before anything is sent changes nothing.
pub(super) fn admit(
    state: &mut BridgeState,
    sink: &EventSink,
    session_id: &str,
    control: &Control,
) -> Result<(Value, ControlAnswer), ProtocolError> {
    let scope = OpScope::Session(session_id.to_string());
    // Any phase but gone: a closing session is held, and refuses with 409 below.
    let session = state
        .sessions
        .session_ref(session_id)
        .ok_or_else(|| refusal(ErrorCode::NotFound, format!("{scope} is not held")))?;
    if matches!(
        session.live.lifecycle,
        crate::runtime_state::SessionLifecycle::Closing
            | crate::runtime_state::SessionLifecycle::ClosingForDelete
    ) {
        return Err(refusal(ErrorCode::NotAllowed, format!("{scope} is closing")));
    }
    let shown = {
        let published = state.protocol.publication.lock();
        published.feed.with_tree(|tree| {
            crate::protocol::admission::target(tree, &scope)
                .and_then(|target| target.child("op"))
                .and_then(|op| op.as_value())
                .map(|(op, _)| (op["kind"].clone(), op["status"].clone()))
        })
    };
    let running_other = |kind: &Value| match control {
        // A close may interrupt a running prompt, and nothing else (protocol §7).
        Control::Close => kind != "prompt",
        _ => kind == "load" || kind == "delete",
    };
    if let Some((kind, status)) = shown
        && status == "running"
        && running_other(&kind)
    {
        return Err(refusal(
            ErrorCode::NotAllowed,
            format!("{scope} is busy with a {}", kind.as_str().unwrap_or("operation")),
        ));
    }
    if !matches!(control, Control::Close)
        && state
            .sessions
            .state(session_id)
            .is_some_and(|session| session.load_attempt.is_some())
    {
        // Optional history replay may still be installing after the public
        // operation completed. Preserve that transaction's control boundary.
        return Err(refusal(
            ErrorCode::NotAllowed,
            format!("{scope} is loading history"),
        ));
    }
    let request_id = format!("action-{}", Uuid::new_v4());
    let command = match control {
        Control::Close => {
            if !state
                .agent_capabilities
                .as_ref()
                .is_some_and(|capabilities| capabilities.session_capabilities.close.is_some())
            {
                return Err(refusal(
                    ErrorCode::Unsupported,
                    "the Agent does not advertise session/close",
                ));
            }
            json!({ "type": "session/close", "requestId": request_id, "sessionId": session_id })
        }
        Control::SetMode { mode_id } => {
            let modes = serde_json::to_value(&session.live.controls.modes)
                .map_err(|error| refusal(ErrorCode::InvalidRequest, error.to_string()))?;
            validate_session_mode_reference((!modes.is_null()).then_some(&modes), mode_id)
                .map_err(|message| refusal(ErrorCode::InvalidRequest, message))?;
            json!({
                "type": "session/set_mode", "requestId": request_id,
                "sessionId": session_id, "modeId": mode_id,
            })
        }
        Control::SetConfig { config_id, value } => {
            if !(value.is_boolean() || value.is_string()) {
                return Err(refusal(
                    ErrorCode::InvalidRequest,
                    "a config value is a string or a boolean",
                ));
            }
            validate_session_config_reference(
                &Value::from(session.live.controls.config_options.clone()),
                config_id,
                value,
            )
            .map_err(|message| refusal(ErrorCode::InvalidRequest, message))?;
            json!({
                "type": "session/set_config_option", "requestId": request_id,
                "sessionId": session_id, "configId": config_id, "value": value,
            })
        }
    };
    if matches!(control, Control::Close) {
        // Like a cancel, a close answers the session's pending permissions
        // cancelled before the Agent hears of it.
        let cancelled = operations::cancel_outcomes(state, &scope);
        let deliveries = operations::deliveries(state, cancelled)?;
        operations::deliver(state, sink, deliveries);
    }
    let (reply, answer) = oneshot::channel();
    if matches!(control, Control::Close) {
        state.action_closes.insert(request_id.clone(), session_id.to_string());
    } else {
        state
            .sessions
            .resources_mut(session_id)
            .expect("a held session owns its resources")
            .control_requests
            .insert(request_id.clone());
        // A parked optional history flow must not replay over this action.
        state.sessions.supersede_history_sync(session_id);
        refresh_observer_timers(state);
    }
    state.action_replies.insert(request_id, reply);
    Ok((command, answer))
}

/// A late result may only update the allocation that admitted its request.
/// A close/reinstall or successful reload invalidates that ownership.
pub(super) fn owns_request(
    state: &BridgeState,
    epoch: &str,
    session_id: &str,
    request_id: &str,
) -> bool {
    state.sessions.epoch() == epoch
        && state
            .sessions
            .resources(session_id)
            .is_ok_and(|resources| resources.control_requests.contains(request_id))
}
