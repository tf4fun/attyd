//! Control publication budgets measure actual queued JSON, not RSS or timing.
//! The same control payload must cost the same bytes beside small and large turns.
use super::*;
use crate::event_queue::{self, EventReceiver};
use crate::runtime_state::RuntimeStateError;

const SESSION: &str = "control-efficiency-session";
const BODY_MARKER: &str = "existing-turn-body:";

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
    validation: SessionUpdateSemanticState,
    body: String,
}

impl ControlTurn {
    fn new(body_bytes: usize) -> Self {
        let mut state = BridgeState::default();
        let epoch = state.sessions.epoch().to_string();
        let incarnation = state
            .sessions
            .open_new_with_replay(&epoch, SESSION, "/workspace", metadata(), Vec::new())
            .unwrap();
        state.sessions.register_new(SESSION, incarnation);
        let history = state
            .sessions
            .state(SESSION)
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        let TurnAdmission::Accepted { operation_id } = state
            .sessions
            .admit_session_turn(
                &epoch,
                SESSION,
                incarnation,
                &history,
                "control-efficiency-turn",
                vec![json!({"type": "text", "text": "Keep this prompt"})],
            )
            .unwrap()
        else {
            panic!("fresh turn must be admitted")
        };
        let body = format!(
            "{BODY_MARKER}{}",
            "x".repeat(body_bytes - BODY_MARKER.len())
        );
        let update = json!({
            "sessionUpdate": "agent_message_chunk", "messageId": "answer",
            "content": {"type": "text", "text": body},
        });
        let mut validation = SessionUpdateSemanticState::default();
        validate_and_track_session_update(&mut validation, &update).unwrap();
        state
            .sessions
            .append_turn_update(SESSION, incarnation, &operation_id, update.clone())
            .unwrap();
        let overlay = state
            .sessions
            .state(SESSION)
            .unwrap()
            .active_turn
            .clone()
            .unwrap();
        state
            .sessions
            .project_turn_update(&epoch, SESSION, incarnation, &overlay, update)
            .unwrap();
        Self {
            state,
            incarnation,
            validation,
            body,
        }
    }

    fn control(&mut self, update: Value) {
        validate_and_track_session_update(&mut self.validation, &update).unwrap();
        let epoch = self.state.sessions.epoch().to_string();
        let key = update["sessionUpdate"].as_str().unwrap().to_string();
        self.state
            .sessions
            .update_control_state(&epoch, SESSION, self.incarnation, key, update)
            .unwrap();
    }
}

fn queue() -> (EventSink, EventReceiver) {
    let (tx, receiver) = event_queue::channel(CancellationToken::new());
    (EventSink { tx }, receiver)
}

fn drain(receiver: &mut EventReceiver) -> Vec<String> {
    std::iter::from_fn(|| receiver.try_recv().ok())
        .map(|event| {
            let (raw, _lease) = event.into_parts();
            raw
        })
        .collect()
}

fn control_publication(body_bytes: usize, update: Value) -> (usize, bool) {
    let mut turn = ControlTurn::new(body_bytes);
    let (sink, mut receiver) = queue();
    flush_runtime(&mut turn.state, &sink);
    drain(&mut receiver);
    assert_eq!(sink.tx.queued_bytes(), 0);
    let before = turn.state.sessions.snapshot();
    turn.control(update.clone());
    flush_runtime(&mut turn.state, &sink);
    let queued_bytes = sink.tx.queued_bytes();
    let raw = drain(&mut receiver);
    assert_eq!(
        raw.len(),
        1,
        "one changed control has one incremental publication"
    );
    let event: Value = serde_json::from_str(&raw[0]).unwrap();
    assert_eq!(event["type"], "bridge/internal_runtime_delta");
    assert_eq!(event["value"]["epoch"], before.epoch);
    assert_eq!(event["value"]["seq"], before.through_seq + 1);
    assert_eq!(queued_bytes, raw[0].len());
    assert_eq!(sink.tx.queued_bytes(), 0);
    let after = turn.state.sessions.snapshot();
    let session = &after.sessions[SESSION];
    assert_eq!(session.incarnation, turn.incarnation);
    assert_eq!(session.revision, before.sessions[SESSION].revision + 1);
    assert_eq!(event["value"]["scopeRevision"], session.revision);
    assert_eq!(
        session.active_turn.as_ref().unwrap().updates[0]["content"]["text"],
        turn.body
    );
    assert_eq!(session.active_turn, before.sessions[SESSION].active_turn);
    assert_eq!(
        session.control_state[update["sessionUpdate"].as_str().unwrap()],
        update
    );
    assert_eq!(turn.state.published_runtime_seq, after.through_seq);
    assert!(
        turn.state
            .sessions
            .deltas_after(after.through_seq)
            .unwrap()
            .is_empty()
    );
    (queued_bytes, raw[0].contains(BODY_MARKER))
}

fn assert_control_does_not_republish_body(update: Value) {
    let small = control_publication(4 * 1024, update.clone());
    let large = control_publication(128 * 1024, update);
    assert_eq!(
        large.0, small.0,
        "a fixed control payload grew from {} to {} queued bytes with existing turn content",
        small.0, large.0
    );
    assert!(
        !small.1 && !large.1,
        "control publication must not resend the existing answer"
    );
}

#[test]
fn memory_efficiency_usage_publication_bytes_do_not_grow_with_existing_turn() {
    assert_control_does_not_republish_body(controls().remove(0));
}

#[test]
fn memory_efficiency_mode_publication_bytes_do_not_grow_with_existing_turn() {
    assert_control_does_not_republish_body(controls().remove(1));
}

#[test]
fn memory_efficiency_config_publication_bytes_do_not_grow_with_existing_turn() {
    assert_control_does_not_republish_body(controls().remove(2));
}

#[test]
fn memory_efficiency_unchanged_controls_publish_nothing_and_keep_their_cut() {
    let mut turn = ControlTurn::new(128 * 1024);
    let (sink, mut receiver) = queue();
    for update in controls() {
        turn.control(update.clone());
        flush_runtime(&mut turn.state, &sink);
        drain(&mut receiver);
        let before = turn.state.sessions.snapshot();
        turn.control(update);
        flush_runtime(&mut turn.state, &sink);
        assert_eq!(turn.state.sessions.snapshot(), before);
        assert_eq!(turn.state.published_runtime_seq, before.through_seq);
        assert_eq!(sink.tx.queued_bytes(), 0);
        assert!(drain(&mut receiver).is_empty());
    }
}

#[test]
fn memory_efficiency_controls_reject_old_owners_without_publication_or_mutation() {
    let mut turn = ControlTurn::new(4096);
    let (sink, mut receiver) = queue();
    flush_runtime(&mut turn.state, &sink);
    drain(&mut receiver);
    let epoch = turn.state.sessions.epoch().to_string();
    let before = turn.state.sessions.snapshot();
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
        assert_eq!(turn.state.sessions.snapshot(), before);
    }
    flush_runtime(&mut turn.state, &sink);
    assert!(drain(&mut receiver).is_empty());

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
    flush_runtime(&mut turn.state, &sink);
    drain(&mut receiver);
    let reopened_snapshot = turn.state.sessions.snapshot();
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
        assert_eq!(turn.state.sessions.snapshot(), reopened_snapshot);
    }
    flush_runtime(&mut turn.state, &sink);
    assert!(drain(&mut receiver).is_empty());
    assert_eq!(sink.tx.queued_bytes(), 0);
}
