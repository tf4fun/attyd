use super::workflow::*;
use super::*;

type ObservationResult = oneshot::Receiver<Result<(), SessionViewError>>;

fn pending_observer(
    agent: &TestAgent,
    session_id: &str,
    observer_id: u64,
    cwd: Option<&str>,
) -> (ObservationLease, ObservationResult) {
    let lease = ObservationLease::new();
    let (reply, result) = oneshot::channel();
    agent
        .commands
        .send(BridgeInput::ObserveSession {
            session_id: session_id.into(),
            cwd: cwd.map(str::to_owned),
            expected_owner: None,
            observer_id,
            lease: lease.clone(),
            reply,
        })
        .unwrap();
    (lease, result)
}

async fn observation_completed(result: ObservationResult) {
    tokio::time::timeout(Duration::from_secs(1), result)
        .await
        .expect("materialization must deliver its observer")
        .unwrap()
        .unwrap();
}

fn leave(agent: &TestAgent, session_id: &str, observer_id: u64, lease: ObservationLease) {
    lease.cancel();
    agent
        .commands
        .send(BridgeInput::UnobserveSession {
            session_id: session_id.into(),
            observer_id,
            lease,
        })
        .unwrap();
}

fn assert_not_retired(agent: &mut TestAgent, session_id: &str) {
    assert!(
        !agent.recorded_events().iter().any(|event| {
            event["type"] == "bridge/session_retired" && event["sessionId"] == session_id
        }),
        "an unfinished workflow or observer must keep {session_id} alive"
    );
}

async fn close_after_full_interval(agent: &mut TestAgent, session_id: &str, seconds: u64) {
    let settled = tokio::time::Instant::now();
    if seconds != 0 {
        assert!(
            agent
                .request_within(Duration::from_secs(seconds) - Duration::from_millis(100))
                .await
                .is_none(),
            "workflow completion must start a complete idle interval"
        );
    }
    let close = agent
        .request_within(Duration::from_secs(1))
        .await
        .expect("a finished workflow must not leave permanent busy state");
    assert_eq!(close["method"], "session/close", "{close}");
    assert_eq!(close["params"]["sessionId"], session_id);
    assert!(settled.elapsed() >= Duration::from_secs(seconds));
    agent.reply(&close, json!({})).await;
    agent.event("bridge/session_retired", session_id).await;
    assert!(agent.request_within(Duration::from_secs(1)).await.is_none());
}

fn assert_completion_published_before_retirement(agent: &mut TestAgent, kind: &str) {
    let events = agent.recorded_events();
    let completed = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            let event = if event["type"] == "bridge/internal_direct" {
                &event["event"]
            } else {
                event
            };
            event["type"] == kind
                && (event["sessionId"] == TARGET || event["response"]["sessionId"] == TARGET)
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let retired = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event["type"] == "bridge/session_retired" && event["sessionId"] == TARGET
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(
        completed.len(),
        1,
        "the attachment must publish one terminal event"
    );
    assert_eq!(retired.len(), 1, "the idle target must retire once");
    assert!(
        completed[0] < retired[0],
        "zero timeout must not publish retirement before the attachment's completion"
    );
}

async fn create_observed_source(agent: &mut TestAgent) -> ObservationLease {
    let completion = agent.submit(json!({
        "type":"session/new", "requestId":"handoff-source", "cwd":"/repo",
    }));
    let request = agent.next_request().await;
    assert_eq!(request["method"], "session/new");
    agent.reply(&request, json!({"sessionId":SOURCE})).await;
    TestAgent::result(completion).await.unwrap();
    let update = json!({
        "sessionUpdate":"agent_message_chunk", "messageId":"handoff-context",
        "content":{"type":"text", "text":"Context retained across fallback"},
    });
    agent
        .send(
            json!({"jsonrpc":"2.0", "method":"session/update", "params":{
                "sessionId":SOURCE, "update":update,
            }}),
        )
        .await;
    loop {
        let published = agent.event("bridge/session_view", SOURCE).await;
        if published["view"]["baseline"]["updates"] == json!([update]) {
            break;
        }
    }
    let (lease, _) = observe(agent, SOURCE, 101).await;
    lease
}

