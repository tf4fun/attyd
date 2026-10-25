//! Closing is an admission boundary, while a retry of an accepted identity stays a retry.
use super::*;

use crate::protocol::tree_stream::{client::Client, wire::Event};

const SESSION: &str = "session/s";
const PATH: &str = "/sessions/s";
const PROMPT_ID: &str = "Zm9yLXRoZS1jbGllbnQtMQ";

/// Read the native node so assertions include the bounded operation records (`ops`), which
/// the compatibility document intentionally does not expose. No SSE observer is needed.
async fn session(bench: &Bench) -> Value {
    let (status, body) = bench.get("/api/v2/nodes/sessions/s").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let event: Event = serde_json::from_value(json!({
        "instance": body["instance"], "rev": body["rev"], "changes": [body["change"]],
    }))
    .unwrap();
    let mut client = Client::following(&[PATH]);
    client.apply(&event).unwrap();
    client.value(PATH).expect("the session is held").clone()
}

async fn until_session(bench: &Bench, condition: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(WAIT, async {
        loop {
            let current = session(bench).await;
            if condition(&current) {
                return current;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the session did not reach the expected state")
}

fn close(bench: &Bench) -> JoinHandle<(StatusCode, Value)> {
    let address = bench.address;
    tokio::spawn(async move {
        body_of(
            send(
                address,
                Method::POST,
                "/api/v2/targets/sessions/s/actions/close",
                Some(json!({})),
            )
            .await,
        )
        .await
    })
}

fn assert_write_state(current: &Value, before: &Value) {
    assert!(before["op"]["id"].is_string());
    assert!(before["ops"].is_array());
    assert!(before["entries"].is_array());
    for member in ["op", "ops", "opDetail", "entries", "prompts"] {
        assert_eq!(
            current[member], before[member],
            "{member} changed without a new admission"
        );
    }
}

#[tokio::test]
async fn a_prompt_during_close_is_refused_without_admission_and_can_start_after_close_fails() {
    let (bench, mut link) = Bench::ws(json!({"sessionCapabilities": {"close": {}}})).await;
    bench.create_with(&mut link, "s", json!({})).await;
    let before = session(&bench).await;
    let expect = before["op"]["id"].clone();
    let args = json!({"prompt": [text("after close refused")]});

    let closing = close(&bench);
    let request = link.request("session/close").await;
    assert_eq!(request["params"]["sessionId"], "s");
    let held = until_session(&bench, |session| session["closing"] == true).await;
    assert_write_state(&held, &before);
    assert!(!closing.is_finished(), "the Agent has not answered close");

    let (status, body) = bench
        .op_with_id(SESSION, &expect, PROMPT_ID, "prompt", args.clone())
        .await;
    not_allowed(status, &body, "a new prompt while close is in flight");
    assert_write_state(&session(&bench).await, &held);
    link.quiet(QUIET).await;
    assert_write_state(&session(&bench).await, &held);

    link.refuse(&request).await;
    let (status, body) = closing.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["error"]["code"], "agent_error", "{body}");
    let usable = until_session(&bench, |session| session["closing"] == false).await;
    assert_write_state(&usable, &before);

    // The refused write installed neither its identity nor a failed op: the exact same
    // identity, expect and input can now be admitted and must actually reach the Agent.
    let (status, body) = bench
        .op_with_id(SESSION, &expect, PROMPT_ID, "prompt", args)
        .await;
    assert_eq!(
        assert_accepted(status, &body, "after close failed"),
        PROMPT_ID
    );
    let prompt = link.request("session/prompt").await;
    assert_eq!(
        prompt["params"]["prompt"],
        json!([text("after close refused")])
    );
    link.update(
        "s",
        json!({
            "sessionUpdate": "agent_message_chunk", "messageId": "reply",
            "content": text("ok"),
        }),
    )
    .await;
    link.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    let ended = until_session(&bench, |session| {
        session["op"]["id"] == PROMPT_ID && session["op"]["status"] == "completed"
    })
    .await;
    assert_eq!(ended["entries"].as_array().unwrap().len(), 2);
    assert_eq!(ended["entries"][0]["role"], "user");
    assert_eq!(
        ended["entries"][0]["chunks"][0]["content"],
        text("after close refused")
    );
    assert_eq!(ended["entries"][1]["chunks"][0]["content"], text("ok"));
    assert_eq!(
        ended["ops"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|op| op["id"] == PROMPT_ID)
            .count(),
        1
    );
    link.quiet(QUIET).await;
    bench.hub.shutdown().await;
}

#[tokio::test]
async fn an_accepted_prompt_identity_stays_idempotent_during_close() {
    for already_completed in [false, true] {
        let (bench, mut link) = Bench::ws(json!({"sessionCapabilities": {"close": {}}})).await;
        bench.create_with(&mut link, "s", json!({})).await;
        let expect = bench.expect_of(SESSION).await;
        let args = json!({"prompt": [text("once")]});
        let (status, body) = bench
            .op_with_id(SESSION, &expect, PROMPT_ID, "prompt", args.clone())
            .await;
        assert_eq!(
            assert_accepted(status, &body, "the original prompt"),
            PROMPT_ID
        );
        let prompt = link.request("session/prompt").await;
        if already_completed {
            link.reply(&prompt, json!({"stopReason": "end_turn"})).await;
            until_session(&bench, |session| session["op"]["status"] == "completed").await;
        }

        let closing = close(&bench);
        let request = link.request("session/close").await;
        let held = until_session(&bench, |session| session["closing"] == true).await;
        assert_eq!(held["op"]["id"], PROMPT_ID);
        assert!(!closing.is_finished());
        // The original expect is now stale, but a repeated accepted id is recognized
        // before both the expect check and the new closing admission guard.
        let (status, body) = bench
            .op_with_id(SESSION, &expect, PROMPT_ID, "prompt", args)
            .await;
        assert_eq!(
            assert_accepted(status, &body, "the retry while closing"),
            PROMPT_ID
        );
        assert_write_state(&session(&bench).await, &held);
        link.quiet(QUIET).await;
        assert_write_state(&session(&bench).await, &held);

        link.refuse(&request).await;
        let (status, body) = closing.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["error"]["code"], "agent_error", "{body}");
        until_session(&bench, |session| session["closing"] == false).await;
        if !already_completed {
            link.reply(&prompt, json!({"stopReason": "end_turn"})).await;
        }
        let ended = until_session(&bench, |session| {
            session["op"]["id"] == PROMPT_ID && session["op"]["status"] == "completed"
        })
        .await;
        assert_eq!(ended["entries"].as_array().unwrap().len(), 1);
        assert_eq!(ended["entries"][0]["chunks"][0]["content"], text("once"));
        assert_eq!(
            ended["ops"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|op| op["id"] == PROMPT_ID)
                .count(),
            1
        );
        link.quiet(QUIET).await;
        bench.hub.shutdown().await;
    }
}
