//! Shared protocol drivers for the W1–W8 workflow regression gates. These only
//! use the same business/observation inputs and ACP messages as real clients.

use super::*;

pub(super) const SOURCE: &str = "workflow-source";
pub(super) const TARGET: &str = "workflow-target";
pub(super) type Completion = oneshot::Receiver<Result<Value, BridgeRequestError>>;

pub(super) fn modes() -> Value {
    json!({
        "currentModeId": "build",
        "availableModes": [{"id": "build", "name": "Build"}, {"id": "plan", "name": "Plan"}],
    })
}

pub(super) fn source_update() -> Value {
    json!({
        "sessionUpdate": "agent_message_chunk", "messageId": "source-context",
        "content": {"type": "text", "text": "Source context"},
    })
}

pub(super) async fn view(agent: &mut TestAgent, session_id: &str) -> Value {
    let (response, result) = oneshot::channel();
    agent
        .commands
        .send(BridgeInput::SessionViewRequest {
            session_id: session_id.into(),
            cwd: None,
            expected_owner: None,
            response,
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), result)
        .await
        .expect("materialized session view must remain readable")
        .unwrap()
        .unwrap()
}

pub(super) async fn observe(
    agent: &mut TestAgent,
    session_id: &str,
    observer_id: u64,
) -> (ObservationLease, Value) {
    let lease = ObservationLease::new();
    let (reply, observed) = oneshot::channel();
    agent
        .commands
        .send(BridgeInput::ObserveSession {
            session_id: session_id.into(),
            cwd: None,
            expected_owner: None,
            observer_id,
            lease: lease.clone(),
            reply,
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), observed)
        .await
        .expect("materialized session observation must settle")
        .unwrap()
        .unwrap();
    (lease, view(agent, session_id).await)
}

pub(super) async fn fork_to_first_load(
    timeout_seconds: i64,
    supports_close: bool,
) -> (TestAgent, Completion, Value) {
    let mut capabilities = json!({
        "loadSession": true,
        "sessionCapabilities": {"fork": {}, "list": {}, "delete": {}},
    });
    if supports_close {
        capabilities["sessionCapabilities"]["close"] = json!({});
    }
    let mut agent = TestAgent::start(timeout_seconds, capabilities).await;
    let creating = agent.submit(json!({
        "type": "session/new", "requestId": "workflow-new", "cwd": "/workspace",
    }));
    let request = agent.next_request().await;
    assert_eq!(request["method"], "session/new");
    agent
        .reply(&request, json!({"sessionId": SOURCE, "modes": modes()}))
        .await;
    TestAgent::result(creating).await.unwrap();
    agent
        .send(
            json!({"jsonrpc": "2.0", "method": "session/update", "params": {
                "sessionId": SOURCE, "update": source_update(),
            }}),
        )
        .await;
    loop {
        let published = agent.event("bridge/session_view", SOURCE).await;
        if published["view"]["baseline"]["updates"] == json!([source_update()]) {
            break;
        }
    }
    // Source observation remains alive throughout each target-specific test.
    let (_, source) = observe(&mut agent, SOURCE, 1).await;
    assert_eq!(source["baseline"]["updates"], json!([source_update()]));

    let completion = agent.submit(json!({
        "type": "session/fork", "requestId": "workflow-fork", "sessionId": SOURCE,
    }));
    let request = agent.next_request().await;
    assert_eq!(request["method"], "session/fork");
    agent
        .reply(&request, json!({"sessionId": TARGET, "modes": modes()}))
        .await;
    let load = agent.next_request().await;
    assert_eq!(load["method"], "session/load");
    assert_eq!(load["params"]["sessionId"], TARGET);
    (agent, completion, load)
}

pub(super) async fn reject_load(agent: &mut TestAgent, request: &Value, code: i32) {
    agent
        .send(json!({
            "jsonrpc": "2.0", "id": request["id"],
            "error": {"code": code, "message": "History unavailable for this attempt"},
        }))
        .await;
}

pub(super) async fn enter_two_second_backoff(
    agent: &mut TestAgent,
    mut load: Value,
) -> tokio::time::Instant {
    for backoff_ms in [250, 500, 1000, 2000] {
        assert_eq!(load["method"], "session/load");
        assert_eq!(load["params"]["sessionId"], TARGET);
        reject_load(agent, &load, -32603).await;
        loop {
            let event = agent.event("bridge/session_sync", TARGET).await;
            if event["phase"] == "retrying" {
                assert_eq!(event["retryAfterMs"], backoff_ms);
                break;
            }
        }
        if backoff_ms != 2000 {
            let previous_id = load["id"].clone();
            load = agent.next_request().await;
            assert_ne!(
                load["id"], previous_id,
                "each attempt must have a fresh wire identity"
            );
        }
    }
    tokio::time::Instant::now()
}

pub(super) async fn catalog_barrier(agent: &mut TestAgent) -> Value {
    let listing = agent.submit(json!({
        "type": "session/list", "requestId": format!("workflow-list-{}", Uuid::new_v4()),
    }));
    let list = agent.next_request().await;
    assert_eq!(list["method"], "session/list");
    agent
        .reply(
            &list,
            json!({"sessions": [
                {"sessionId": SOURCE, "cwd": "/workspace"},
                {"sessionId": TARGET, "cwd": "/workspace"},
            ]}),
        )
        .await;
    TestAgent::result(listing).await.unwrap()
}

pub(super) fn target_retirements(agent: &mut TestAgent) -> Vec<Value> {
    agent
        .recorded_events()
        .iter()
        .filter(|event| event["type"] == "bridge/session_retired" && event["sessionId"] == TARGET)
        .cloned()
        .collect()
}