async fn zero_timeout_fork_handoff(supports_close: bool) {
    let mut capabilities = json!({"loadSession":true, "sessionCapabilities":{"fork":{}}});
    if supports_close {
        capabilities["sessionCapabilities"]["close"] = json!({});
    }
    let mut agent = TestAgent::start(0, capabilities).await;
    // A cold observer installs the source atomically. Creating an unobserved
    // source at timeout=0 would test an unrelated source-retirement race.
    let (_source_lease, source_ready) = pending_observer(&agent, SOURCE, 101, Some("/repo"));
    let source_load = agent.next_request().await;
    assert_eq!(source_load["method"], "session/load");
    assert_eq!(source_load["params"]["sessionId"], SOURCE);
    agent.reply(&source_load, json!({})).await;
    observation_completed(source_ready).await;

    let completion = agent.submit(json!({
        "type":"session/fork", "requestId":"zero-fork", "sessionId":SOURCE,
    }));
    let fork = agent.next_request().await;
    assert_eq!(fork["method"], "session/fork");
    agent.reply(&fork, json!({"sessionId":TARGET})).await;
    let load = agent.next_request().await;
    assert_eq!(
        load["method"], "session/load",
        "target retired during fork handoff: {load}"
    );
    assert_eq!(load["params"]["sessionId"], TARGET);
    assert!(agent.request_within(Duration::from_secs(1)).await.is_none());
    assert_not_retired(&mut agent, TARGET);
    agent.reply(&load, json!({})).await;
    let result = TestAgent::result(completion).await.unwrap();
    assert_eq!(result["sessionId"], TARGET);
    assert_eq!(result["view"]["session"]["sessionId"], TARGET);
    if supports_close {
        close_after_full_interval(&mut agent, TARGET, 0).await;
    } else {
        let retired = agent.event("bridge/session_retired", TARGET).await;
        assert_eq!(retired["reason"], "unobserved");
        assert!(agent.request_within(Duration::from_secs(1)).await.is_none());
    }
    assert_completion_published_before_retirement(&mut agent, "acp/session_forked");
    agent.stop().await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_workflow_zero_timeout_fork_publishes_response_before_retiring() {
    zero_timeout_fork_handoff(true).await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_workflow_zero_timeout_fork_publishes_response_before_local_retirement() {
    zero_timeout_fork_handoff(false).await;
}

async fn zero_timeout_resume_handoff(supports_close: bool) {
    let mut capabilities = json!({"loadSession":true, "sessionCapabilities":{"resume":{}}});
    if supports_close {
        capabilities["sessionCapabilities"]["close"] = json!({});
    }
    let mut agent = TestAgent::start(0, capabilities).await;
    let completion = agent.submit(json!({
        "type":"session/resume", "requestId":"zero-resume", "sessionId":TARGET, "cwd":"/repo",
    }));
    let resume = agent.next_request().await;
    assert_eq!(resume["method"], "session/resume");
    agent.reply(&resume, json!({})).await;
    let load = agent.next_request().await;
    assert_eq!(
        load["method"], "session/load",
        "target retired during resume handoff: {load}"
    );
    assert_eq!(load["params"]["sessionId"], TARGET);
    assert!(agent.request_within(Duration::from_secs(1)).await.is_none());
    assert_not_retired(&mut agent, TARGET);
    agent.reply(&load, json!({})).await;
    TestAgent::result(completion).await.unwrap();
    if supports_close {
        close_after_full_interval(&mut agent, TARGET, 0).await;
    } else {
        let retired = agent.event("bridge/session_retired", TARGET).await;
        assert_eq!(retired["reason"], "unobserved");
        assert!(agent.request_within(Duration::from_secs(1)).await.is_none());
    }
    assert_completion_published_before_retirement(&mut agent, "acp/session_attached");
    agent.stop().await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_workflow_zero_timeout_resume_finishes_before_retiring() {
    zero_timeout_resume_handoff(true).await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_workflow_zero_timeout_resume_finishes_before_local_retirement() {
    zero_timeout_resume_handoff(false).await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_workflow_cold_observers_share_outer_retries_until_delivery() {
    let mut agent = TestAgent::start(
        0,
        json!({
            "loadSession":true, "sessionCapabilities":{"close":{}},
        }),
    )
    .await;
    let (first_lease, mut first_ready) = pending_observer(&agent, TARGET, 201, Some("/repo"));
    let first_load = agent.next_request().await;
    assert_eq!(first_load["method"], "session/load");
    reject_load(&mut agent, &first_load, -32603).await;
    let retry = agent.event("bridge/session_sync", TARGET).await;
    assert_eq!(retry["phase"], "retrying");
    assert_eq!(retry["retryAfterMs"], 250);
    let (second_lease, mut second_ready) = pending_observer(&agent, TARGET, 202, Some("/repo"));
    assert!(
        agent
            .request_within(Duration::from_millis(200))
            .await
            .is_none()
    );
    assert!(matches!(
        first_ready.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        second_ready.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    let second_load = agent.next_request().await;
    assert_eq!(second_load["method"], "session/load");
    reject_load(&mut agent, &second_load, -32603).await;
    let retry = agent.event("bridge/session_sync", TARGET).await;
    assert_eq!(retry["phase"], "retrying");
    assert_eq!(retry["retryAfterMs"], 500);
    assert!(
        agent
            .request_within(Duration::from_millis(400))
            .await
            .is_none()
    );
    let third_load = agent.next_request().await;
    assert_eq!(third_load["method"], "session/load");
    assert_eq!(third_load["params"]["sessionId"], TARGET);
    let history = json!({
        "sessionUpdate":"agent_message_chunk", "messageId":"restored-cold-context",
        "content":{"type":"text", "text":"Shared authoritative replay"},
    });
    agent.send(json!([
        {"jsonrpc":"2.0", "method":"session/update", "params":{"sessionId":TARGET, "update":history}},
        {"jsonrpc":"2.0", "id":third_load["id"], "result":{}},
    ])).await;
    observation_completed(first_ready).await;
    observation_completed(second_ready).await;
    let loaded = view(&mut agent, TARGET).await;
    assert_eq!(loaded["baseline"]["updates"], json!([history]));
    let ready_events = agent
        .recorded_events()
        .iter()
        .filter(|event| {
            event["type"] == "bridge/internal_observer_ready" && event["sessionId"] == TARGET
        })
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        ready_events.len(),
        2,
        "each waiter must be delivered exactly once"
    );
    let first_incarnation = ready_events[0]["reset"]["sessionIncarnation"]
        .as_u64()
        .expect("first observer must name its installed incarnation");
    let second_incarnation = ready_events[1]["reset"]["sessionIncarnation"]
        .as_u64()
        .expect("second observer must name its installed incarnation");
    assert_eq!(first_incarnation, second_incarnation);
    assert_eq!(
        loaded["session"]["incarnation"].as_u64().unwrap(),
        first_incarnation
    );
    let mut delivered_observers = ready_events
        .iter()
        .map(|event| event["observerId"].as_u64().unwrap())
        .collect::<Vec<_>>();
    delivered_observers.sort_unstable();
    assert_eq!(delivered_observers, vec![201, 202]);
    assert!(agent.request_within(Duration::from_secs(1)).await.is_none());
    assert_not_retired(&mut agent, TARGET);
    leave(&agent, TARGET, 201, first_lease);
    assert!(agent.request_within(Duration::from_secs(1)).await.is_none());
    leave(&agent, TARGET, 202, second_lease);
    close_after_full_interval(&mut agent, TARGET, 0).await;
    agent.stop().await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_workflow_fork_without_load_falls_back_and_retires() {
    let mut agent =
        TestAgent::start(2, json!({"sessionCapabilities":{"fork":{}, "close":{}}})).await;
    let _source_lease = create_observed_source(&mut agent).await;
    let completion = agent
        .submit(json!({"type":"session/fork", "requestId":"no-load-fork", "sessionId":SOURCE}));
    let fork = agent.next_request().await;
    assert_eq!(fork["method"], "session/fork");
    agent.reply(&fork, json!({"sessionId":TARGET})).await;
    let result = TestAgent::result(completion).await.unwrap();
    let current = &result["view"];
    assert_eq!(current["session"]["phase"], "ready");
    assert!(
        current["baseline"]["updates"]
            .to_string()
            .contains("Context retained across fallback")
    );
    assert!(
        current["session"]["historyNotice"]
            .as_str()
            .is_some_and(|notice| !notice.is_empty())
    );
    close_after_full_interval(&mut agent, TARGET, 2).await;
    agent.stop().await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_workflow_resume_without_load_falls_back_and_retires() {
    let mut agent =
        TestAgent::start(2, json!({"sessionCapabilities":{"resume":{}, "close":{}}})).await;
    let completion = agent.submit(json!({
        "type":"session/resume", "requestId":"no-load-resume", "sessionId":TARGET, "cwd":"/repo",
    }));
    let resume = agent.next_request().await;
    assert_eq!(resume["method"], "session/resume");
    let context = json!({
        "sessionUpdate":"agent_message_chunk", "messageId":"resume-context",
        "content":{"type":"text", "text":"Context received during resume"},
    });
    agent.send(json!([
        {"jsonrpc":"2.0", "method":"session/update", "params":{"sessionId":TARGET, "update":context}},
        {"jsonrpc":"2.0", "id":resume["id"], "result":{}},
    ])).await;
    TestAgent::result(completion).await.unwrap();
    let current = view(&mut agent, TARGET).await;
    assert_eq!(current["session"]["phase"], "ready");
    assert_eq!(current["baseline"]["updates"], json!([context]));
    assert!(
        current["session"]["historyNotice"]
            .as_str()
            .is_some_and(|notice| !notice.is_empty())
    );
    close_after_full_interval(&mut agent, TARGET, 2).await;
    agent.stop().await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_workflow_returning_observer_keeps_one_optional_sync() {
    let (mut agent, completion, first_load) = fork_to_first_load(1, true).await;
    enter_two_second_backoff(&mut agent, first_load).await;
    let (lease, before) = observe(&mut agent, TARGET, 301).await;
    assert_eq!(before["session"]["phase"], "ready");
    assert!(
        agent
            .request_within(Duration::from_millis(1900))
            .await
            .is_none(),
        "observing must not start another load"
    );
    let retry = agent.next_request().await;
    assert_eq!(
        retry["method"], "session/load",
        "observation must not cancel optional synchronization"
    );
    agent.reply(&retry, json!({})).await;
    let result = TestAgent::result(completion).await.unwrap();
    assert_eq!(
        result["view"]["session"]["incarnation"],
        before["session"]["incarnation"]
    );
    assert!(agent.request_within(Duration::from_secs(2)).await.is_none());
    assert_not_retired(&mut agent, TARGET);
    leave(&agent, TARGET, 301, lease);
    close_after_full_interval(&mut agent, TARGET, 1).await;
    agent.stop().await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_workflow_invalid_replay_fallback_does_not_leak_work() {
    let (mut agent, completion, load) = fork_to_first_load(2, true).await;
    // Structurally valid ACP, semantically invalid controls: the selected mode
    // was never advertised. This takes replay validation's early-error path.
    agent
        .reply(
            &load,
            json!({"modes":{
                "currentModeId":"missing", "availableModes":[{"id":"build", "name":"Build"}],
            }}),
        )
        .await;
    let result = TestAgent::result(completion).await.unwrap();
    assert_eq!(result["view"]["session"]["phase"], "ready");
    assert!(
        result["view"]["baseline"]["updates"]
            .to_string()
            .contains("Source context")
    );
    assert!(
        result["view"]["session"]["historyNotice"]
            .as_str()
            .is_some_and(|notice| !notice.is_empty())
    );
    close_after_full_interval(&mut agent, TARGET, 2).await;
    agent.stop().await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_workflow_rejected_fork_target_does_not_strand_source() {
    let mut agent =
        TestAgent::start(2, json!({"sessionCapabilities":{"fork":{}, "close":{}}})).await;
    let source_lease = create_observed_source(&mut agent).await;
    let completion = agent
        .submit(json!({"type":"session/fork", "requestId":"invalid-fork", "sessionId":SOURCE}));
    let fork = agent.next_request().await;
    assert_eq!(fork["method"], "session/fork");
    agent.reply(&fork, json!({"sessionId":SOURCE})).await;
    assert!(
        TestAgent::result(completion).await.is_err(),
        "fork cannot replace its source allocation"
    );
    let current = view(&mut agent, SOURCE).await;
    assert!(
        current["baseline"]["updates"]
            .to_string()
            .contains("Context retained across fallback")
    );
    assert!(agent.request_within(Duration::from_secs(3)).await.is_none());
    leave(&agent, SOURCE, 101, source_lease);
    close_after_full_interval(&mut agent, SOURCE, 2).await;
    agent.stop().await;
}
