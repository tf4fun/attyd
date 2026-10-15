//! Publication ownership gates. Small payloads and Weak references distinguish
//! live retained versions from allocator high-water marks without measuring RSS.
use super::*;
use crate::event_queue::{self, EventReceiver};
use crate::semantic::{SessionUpdateSemanticState, validate_and_track_session_update};
use std::sync::Weak;

const SESSION: &str = "publication-memory-session";

struct JournalTurn {
    state: BridgeState,
    incarnation: u64,
    operation_id: String,
    intent: String,
    validation: SessionUpdateSemanticState,
}

impl JournalTurn {
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
            intent: String::new(),
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
        self.intent = intent.into();
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

    fn text(&mut self, text: &str) -> Weak<Vec<Arc<Value>>> {
        self.append(json!({
            "sessionUpdate": "agent_message_chunk", "messageId": "answer",
            "content": {"type": "text", "text": text},
        }))
    }

    fn usage(&mut self, used: usize) {
        append_unpublished_usage(
            &mut self.state,
            &mut self.validation,
            self.incarnation,
            used,
        );
    }

    fn pin_overlay_with_cancel(&mut self) {
        // Cancel intent is a real full-session transition. It must still carry
        // the current turn even when usage publication becomes a small patch.
        // No Agent terminal response is injected; subsequent output is legal.
        let epoch = self.state.sessions.epoch().to_string();
        self.state
            .sessions
            .request_cancel(&epoch, SESSION, self.incarnation)
            .unwrap();
    }

    fn complete(&mut self) {
        let epoch = self.state.sessions.epoch().to_string();
        let terminal = json!({"stopReason": "end_turn"});
        self.state
            .sessions
            .complete_prompt(
                &epoch,
                SESSION,
                self.incarnation,
                &self.intent,
                terminal.clone(),
            )
            .unwrap();
        self.state
            .sessions
            .complete_turn(SESSION, self.incarnation, &self.operation_id, terminal)
            .unwrap();
        self.state
            .sessions
            .commit_completed_turn_from_memory(SESSION, self.incarnation, &self.operation_id)
            .unwrap();
        self.validation.retire_turn();
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
    let overlay = state
        .sessions
        .state(SESSION)
        .unwrap()
        .active_turn
        .clone()
        .unwrap();
    let epoch = state.sessions.epoch().to_string();
    state
        .sessions
        .project_turn_update(&epoch, SESSION, incarnation, &overlay, update)
        .unwrap();
    Arc::downgrade(&overlay.updates)
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

fn publication_queue() -> (EventSink, EventReceiver) {
    let (tx, receiver) = event_queue::channel(CancellationToken::new());
    (EventSink { tx }, receiver)
}

fn drain_events(receiver: &mut EventReceiver) -> Vec<Value> {
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        let (event, _lease) = event.into_json_parts();
        events.push(serde_json::from_str(&event).unwrap());
    }
    events
}

fn pending_sequences(state: &BridgeState, after: u64) -> Vec<u64> {
    state
        .sessions
        .deltas_after(after)
        .unwrap()
        .iter()
        .map(|delta| delta.seq)
        .collect()
}

#[test]
fn memory_retention_delta_transfer_empties_journal_and_releases_overlay_on_consumption() {
    let mut turn = JournalTurn::new();
    let (sink, mut receiver) = publication_queue();
    flush_runtime(&mut turn.state, &sink);
    drain_events(&mut receiver);
    let before = turn.state.published_runtime_seq;
    let old = turn.text("first-");
    turn.usage(1);
    turn.pin_overlay_with_cancel();
    let current = turn.text("second");
    turn.usage(2);
    assert!(
        old.upgrade().is_some(),
        "unpublished state still owns its delivery payload"
    );

    flush_runtime(&mut turn.state, &sink);
    assert_eq!(turn.state.published_runtime_seq, turn.state.sessions.seq());
    assert!(
        sink.tx.queued_bytes() > 0,
        "the receiving queue has not consumed the transfer"
    );
    assert!(
        current.upgrade().is_some(),
        "the live overlay must survive publication"
    );
    // The typed queue shares the transferred state instead of copying it, so
    // the journal must be empty while the queue alone owns the delivery.
    assert!(
        pending_sequences(&turn.state, turn.state.published_runtime_seq).is_empty(),
        "successful transfer left deltas in the publication journal"
    );
    let delivered = drain_events(&mut receiver);
    let sequences = delivered
        .iter()
        .map(|event| {
            assert_eq!(event["type"], "bridge/internal_runtime_delta");
            event["value"]["seq"].as_u64().unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        sequences,
        ((before + 1)..=turn.state.sessions.seq()).collect::<Vec<_>>()
    );
    assert_eq!(turn.current_updates()[0]["content"]["text"], "first-second");
    assert_eq!(sink.tx.queued_bytes(), 0);
    assert!(
        old.upgrade().is_none(),
        "a consumed transfer must not leave the obsolete overlay alive"
    );
}

fn fixed_output_with_replacements(replacements: usize) -> (usize, Value) {
    let mut turn = JournalTurn::new();
    let (sink, mut receiver) = publication_queue();
    flush_runtime(&mut turn.state, &sink);
    drain_events(&mut receiver);
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
        flush_runtime(&mut turn.state, &sink);
        drain_events(&mut receiver);
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
        "a caught-up publication must retain current content, not one old overlay per replacement"
    );
}

fn fallback_snapshot_fixture() -> (JournalTurn, Weak<Vec<Arc<Value>>>) {
    let mut turn = JournalTurn::new();
    let (sink, mut receiver) = publication_queue();
    flush_runtime(&mut turn.state, &sink);
    drain_events(&mut receiver);
    turn.text("completed history");
    turn.usage(1);
    // Completion invalidates the journal prefix before it has been flushed.
    // This is the normal production reason for snapshot fallback, not a made-up cursor.
    turn.complete();
    turn.start_turn("publication-second-turn");
    let old = turn.text("new-");
    turn.usage(2);
    turn.pin_overlay_with_cancel();
    turn.text("answer");
    turn.usage(3);
    assert!(
        turn.state
            .sessions
            .deltas_after(turn.state.published_runtime_seq)
            .is_none()
    );
    (turn, old)
}

#[test]
fn memory_retention_fallback_snapshot_releases_the_covered_overlay_prefix() {
    let (mut turn, old) = fallback_snapshot_fixture();
    let (sink, mut receiver) = publication_queue();
    let through = turn.state.sessions.seq();
    flush_runtime(&mut turn.state, &sink);
    let events = drain_events(&mut receiver);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"], "bridge/internal_runtime_snapshot");
    assert_eq!(events[0]["value"]["throughSeq"], through);
    assert_eq!(
        events[0]["value"]["sessions"][SESSION]["activeTurn"]["updates"][0]["content"]["text"],
        "new-answer"
    );
    assert_eq!(turn.state.published_runtime_seq, through);
    assert!(
        old.upgrade().is_none(),
        "the fallback snapshot left a covered obsolete overlay alive"
    );
}

#[test]
fn memory_retention_failed_fallback_snapshot_does_not_advance_the_watermark() {
    let (mut turn, old) = fallback_snapshot_fixture();
    let before = turn.state.published_runtime_seq;
    let (sink, receiver) = publication_queue();
    drop(receiver);
    flush_runtime(&mut turn.state, &sink);
    assert!(
        old.upgrade().is_some(),
        "failed transfer must retain the unpublished payload"
    );
    assert_eq!(
        turn.state.published_runtime_seq, before,
        "a failed snapshot cannot acknowledge its throughSeq"
    );
}

#[test]
fn memory_retention_closed_publication_queue_keeps_watermark_and_unpublished_suffix() {
    let mut turn = JournalTurn::new();
    let old = turn.text("unpublished-");
    turn.usage(1);
    turn.pin_overlay_with_cancel();
    turn.text("suffix");
    turn.usage(2);
    let before = turn.state.published_runtime_seq;
    let pending = pending_sequences(&turn.state, before);
    let (sink, receiver) = publication_queue();
    drop(receiver);
    flush_runtime(&mut turn.state, &sink);
    assert!(old.upgrade().is_some());
    assert_eq!(pending_sequences(&turn.state, before), pending);
    assert_eq!(
        turn.state.published_runtime_seq, before,
        "failed enqueue is not publication"
    );
}

#[test]
fn memory_retention_mid_batch_failure_acknowledges_only_the_contiguous_prefix() {
    let mut turn = JournalTurn::new();
    let old = turn.text("pending-");
    turn.usage(1);
    turn.pin_overlay_with_cancel();
    turn.text("replacement");
    turn.usage(2);
    let pending = pending_sequences(&turn.state, 0);
    assert!(pending.len() >= 3);
    let (sender, mut receiver, fault) = EventSender::test_failing_after(1);
    let sink = EventSink { tx: sender };
    flush_runtime(&mut turn.state, &sink);
    let events = drain_events(&mut receiver);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["value"]["seq"], pending[0]);
    assert!(
        old.upgrade().is_some(),
        "the unsent suffix must remain available for recovery"
    );
    assert_eq!(pending_sequences(&turn.state, pending[0]), pending[1..]);
    assert_eq!(
        (turn.state.published_runtime_seq, fault.delta_attempts()),
        (pending[0], 2),
        "stop at the first failed transfer; later sequence numbers must not hide the gap"
    );
}

