//! Asking the Agent to close cancels permissions; elicitation keeps its actual lifetime.
use super::*;

const SESSION: &str = "session/s";

async fn until_live(bench: &Bench, condition: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(WAIT, async {
        loop {
            let live = bench.document(SESSION).await["live"].clone();
            if condition(&live) {
                return live;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the session interactions did not settle")
}

async fn ask_forms(link: &mut Link, parent: &Value, suffix: &str) {
    link.agent_request(
        &format!("session-{suffix}"),
        "elicitation/create",
        form(json!({"sessionId": "s"}), &format!("session-{suffix}")),
    )
    .await;
    link.agent_request(
        &format!("request-{suffix}"),
        "elicitation/create",
        form(
            json!({"requestId": parent["id"]}),
            &format!("request-{suffix}"),
        ),
    )
    .await;
}

#[tokio::test]
async fn a_failed_close_preserves_elicitation_until_answer_parent_end_or_successful_close() {
    let (bench, mut link) = Bench::ws(json!({"sessionCapabilities": {"close": {}}})).await;
    bench.create_with(&mut link, "s", json!({})).await;
    let expect = bench.expect_of(SESSION).await;
    let (status, body) = bench
        .op(
            SESSION,
            &expect,
            "prompt",
            json!({"prompt": [text("work")]}),
        )
        .await;
    let op = assert_accepted(status, &body, "the parent prompt");
    let prompt = link.request("session/prompt").await;

    ask_forms(&mut link, &prompt, "answer").await;
    link.agent_request(
        "permission",
        "session/request_permission",
        permission("s", "tool"),
    )
    .await;
    let pending = until_live(&bench, |live| interaction_ids(live).len() == 3).await;
    let session_form = interaction_for(&pending, "session-answer");
    let request_form = interaction_for(&pending, "request-answer");
    let permission = interaction_for(&pending, "tool");

    let closing = bench.spawn_action("s", "close", json!({}));
    // Inspect every ACP message at this boundary, so a premature elicitation cancel cannot
    // be skipped by a helper waiting for the close request or the permission's response.
    let mut permission_cancelled = false;
    let mut close = None;
    for _ in 0..2 {
        let message = link
            .next(WAIT)
            .await
            .expect("close and permission cancellation reach the Agent");
        if message["method"] == "session/close" {
            assert!(close.replace(message).is_none(), "close was sent twice");
        } else {
            assert_eq!(
                message["id"], "permission",
                "elicitation must not be cancelled by close admission: {message}"
            );
            assert!(!permission_cancelled, "the permission was answered twice");
            assert_eq!(
                message["result"],
                json!({"outcome": {"outcome": "cancelled"}})
            );
            permission_cancelled = true;
        }
    }
    assert!(permission_cancelled);
    let close = close.expect("session/close was sent");
    assert_eq!(close["params"]["sessionId"], "s");
    assert!(
        !closing.is_finished(),
        "the close response is still withheld"
    );
    let held = until_live(&bench, |live| live["closing"] == true).await;
    assert_eq!(interaction_ids(&held).len(), 2);
    for id in [&session_form, &request_form] {
        assert_eq!(held["interactions"][id], pending["interactions"][id]);
    }
    assert!(held["interactions"].get(&permission).is_none());
    assert_eq!(held["op"]["id"], op);
    assert_eq!(held["op"]["status"], "running");
    link.quiet(QUIET).await;

    link.refuse(&close).await;
    let (status, body) = closing.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["error"]["code"], "agent_error", "{body}");
    let usable = until_live(&bench, |live| live["closing"] == false).await;
    assert_eq!(usable["interactions"], held["interactions"]);
    assert_eq!(
        usable["op"]["status"], "running",
        "the elicitation parent is still pending"
    );
    for id in [&session_form, &request_form] {
        let (status, body) = bench.act(SESSION, id, "answer", accept()).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
        let (status, body) = bench.act(SESSION, id, "answer", accept()).await;
        not_found(status, &body, "a form answered once after close failed");
    }
    let responses = link.responses(&["session-answer", "request-answer"]).await;
    for response in responses.values() {
        assert_eq!(
            response["result"],
            json!({"action": "accept", "content": {}})
        );
    }
    let (status, body) = bench.act(SESSION, &permission, "answer", allow()).await;
    not_found(status, &body, "the permission already cancelled by close");
    until_live(&bench, |live| interaction_ids(live).is_empty()).await;
    link.quiet(QUIET).await;

    // Retaining elicitation at close admission must not retain it past its real lifetime.
    // A second pair stays unanswered: the request-scoped one ends with the still-running
    // parent; the session-scoped one survives that boundary and ends with a confirmed close.
    ask_forms(&mut link, &prompt, "pending").await;
    let pending = until_live(&bench, |live| interaction_ids(live).len() == 2).await;
    let session_form = interaction_for(&pending, "session-pending");
    let request_form = interaction_for(&pending, "request-pending");
    link.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    let response = link.response("request-pending").await;
    assert_eq!(response["result"], json!({"action": "cancel"}));
    let ended = until_live(&bench, |live| {
        live["op"]["status"] == "completed" && interaction_ids(live).len() == 1
    })
    .await;
    assert_eq!(
        ended["interactions"][&session_form],
        pending["interactions"][&session_form]
    );
    assert!(ended["interactions"].get(&request_form).is_none());
    let (status, body) = bench.act(SESSION, &request_form, "answer", accept()).await;
    not_found(status, &body, "the form whose parent request ended");
    link.quiet(QUIET).await;

    let closing = bench.spawn_action("s", "close", json!({}));
    let close = link.request("session/close").await;
    let held = until_live(&bench, |live| live["closing"] == true).await;
    assert_eq!(
        held["interactions"][&session_form],
        pending["interactions"][&session_form]
    );
    link.quiet(QUIET).await;
    link.reply(&close, json!({})).await;
    let (status, body) = closing.await.unwrap();
    assert_eq!((status, body), (StatusCode::OK, json!({})));
    let response = link.response("session-pending").await;
    assert_eq!(response["result"], json!({"action": "cancel"}));
    let (status, body) = bench.act(SESSION, &session_form, "answer", accept()).await;
    not_found(status, &body, "the form whose session closed");
    link.quiet(QUIET).await;
    bench.hub.shutdown().await;
}
