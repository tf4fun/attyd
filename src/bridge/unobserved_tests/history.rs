use super::*;

const IDLE_SECONDS: i64 = 10;

async fn assert_optional_history_gets_a_fresh_idle_interval(
    mut agent: TestAgent,
    mut completion: oneshot::Receiver<Result<Value, BridgeRequestError>>,
    session_id: &str,
    load_duration: Duration,
) {
    let load = agent.next_request().await;
    assert_eq!(load["method"], "session/load");
    assert_eq!(load["params"]["sessionId"], session_id);

    // Keep the authoritative replay genuinely in flight for the chosen interval.
    // A catalog request or replay notification here would refresh every session's
    // timers and hide a missing refresh at the optional-load transition itself.
    assert!(
        agent.request_within(load_duration).await.is_none(),
        "optional history loading must prevent retirement"
    );
    assert!(matches!(
        completion.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));

    // An empty replay is a valid authoritative result and requires no updates.
    agent.reply(&load, json!({})).await;
    TestAgent::result(completion).await.unwrap();
    let idle_started = tokio::time::Instant::now();

    let early_close = agent.request_within(Duration::from_secs(9)).await;
    let close = match early_close {
        Some(request) => request,
        None => agent
            .request_within(Duration::from_secs(2))
            .await
            .expect("an unobserved idle session must eventually retire"),
    };
    let idle_before_close = idle_started.elapsed();
    assert_eq!(close["method"], "session/close");
    assert_eq!(close["params"]["sessionId"], session_id);
    agent.reply(&close, json!({})).await;
    agent.event("bridge/session_retired", session_id).await;
    agent.stop().await;

    assert!(
        idle_before_close >= Duration::from_secs(IDLE_SECONDS as u64),
        "optional history loading consumed the idle interval: close arrived after {idle_before_close:?} of idle"
    );
}

async fn resume_history_completion_starts_a_full_interval(load_duration: Duration) {
    let mut agent = TestAgent::start(
        IDLE_SECONDS,
        json!({
            "loadSession": true,
            "sessionCapabilities": { "resume": {}, "close": {} },
        }),
    )
    .await;
    let completion = agent.submit(json!({
        "type": "session/resume",
        "requestId": "resume",
        "sessionId": "saved",
        "cwd": "/workspace",
    }));
    let resume = agent.next_request().await;
    assert_eq!(resume["method"], "session/resume");
    agent.reply(&resume, json!({})).await;

    assert_optional_history_gets_a_fresh_idle_interval(agent, completion, "saved", load_duration)
        .await;
}

async fn fork_history_completion_starts_a_full_interval(load_duration: Duration) {
    let mut agent = TestAgent::start(
        IDLE_SECONDS,
        json!({
            "loadSession": true,
            "sessionCapabilities": { "fork": {}, "close": {} },
        }),
    )
    .await;
    let created = agent.submit(json!({
        "type": "session/new", "requestId": "new", "cwd": "/workspace",
    }));
    let new = agent.next_request().await;
    assert_eq!(new["method"], "session/new");
    agent.reply(&new, json!({ "sessionId": "source" })).await;
    TestAgent::result(created).await.unwrap();

    // Keep the source observed so its own retirement cannot refresh the target's
    // timer while the target's optional history request is in flight.
    let (reply, observed) = oneshot::channel();
    agent
        .commands
        .send(BridgeInput::ObserveSession {
            session_id: "source".into(),
            cwd: None,
            expected_owner: None,
            observer_id: 1,
            lease: ObservationLease::new(),
            reply,
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), observed)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    let completion = agent.submit(json!({
        "type": "session/fork", "requestId": "fork", "sessionId": "source",
    }));
    let fork = agent.next_request().await;
    assert_eq!(fork["method"], "session/fork");
    agent.reply(&fork, json!({ "sessionId": "target" })).await;

    assert_optional_history_gets_a_fresh_idle_interval(agent, completion, "target", load_duration)
        .await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_resume_history_completion_starts_a_full_interval() {
    resume_history_completion_starts_a_full_interval(Duration::from_secs(15)).await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_fork_history_completion_starts_a_full_interval() {
    fork_history_completion_starts_a_full_interval(Duration::from_secs(15)).await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_short_resume_history_completion_starts_a_full_interval() {
    resume_history_completion_starts_a_full_interval(Duration::from_secs(4)).await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_short_fork_history_completion_starts_a_full_interval() {
    fork_history_completion_starts_a_full_interval(Duration::from_secs(4)).await;
}
