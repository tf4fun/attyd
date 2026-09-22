//! Publication ownership gates. Small payloads and Weak references distinguish
//! live retained versions from allocator high-water marks without measuring RSS.
use super::*;
use crate::event_queue::{self, EventReceiver};
use crate::semantic::{SessionUpdateSemanticState, validate_and_track_session_update};
use agent_client_protocol::Lines;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use std::collections::VecDeque;
use std::io;
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
    static COORDINATOR_PROBE: std::cell::RefCell<Option<oneshot::Sender<Weak<Mutex<BridgeState>>>>> = const { std::cell::RefCell::new(None) };
}

pub(super) fn capture_coordinator_state(state: &Arc<Mutex<BridgeState>>) {
    COORDINATOR_PROBE.with(|probe| {
        if let Some(response) = probe.borrow_mut().take() {
            let _ = response.send(Arc::downgrade(state));
        }
    });
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
        let (event, _lease) = event.into_parts();
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
fn memory_retention_delta_transfer_releases_old_overlay_before_queue_consumption() {
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
    let old_retained_after_transfer = old.upgrade().is_some();
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
        !old_retained_after_transfer,
        "successful transfer left the obsolete overlay alive in the publication journal"
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

#[test]
fn memory_retention_internal_serialization_failure_publishes_no_runtime_record() {
    struct InvalidSerialization;
    impl Serialize for InvalidSerialization {
        fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("injected serialization failure"))
        }
    }
    let (sink, mut receiver) = publication_queue();
    sink.internal_typed("bridge/internal_runtime_delta", InvalidSerialization);
    let events = drain_events(&mut receiver);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"], "bridge/error");
    assert_eq!(events[0]["operation"], "canonical/runtime");
    assert!(
        events[0]["message"]
            .as_str()
            .unwrap()
            .contains("injected serialization failure")
    );
}

struct PublicationPeer {
    commands: mpsc::UnboundedSender<BridgeInput>,
    requests: futures::channel::mpsc::Receiver<String>,
    responses: futures::channel::mpsc::Sender<io::Result<String>>,
    buffered: VecDeque<Value>,
    events: EventReceiver,
    cancellation: CancellationToken,
    task: Option<tokio::task::JoinHandle<Result<(), Error>>>,
}

impl PublicationPeer {
    async fn start(sender: EventSender, events: EventReceiver) -> Self {
        let (outgoing, requests) = futures::channel::mpsc::channel::<String>(64);
        let (responses, incoming) = futures::channel::mpsc::channel::<io::Result<String>>(64);
        let transport = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
        let mut options =
            Options::try_parse_from(["attyd", "--transport", "ws", "--", "ws://127.0.0.1:1/acp"])
                .unwrap()
                .normalized()
                .unwrap();
        options.session_unobserved_timeout = -1;
        let (commands, receiver) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(run_connection(
            transport,
            Arc::new(options),
            receiver,
            EventSink { tx: sender },
            cancellation.clone(),
        ));
        let mut peer = Self {
            commands,
            requests,
            responses,
            buffered: VecDeque::new(),
            events,
            cancellation,
            task: Some(task),
        };
        let initialize = peer.request().await;
        assert_eq!(initialize["method"], "initialize");
        peer.reply(
            &initialize,
            json!({"protocolVersion": 1, "agentCapabilities": {}, "authMethods": []}),
        )
        .await;
        peer
    }

    async fn request(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(request) = self.buffered.pop_front() {
                    return request;
                }
                let raw = self.requests.next().await.expect("Agent transport closed");
                match serde_json::from_str(&raw).unwrap() {
                    Value::Array(batch) => self.buffered.extend(batch),
                    value => self.buffered.push_back(value),
                }
            }
        })
        .await
        .expect("Agent did not receive the request")
    }

    async fn send(&mut self, value: Value) {
        self.responses.send(Ok(value.to_string())).await.unwrap();
    }

    async fn reply(&mut self, request: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": request["id"], "result": result}))
            .await;
    }

    async fn event(&mut self, matches: impl Fn(&Value) -> bool) -> Value {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let (event, _lease) = self
                    .events
                    .recv()
                    .await
                    .expect("event stream closed")
                    .into_parts();
                let event: Value = serde_json::from_str(&event).unwrap();
                if matches(&event) {
                    return event;
                }
            }
        })
        .await
        .expect("publication event did not arrive")
    }

    async fn stop(&mut self) {
        self.cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(3), self.task.take().unwrap())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