#[test]
fn memory_retention_later_changes_remain_an_unpublished_contiguous_suffix() {
    let mut turn = JournalTurn::new();
    let (sink, mut receiver) = publication_queue();
    turn.text("first");
    turn.usage(1);
    flush_runtime(&mut turn.state, &sink);
    drain_events(&mut receiver);
    let cut = turn.state.published_runtime_seq;
    let pending = turn.text("-pending");
    turn.usage(2);
    turn.pin_overlay_with_cancel();
    turn.text("-latest");
    assert!(pending.upgrade().is_some());
    assert_eq!(turn.state.published_runtime_seq, cut);
    assert_eq!(
        pending_sequences(&turn.state, cut),
        ((cut + 1)..=turn.state.sessions.seq()).collect::<Vec<_>>()
    );
    flush_runtime(&mut turn.state, &sink);
    let events = drain_events(&mut receiver);
    assert_eq!(events.first().unwrap()["value"]["seq"], cut + 1);
    assert_eq!(
        events.last().unwrap()["value"]["seq"],
        turn.state.sessions.seq()
    );
}

#[test]
fn memory_retention_epoch_teardown_releases_payloads_even_after_failed_publication() {
    let mut turn = JournalTurn::new();
    let old = turn.text("old epoch");
    turn.usage(1);
    turn.pin_overlay_with_cancel();
    let current = turn.text(" still running");
    turn.usage(2);
    let (sink, receiver) = publication_queue();
    drop(receiver);
    flush_runtime(&mut turn.state, &sink);
    assert!(old.upgrade().is_some());
    assert!(current.upgrade().is_some());
    let old_epoch = turn.state.sessions.epoch().to_string();
    drop(turn);
    assert!(old.upgrade().is_none());
    assert!(current.upgrade().is_none());
    let replacement = JournalTurn::new();
    assert_ne!(replacement.state.sessions.epoch(), old_epoch);
    assert!(replacement.current_updates().is_empty());
}
