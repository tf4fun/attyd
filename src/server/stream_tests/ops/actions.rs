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
