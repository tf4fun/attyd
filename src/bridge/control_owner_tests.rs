//! A control update from an old epoch is refused and changes nothing, beside
//! a running turn.
use super::*;
use crate::runtime_state::RuntimeStateError;

const SESSION: &str = "control-owner-session";

fn metadata() -> Value {
    json!({
        "sessionId": SESSION,
        "modes": {"currentModeId": "build", "availableModes": [
            {"id": "build", "name": "Build"}, {"id": "plan", "name": "Plan"},
        ]},
        "configOptions": [{"type": "boolean", "id": "verbose", "name": "Verbose", "currentValue": false}],
    })
}

fn controls() -> Vec<Value> {
    vec![
        json!({"sessionUpdate": "usage_update", "used": 12, "size": 100}),
        json!({"sessionUpdate": "current_mode_update", "currentModeId": "plan"}),
        json!({"sessionUpdate": "config_option_update", "configOptions": [
            {"type": "boolean", "id": "verbose", "name": "Verbose", "currentValue": true},
        ]}),
    ]
}

struct ControlTurn {
    state: BridgeState,
}

impl ControlTurn {
    fn new() -> Self {
        let mut state = BridgeState::default();
        let epoch = state.sessions.epoch().to_string();
        state
            .sessions
            .open_new_with_replay(&epoch, SESSION, "/workspace", metadata(), Vec::new())
            .unwrap();
        state.sessions.register_new(SESSION);
        let operation_id = state
            .sessions
            .admit_session_turn(
                &epoch,
                SESSION,
                "control-owner-turn",
                vec![json!({"type": "text", "text": "Keep this prompt"})],
            )
            .unwrap();
        let update = json!({
            "sessionUpdate": "agent_message_chunk", "messageId": "answer",
            "content": {"type": "text", "text": "existing turn body"},
        });
        let mut validation = SessionUpdateSemanticState::default();
        validate_and_track_session_update(&mut validation, &update).unwrap();
        state
            .sessions
            .append_turn_update(SESSION, &operation_id, update)
            .unwrap();
        Self { state }
    }
}

#[test]
fn controls_from_old_owners_are_refused_without_mutation() {
    let mut turn = ControlTurn::new();
    let before = turn.state.sessions.session(SESSION);
    for update in controls() {
        assert_eq!(
            turn.state
                .sessions
                .update_control_state("old-epoch", SESSION, update),
            Err(RuntimeStateError::EpochMismatch)
        );
        assert_eq!(turn.state.sessions.session(SESSION), before);
    }
}
