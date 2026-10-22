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
    session_id: &str,
    control: &Control,
) -> Result<(Value, ControlAnswer), ProtocolError> {
    let scope = OpScope::Session(session_id.to_string());
    let session = state
        .sessions
        .active_session(session_id)
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
        published
            .publisher
            .live(&scope)
            .map(|live| (live["op"]["kind"].clone(), live["op"]["status"].clone()))
    };
    if let Some((kind, status)) = shown
        && status == "running"
        && (kind == "load" || kind == "delete")
    {
        return Err(refusal(
            ErrorCode::NotAllowed,
            format!("{scope} is busy with a {}", kind.as_str().unwrap_or("operation")),
        ));
    }
    let request_id = format!("action-{}", Uuid::new_v4());
    let command = match control {
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
    let (reply, answer) = oneshot::channel();
    state.action_replies.insert(request_id, reply);
    Ok((command, answer))
}
