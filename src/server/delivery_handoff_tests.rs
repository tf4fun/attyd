//! Deterministic interleavings at the real Hub -> SSE payload handoff.
//!
//! The hooks only park an existing production operation between two of its
//! instructions. They never read, replace, or drop a payload on its behalf.

use super::tests::observation_hub;
use super::*;
use crate::runtime_state::RuntimeState;
use serde_json::Value;
use std::sync::{Condvar, OnceLock, Weak};

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum PausePoint {
    PayloadCaptured,
    PayloadReplaced,
}

struct Pause {
    arrived: Notify,
    released: std::sync::Mutex<bool>,
    release: Condvar,
}

impl Pause {
    fn park(&self) {
        self.arrived.notify_one();
        let released = self.released.lock().unwrap();
        drop(
            self.release
                .wait_while(released, |released| !*released)
                .unwrap(),
        );
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}

type PauseKey = (usize, PausePoint);

fn pauses() -> &'static std::sync::Mutex<HashMap<PauseKey, Weak<Pause>>> {
    static PAUSES: OnceLock<std::sync::Mutex<HashMap<PauseKey, Weak<Pause>>>> = OnceLock::new();
    PAUSES.get_or_init(Default::default)
}

struct PauseRegistration {
    key: PauseKey,
    pause: Arc<Pause>,
    // Keep the identity allocation alive until deregistration, even if the
    // fixture finishes first and another test allocates a fresh subscriber.
    _ledger: Arc<AtomicUsize>,
}

impl PauseRegistration {
    fn new(ledger: &Arc<AtomicUsize>, point: PausePoint) -> Self {
        let key = (Arc::as_ptr(ledger) as usize, point);
        let pause = Arc::new(Pause {
            arrived: Notify::new(),
            released: std::sync::Mutex::new(false),
            release: Condvar::new(),
        });
        assert!(
            pauses()
                .lock()
                .unwrap()
                .insert(key, Arc::downgrade(&pause))
                .is_none()
        );
        Self {
            key,
            pause,
            _ledger: ledger.clone(),
        }
    }

    async fn arrived(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.pause.arrived.notified())
            .await
            .expect("the production operation did not reach the handoff barrier");
    }

    fn release(&self) {
        self.pause.release();
    }
}

impl Drop for PauseRegistration {
    fn drop(&mut self) {
        pauses().lock().unwrap().remove(&self.key);
        // Failed setup assertions must not strand a blocked runtime worker.
        self.pause.release();
    }
}

fn pause_at(ledger: &Arc<AtomicUsize>, point: PausePoint) {
    let pause = pauses()
        .lock()
        .unwrap()
        .remove(&(Arc::as_ptr(ledger) as usize, point))
        .and_then(|pause| pause.upgrade());
    if let Some(pause) = pause {
        pause.park();
    }
}

pub(super) fn after_payload_capture(ledger: &Arc<AtomicUsize>) {
    pause_at(ledger, PausePoint::PayloadCaptured);
}

pub(super) fn after_payload_replace(ledger: &Arc<AtomicUsize>) {
    pause_at(ledger, PausePoint::PayloadReplaced);
}

struct Fixture {
    hub: Arc<BridgeHub>,
    _commands: mpsc::UnboundedReceiver<bridge::BridgeInput>,
    subscription: BridgeSubscription,
    _guard: SubscriptionGuard,
    lease: ObservationLease,
    ledger: Arc<AtomicUsize>,
    runtime: RuntimeState,
    incarnation: u64,
    operation: String,
    published_seq: u64,
}

