use super::workflow::*;
use super::*;

const IDLE_SECONDS: i64 = 10;
const PAST_MAX_BACKOFF: Duration = Duration::from_secs(9);

fn assert_target_request(request: &Value, method: &str) {
    assert_eq!(request["method"], method, "{request}");
    assert_eq!(request["params"]["sessionId"], TARGET, "{request}");
}

fn retired_count(agent: &mut TestAgent) -> usize {
    agent
        .recorded_events()
        .iter()
        .filter(|event| event["type"] == "bridge/session_retired" && event["sessionId"] == TARGET)
        .count()
}

async fn finish_replay(agent: &mut TestAgent, request: &Value, marker: &str) -> Value {
    let update = json!({
        "sessionUpdate": "agent_message_chunk", "messageId": marker,
        "content": {"type": "text", "text": marker},
    });
    agent
        .send(json!([
            {
                "jsonrpc": "2.0", "method": "session/update",
                "params": {"sessionId": TARGET, "update": update},
            },
            {"jsonrpc": "2.0", "id": request["id"], "result": {}},
        ]))
        .await;
    update
}

async fn assert_no_resurrection_after_retirement(
    agent: &mut TestAgent,
    event_cut: usize,
    expected_retirements: usize,
) {
    let unexpected = agent.request_within(PAST_MAX_BACKOFF).await;
    assert!(
        unexpected.is_none(),
        "a retired history workflow dispatched another request: {unexpected:?}"
    );
    assert!(
        agent.recorded_events()[event_cut..].iter().all(|event| {
            event["sessionId"] != TARGET
                || !(event["type"] == "bridge/session_view"
                    || (event["type"] == "bridge/session_sync" && event["phase"] == "ready"))
        }),
        "retired workflow published another target view or ready state"
    );
    assert_eq!(
        retired_count(agent),
        expected_retirements,
        "old workflow must not publish another retirement"
    );
}

