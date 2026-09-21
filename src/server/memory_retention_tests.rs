//! Memory acceptance gates at the production Hub publication and SSE boundary.
//! Bytes below are serialized queue or conversation payload bytes, not RSS.

use super::tests::{observation_hub, observer_ready};
use super::*;
use crate::runtime_state::RuntimeState;
use serde_json::Value;

struct MemorySession {
    runtime: RuntimeState,
    incarnation: u64,
    operation: String,
    published_seq: u64,
}

impl MemorySession {
    async fn new(hub: &BridgeHub) -> Self {
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
        let prompt = vec![json!({"type":"text", "text":"retry this exact request"})];
        let crate::session_state::TurnAdmission::Accepted { operation_id } = runtime
            .admit_session_turn(
                "epoch",
                "session",
                incarnation,
                &revision,
                "prompt",
                prompt.clone(),
            )
            .unwrap()
        else {
            unreachable!("first prompt is a fresh admission")
        };
        let published_seq = runtime.seq();
        hub.publish(
            1,
            json!({
                "type":"bridge/internal_runtime_snapshot", "value":runtime.snapshot(),
            })
            .to_string(),
        )
        .await;
        hub.publish(1, json!({
            "type":"acp/session_created", "cwd":"/workspace", "response":{"sessionId":"session"},
        }).to_string()).await;
        hub.publish(1, json!({
            "type":"acp/prompt_started", "sessionId":"session", "requestId":"prompt", "prompt":prompt,
        }).to_string()).await;
        Self {
            runtime,
            incarnation,
            operation: operation_id,
            published_seq,
        }
    }

    fn revision(&self) -> u64 {
        self.runtime.state("session").unwrap().view_revision
    }

    fn view(&mut self) -> Value {
        self.runtime
            .view_value("session", self.incarnation)
            .unwrap()
    }

    async fn append(&mut self, hub: &BridgeHub, update: Value) -> Value {
        let from = self.revision();
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
        for delta in self.runtime.deltas_after(self.published_seq).unwrap() {
            self.published_seq = delta.seq;
            hub.publish(
                1,
                json!({"type":"bridge/internal_runtime_delta", "value":delta}).to_string(),
            )
            .await;
        }
        hub.publish(
            1,
            json!({"type":"acp/session_update", "notification":{
                "sessionId":"session", "update":update,
            }})
            .to_string(),
        )
        .await;
        let delta = json!({
            "type":"bridge/session_delta", "bridgeEpoch":"epoch", "sessionId":"session",
            "sessionIncarnation":self.incarnation, "fromRevision":from, "viewRevision":self.revision(),
            "change":{"kind":"turn_update", "operationId":self.operation, "update":update},
        });
        hub.publish(1, delta.to_string()).await;
        delta
    }
}

struct Observed {
    subscription: BridgeSubscription,
    _guard: SubscriptionGuard,
    lease: ObservationLease,
    queued_bytes: Arc<AtomicUsize>,
}

async fn observe(
    hub: &Arc<BridgeHub>,
    commands: &mut mpsc::UnboundedReceiver<bridge::BridgeInput>,
    incarnation: u64,
    revision: u64,
) -> Observed {
    let mut pending = Box::pin(hub.observe_session("session".into(), None));
    assert!(futures::poll!(pending.as_mut()).is_pending());
    let bridge::BridgeInput::ObserveSession {
        observer_id,
        lease,
        reply,
        ..
    } = commands.try_recv().unwrap()
    else {
        unreachable!("observation uses its owner registration input")
    };
    let mut ready: Value = serde_json::from_str(&observer_ready(observer_id, revision)).unwrap();
    ready["reset"]["sessionIncarnation"] = json!(incarnation);
    ready["reset"]["phase"] = json!("running");
    hub.publish(1, ready.to_string()).await;
    reply.send(Ok(())).unwrap();
    let (mut subscription, guard) = pending.await.unwrap();
    let reset: Value =
        serde_json::from_str(&subscription.events.try_recv().unwrap().into_string()).unwrap();
    assert_eq!(reset["sessionIncarnation"], incarnation);
    assert_eq!(reset["viewRevision"], revision);
    let queued_bytes = hub.state.lock().await.session_subscribers[&observer_id]
        .sender
        .queued_bytes
        .clone();
    Observed {
        subscription,
        _guard: guard,
        lease,
        queued_bytes,
    }
}