impl Fixture {
    async fn new() -> Self {
        let (hub, mut commands) = observation_hub(1).await;
        let mut runtime = RuntimeState::new("epoch");
        let incarnation = runtime
            .open_new(
                "epoch",
                "session",
                "/workspace",
                json!({"sessionId":"session"}),
            )
            .unwrap();
        runtime.register_new("session", incarnation);
        let revision = runtime
            .view("session", incarnation)
            .unwrap()
            .session
            .history_revision
            .unwrap();
        let crate::session_state::TurnAdmission::Accepted { operation_id } = runtime
            .admit_session_turn(
                "epoch",
                "session",
                incarnation,
                &revision,
                "handoff-intent",
                vec![json!({"type":"text", "text":"keep my retry prompt"})],
            )
            .unwrap()
        else {
            unreachable!("the first prompt must be admitted")
        };
        let published_seq = runtime.seq();
        hub.publish(
            1,
            json!({"type":"bridge/internal_runtime_snapshot", "value":runtime.snapshot()})
                .to_string(),
        )
        .await;
        let mut pending = Box::pin(hub.observe_session("session".into(), None));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        let bridge::BridgeInput::ObserveSession {
            observer_id,
            lease,
            reply,
            ..
        } = commands.try_recv().unwrap()
        else {
            unreachable!("observation must be registered with the coordinator")
        };
        let view = runtime.view_value("session", incarnation).unwrap();
        hub.publish(1, json!({
            "type":"bridge/internal_observer_ready", "observerId":observer_id, "sessionId":"session",
            "reset":session_reset_value("session", &view).unwrap(),
        }).to_string()).await;
        reply.send(Ok(())).unwrap();
        let (mut subscription, guard) = pending.await.unwrap();
        let reset: Value =
            serde_json::from_str(&subscription.events.try_recv().unwrap().into_string()).unwrap();
        assert_eq!(reset["viewRevision"], view["session"]["viewRevision"]);
        assert_eq!(reset["sessionIncarnation"], incarnation);
        let ledger = hub.state.lock().await.session_subscribers[&observer_id]
            .sender
            .queued_bytes
            .clone();
        Self {
            hub,
            _commands: commands,
            subscription,
            _guard: guard,
            lease,
            ledger,
            runtime,
            incarnation,
            operation: operation_id,
            published_seq,
        }
    }

    fn revision(&self) -> u64 {
        self.runtime.state("session").unwrap().view_revision
    }

    fn update(&mut self, text: &str) -> Vec<Value> {
        let from_revision = self.revision();
        let update = json!({"sessionUpdate":"agent_message_chunk", "messageId":"answer", "content":{"type":"text", "text":text}});
        self.runtime
            .append_turn_update("session", self.incarnation, &self.operation, update.clone())
            .unwrap();
        let overlay = self
            .runtime
            .state("session")
            .unwrap()
            .active_turn
            .clone()
            .unwrap();
        self.runtime
            .project_turn_update(
                "epoch",
                "session",
                self.incarnation,
                &overlay,
                update.clone(),
            )
            .unwrap();
        // Match session/update delivery: the business delta is enqueued before
        // flush_runtime publishes the corresponding internal projection.
        let mut events = vec![json!({
            "type":"bridge/session_delta", "bridgeEpoch":"epoch", "sessionId":"session", "sessionIncarnation":self.incarnation,
            "fromRevision":from_revision, "viewRevision":self.revision(),
            "change":{"kind":"turn_update", "operationId":self.operation, "update":update},
        })];
        events.extend(
            self.runtime
                .deltas_after(self.published_seq)
                .unwrap()
                .into_iter()
                .map(|delta| {
                    self.published_seq = delta.seq;
                    json!({"type":"bridge/internal_runtime_delta", "value":delta})
                }),
        );
        events
    }

