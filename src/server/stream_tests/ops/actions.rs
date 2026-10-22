//! Interaction answers as resource actions, `POST /api/v3/scopes/{scope}/resources/
//! {interactionId}/answer` (protocol §7): one interaction at a time, at most once.
use super::*;

/// The IDs of the interactions the session's operation shows, by their request's
/// tool (the permission's `toolCallId`).
async fn pending(server: &Server, session: &str) -> BTreeMap<String, String> {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let (_, document) = server.read(&format!("session/{session}")).await;
        let interactions = document["live"]["op"]["interactions"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        if !interactions.is_empty() {
            return interactions
                .into_iter()
                .map(|(id, interaction)| {
                    let tool = interaction["request"]["toolCall"]["toolCallId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    (tool, id)
                })
                .collect();
        }
        assert!(tokio::time::Instant::now() < deadline, "no interaction is pending");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

async fn answer(server: &Server, session: &str, interaction: &str, body: Value) -> (StatusCode, Value) {
    let path = format!(
        "/api/v3/scopes/session%2F{session}/resources/{interaction}/answer"
    );
    let response = send(server.address, Method::POST, &path, Some(body)).await;
    let status = response.status();
    let bytes = axum::body::to_bytes(Body::new(response.into_body()), usize::MAX)
        .await
        .unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

#[tokio::test]
async fn a_permission_is_answered_alone_and_at_most_once() {
    let (server, mut link) = Server::start(json!({})).await;
    server.create(&mut link, "s").await;
    link.agent_request("one", "session/request_permission", permission("s", "one"))
        .await;
    link.agent_request("two", "session/request_permission", permission("s", "two"))
        .await;
    let ids = loop {
        let ids = pending(&server, "s").await;
        if ids.len() == 2 {
            break ids;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    };

    // One interaction is answered; the other stays pending.
    let (status, body) = answer(
        &server,
        "s",
        &ids["one"],
        json!({"outcome": {"outcome": "selected", "optionId": "allow"}}),
    )
    .await;
    assert_eq!((status, &body), (StatusCode::NO_CONTENT, &Value::Null));
    let response = link.response("one").await;
    assert_eq!(response["result"]["outcome"]["optionId"], "allow", "{response}");
    link.quiet(Duration::from_millis(200)).await;

    // At most once: a second answer, from anyone, finds nothing.
    let (status, body) = answer(
        &server,
        "s",
        &ids["one"],
        json!({"outcome": {"outcome": "selected", "optionId": "deny"}}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "not_found", "{body}");

    // An option the request did not offer is refused and changes nothing.
    let (status, body) = answer(
        &server,
        "s",
        &ids["two"],
        json!({"outcome": {"outcome": "selected", "optionId": "nope"}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "invalid_request", "{body}");
    link.quiet(Duration::from_millis(200)).await;

    // Cancelled is an outcome too.
    let (status, _) = answer(&server, "s", &ids["two"], json!({"outcome": {"outcome": "cancelled"}})).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let response = link.response("two").await;
    assert_eq!(response["result"]["outcome"]["outcome"], "cancelled", "{response}");
}

#[tokio::test]
async fn an_interaction_that_does_not_exist_is_not_found() {
    let (server, mut link) = Server::start(json!({})).await;
    server.create(&mut link, "s").await;
    let (status, body) = answer(
        &server,
        "s",
        "no-such-interaction",
        json!({"outcome": {"outcome": "cancelled"}}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

// -- Controls (protocol §7): set_mode and set_config, also while a prompt runs -----------------

async fn create_with_controls(server: &Server, link: &mut Link, session_id: &str) {
    let (_, global) = server.read("global").await;
    let body = json!({
        "id": "Cr3ateControlsSess-001", "expect": global["live"]["op"]["id"],
        "kind": "create", "args": {"cwd": CWD},
    });
    let address = server.address;
    let created = tokio::spawn(async move {
        send(address, Method::POST, "/api/v3/scopes/global/ops", Some(body)).await
    });
    let new = link.request("session/new").await;
    let mut response = controls();
    response["sessionId"] = json!(session_id);
    link.reply(&new, response).await;
    assert_eq!(created.await.unwrap().status(), StatusCode::ACCEPTED);
    let deadline = tokio::time::Instant::now() + WAIT;
    while server.read(&format!("session/{session_id}")).await.0 != StatusCode::OK {
        assert!(tokio::time::Instant::now() < deadline, "the session never appeared");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

async fn control(server: &Server, session: &str, action: &str, body: Value) -> (StatusCode, Value) {
    let path = format!("/api/v3/scopes/session%2F{session}/actions/{action}");
    let response = send(server.address, Method::POST, &path, Some(body)).await;
    let status = response.status();
    let bytes = axum::body::to_bytes(Body::new(response.into_body()), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// `control`, started now and answered when the Agent answers.
fn spawn_control(
    server: &Server,
    session: &str,
    action: &str,
    body: Value,
) -> tokio::task::JoinHandle<(StatusCode, Value)> {
    let address = server.address;
    let path = format!("/api/v3/scopes/session%2F{session}/actions/{action}");
    tokio::spawn(async move {
        let response = send(address, Method::POST, &path, Some(body)).await;
        let status = response.status();
        let bytes = axum::body::to_bytes(Body::new(response.into_body()), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    })
}

#[tokio::test]
async fn a_mode_is_set_when_the_agent_answers_and_the_answer_is_the_response() {
    let (server, mut link) = Server::start(json!({})).await;
    create_with_controls(&server, &mut link, "s").await;
    let set = spawn_control(&server, "s", "set_mode", json!({"modeId": "plan"}));
    let request = link.request("session/set_mode").await;
    assert_eq!(request["params"]["modeId"], "plan");
    // Held until the Agent answers.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!set.is_finished());
    link.reply(&request, json!({})).await;
    let (status, body) = set.await.unwrap();
    assert_eq!((status, &body), (StatusCode::OK, &json!({})));
    let (_, document) = server.read("session/s").await;
    assert_eq!(document["live"]["controls"]["modes"]["currentModeId"], "plan");
}

#[tokio::test]
async fn an_agent_error_is_the_response_and_changes_nothing() {
    let (server, mut link) = Server::start(json!({})).await;
    create_with_controls(&server, &mut link, "s").await;
    let set = spawn_control(&server, "s", "set_mode", json!({"modeId": "plan"}));
    let request = link.request("session/set_mode").await;
    link.refuse(&request).await;
    let (status, body) = set.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["error"]["code"].is_string(), "{body}");
    let (_, document) = server.read("session/s").await;
    assert_eq!(document["live"]["controls"]["modes"]["currentModeId"], "ask");
}

#[tokio::test]
async fn a_boolean_option_is_set_and_one_not_offered_is_refused_before_the_agent_hears() {
    let (server, mut link) = Server::start(json!({})).await;
    create_with_controls(&server, &mut link, "s").await;
    let (status, body) = control(&server, "s", "set_mode", json!({"modeId": "nope"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "invalid_request", "{body}");
    link.quiet(Duration::from_millis(200)).await;

    let set = spawn_control(&server, "s", "set_config", json!({"configId": "verbose", "value": true}));
    let request = link.request("session/set_config_option").await;
    assert_eq!(request["params"]["configId"], "verbose");
    assert_eq!(request["params"]["value"], true);
    link.reply(&request, json!({"configOptions": verbose(true)})).await;
    let (status, body) = set.await.unwrap();
    assert_eq!((status, &body), (StatusCode::OK, &json!({})));
}

#[tokio::test]
async fn a_control_runs_while_a_prompt_runs() {
    let (server, mut link) = Server::start(json!({})).await;
    create_with_controls(&server, &mut link, "s").await;
    let (_, document) = server.read("session/s").await;
    let body = json!({
        "id": "Pr0mptWhileControl-001", "expect": document["live"]["op"]["id"],
        "kind": "prompt", "args": {"prompt": [text("work")]},
    });
    let response = send(server.address, Method::POST, "/api/v3/scopes/session%2Fs/ops", Some(body)).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let prompt = link.request("session/prompt").await;

    let set = spawn_control(&server, "s", "set_mode", json!({"modeId": "plan"}));
    let request = link.request("session/set_mode").await;
    link.reply(&request, json!({})).await;
    let (status, body) = set.await.unwrap();
    assert_eq!((status, &body), (StatusCode::OK, &json!({})));

    // The prompt was not touched.
    let (_, document) = server.read("session/s").await;
    assert_eq!(document["live"]["op"]["kind"], "prompt");
    assert_eq!(document["live"]["op"]["status"], "running");
    link.reply(&prompt, json!({"stopReason": "end_turn"})).await;
}

#[tokio::test]
async fn a_control_of_a_session_that_is_not_held_is_not_found() {
    let (server, _link) = Server::start(json!({})).await;
    let (status, body) = control(&server, "nowhere", "set_mode", json!({"modeId": "plan"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}
