use super::workflow::*;
use super::*;

fn submit_turn(
    agent: &TestAgent,
    session_id: &str,
    history_revision: &str,
    intent: &str,
) -> oneshot::Receiver<Result<Value, String>> {
    let (response, result) = oneshot::channel();
    agent
        .commands
        .send(BridgeInput::TurnRequest {
            session_id: session_id.into(),
            history_revision: history_revision.into(),
            client_intent_id: intent.into(),
            prompt: vec![json!({"type": "text", "text": intent})],
            response,
        })
        .unwrap();
    result
}

async fn turn_result(result: oneshot::Receiver<Result<Value, String>>) -> Result<Value, String> {
    tokio::time::timeout(Duration::from_secs(1), result)
        .await
        .expect("turn admission must not wait for the unrelated history workflow")
        .unwrap()
}

async fn answer_prompt(agent: &mut TestAgent, request: &Value, session_id: &str, text: &str) {
    assert_eq!(request["method"], "session/prompt");
    assert_eq!(request["params"]["sessionId"], session_id);
    agent
        .send(json!([
            {
                "jsonrpc": "2.0", "method": "session/update",
                "params": {
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "agent_message_chunk", "messageId": "new-answer",
                        "content": {"type": "text", "text": text},
                    },
                },
            },
            {"jsonrpc": "2.0", "id": request["id"], "result": {"stopReason": "end_turn"}},
        ]))
        .await;
    agent.event("acp/prompt_complete", session_id).await;
}

async fn other_session_and_catalog_progress(backoff: bool) {
    let (mut agent, mut completion, first_load) = fork_to_first_load(-1, false).await;
    let held_load = if backoff {
        enter_two_second_backoff(&mut agent, first_load).await;
        None
    } else {
        Some(first_load)
    };
    let work_started = tokio::time::Instant::now();

    let creating = agent.submit(json!({
        "type": "session/new", "requestId": "other-new", "cwd": "/workspace",
    }));
    let new = agent.next_request().await;
    assert_eq!(new["method"], "session/new");
    agent
        .reply(&new, json!({"sessionId": "other-session"}))
        .await;
    let created = TestAgent::result(creating).await.unwrap();
    let admitted = turn_result(submit_turn(
        &agent,
        "other-session",
        created["view"]["session"]["historyRevision"]
            .as_str()
            .unwrap(),
        "independent-turn",
    ))
    .await
    .unwrap();
    assert_eq!(admitted["disposition"], "accepted");
    let prompt = agent.next_request().await;
    answer_prompt(
        &mut agent,
        &prompt,
        "other-session",
        "Independent work finished",
    )
    .await;
    let other = view(&mut agent, "other-session").await;
    assert!(
        other["baseline"]["updates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|update| {
                update["messageId"] == "new-answer"
                    && update["content"]["text"] == "Independent work finished"
            })
    );
    catalog_barrier(&mut agent).await;
    assert!(matches!(
        completion.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    if backoff {
        assert!(
            work_started.elapsed() < Duration::from_secs(2),
            "independent session and catalog must finish before A's retry is due"
        );
    }

    let load = match held_load {
        Some(load) => load,
        None => agent.next_request().await,
    };
    assert_eq!(load["method"], "session/load");
    assert_eq!(load["params"]["sessionId"], TARGET);
    agent.reply(&load, json!({})).await;
    let forked = TestAgent::result(completion).await.unwrap();
    assert_eq!(forked["sessionId"], TARGET);
    agent.stop().await;
}

#[tokio::test(start_paused = true)]
async fn history_workflow_inflight_load_allows_other_session_and_catalog_progress() {
    other_session_and_catalog_progress(false).await;
}

#[tokio::test(start_paused = true)]
async fn history_workflow_backoff_allows_other_session_and_catalog_progress() {
    other_session_and_catalog_progress(true).await;
}

// Drain an incorrectly dispatched old retry with a truthful replay. The failed
// implementation can then finish, letting the gate inspect lost turn boundaries
// as well as the forbidden RPC; failure never relies on a hanging request.
async fn settle_unexpected_retry(agent: &mut TestAgent, request: &Value, current: &Value) {
    assert_eq!(request["method"], "session/load");
    assert_eq!(request["params"]["sessionId"], TARGET);
    let mut messages = current["baseline"]["updates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|update| {
            json!({
                "jsonrpc": "2.0", "method": "session/update",
                "params": {"sessionId": TARGET, "update": update},
            })
        })
        .collect::<Vec<_>>();
    messages.extend(
        current["live"]["controlState"]
            .as_object()
            .unwrap()
            .values()
            .map(|update| {
                json!({
                    "jsonrpc": "2.0", "method": "session/update",
                    "params": {"sessionId": TARGET, "update": update},
                })
            }),
    );
    let mut current_modes = current["live"]["session"]["modes"].clone();
    if let Some(mode) =
        current["live"]["controlState"]["current_mode_update"]["currentModeId"].as_str()
    {
        current_modes["currentModeId"] = json!(mode);
    }
    messages.push(json!({
        "jsonrpc": "2.0", "id": request["id"],
        "result": {"modes": current_modes},
    }));
    agent.send(Value::Array(messages)).await;
}