fn tool_content(text: String, first: bool) -> Value {
    let mut update = json!({
        "sessionUpdate":if first { "tool_call" } else { "tool_call_update" },
        "toolCallId":"tool", "status":"in_progress",
        "content":[{"type":"content", "content":{"type":"text", "text":text}}],
    });
    if first {
        update["title"] = json!("Growing tool output");
    }
    update
}

fn tool_text(updates: &[Value]) -> &str {
    assert_eq!(
        updates.len(),
        1,
        "replacing one tool must not duplicate entities"
    );
    updates[0]["content"][0]["content"]["text"]
        .as_str()
        .unwrap()
}

fn drain(receiver: &mut mpsc::UnboundedReceiver<QueuedSubscriberEvent>) -> Vec<Value> {
    std::iter::from_fn(|| receiver.try_recv().ok())
        .map(|event| serde_json::from_str(&event.into_string()).unwrap())
        .collect()
}

async fn fenced_view(
    hub: &Arc<BridgeHub>,
    commands: &mut mpsc::UnboundedReceiver<bridge::BridgeInput>,
    fixture: &mut MemorySession,
) -> Value {
    let owner = SessionResourceOwner::new("epoch", "session", fixture.incarnation);
    let mut request =
        Box::pin(hub.session_view_for_owner("session".into(), None, Some(owner.clone())));
    assert!(futures::poll!(request.as_mut()).is_pending());
    let bridge::BridgeInput::SessionViewRequest {
        expected_owner,
        response,
        ..
    } = commands.try_recv().unwrap()
    else {
        unreachable!("reset recovery queries the current owner")
    };
    assert_eq!(expected_owner, Some(owner));
    response.send(Ok(fixture.view())).unwrap();
    request.await.unwrap()
}

async fn hub_retention_with_observers(count: usize) -> (usize, usize) {
    let (hub, mut commands) = observation_hub(count).await;
    let mut fixture = MemorySession::new(&hub).await;
    let mut observers = Vec::new();
    for _ in 0..count {
        observers.push(observe(&hub, &mut commands, fixture.incarnation, fixture.revision()).await);
    }
    let mut latest = String::new();
    for step in 1..=32 {
        latest.push_str(&"x".repeat(4096));
        fixture
            .append(&hub, tool_content(latest.clone(), step == 1))
            .await;
        for observer in &mut observers {
            drop(drain(&mut observer.subscription.events));
            assert_eq!(observer.queued_bytes.load(Ordering::Acquire), 0);
            assert!(!observer.lease.is_cancelled());
        }
    }
    let retained_text = {
        let state = hub.state.lock().await;
        let canonical = state.canonical.snapshot.as_ref().unwrap().sessions["session"]
            .active_turn
            .as_ref()
            .unwrap();
        assert_eq!(tool_text(&canonical.updates), latest);
        state
            .runtime
            .replay_events()
            .iter()
            .filter_map(|raw| {
                let event: Value = serde_json::from_str(raw).unwrap();
                event
                    .pointer("/notification/update/content/0/content/text")
                    .and_then(Value::as_str)
                    .map(str::len)
            })
            .sum::<usize>()
    };
    drop(observers);
    hub.shutdown().await;
    (retained_text, latest.len())
}

#[tokio::test]
async fn memory_retention_m04_hub_without_observers_keeps_only_current_conversation() {
    let (retained, current) = hub_retention_with_observers(0).await;
    assert!(
        retained <= current,
        "no browser exists, but legacy replay retains {retained} text bytes for {current} current bytes"
    );
}

