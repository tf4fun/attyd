use super::*;

async fn creation_replay_after_retirement(
    late_retired_update: bool,
) -> (Value, Value, Value, Value) {
    let mut agent = TestAgent::start(0, json!({})).await;
    let first = agent.submit(json!({
        "type": "session/new", "requestId": "create-old", "cwd": "/repo",
    }));
    let request = agent.next_request().await;
    assert_eq!(request["method"], "session/new");
    agent.reply(&request, json!({"sessionId": "old"})).await;
    TestAgent::result(first).await.unwrap();
    let retired = agent.event("bridge/session_retired", "old").await;
    assert_eq!(retired["reason"], "unobserved");

    let creating = agent.submit(json!({
        "type": "session/new", "requestId": "create-new", "cwd": "/repo",
    }));
    let request = agent.next_request().await;
    assert_eq!(
        request["method"], "session/new",
        "local retirement must not send an unsupported close RPC"
    );
    let controls = json!({
        "sessionUpdate": "available_commands_update",
        "availableCommands": [{"name": "new-command", "description": "For the new session"}],
    });
    let early = json!([
        {
            "sessionId": "new",
            "update": {
                "sessionUpdate": "agent_message_chunk", "messageId": "welcome",
                "content": {"type": "text", "text": "Welcome to the new session"},
            },
        },
        {"sessionId": "new", "update": controls},
    ]);
    let mut batch = Vec::new();
    if late_retired_update {
        // The Agent still owns locally retired sessions and can update their
        // available commands while an unrelated new-session RPC is pending.
        batch.push(json!({
            "jsonrpc": "2.0", "method": "session/update",
            "params": {
                "sessionId": "old",
                "update": {
                    "sessionUpdate": "available_commands_update",
                    "availableCommands": [{"name": "old-command", "description": "Old session"}],
                },
            },
        }));
    }
    batch.extend(early.as_array().unwrap().iter().map(|notification| {
        json!({"jsonrpc": "2.0", "method": "session/update", "params": notification})
    }));
    batch.push(json!({
        "jsonrpc": "2.0", "id": request["id"], "result": {"sessionId": "new"},
    }));
    agent.send(Value::Array(batch)).await;
    let created = TestAgent::result(creating).await.unwrap();
    let event = agent.event("acp/session_created", "new").await;
    agent.stop().await;
    (created, event, controls, early)
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_late_update_preserves_other_sessions_creation_replay() {
    let (created, event, controls, early) = creation_replay_after_retirement(true).await;
    assert_eq!(
        (
            created["view"]["live"]["controlState"]["available_commands_update"].clone(),
            event["earlyUpdates"].clone(),
        ),
        (controls, early),
        "retired-session updates must not occupy another session's creation replay slot"
    );
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_preserves_new_session_early_replay_without_late_update() {
    let (created, event, controls, early) = creation_replay_after_retirement(false).await;
    assert_eq!(
        (
            created["view"]["live"]["controlState"]["available_commands_update"].clone(),
            event["earlyUpdates"].clone(),
        ),
        (controls, early),
        "new-session replay must preserve control state and message identity, text, and order"
    );
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_allows_same_session_id_to_reopen_with_authoritative_replay() {
    let mut agent = TestAgent::start(0, json!({"loadSession": true})).await;
    let creating = agent.submit(json!({
        "type": "session/new", "requestId": "create-saved", "cwd": "/repo",
    }));
    let request = agent.next_request().await;
    assert_eq!(request["method"], "session/new");
    agent.reply(&request, json!({"sessionId": "saved"})).await;
    let created = TestAgent::result(creating).await.unwrap();
    agent.event("bridge/session_retired", "saved").await;

    let (response, view) = oneshot::channel();
    agent
        .commands
        .send(BridgeInput::SessionViewRequest {
            session_id: "saved".into(),
            cwd: Some("/repo".into()),
            expected_owner: None,
            response,
        })
        .unwrap();
    let request = agent.next_request().await;
    assert_eq!(request["method"], "session/load");
    assert_eq!(request["params"]["sessionId"], "saved");
    let update = json!({
        "sessionUpdate": "agent_message_chunk", "messageId": "saved-history",
        "content": {"type": "text", "text": "Restored from Agent history"},
    });
    agent
        .send(json!([
            {
                "jsonrpc": "2.0", "method": "session/update",
                "params": {"sessionId": "saved", "update": update},
            },
            {"jsonrpc": "2.0", "id": request["id"], "result": {}},
        ]))
        .await;
    let view = tokio::time::timeout(Duration::from_secs(3), view)
        .await
        .expect("reopening must finish")
        .unwrap()
        .unwrap();
    agent.stop().await;

    assert_eq!(view["session"]["sessionId"], "saved");
    assert!(
        view["session"]["incarnation"].as_u64().unwrap()
            > created["view"]["session"]["incarnation"].as_u64().unwrap(),
        "reopening must allocate a new owner"
    );
    assert_eq!(view["baseline"]["updates"], json!([update]));
}
