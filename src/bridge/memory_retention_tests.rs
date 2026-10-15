//! Turn retention gates. Small payloads and Weak references distinguish live
//! retained versions from allocator high-water marks without measuring RSS.
use super::*;
use crate::semantic::{SessionUpdateSemanticState, validate_and_track_session_update};
use std::sync::Weak;

const SESSION: &str = "publication-memory-session";

struct RetentionTurn {
    state: BridgeState,
    incarnation: u64,
    operation_id: String,
    validation: SessionUpdateSemanticState,
}

impl RetentionTurn {
    fn new() -> Self {
        let mut state = BridgeState::default();
        let epoch = state.sessions.epoch().to_string();
        let incarnation = state
            .sessions
            .open_new_with_replay(
                &epoch,
                SESSION,
                "/workspace",
                json!({"sessionId": SESSION}),
                Vec::new(),
            )
            .unwrap();
        state.sessions.register_new(SESSION, incarnation);
        let mut fixture = Self {
            state,
            incarnation,
            operation_id: String::new(),
            validation: SessionUpdateSemanticState::default(),
        };
        fixture.start_turn("publication-first-turn");
        fixture
    }

    fn start_turn(&mut self, intent: &str) {
        let epoch = self.state.sessions.epoch().to_string();
        let revision = self
            .state
            .sessions
            .state(SESSION)
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        let TurnAdmission::Accepted { operation_id } = self
            .state
            .sessions
            .admit_session_turn(
                &epoch,
                SESSION,
                self.incarnation,
                &revision,
                intent,
                vec![json!({"type": "text", "text": "Read output"})],
            )
            .unwrap()
        else {
            panic!("a fresh valid turn must be admitted")
        };
        self.operation_id = operation_id;
    }

    fn append(&mut self, update: Value) -> Weak<Vec<Arc<Value>>> {
        append_unpublished_update(
            &mut self.state,
            &mut self.validation,
            self.incarnation,
            &self.operation_id,
            update,
        )
    }

    fn usage(&mut self, used: usize) {
        append_unpublished_usage(
            &mut self.state,
            &mut self.validation,
            self.incarnation,
            used,
        );
    }

    fn current_updates(&self) -> &crate::runtime_state::SharedTurnUpdates {
        &self
            .state
            .sessions
            .state(SESSION)
            .unwrap()
            .active_turn
            .as_ref()
            .unwrap()
            .updates
    }
}

// The coordinator hook exposes only a weak reference and is opt-in for one
// connection. It neither changes scheduling nor owns the registry after teardown.
thread_local! {
    static COORDINATOR_PROBE: std::cell::RefCell<Option<oneshot::Sender<Weak<BridgeLock>>>> = const { std::cell::RefCell::new(None) };
    static RETAINED_COORDINATOR_PROBE: std::cell::RefCell<Option<oneshot::Sender<Arc<BridgeLock>>>> = const { std::cell::RefCell::new(None) };
}

pub(super) fn capture_coordinator_state(state: &Arc<BridgeLock>) {
    RETAINED_COORDINATOR_PROBE.with(|probe| {
        if let Some(response) = probe.borrow_mut().take() {
            let _ = response.send(Arc::clone(state));
        }
    });
    COORDINATOR_PROBE.with(|probe| {
        if let Some(response) = probe.borrow_mut().take() {
            let _ = response.send(Arc::downgrade(state));
        }
    });
}

/// The next connection's bridge state, for gates that drive it in process.
pub(super) fn probe_coordinator_state() -> oneshot::Receiver<Weak<BridgeLock>> {
    let (response, probe) = oneshot::channel();
    COORDINATOR_PROBE.with(|slot| {
        assert!(slot.borrow_mut().replace(response).is_none());
    });
    probe
}

// Opt-in only: setup-error tests need to inspect the last published document
// after the connection has returned. Retention tests keep using the Weak probe.
pub(super) fn probe_retained_coordinator_state() -> oneshot::Receiver<Arc<BridgeLock>> {
    let (response, probe) = oneshot::channel();
    RETAINED_COORDINATOR_PROBE.with(|slot| {
        assert!(slot.borrow_mut().replace(response).is_none());
    });
    probe
}

fn append_unpublished_update(
    state: &mut BridgeState,
    validation: &mut SessionUpdateSemanticState,
    incarnation: u64,
    operation_id: &str,
    update: Value,
) -> Weak<Vec<Arc<Value>>> {
    validate_and_track_session_update(validation, &update).unwrap();
    state
        .sessions
        .append_turn_update(SESSION, incarnation, operation_id, update.clone())
        .unwrap();
    let overlay = state.sessions.state(SESSION).unwrap().active_turn.as_ref();
    Arc::downgrade(&overlay.unwrap().updates)
}

fn append_unpublished_usage(
    state: &mut BridgeState,
    validation: &mut SessionUpdateSemanticState,
    incarnation: u64,
    used: usize,
) {
    let update = json!({"sessionUpdate": "usage_update", "used": used, "size": 100_000});
    validate_and_track_session_update(validation, &update).unwrap();
    let epoch = state.sessions.epoch().to_string();
    state
        .sessions
        .update_control_state(&epoch, SESSION, incarnation, "usage_update", update)
        .unwrap();
}

fn fixed_output_with_replacements(replacements: usize) -> (usize, Value) {
    let mut turn = RetentionTurn::new();
    turn.append(json!({
        "sessionUpdate": "tool_call", "toolCallId": "output", "title": "Read output",
        "kind": "read", "status": "in_progress", "content": [],
    }));
    let mut versions = Vec::new();
    for index in 0..replacements {
        let character = if index + 1 == replacements {
            'F'
        } else {
            char::from(b'a' + (index % 26) as u8)
        };
        let update = json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "output",
            "content": [{"type": "content", "content": {
                "type": "text", "text": character.to_string().repeat(4096),
            }}],
        });
        versions.push(turn.append(update));
        turn.usage(index + 1);
    }
    let retained_old = versions[..versions.len() - 1]
        .iter()
        .filter(|version| version.upgrade().is_some())
        .count();
    assert_eq!(turn.current_updates().len(), 1);
    let final_tool = (*turn.current_updates()[0]).clone();
    assert_eq!(
        final_tool["content"][0]["content"]["text"],
        "F".repeat(4096)
    );
    (retained_old, final_tool)
}

#[test]
fn memory_retention_fixed_final_output_does_not_retain_more_intermediate_versions() {
    let samples = [8, 32, 64].map(|count| (count, fixed_output_with_replacements(count)));
    for pair in samples.windows(2) {
        assert_eq!(
            pair[0].1.1, pair[1].1.1,
            "final business content must be identical"
        );
    }
    let retained = samples
        .iter()
        .map(|(count, (old, _))| (*count, *old))
        .collect::<Vec<_>>();
    assert_eq!(
        retained,
        vec![(8, 0), (32, 0), (64, 0)],
        "a turn must retain current content, not one old overlay per replacement"
    );
}