#[tokio::test]
async fn memory_retention_m04_hub_one_healthy_observer_does_not_archive_replaced_content() {
    let (retained, current) = hub_retention_with_observers(1).await;
    assert!(
        retained <= current,
        "a caught-up observer leaves {retained} text bytes for {current} current bytes"
    );
}

#[tokio::test]
async fn memory_retention_m04_hub_multiple_observers_do_not_archive_replaced_content() {
    let (retained, current) = hub_retention_with_observers(3).await;
    assert!(
        retained <= current,
        "caught-up observers leave {retained} text bytes for {current} current bytes"
    );
}

#[tokio::test]
async fn memory_retention_m07_slow_observer_replaces_payload_with_latest_reset() {
    let (hub, mut commands) = observation_hub(2).await;
    let mut fixture = MemorySession::new(&hub).await;
    let mut slow = observe(&hub, &mut commands, fixture.incarnation, fixture.revision()).await;
    let mut healthy = observe(&hub, &mut commands, fixture.incarnation, fixture.revision()).await;
    let mut healthy_updates = Vec::new();
    let mut old_payloads = Vec::new();
    let mut latest = String::new();
    for step in 1..=32 {
        latest.push_str(&"content".repeat(1024));
        let published = fixture
            .append(&hub, tool_content(latest.clone(), step == 1))
            .await;
        let payload = healthy.subscription.events.try_recv().unwrap().into_arc();
        let event: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(
            event, published,
            "healthy observers must retain their continuous incremental path"
        );
        healthy_updates =
            fold_active_turn_update(&healthy_updates, &event["change"]["update"]).unwrap();
        old_payloads.push(Arc::downgrade(&payload));
        drop(payload);
        assert_eq!(healthy.queued_bytes.load(Ordering::Acquire), 0);
        assert!(!healthy.lease.is_cancelled());
        assert!(!slow.lease.is_cancelled());
    }
    assert_eq!(tool_text(&healthy_updates), latest);
    let reset = session_reset_value("session", &fixture.view()).unwrap();
    let retained_bytes = slow.queued_bytes.load(Ordering::Acquire);
    let old_payloads_released = old_payloads
        .iter()
        .all(|payload| payload.upgrade().is_none());
    let pending = drain(&mut slow.subscription.events);
    assert_eq!(slow.queued_bytes.load(Ordering::Acquire), 0);
    let recovered = fenced_view(&hub, &mut commands, &mut fixture).await;
    assert_eq!(
        tool_text(
            recovered["session"]["activeTurn"]["updates"]
                .as_array()
                .unwrap()
        ),
        latest
    );
    assert_eq!(hub.state.lock().await.session_subscribers.len(), 2);
    assert!(
        commands.try_recv().is_err(),
        "slow delivery must not withdraw observation or cancel work"
    );
    drop((slow, healthy));
    hub.shutdown().await;
    assert!(
        pending == vec![reset.clone()],
        "fully unconsumed state updates must become one current reset; got {} events retaining {} bytes",
        pending.len(),
        retained_bytes,
    );
    assert_eq!(
        retained_bytes,
        reset.to_string().len(),
        "queue accounting must release every replaced payload immediately"
    );
    assert!(
        old_payloads_released,
        "slow backlog still owns superseded payload Arcs"
    );
}