async fn manual_retirement_during_backoff(delete: bool, close_capability: bool) {
    let (mut agent, completion, load) = fork_to_first_load(IDLE_SECONDS, close_capability).await;
    enter_two_second_backoff(&mut agent, load).await;
    let command = if delete {
        "session/delete"
    } else {
        "session/close"
    };
    let retiring = agent.submit(json!({
        "type": command, "requestId": "manual-retirement", "sessionId": TARGET,
    }));
    let request = agent.next_request().await;
    if close_capability {
        assert_target_request(&request, "session/close");
        agent.reply(&request, json!({})).await;
        if delete {
            let request = agent.next_request().await;
            assert_target_request(&request, "session/delete");
            agent.reply(&request, json!({})).await;
        }
    } else {
        assert!(delete);
        assert_target_request(&request, "session/delete");
        agent.reply(&request, json!({})).await;
    }
    TestAgent::result(retiring).await.unwrap();
    let retired = agent.event("bridge/session_retired", TARGET).await;
    assert_eq!(
        retired["reason"],
        if close_capability {
            "closed"
        } else {
            "deleted"
        }
    );
    if delete {
        agent.event("acp/session_deleted", TARGET).await;
    } else {
        agent.event("acp/session_closed", TARGET).await;
    }
    // Delete with close support has two existing terminal notifications: the
    // confirmed close, then the confirmed deletion. Neither is a retry.
    let expected_reasons = if delete && close_capability {
        vec![json!("closed"), json!("deleted")]
    } else if delete {
        vec![json!("deleted")]
    } else {
        vec![json!("closed")]
    };
    assert_eq!(
        target_retirements(&mut agent)
            .iter()
            .map(|event| event["reason"].clone())
            .collect::<Vec<_>>(),
        expected_reasons
    );
    let event_cut = agent.recorded_events().len();
    assert!(
        TestAgent::result(completion).await.is_err(),
        "a fork whose target was retired must settle as cancelled or conflicted"
    );
    assert_no_resurrection_after_retirement(&mut agent, event_cut, expected_reasons.len()).await;
    agent.stop().await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_workflow_manual_close_during_backoff_stops_retries() {
    manual_retirement_during_backoff(false, true).await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_workflow_manual_delete_during_backoff_stops_retries() {
    manual_retirement_during_backoff(true, false).await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_workflow_manual_delete_closes_then_deletes_during_backoff() {
    manual_retirement_during_backoff(true, true).await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_workflow_inflight_load_keeps_manual_retirement_admission() {
    let (mut agent, completion, load) = fork_to_first_load(IDLE_SECONDS, true).await;
    for (command, request_id) in [
        ("session/close", "busy-close"),
        ("session/delete", "busy-delete"),
    ] {
        let retiring = agent.submit(json!({
            "type": command, "requestId": request_id, "sessionId": TARGET,
        }));
        let error = TestAgent::result(retiring)
            .await
            .expect_err("manual retirement cannot bypass the in-flight load admission");
        assert_eq!(error.code, Some(-32600), "{error:?}");
    }
    assert!(
        agent
            .request_within(Duration::from_millis(100))
            .await
            .is_none()
    );
    let update = finish_replay(&mut agent, &load, "history-survives-rejected-close").await;
    let forked = TestAgent::result(completion).await.unwrap();
    assert_eq!(forked["view"]["baseline"]["updates"], json!([update]));
    let idle_started = tokio::time::Instant::now();
    finish_idle_retirement(&mut agent, idle_started, 1).await;
    agent.stop().await;
}

async fn finish_idle_retirement(
    agent: &mut TestAgent,
    idle_started: tokio::time::Instant,
    total_retirements: usize,
) {
    // Callers may already have crossed the retry horizon. Use the remaining
    // interval so this checks both no premature retirement and eventual cleanup.
    let before_deadline = Duration::from_secs(IDLE_SECONDS as u64)
        .saturating_sub(idle_started.elapsed())
        .saturating_sub(Duration::from_millis(100));
    if !before_deadline.is_zero() {
        assert!(
            agent.request_within(before_deadline).await.is_none(),
            "the successor lost part of its fresh idle interval"
        );
    }
    let close = agent
        .request_within(Duration::from_secs(2))
        .await
        .expect("a finished successor must eventually become reclaimable");
    assert_target_request(&close, "session/close");
    assert!(idle_started.elapsed() >= Duration::from_secs(IDLE_SECONDS as u64));
    agent.reply(&close, json!({})).await;
    agent.event("bridge/session_retired", TARGET).await;
    assert!(agent.request_within(PAST_MAX_BACKOFF).await.is_none());
    assert_eq!(retired_count(agent), total_retirements);
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_workflow_explicit_reload_supersedes_same_incarnation_retry() {
    let (mut agent, completion, load) = fork_to_first_load(IDLE_SECONDS, true).await;
    enter_two_second_backoff(&mut agent, load).await;
    let original = view(&mut agent, TARGET).await;
    let reloaded = agent.submit(json!({
        "type": "session/load", "requestId": "explicit-reload", "sessionId": TARGET,
    }));
    let reload = agent.next_request().await;
    assert_target_request(&reload, "session/load");
    let update = finish_replay(&mut agent, &reload, "explicit-reload-baseline").await;
    TestAgent::result(reloaded).await.unwrap();
    let idle_started = tokio::time::Instant::now();

    // The replacement has already completed before the old 2s retry wakes.
    // Checking only operation/load_attempt at wake-up would miss this handoff.
    let stale_retry = agent.request_within(PAST_MAX_BACKOFF).await;
    assert!(
        stale_retry.is_none(),
        "superseded optional history retried after an explicit reload: {stale_retry:?}"
    );
    let forked = TestAgent::result(completion).await.unwrap();
    let current = view(&mut agent, TARGET).await;
    assert_eq!(
        current["session"]["incarnation"],
        original["session"]["incarnation"]
    );
    assert_eq!(current["baseline"]["updates"], json!([update]));
    assert_eq!(
        forked["view"]["baseline"]["updates"],
        current["baseline"]["updates"]
    );
    assert_eq!(current["session"]["phase"], "ready");
    finish_idle_retirement(&mut agent, idle_started, 1).await;
    agent.stop().await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_workflow_old_incarnation_cannot_finish_reopened_owner() {
    let (mut agent, completion, load) = fork_to_first_load(IDLE_SECONDS, true).await;
    enter_two_second_backoff(&mut agent, load).await;
    let original = view(&mut agent, TARGET).await;
    let closing = agent.submit(json!({
        "type": "session/close", "requestId": "close-old-owner", "sessionId": TARGET,
    }));
    let close = agent.next_request().await;
    assert_target_request(&close, "session/close");
    agent.reply(&close, json!({})).await;
    TestAgent::result(closing).await.unwrap();
    agent.event("bridge/session_retired", TARGET).await;

    let reopening = agent.submit(json!({
        "type": "session/load", "requestId": "reopen-new-owner", "sessionId": TARGET,
        "cwd": "/workspace",
    }));
    let reopen = agent.next_request().await;
    assert_target_request(&reopen, "session/load");
    assert!(TestAgent::result(completion).await.is_err());
    assert!(
        agent
            .request_within(Duration::from_secs(11))
            .await
            .is_none(),
        "the retired flow must not load or retire the new in-flight owner"
    );
    assert_eq!(retired_count(&mut agent), 1);

    let update = finish_replay(&mut agent, &reopen, "new-incarnation-baseline").await;
    TestAgent::result(reopening).await.unwrap();
    let idle_started = tokio::time::Instant::now();
    let current = view(&mut agent, TARGET).await;
    assert!(
        current["session"]["incarnation"].as_u64().unwrap()
            > original["session"]["incarnation"].as_u64().unwrap()
    );
    assert_eq!(current["baseline"]["updates"], json!([update]));
    assert_eq!(current["session"]["phase"], "ready");
    finish_idle_retirement(&mut agent, idle_started, 2).await;
    agent.stop().await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_workflow_old_fork_cannot_finish_replacement_fork_scope() {
    let (mut agent, old_completion, load) = fork_to_first_load(1, true).await;
    let old_backoff_started = enter_two_second_backoff(&mut agent, load).await;
    let original = view(&mut agent, TARGET).await;
    let closing = agent.submit(json!({
        "type": "session/close", "requestId": "close-replaced-fork", "sessionId": TARGET,
    }));
    let close = agent.next_request().await;
    assert_target_request(&close, "session/close");
    agent.reply(&close, json!({})).await;
    TestAgent::result(closing).await.unwrap();
    agent.event("bridge/session_retired", TARGET).await;

    let replacement = agent.submit(json!({
        "type": "session/fork", "requestId": "replacement-fork", "sessionId": SOURCE,
    }));
    let fork = agent.next_request().await;
    assert_eq!(fork["method"], "session/fork");
    assert_eq!(fork["params"]["sessionId"], SOURCE);
    agent
        .reply(&fork, json!({"sessionId": TARGET, "modes": modes()}))
        .await;
    let load = agent.next_request().await;
    assert_target_request(&load, "session/load");
    let replacement_backoff_started = enter_two_second_backoff(&mut agent, load).await;
    assert!(
        old_backoff_started.elapsed() < Duration::from_secs(2),
        "the replacement must enter backoff before the old retry deadline"
    );

    // Four failures take 250ms + 500ms + 1s. In the original implementation
    // the old 2s retry now finishes 250ms into the replacement's backoff. A
    // cancellation-aware implementation may finish it earlier; that is valid.
    // In either case the old result must not remove the replacement's scope.
    assert!(TestAgent::result(old_completion).await.is_err());
    catalog_barrier(&mut agent).await;
    assert_eq!(retired_count(&mut agent), 1);
    let retry = agent.next_request().await;
    assert_target_request(&retry, "session/load");
    assert!(replacement_backoff_started.elapsed() >= Duration::from_secs(2));
    assert_eq!(
        retired_count(&mut agent),
        1,
        "finishing the retired fork must not expose the new workflow to idle retirement"
    );

    let update = finish_replay(&mut agent, &retry, "replacement-fork-baseline").await;
    let replaced = TestAgent::result(replacement).await.unwrap();
    let idle_started = tokio::time::Instant::now();
    assert!(
        replaced["view"]["session"]["incarnation"].as_u64().unwrap()
            > original["session"]["incarnation"].as_u64().unwrap()
    );
    assert_eq!(replaced["view"]["baseline"]["updates"], json!([update]));
    assert!(
        agent
            .request_within(Duration::from_millis(900))
            .await
            .is_none()
    );
    let close = agent
        .request_within(Duration::from_millis(200))
        .await
        .expect("the replacement workflow must release its own protection after success");
    assert_target_request(&close, "session/close");
    assert!(idle_started.elapsed() >= Duration::from_secs(1));
    agent.reply(&close, json!({})).await;
    agent.event("bridge/session_retired", TARGET).await;
    assert!(agent.request_within(PAST_MAX_BACKOFF).await.is_none());
    assert_eq!(retired_count(&mut agent), 2);
    agent.stop().await;
}

async fn connection_stop_during_history(backoff: bool) {
    let (mut agent, completion, load) = fork_to_first_load(IDLE_SECONDS, true).await;
    if backoff {
        enter_two_second_backoff(&mut agent, load.clone()).await;
    }
    agent.cancellation.cancel();
    if !backoff {
        // Keep the RPC drain contract: stopping the connection does not mean
        // silently abandoning an outstanding Agent request. Supply its terminal
        // failure while shutdown is pending, then verify no retry is dispatched.
        reject_load(&mut agent, &load, -32603).await;
    }
    tokio::time::timeout(Duration::from_secs(3), agent.task.take().unwrap())
        .await
        .expect("connection cancellation must finish history cleanup")
        .unwrap()
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), completion)
        .await
        .expect("the accepted fork must not stay pending after connection shutdown");
    assert!(
        !matches!(result, Ok(Ok(_))),
        "connection cancellation must not publish a successful fork fallback"
    );

    tokio::time::advance(PAST_MAX_BACKOFF).await;
    assert!(agent.buffered.is_empty());
    assert!(
        agent.requests.next().await.is_none(),
        "a stopped connection dispatched another request"
    );
    assert!(agent.recorded_events().iter().all(|event| {
        !(event["type"] == "acp/session_forked" && event["response"]["sessionId"] == TARGET)
    }));
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_workflow_connection_stop_during_rpc_drains_without_retry() {
    connection_stop_during_history(false).await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_workflow_connection_stop_during_backoff_stops_retries() {
    connection_stop_during_history(true).await;
}
