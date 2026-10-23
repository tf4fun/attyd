//! Migration step 8 (docs/state-sync.md §5): v1 is removed. A v1 read, write
//! or event stream is an unknown API route and reaches no Agent, while the
//! same reads, writes and stream through v2 still work.
use super::*;

/// Representative v1 requests, reads first, then writes, then the event
/// streams. Writes carry a JSON body, so they pass the Origin check.
fn v1_requests() -> Vec<(Method, &'static str, Option<Value>)> {
    vec![
        (Method::GET, "/api/v1/runtime", None),
        (Method::GET, "/api/v1/sessions", None),
        (Method::GET, "/api/v1/sessions/s", None),
        (Method::GET, "/api/v1/sessions/s/attachments/a", None),
        (Method::GET, "/api/v1/sessions/s/turns/o/process", None),
        (Method::GET, "/api/v1/context/search?query=x", None),
        (
            Method::POST,
            "/api/v1/sessions",
            Some(json!({ "cwd": CWD })),
        ),
        (
            Method::POST,
            "/api/v1/sessions/s/turns",
            Some(json!({ "prompt": [text("hello")] })),
        ),
        (
            Method::POST,
            "/api/v1/sessions/s/turns/o/cancel",
            Some(json!({})),
        ),
        (
            Method::POST,
            "/api/v1/sessions/s/mode",
            Some(json!({ "modeId": "m" })),
        ),
        (Method::POST, "/api/v1/sessions/s/fork", Some(json!({}))),
        (Method::POST, "/api/v1/sessions/s/close", Some(json!({}))),
        (Method::DELETE, "/api/v1/sessions/s", Some(json!({}))),
        (
            Method::POST,
            "/api/v1/sessions/s/interactions/i/response",
            Some(json!({})),
        ),
        (Method::POST, "/api/v1/auth/logout", Some(json!({}))),
        (
            Method::POST,
            "/api/v1/auth/terminal",
            Some(json!({ "methodId": "terminal-login", "cols": 80, "rows": 24 })),
        ),
        (Method::GET, "/api/v1/events", None),
        (Method::GET, "/api/v1/sessions/s/events", None),
    ]
}

#[tokio::test]
async fn v1_reads_writes_and_event_streams_are_unknown_routes_that_reach_no_agent() {
    let (bench, mut link) = Bench::ws(json!({ "loadSession": true })).await;
    bench.create_with(&mut link, "s", json!({})).await;

    for (method, path, body) in v1_requests() {
        let response = send(bench.address, method.clone(), path, body).await;
        // The status is checked before the body is read: a v1 event stream
        // that still answers never ends.
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/json",
            "{method} {path}"
        );
        let (_, body) = body_of(response).await;
        assert_eq!(
            body,
            json!({ "error": "API route not found" }),
            "{method} {path}"
        );
    }
    link.quiet(QUIET).await;

    // The same intents through v2: reads, the stream and a write that
    // reaches the Agent.
    let (status, global) = bench.read_document("global").await;
    assert_eq!(status, StatusCode::OK, "{global}");
    let (status, session) = bench.read_document("session/s").await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let mut watch = bench.watch("scope=global&scope=session%2Fs").await;
    watch
        .until("both scopes align", |watch| {
            watch.has("global") && watch.has("session/s")
        })
        .await;
    bench.create_with(&mut link, "t", json!({})).await;
}