#[tokio::test(start_paused = true)]
async fn history_workflow_completed_prompt_supersedes_backoff_without_losing_turn_boundary() {
    let (mut agent, completion, first_load) = fork_to_first_load(-1, false).await;
    let backoff_started = enter_two_second_backoff(&mut agent, first_load).await;
    let before = view(&mut agent, TARGET).await;
    let admitted = turn_result(submit_turn(
        &agent,
        TARGET,
        before["session"]["historyRevision"].as_str().unwrap(),
        "take-over-history-sync",
    ))
    .await
    .unwrap();
    assert_eq!(admitted["disposition"], "accepted");
    let prompt = agent.next_request().await;
    answer_prompt(
        &mut agent,
        &prompt,
        TARGET,
        "Accepted while history was retrying",
    )
    .await;
    let after_turn = view(&mut agent, TARGET).await;
    assert!(backoff_started.elapsed() < Duration::from_secs(2));
    assert_ne!(
        after_turn["session"]["historyRevision"],
        before["session"]["historyRevision"]
    );
    let outcomes = after_turn["session"]["turnOutcomes"].as_array().unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0]["operationId"], admitted["operationId"]);
    assert_eq!(outcomes[0]["response"]["stopReason"], "end_turn");
    assert_eq!(
        outcomes[0]["afterUpdate"],
        after_turn["baseline"]["updates"].as_array().unwrap().len()
    );
    assert!(
        after_turn["baseline"]["updates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|update| {
                update["messageId"] == "new-answer"
                    && update["content"]["text"] == "Accepted while history was retrying"
            })
    );

    let old_retry = agent.request_within(Duration::from_secs(9)).await;
    if let Some(request) = &old_retry {
        settle_unexpected_retry(&mut agent, request, &after_turn).await;
    }
    let forked = TestAgent::result(completion).await.unwrap();
    let final_view = view(&mut agent, TARGET).await;
    agent.stop().await;
    assert_eq!(forked["sessionId"], TARGET);
    assert_eq!(
        (
            old_retry.is_none(),
            final_view["baseline"].clone(),
            final_view["session"]["turnOutcomes"].clone(),
        ),
        (
            true,
            after_turn["baseline"].clone(),
            after_turn["session"]["turnOutcomes"].clone()
        ),
        "a completed successor turn must irreversibly supersede the old optional history workflow"
    );
}

#[tokio::test(start_paused = true)]
async fn history_workflow_completed_control_supersedes_backoff_without_reloading() {
    let (mut agent, completion, first_load) = fork_to_first_load(-1, false).await;
    let backoff_started = enter_two_second_backoff(&mut agent, first_load).await;
    let control = agent.submit(json!({
        "type": "session/set_mode", "requestId": "takeover-mode", "sessionId": TARGET,
        "modeId": "plan",
    }));
    let request = agent.next_request().await;
    assert_eq!(request["method"], "session/set_mode");
    assert_eq!(request["params"]["sessionId"], TARGET);
    assert_eq!(request["params"]["modeId"], "plan");
    agent.reply(&request, json!({})).await;
    TestAgent::result(control).await.unwrap();
    let after_control = view(&mut agent, TARGET).await;
    assert!(backoff_started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        after_control["live"]["controlState"]["current_mode_update"]["currentModeId"],
        "plan"
    );

    let old_retry = agent.request_within(Duration::from_secs(9)).await;
    if let Some(request) = &old_retry {
        settle_unexpected_retry(&mut agent, request, &after_control).await;
    }
    let forked = TestAgent::result(completion).await.unwrap();
    let final_view = view(&mut agent, TARGET).await;
    agent.stop().await;
    assert_eq!(forked["sessionId"], TARGET);
    assert_eq!(
        (
            old_retry.is_none(),
            final_view["baseline"].clone(),
            final_view["live"]["controlState"]["current_mode_update"].clone(),
        ),
        (
            true,
            after_control["baseline"].clone(),
            after_control["live"]["controlState"]["current_mode_update"].clone(),
        ),
        "finishing a control before backoff ends must not let the old optional workflow restart"
    );
}

#[tokio::test(start_paused = true)]
async fn history_workflow_failed_turn_cas_does_not_cancel_history_retry() {
    let (mut agent, mut completion, first_load) = fork_to_first_load(-1, false).await;
    let backoff_started = enter_two_second_backoff(&mut agent, first_load).await;
    let before = view(&mut agent, TARGET).await;
    let source = view(&mut agent, SOURCE).await;
    let stale_revision = source["session"]["historyRevision"].as_str().unwrap();
    assert_ne!(before["session"]["historyRevision"], stale_revision);
    let rejected = turn_result(submit_turn(
        &agent,
        TARGET,
        stale_revision,
        "rejected-takeover",
    ))
    .await;
    assert!(
        rejected.is_err(),
        "a stale history CAS cannot acquire a turn"
    );
    assert!(backoff_started.elapsed() < Duration::from_secs(2));
    assert!(matches!(
        completion.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));

    let retry = agent.next_request().await;
    assert_eq!(
        retry["method"], "session/load",
        "rejected prompt must not reach the Agent"
    );
    assert_eq!(retry["params"]["sessionId"], TARGET);
    let update = json!({
        "sessionUpdate": "agent_message_chunk", "messageId": "successful-retry",
        "content": {"type": "text", "text": "The original workflow still completed"},
    });
    agent.send(json!([
        {"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": TARGET, "update": update}},
        {"jsonrpc": "2.0", "id": retry["id"], "result": {}},
    ])).await;
    let forked = TestAgent::result(completion).await.unwrap();
    let final_view = view(&mut agent, TARGET).await;
    assert!(agent.request_within(Duration::from_secs(9)).await.is_none());
    agent.stop().await;
    assert_eq!(forked["sessionId"], TARGET);
    assert_eq!(
        final_view["session"]["incarnation"],
        before["session"]["incarnation"]
    );
    assert_eq!(final_view["baseline"]["updates"], json!([update]));
}