impl Drop for PublicationPeer {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn live_publication_peer() -> (PublicationPeer, Weak<Mutex<BridgeState>>, Value) {
    let (probe_response, probe) = oneshot::channel();
    COORDINATOR_PROBE.with(|slot| {
        assert!(slot.borrow_mut().replace(probe_response).is_none());
    });
    let (sender, events) = event_queue::channel(CancellationToken::new());
    let mut peer = PublicationPeer::start(sender, events).await;
    let state_probe = probe.await.unwrap();
    let (response, created) = oneshot::channel();
    peer.commands.send(BridgeInput::BusinessRequest {
        command: json!({"type": "session/new", "requestId": "publication-new", "cwd": "/workspace"}),
        response,
    }).unwrap();
    let new = peer.request().await;
    assert_eq!(new["method"], "session/new");
    peer.reply(&new, json!({"sessionId": SESSION})).await;
    let created = tokio::time::timeout(Duration::from_secs(3), created)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let (response, admitted) = oneshot::channel();
    peer.commands
        .send(BridgeInput::TurnRequest {
            session_id: SESSION.into(),
            history_revision: created["view"]["session"]["historyRevision"]
                .as_str()
                .unwrap()
                .into(),
            client_intent_id: "publication-live-turn".into(),
            prompt: vec![json!({"type": "text", "text": "Read output"})],
            response,
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), admitted)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let prompt = peer.request().await;
    assert_eq!(prompt["method"], "session/prompt");
    (peer, state_probe, prompt)
}

async fn unpublished_overlay_versions(
    state_probe: &Weak<Mutex<BridgeState>>,
) -> (Weak<Vec<Arc<Value>>>, Weak<Vec<Arc<Value>>>) {
    // Establish unflushed changes at the real coordinator's publication boundary.
    // This hook calls the same registry transitions as the live reducers; it does
    // not fabricate journal records or require an epoch to survive a Closed send.
    {
        let state = state_probe.upgrade().unwrap();
        let mut state = state.lock().await;
        let session = state.sessions.state(SESSION).unwrap();
        let incarnation = session.incarnation;
        let operation_id = session.active_turn.as_ref().unwrap().operation_id.clone();
        let mut validation = SessionUpdateSemanticState::default();
        let old = append_unpublished_update(
            &mut state,
            &mut validation,
            incarnation,
            &operation_id,
            json!({
                "sessionUpdate": "agent_message_chunk", "messageId": "answer", "content": {"type": "text", "text": "older-"},
            }),
        );
        append_unpublished_usage(&mut state, &mut validation, incarnation, 1);
        let epoch = state.sessions.epoch().to_string();
        state
            .sessions
            .request_cancel(&epoch, SESSION, incarnation)
            .unwrap();
        let current = append_unpublished_update(
            &mut state,
            &mut validation,
            incarnation,
            &operation_id,
            json!({
                "sessionUpdate": "agent_message_chunk", "messageId": "answer", "content": {"type": "text", "text": "latest"},
            }),
        );
        append_unpublished_usage(&mut state, &mut validation, incarnation, 2);
        assert!(state.sessions.seq() > state.published_runtime_seq);
        assert!(
            old.upgrade().is_some(),
            "unpublished changes must still own the earlier payload"
        );
        (old, current)
    }
}

#[tokio::test(start_paused = true)]
async fn memory_retention_connection_teardown_releases_unpublished_running_turn() {
    let (mut peer, state_probe, _pending_prompt) = live_publication_peer().await;
    let (old, current) = unpublished_overlay_versions(&state_probe).await;
    assert!(old.upgrade().is_some());
    assert!(current.upgrade().is_some());
    // No Agent response: the generation itself owns shutdown of the pending RPC.
    peer.stop().await;
    assert!(
        state_probe.upgrade().is_none(),
        "the stopped generation still owns its coordinator"
    );
    assert!(
        old.upgrade().is_none(),
        "shutdown retained an unpublished historical overlay"
    );
    assert!(
        current.upgrade().is_none(),
        "shutdown retained the running turn's current overlay"
    );
}

#[tokio::test(start_paused = true)]
async fn memory_retention_explicit_snapshot_releases_previously_unpublished_overlays() {
    let (mut peer, state_probe, prompt) = live_publication_peer().await;
    let (old, _current) = unpublished_overlay_versions(&state_probe).await;
    peer.commands
        .send(BridgeInput::RuntimeSnapshotRequest)
        .unwrap();
    let snapshot = peer.event(|event| {
        event["type"] == "bridge/internal_runtime_snapshot"
            && event["value"]["sessions"][SESSION]["activeTurn"]["updates"][0]["content"]["text"] == "older-latest"
    }).await;
    let through = snapshot["value"]["throughSeq"].as_u64().unwrap();
    let retained_after_snapshot = old.upgrade().is_some();
    peer.send(json!({
        "jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": SESSION, "update": {
            "sessionUpdate": "agent_message_chunk", "messageId": "answer", "content": {"type": "text", "text": "-suffix"},
        }},
    })).await;
    let suffix = peer
        .event(|event| event["type"] == "bridge/internal_runtime_delta")
        .await;
    assert_eq!(
        suffix["value"]["seq"],
        through + 1,
        "explicit snapshot establishes the next contiguous delta cut"
    );
    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    peer.event(|event| event["type"] == "acp/prompt_complete" && event["sessionId"] == SESSION)
        .await;
    peer.stop().await;
    assert!(
        old.upgrade().is_none(),
        "generation teardown must release any remaining old owner"
    );
    assert!(
        !retained_after_snapshot,
        "successful explicit snapshot left its covered, previously unpublished overlay alive"
    );
}