#[tokio::test]
async fn memory_retention_m08_snapshot_suffix_and_gap_recovery_match_current_state() {
    let (hub, mut commands) = observation_hub(0).await;
    let mut fixture = MemorySession::new(&hub).await;
    fixture.append(&hub, tool_content("A".into(), true)).await;
    let captured = fixture.runtime.snapshot();
    let mut subscriber = hub.subscribe().await.unwrap();
    let initial: Value = subscriber
        .initial_events
        .iter()
        .map(|raw| serde_json::from_str::<Value>(raw).unwrap())
        .find(|event| event["type"] == "bridge/runtime_snapshot")
        .unwrap();
    assert_eq!(
        initial["snapshot"],
        serde_json::to_value(&captured).unwrap()
    );
    fixture.append(&hub, tool_content("AB".into(), false)).await;
    let suffix = drain(&mut subscriber.events)
        .into_iter()
        .filter(|event| event["type"] == "bridge/runtime_delta")
        .collect::<Vec<_>>();
    let mut reconstructed = CanonicalProjection {
        snapshot: Some(captured),
    };
    for event in suffix {
        assert!(reconstructed.apply_delta(serde_json::from_value(event["delta"].clone()).unwrap()));
    }
    assert_eq!(
        serde_json::to_value(reconstructed.snapshot.unwrap()).unwrap(),
        serde_json::to_value(fixture.runtime.snapshot()).unwrap()
    );

    let current_seq = fixture.runtime.seq();
    for seq in [current_seq + 2, current_seq + 3] {
        hub.publish(
            1,
            json!({"type":"bridge/internal_runtime_delta", "value":RuntimeDelta {
                epoch:"epoch".into(), seq, scope_revision:None,
                change:RuntimeChange::SessionRemoved { session_id:"missing".into(), incarnation:1 },
            }})
            .to_string(),
        )
        .await;
    }
    assert!(matches!(
        commands.try_recv().unwrap(),
        bridge::BridgeInput::RuntimeSnapshotRequest
    ));
    assert!(
        commands.try_recv().is_err(),
        "one gap must share one snapshot recovery"
    );
    let recovery_cut = fixture.runtime.snapshot();
    hub.publish(
        1,
        json!({"type":"bridge/internal_runtime_snapshot", "value":recovery_cut}).to_string(),
    )
    .await;
    fixture
        .append(&hub, tool_content("ABC".into(), false))
        .await;
    let current = hub.state.lock().await.canonical.snapshot.clone().unwrap();
    assert_eq!(
        serde_json::to_value(current.clone()).unwrap(),
        serde_json::to_value(fixture.runtime.snapshot()).unwrap()
    );
    assert_eq!(
        tool_text(
            &current.sessions["session"]
                .active_turn
                .as_ref()
                .unwrap()
                .updates
        ),
        "ABC"
    );
    drop(subscriber);
    hub.shutdown().await;
}

#[tokio::test]
async fn memory_retention_m08_stale_sse_cursor_gets_current_reset_and_contiguous_suffix() {
    let (hub, mut commands) = observation_hub(1).await;
    let mut fixture = MemorySession::new(&hub).await;
    fixture
        .append(&hub, tool_content("before reconnect".into(), true))
        .await;
    let cut = fixture.revision();
    let mut headers = HeaderMap::new();
    headers.insert("last-event-id", "epoch:1:1".parse().unwrap());
    let mut handshake = Box::pin(session_events(
        Path("session".into()),
        State(AppState {
            bridge: hub.clone(),
        }),
        Query(SessionViewQuery::default()),
        headers,
    ));
    assert!(futures::poll!(handshake.as_mut()).is_pending());
    let bridge::BridgeInput::ObserveSession {
        observer_id,
        expected_owner,
        reply,
        ..
    } = commands.try_recv().unwrap()
    else {
        unreachable!("SSE cursor reconnect uses ObserveSession")
    };
    assert_eq!(
        expected_owner,
        Some(SessionResourceOwner::new(
            "epoch",
            "session",
            fixture.incarnation
        ))
    );
    hub.publish(1, observer_ready(observer_id, cut)).await;
    reply.send(Ok(())).unwrap();
    let mut body = handshake.await.into_body().into_data_stream();
    let first = futures::StreamExt::next(&mut body).await.unwrap().unwrap();
    let first = std::str::from_utf8(&first).unwrap();
    assert!(first.contains("bridge/session_reset"));
    assert!(first.contains(&format!("id: epoch:1:{cut}")));
    fixture
        .append(&hub, tool_content("after reconnect".into(), false))
        .await;
    let next = futures::StreamExt::next(&mut body).await.unwrap().unwrap();
    let next = std::str::from_utf8(&next).unwrap();
    assert!(next.contains("bridge/session_delta"));
    assert!(next.contains(&format!("\"fromRevision\":{cut}")));
    assert!(next.contains("after reconnect"));
    drop(body);
    hub.shutdown().await;
}

