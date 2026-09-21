use super::workflow::*;
use super::*;

async fn backoff_blocks_retirement(supports_close: bool) {
    let (mut agent, mut completion, load) = fork_to_first_load(1, supports_close).await;
    let original = view(&mut agent, TARGET).await;
    let backoff_started = enter_two_second_backoff(&mut agent, load).await;
    catalog_barrier(&mut agent).await;

    // The fourth failure schedules a 2s retry. Cross the 1s retirement deadline
    // before that retry, without observing the target or manually refreshing it.
    let premature = agent.request_within(Duration::from_millis(1500)).await;
    assert!(
        premature.is_none(),
        "W1: optional history is unfinished during backoff; premature Agent request: {premature:?}"
    );
    let retired = target_retirements(&mut agent);
    assert!(
        retired.is_empty(),
        "W1: unfinished fork was locally retired during backoff: {retired:?}"
    );
    assert!(matches!(
        completion.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));

    let fifth_load = agent.next_request().await;
    assert_eq!(fifth_load["method"], "session/load");
    assert_eq!(fifth_load["params"]["sessionId"], TARGET);
    assert!(backoff_started.elapsed() >= Duration::from_secs(2));
    agent.reply(&fifth_load, json!({})).await;
    let forked = TestAgent::result(completion)
        .await
        .expect("successful fork must survive optional history retries");
    assert_eq!(forked["sessionId"], TARGET);
    assert_eq!(
        forked["view"]["session"]["incarnation"],
        original["session"]["incarnation"]
    );
    assert_eq!(
        forked["view"]["baseline"]["updates"],
        json!([]),
        "successful empty authoritative replay replaces the source fallback"
    );
    assert!(target_retirements(&mut agent).is_empty());
    agent.stop().await;
}

#[tokio::test(start_paused = true)]
async fn history_workflow_retry_backoff_blocks_agent_close() {
    backoff_blocks_retirement(true).await;
}

#[tokio::test(start_paused = true)]
async fn history_workflow_retry_backoff_blocks_local_retirement() {
    backoff_blocks_retirement(false).await;
}

async fn assert_full_idle_interval(agent: &mut TestAgent, supports_close: bool) {
    let settled = tokio::time::Instant::now();
    let premature = agent.request_within(Duration::from_millis(9900)).await;
    assert!(
        premature.is_none(),
        "W2: idle interval started before workflow completion: {premature:?}"
    );
    assert!(
        target_retirements(agent).is_empty(),
        "W2: local retirement consumed the retry interval"
    );

    if supports_close {
        let close = agent
            .request_within(Duration::from_millis(200))
            .await
            .expect("completed workflow must eventually release its idle protection");
        assert_eq!(close["method"], "session/close");
        assert_eq!(close["params"]["sessionId"], TARGET);
        assert!(settled.elapsed() >= Duration::from_secs(10));
        agent.reply(&close, json!({})).await;
        agent.event("bridge/session_retired", TARGET).await;
    } else {
        tokio::time::timeout(
            Duration::from_millis(200),
            agent.event("bridge/session_retired", TARGET),
        )
        .await
        .expect("completed workflow must become locally reclaimable at its idle deadline");
        assert!(
            settled.elapsed() >= Duration::from_secs(10),
            "local retirement must measure the full interval, not just fall inside a tolerance window"
        );
    }
    let retired = target_retirements(agent);
    assert_eq!(
        retired.len(),
        1,
        "workflow must retire exactly once after a full idle interval: {retired:?}"
    );
    assert_eq!(
        retired[0]["reason"],
        if supports_close {
            "closed"
        } else {
            "unobserved"
        }
    );
    assert!(
        agent
            .request_within(Duration::from_secs(20))
            .await
            .is_none()
    );
    assert_eq!(target_retirements(agent).len(), 1);
}

async fn retry_completion_gets_full_idle_interval(supports_close: bool, fallback: bool) {
    let (mut agent, completion, load) = fork_to_first_load(10, supports_close).await;
    enter_two_second_backoff(&mut agent, load).await;
    catalog_barrier(&mut agent).await;
    let fifth_load = agent.next_request().await;
    assert_eq!(fifth_load["method"], "session/load");
    // Include nonzero in-flight time after the retry wait. Neither interval
    // belongs to the idle countdown that starts after the final result.
    assert!(
        agent
            .request_within(Duration::from_millis(400))
            .await
            .is_none()
    );
    if fallback {
        reject_load(&mut agent, &fifth_load, -32600).await;
    } else {
        agent.reply(&fifth_load, json!({})).await;
    }
    let result = TestAgent::result(completion).await.unwrap();
    assert_eq!(result["sessionId"], TARGET);
    assert_eq!(result["view"]["session"]["phase"], "ready");
    if fallback {
        assert_eq!(
            result["view"]["baseline"]["updates"],
            json!([source_update()])
        );
        assert!(
            result["view"]["session"]["historyNotice"]
                .as_str()
                .is_some_and(|notice| !notice.is_empty())
        );
    } else {
        assert_eq!(result["view"]["baseline"]["updates"], json!([]));
        assert!(result["view"]["session"]["historyNotice"].is_null());
    }
    assert_full_idle_interval(&mut agent, supports_close).await;
    agent.stop().await;
}

#[tokio::test(start_paused = true)]
async fn history_workflow_retry_success_starts_full_interval_before_close() {
    retry_completion_gets_full_idle_interval(true, false).await;
}

#[tokio::test(start_paused = true)]
async fn history_workflow_retry_success_starts_full_interval_before_local_retirement() {
    retry_completion_gets_full_idle_interval(false, false).await;
}

#[tokio::test(start_paused = true)]
async fn history_workflow_retry_fallback_starts_full_interval_before_close() {
    retry_completion_gets_full_idle_interval(true, true).await;
}

#[tokio::test(start_paused = true)]
async fn history_workflow_retry_fallback_starts_full_interval_before_local_retirement() {
    retry_completion_gets_full_idle_interval(false, true).await;
}

#[tokio::test(start_paused = true)]
async fn history_workflow_dropped_business_reply_does_not_cancel_or_leak_sync() {
    let (mut agent, completion, load) = fork_to_first_load(10, true).await;
    enter_two_second_backoff(&mut agent, load).await;
    // A vanished HTTP receiver is not an explicit business cancellation.
    drop(completion);
    let retry = agent.next_request().await;
    assert_eq!(retry["method"], "session/load");
    assert_eq!(retry["params"]["sessionId"], TARGET);
    agent.reply(&retry, json!({})).await;
    let forked = agent.event("acp/session_forked", TARGET).await;
    assert_eq!(forked["response"]["sessionId"], TARGET);
    assert_full_idle_interval(&mut agent, true).await;
    agent.stop().await;
}