    fn fail(&mut self) -> (Vec<Value>, Value) {
        let prompt = self
            .runtime
            .view_value("session", self.incarnation)
            .unwrap()["session"]["activeTurn"]["prompt"]
            .clone();
        let error = json!({"code":-32603, "message":"failure must survive the handoff"});
        self.runtime
            .fail_prompt(
                "epoch",
                "session",
                self.incarnation,
                "handoff-intent",
                error.clone(),
            )
            .unwrap();
        self.runtime
            .complete_turn(
                "session",
                self.incarnation,
                &self.operation,
                json!({"error":error}),
            )
            .unwrap();
        self.runtime
            .commit_completed_turn_from_memory("session", self.incarnation, &self.operation)
            .unwrap();
        let view = self
            .runtime
            .view_value("session", self.incarnation)
            .unwrap();
        assert_eq!(view["session"]["phase"], "ready");
        assert!(
            view["session"]["turnOutcomes"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let failed = json!({
            "type":"bridge/session_turn_failed", "bridgeEpoch":"epoch", "sessionId":"session", "sessionIncarnation":self.incarnation,
            "viewRevision":self.revision(), "operationId":self.operation, "clientIntentId":"handoff-intent", "phase":"ready", "prompt":prompt, "error":error,
        });
        (
            vec![
                json!({"type":"bridge/internal_runtime_snapshot", "value":self.runtime.snapshot()}),
                json!({"type":"bridge/session_view", "sessionId":"session", "view":view}),
                failed.clone(),
            ],
            failed,
        )
    }

    fn drain(&mut self) -> Vec<Value> {
        std::iter::from_fn(|| self.subscription.events.try_recv().ok())
            .map(|event| serde_json::from_str(&event.into_string()).unwrap())
            .collect()
    }

    async fn stop(self) {
        self.hub
            .unsubscribe(self.subscription.id, self.subscription.generation)
            .await;
        self.hub.shutdown().await;
    }
}

async fn publish(hub: &BridgeHub, events: Vec<Value>) {
    for event in events {
        hub.publish(1, event.to_string()).await;
    }
}

fn last_revision(events: &[Value]) -> u64 {
    events
        .iter()
        .filter_map(|event| event["viewRevision"].as_u64())
        .max()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_efficiency_state_handoff_keeps_the_last_published_revision_deliverable() {
    let mut fixture = Fixture::new().await;
    let first = fixture.update("first");
    publish(&fixture.hub, first).await;
    let capture = PauseRegistration::new(&fixture.ledger, PausePoint::PayloadCaptured);
    let queued = fixture.subscription.events.try_recv().unwrap();
    let consumer = tokio::task::spawn_blocking(move || queued.into_string());
    capture.arrived().await;

    let next = fixture.update(" and last");
    let latest_revision = fixture.revision();
    publish(&fixture.hub, next).await;
    capture.release();
    let sent: Value = serde_json::from_str(&consumer.await.unwrap()).unwrap();
    let mut delivered = vec![sent];
    delivered.extend(fixture.drain());
    let delivered_revision = last_revision(&delivered);
    assert!(!fixture.lease.is_cancelled());
    assert_eq!(fixture.ledger.load(Ordering::Acquire), 0);
    fixture.stop().await;
    assert_eq!(
        delivered_revision, latest_revision,
        "the final update was replaced into a payload the consumer had already captured; no later Agent update may be required for recovery"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_efficiency_state_replacement_and_consumer_drop_release_exact_queue_bytes() {
    let mut fixture = Fixture::new().await;
    let first = fixture.update("first payload has a different serialized length");
    publish(&fixture.hub, first).await;
    let replacement = PauseRegistration::new(&fixture.ledger, PausePoint::PayloadReplaced);
    let next = fixture.update("latest");
    let latest_revision = fixture.revision();
    let hub = fixture.hub.clone();
    let producer = tokio::spawn(async move { publish(&hub, next).await });
    replacement.arrived().await;

    let sent: Value = serde_json::from_str(
        &fixture
            .subscription
            .events
            .try_recv()
            .unwrap()
            .into_string(),
    )
    .unwrap();
    replacement.release();
    producer.await.unwrap();
    let mut delivered = vec![sent];
    delivered.extend(fixture.drain());
    assert_eq!(last_revision(&delivered), latest_revision);
    assert!(!fixture.lease.is_cancelled());
    let retained_bytes = fixture.ledger.load(Ordering::Acquire);
    fixture.stop().await;
    assert_eq!(
        retained_bytes, 0,
        "all real queue payloads were consumed; overlapping replacement and consumer Drop must not leave phantom or wrapped bytes"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_efficiency_reliable_failure_survives_an_in_flight_state_handoff() {
    let mut fixture = Fixture::new().await;
    let first = fixture.update("before failure");
    publish(&fixture.hub, first).await;
    let capture = PauseRegistration::new(&fixture.ledger, PausePoint::PayloadCaptured);
    let queued = fixture.subscription.events.try_recv().unwrap();
    let consumer = tokio::task::spawn_blocking(move || queued.into_string());
    capture.arrived().await;

    let (events, failed) = fixture.fail();
    publish(&fixture.hub, events).await;
    capture.release();
    let sent: Value = serde_json::from_str(&consumer.await.unwrap()).unwrap();
    let mut delivered = vec![sent];
    delivered.extend(fixture.drain());
    let failures = delivered
        .iter()
        .filter(|event| event["type"] == "bridge/session_turn_failed")
        .collect::<Vec<_>>();
    assert_eq!(failures, vec![&failed]);
    assert_eq!(last_revision(&delivered), fixture.revision());
    assert!(!fixture.lease.is_cancelled());
    assert_eq!(fixture.ledger.load(Ordering::Acquire), 0);
    fixture.stop().await;
}