#[tokio::test]
async fn memory_retention_m09_reset_preserves_unreconstructible_failure_and_retry_prompt() {
    let (hub, mut commands) = observation_hub(1).await;
    let mut fixture = MemorySession::new(&hub).await;
    let mut observer = observe(&hub, &mut commands, fixture.incarnation, fixture.revision()).await;
    for step in 0..32 {
        fixture
            .append(
                &hub,
                tool_content(
                    format!("old version {step}: {}", "x".repeat(4096)),
                    step == 0,
                ),
            )
            .await;
    }
    let prompt = fixture.view()["session"]["activeTurn"]["prompt"].clone();
    assert_eq!(
        prompt,
        json!([{"type":"text", "text":"retry this exact request"}])
    );
    let error = json!({"code":-32603, "message":"specific failure", "data":{"retry":"exact"}});
    fixture
        .runtime
        .fail_prompt(
            "epoch",
            "session",
            fixture.incarnation,
            "prompt",
            error.clone(),
        )
        .unwrap();
    fixture
        .runtime
        .complete_turn(
            "session",
            fixture.incarnation,
            &fixture.operation,
            json!({"error":error}),
        )
        .unwrap();
    fixture
        .runtime
        .commit_completed_turn_from_memory("session", fixture.incarnation, &fixture.operation)
        .unwrap();
    let settled = fixture.view();
    assert_eq!(settled["session"]["phase"], "ready");
    assert!(settled["session"]["activeTurn"].is_null());
    assert!(
        settled["session"]["turnOutcomes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    hub.publish(
        1,
        json!({"type":"bridge/internal_runtime_snapshot", "value":fixture.runtime.snapshot()})
            .to_string(),
    )
    .await;
    let failed = json!({
        "type":"bridge/session_turn_failed", "bridgeEpoch":"epoch", "sessionId":"session",
        "sessionIncarnation":fixture.incarnation, "viewRevision":fixture.revision(),
        "operationId":fixture.operation, "clientIntentId":"prompt", "phase":"ready",
        "prompt":prompt, "error":error,
    });
    hub.publish(1, failed.to_string()).await;
    // A reset at a newer revision cannot reconstruct this RPC error from
    // turnOutcomes. It must not subsume the reliable failure notification.
    fixture
        .runtime
        .append_session_update(
            "session",
            fixture.incarnation,
            json!({
                "sessionUpdate":"agent_message_chunk", "messageId":"after-failure",
                "content":{"type":"text", "text":"a later unsolicited update"},
            }),
        )
        .unwrap();
    let view = fixture.view();
    assert!(
        view["session"]["turnOutcomes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    hub.publish(
        1,
        json!({"type":"bridge/session_view", "sessionId":"session", "view":view}).to_string(),
    )
    .await;
    let delivered = drain(&mut observer.subscription.events);
    let failures = delivered
        .iter()
        .filter(|event| event["type"] == "bridge/session_turn_failed")
        .collect::<Vec<_>>();
    assert_eq!(failures, vec![&failed]);
    assert_eq!(failures[0]["prompt"], prompt);
    assert!(
        delivered
            .iter()
            .any(|event| event["type"] == "bridge/session_reset"
                && event["viewRevision"] == fixture.revision())
    );
    assert!(!observer.lease.is_cancelled());
    drop(observer);
    hub.shutdown().await;
}

#[tokio::test]
async fn memory_retention_m11_retired_marker_survives_compaction_and_owner_end() {
    let (hub, mut commands) = observation_hub(1).await;
    let mut fixture = MemorySession::new(&hub).await;
    let mut observer = observe(&hub, &mut commands, fixture.incarnation, fixture.revision()).await;
    for step in 0..32 {
        fixture
            .append(
                &hub,
                tool_content(
                    format!("retiring version {step}: {}", "x".repeat(4096)),
                    step == 0,
                ),
            )
            .await;
    }
    hub.publish(
        1,
        json!({"type":"bridge/session_view", "sessionId":"session", "view":fixture.view()})
            .to_string(),
    )
    .await;
    let retired = json!({"type":"bridge/session_retired", "bridgeEpoch":"epoch", "sessionId":"session",
        "sessionIncarnation":fixture.incarnation, "reason":"closed"});
    hub.publish(1, retired.to_string()).await;
    hub.publish(
        1,
        json!({"type":"bridge/internal_observer_end", "observerId":observer.subscription.id,
        "bridgeEpoch":"epoch", "sessionId":"session", "sessionIncarnation":fixture.incarnation})
        .to_string(),
    )
    .await;
    let delivered = drain(&mut observer.subscription.events);
    assert_eq!(
        delivered
            .iter()
            .filter(|event| event["type"] == "bridge/session_retired")
            .collect::<Vec<_>>(),
        vec![&retired]
    );
    assert_eq!(
        delivered.last(),
        Some(&retired),
        "the terminal owner marker must follow its committed state"
    );
    assert!(observer.lease.is_finished());
    assert!(!observer.lease.is_cancelled());
    assert!(observer.subscription.events.recv().await.is_none());
    assert_eq!(observer.queued_bytes.load(Ordering::Acquire), 0);
    assert!(hub.state.lock().await.session_subscribers.is_empty());
    drop(observer);
    hub.shutdown().await;
}

#[tokio::test]
async fn memory_retention_m12_dropping_observers_releases_independent_backlogs() {
    let (hub, mut commands) = observation_hub(2).await;
    let mut fixture = MemorySession::new(&hub).await;
    let first = observe(&hub, &mut commands, fixture.incarnation, fixture.revision()).await;
    let mut second = observe(&hub, &mut commands, fixture.incarnation, fixture.revision()).await;
    fixture
        .append(&hub, tool_content("pending content".repeat(4096), true))
        .await;
    let first_bytes = first.queued_bytes.clone();
    let second_bytes = second.queued_bytes.clone();
    let first_lease = first.lease.clone();
    assert!(first_bytes.load(Ordering::Acquire) > 0);
    let still_pending = second_bytes.load(Ordering::Acquire);
    drop(first);
    assert_eq!(first_bytes.load(Ordering::Acquire), 0);
    assert!(first_lease.is_cancelled());
    assert_eq!(second_bytes.load(Ordering::Acquire), still_pending);
    assert!(!second.lease.is_cancelled());
    drop(drain(&mut second.subscription.events));
    assert_eq!(second_bytes.load(Ordering::Acquire), 0);
    drop(second);
    hub.shutdown().await;
    assert!(hub.state.lock().await.session_subscribers.is_empty());
}

#[tokio::test]
async fn memory_retention_m12_finished_generation_releases_hub_current_payload() {
    let (hub, mut commands) = observation_hub(1).await;
    let mut fixture = MemorySession::new(&hub).await;
    let observer = observe(&hub, &mut commands, fixture.incarnation, fixture.revision()).await;
    let marker = "ended-epoch-only-payload".repeat(4096);
    fixture
        .append(&hub, tool_content(marker.clone(), true))
        .await;
    let ledger = observer.queued_bytes.clone();
    let lease = observer.lease.clone();
    let old_overlay = {
        let state = hub.state.lock().await;
        Arc::downgrade(
            &state.canonical.snapshot.as_ref().unwrap().sessions["session"]
                .active_turn
                .as_ref()
                .unwrap()
                .updates,
        )
    };
    assert!(old_overlay.upgrade().is_some());
    hub.finish_generation(1).await;
    drop(observer);
    assert_eq!(ledger.load(Ordering::Acquire), 0);
    assert!(lease.is_cancelled());
    assert!(hub.state.lock().await.session_subscribers.is_empty());
    let retained_legacy = hub.state.lock().await.runtime.replay_events().join("\n");
    assert!(
        old_overlay.upgrade().is_none(),
        "a stopped epoch must not pin its canonical active overlay"
    );
    assert!(
        !retained_legacy.contains(&marker),
        "generation teardown must release legacy conversation payload too"
    );
}
