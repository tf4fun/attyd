//! A control update from an old epoch or incarnation is refused and changes
//! nothing, beside a running turn.
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
    incarnation: u64,
}

impl ControlTurn {
    fn new() -> Self {
        let mut state = BridgeState::default();
        let epoch = state.sessions.epoch().to_string();
        let incarnation = state
            .sessions
            .open_new_with_replay(&epoch, SESSION, "/workspace", metadata(), Vec::new())
            .unwrap();
        state.sessions.register_new(SESSION, incarnation);
        let operation_id = state
            .sessions
            .admit_session_turn(
                &epoch,
                SESSION,
                incarnation,
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
            .append_turn_update(SESSION, incarnation, &operation_id, update)
            .unwrap();
        Self { state, incarnation }
    }
}

#[test]
fn controls_from_old_owners_are_refused_without_mutation() {
    let mut turn = ControlTurn::new();
    let epoch = turn.state.sessions.epoch().to_string();
    let before = turn.state.sessions.session(SESSION);
    for update in controls() {
        let key = update["sessionUpdate"].as_str().unwrap().to_string();
        assert_eq!(
            turn.state.sessions.update_control_state(
                "old-epoch",
                SESSION,
                turn.incarnation,
                &key,
                update.clone()
            ),
            Err(RuntimeStateError::EpochMismatch)
        );
        assert_eq!(
            turn.state.sessions.update_control_state(
                &epoch,
                SESSION,
                turn.incarnation + 1,
                &key,
                update
            ),
            Err(RuntimeStateError::StaleIncarnation)
        );
        assert_eq!(turn.state.sessions.session(SESSION), before);
    }

    turn.state
        .sessions
        .start_operation(
            &epoch,
            SESSION,
            turn.incarnation,
            "close",
            RuntimeSessionOperationKind::Close,
            "closing",
        )
        .unwrap();
    turn.state
        .sessions
        .close_session(&epoch, SESSION, turn.incarnation, "close")
        .unwrap();
    turn.state
        .sessions
        .finish_session_cleanup(SESSION, turn.incarnation, SessionAdmission::Close, "close")
        .unwrap();
    let reopened = turn
        .state
        .sessions
        .open_new_with_replay(&epoch, SESSION, "/workspace", metadata(), Vec::new())
        .unwrap();
    turn.state.sessions.register_new(SESSION, reopened);
    assert_ne!(reopened, turn.incarnation);
    let reopened_session = turn.state.sessions.session(SESSION);
    for update in controls() {
        let key = update["sessionUpdate"].as_str().unwrap().to_string();
        assert_eq!(
            turn.state.sessions.update_control_state(
                &epoch,
                SESSION,
                turn.incarnation,
                &key,
                update
            ),
            Err(RuntimeStateError::StaleIncarnation)
        );
        assert_eq!(turn.state.sessions.session(SESSION), reopened_session);
    }
}
