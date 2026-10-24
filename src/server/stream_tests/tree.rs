//! `GET /api/v2/stream?follow=…` over real HTTP (protocol revision 4 §6): path streams of the
//! state tree beside the scope streams.

use super::feed::{event, open};
use super::*;

/// The next event that has `changes` or `check`, as a whole.
async fn next(sse: &mut Sse) -> Value {
    event(sse, WAIT).await.expect("an event")
}

/// The change of `path` in `event`.
fn change<'a>(event: &'a Value, path: &str) -> &'a Value {
    event["changes"]
        .as_array()
        .unwrap_or_else(|| panic!("no changes in {event}"))
        .iter()
        .find(|change| change["path"] == path)
        .unwrap_or_else(|| panic!("no change of {path} in {event}"))
}

#[tokio::test]
async fn a_stream_starts_with_the_instance_and_a_snapshot_of_every_path_it_follows() {
    let (server, _link) = Server::start(json!({})).await;
    let (status, mut sse) = open(&server, "follow=/global&follow=/catalog").await;
    assert_eq!(status, StatusCode::OK);
    let first = next(&mut sse).await;
    assert!(first["instance"].as_str().is_some_and(|instance| instance.len() == 22), "{first}");
    assert!(first["rev"].as_u64().unwrap() >= 1, "{first}");
    let global = &change(&first, "/global")["snapshot"];
    assert_eq!(global["base"], Value::Null);
    assert_eq!(global["node"]["v"]["agent"]["phase"], "ready", "{global}");
    assert_eq!(global["hash"].as_str().unwrap().len(), 22);
    assert!(change(&first, "/catalog")["snapshot"]["node"]["v"]["pages"].is_array());
}

#[tokio::test]
async fn a_stream_with_a_query_that_is_not_one_is_refused() {
    let (server, _link) = Server::start(json!({})).await;
    for query in [
        "follow=global",
        "follow=/global&follow=/global",
        "follow=/sessions/s&follow=/sessions/s/op",
        "follow=/global&have=/global:1",
        "follow=/global&window=/global:10",
        "follow=/global&cwd=s:/w",
        "follow=/global&bogus=1",
    ] {
        let (status, _) = open(&server, query).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query:?}");
    }
}

#[tokio::test]
async fn a_session_path_is_followed_through_its_changes_and_verified_by_a_check() {
    let (server, mut link) = Server::start(json!({})).await;
    server.create(&mut link, "s1").await;
    let path = "/sessions/s1/entries";
    let (_, mut sse) = open(&server, &format!("follow={path}")).await;
    let first = next(&mut sse).await;
    let snapshot = &change(&first, path)["snapshot"];
    assert_eq!(snapshot["node"]["v"], json!([]));
    let held = snapshot["at"].as_u64().unwrap();

    link.update(
        "s1",
        json!({"sessionUpdate": "agent_message_chunk", "messageId": "m", "content": text("hi")}),
    )
    .await;
    let changed = next(&mut sse).await;
    assert!(changed.get("instance").is_none());
    let ops = change(&changed, path);
    assert_eq!(ops["from"], held, "{changed}");
    assert_eq!(ops["ops"][0]["op"], "append");
    assert_eq!(ops["ops"][0]["path"], "");

    // The changes stopped: the check follows after the pause, of the version just applied.
    let check = next(&mut sse).await;
    assert_eq!(check["check"][0]["path"], path, "{check}");
    assert_eq!(check["check"][0]["at"], changed["rev"]);
    assert_eq!(check["check"][0]["hash"].as_str().unwrap().len(), 22);
}

#[tokio::test]
async fn a_stream_loads_a_session_the_bridge_does_not_hold_with_the_cwd_it_gave() {
    let (server, mut link) = Server::start(json!({"loadSession": true})).await;
    let (status, mut sse) = open(
        &server,
        &format!(
            "follow=/sessions/saved/entries&cwd={}",
            query_scope(&format!("saved:{CWD}"))
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let first = next(&mut sse).await;
    assert_eq!(change(&first, "/sessions/saved/entries")["end"]["reason"], "absent");
    let load = link.serve_load("saved", &turns(&[("hello", "hi there")])).await;
    assert_eq!(load["params"]["cwd"], CWD);

    // The session is installed and its history replayed: the stream follows it from a snapshot.
    let deadline = tokio::time::Instant::now() + WAIT;
    let mut entries = 0;
    while entries < 2 {
        assert!(tokio::time::Instant::now() < deadline, "never loaded");
        let event = next(&mut sse).await;
        if let Some(change) = event["changes"].as_array().and_then(|changes| changes.first())
            && let Some(node) = change["snapshot"]["node"].get("v")
        {
            entries = node.as_array().map_or(0, Vec::len);
        }
    }
}

#[tokio::test]
async fn a_stream_that_gave_no_cwd_for_an_unheld_session_is_ended_for_it() {
    let (server, mut link) = Server::start(json!({"loadSession": true})).await;
    let (_, mut sse) = open(&server, "follow=/global&follow=/sessions/saved/op").await;
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        assert!(tokio::time::Instant::now() < deadline, "no end");
        let event = next(&mut sse).await;
        let Some(changes) = event["changes"].as_array() else { continue };
        if let Some(end) = changes.iter().find(|c| c["path"] == "/sessions/saved/op" && c["end"]["reason"] == "load_failed") {
            assert_eq!(end["end"]["error"]["code"], "invalid_request", "{end}");
            break;
        }
    }
    link.quiet(Duration::from_millis(300)).await;
}

#[tokio::test]
async fn a_load_the_agent_refuses_ends_the_path_with_the_error_summary() {
    let (server, mut link) = Server::start(json!({"loadSession": true})).await;
    let (_, mut sse) = open(
        &server,
        &format!("follow=/sessions/saved/op&cwd={}", query_scope(&format!("saved:{CWD}"))),
    )
    .await;
    let load = link.request("session/load").await;
    link.send(json!({
        "jsonrpc": "2.0", "id": load["id"],
        "error": {"code": -32000, "message": "authentication required"},
    }))
    .await;
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        assert!(tokio::time::Instant::now() < deadline, "no end");
        let event = next(&mut sse).await;
        let Some(changes) = event["changes"].as_array() else { continue };
        if let Some(end) = changes.iter().find(|c| c["end"]["reason"] == "load_failed") {
            assert_eq!(end["end"]["error"], json!({"code": "agent_error", "agentCode": -32000}), "{end}");
            break;
        }
    }
}

#[tokio::test]
async fn shutting_the_server_down_ends_the_path_streams_after_what_is_queued() {
    let (server, _link) = Server::start(json!({})).await;
    let (_, mut sse) = open(&server, "follow=/global").await;
    assert!(event(&mut sse, WAIT).await.is_some());
    server.hub.shutdown().await;
    while event(&mut sse, WAIT).await.is_some() {}
    assert!(sse.message(WAIT).await.is_none(), "the stream ended");
}

// --- segments (design §7.1) -----------------------------------------------------------

/// `GET path`: the status and the JSON body.
async fn read(server: &Server, path: &str) -> (StatusCode, Value) {
    let response = send(server.address, Method::GET, path, None).await;
    (response.status(), json_of(response).await)
}

#[tokio::test]
async fn a_device_pages_through_a_long_message_at_the_version_it_holds() {
    let (server, mut link) = Server::start(json!({})).await;
    server.create(&mut link, "s1").await;
    let long = format!("{}{}", "a".repeat(5000), "é".repeat(2000));
    link.update(
        "s1",
        json!({"sessionUpdate": "agent_message_chunk", "messageId": "m", "content": text(&long)}),
    )
    .await;
    let path = "/sessions/s1/entries";
    let deadline = tokio::time::Instant::now() + WAIT;
    let (rev, at) = loop {
        let (_, mut sse) = open(&server, &format!("follow={path}")).await;
        let first = next(&mut sse).await;
        let snapshot = &change(&first, path)["snapshot"];
        if snapshot["node"]["v"].as_array().is_some_and(|entries| !entries.is_empty()) {
            break (first["rev"].clone(), snapshot["at"].clone());
        }
        assert!(tokio::time::Instant::now() < deadline, "the message never arrived");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let _ = rev;
    let base = "/api/v2/nodes/sessions/s1/entries/0/chunks/0/content/text/segments";
    let mut paged = String::new();
    for index in 0..3 {
        let (status, body) = read(&server, &format!("{base}/{index}?at={at}")).await;
        assert_eq!(status, StatusCode::OK, "{index}: {body}");
        assert_eq!(body["before"].as_str().unwrap().len(), 43);
        paged.push_str(body["segment"].as_str().unwrap());
    }
    assert_eq!(paged, long);
    let (status, body) = read(&server, &format!("{base}/3?at={at}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn a_segment_read_needs_a_version_and_a_text_position() {
    let (server, mut link) = Server::start(json!({})).await;
    server.create(&mut link, "s1").await;
    for (path, status) in [
        ("/api/v2/nodes/sessions/s1/entries/0/chunks/0/content/text/segments/0", StatusCode::BAD_REQUEST),
        ("/api/v2/nodes/sessions/s1/entries/0/chunks/0/content/text/segments/x?at=1", StatusCode::BAD_REQUEST),
        ("/api/v2/nodes/sessions/s1/entries/0/chunks/0/content/text/segments/0?at=999999", StatusCode::BAD_REQUEST),
        ("/api/v2/nodes/sessions/s1/entries/0/chunks/0/content/text/segments/0?at=1&at=2", StatusCode::BAD_REQUEST),
        ("/api/v2/nodes/sessions/s1/controls/segments/0?at=1", StatusCode::NOT_FOUND),
        ("/api/v2/nodes/global", StatusCode::NOT_FOUND),
    ] {
        let (got, body) = read(&server, path).await;
        assert_eq!(got, status, "{path}: {body}");
    }
}

// --- writes to targets and the outcome of a lost answer (design §8, §9.3) ---------------

/// A client-chosen operation id: 22 characters of base64url.
const CREATE: &str = "Cr3ateOperation_Id-001";
const OTHER: &str = "Other0peration_Id-002";

/// What a client holds of `global` when it writes: the instance, the version and the `op.id`.
async fn global_held(server: &Server) -> (String, u64, String) {
    let (_, mut sse) = open(server, "follow=/global").await;
    let first = next(&mut sse).await;
    let snapshot = &change(&first, "/global")["snapshot"];
    (
        first["instance"].as_str().unwrap().to_string(),
        snapshot["at"].as_u64().unwrap(),
        snapshot["node"]["v"]["op"]["id"].as_str().unwrap().to_string(),
    )
}

async fn post(server: &Server, path: &str, body: Value) -> (StatusCode, Value) {
    let response = send(server.address, Method::POST, path, Some(body)).await;
    (response.status(), json_of(response).await)
}

fn resolution(id: &str, expect: &str, instance: &str, at: u64) -> String {
    format!("/api/v2/targets/global/ops/{id}?expect={expect}&instance={instance}&at={at}")
}

#[tokio::test]
async fn a_write_to_a_target_is_admitted_and_its_outcome_can_be_asked_for() {
    let (server, mut link) = Server::start(json!({})).await;
    let (instance, at, e) = global_held(&server).await;

    // Not admitted yet: the request is open.
    let (status, body) = read(&server, &resolution(CREATE, &e, &instance, at)).await;
    assert_eq!((status, &body), (StatusCode::OK, &json!({ "outcome": "open" })));

    let write = json!({"id": CREATE, "expect": e, "kind": "create", "args": {"cwd": CWD}});
    let (status, body) = post(&server, "/api/v2/targets/global/ops", write.clone()).await;
    assert_eq!((status, &body), (StatusCode::ACCEPTED, &json!({"op": CREATE})));
    // A repeat of the same request is the same operation.
    let (status, body) = post(&server, "/api/v2/targets/global/ops", write).await;
    assert_eq!((status, &body), (StatusCode::ACCEPTED, &json!({"op": CREATE})));

    let new = link.request("session/new").await;
    link.reply(&new, json!({"sessionId": "s1"})).await;

    let (_, admitted) = read(&server, &resolution(CREATE, &e, &instance, at)).await;
    assert_eq!(admitted["outcome"], "admitted", "{admitted}");
    assert_eq!(admitted["record"]["id"], CREATE);
    assert_eq!(admitted["record"]["after"], e);
    assert_eq!(admitted["record"]["kind"], "create");
    // Another request for the same `e` can never be admitted now.
    let (_, superseded) = read(&server, &resolution(OTHER, &e, &instance, at)).await;
    assert_eq!(superseded, json!({ "outcome": "superseded" }));
    link.quiet(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn a_request_of_another_instance_is_unknown_restarted_and_bad_reads_are_refused() {
    let (server, _link) = Server::start(json!({})).await;
    let (instance, at, e) = global_held(&server).await;
    let (status, body) = read(&server, &resolution(CREATE, &e, "AnotherInstance0000000", at)).await;
    assert_eq!((status, &body), (StatusCode::OK, &json!({ "outcome": "unknown", "reason": "restarted" })));
    let (status, body) = read(&server, "/api/v2/targets/sessions/ghost/ops/x?expect=e&instance=I&at=1").await;
    assert_eq!((status, &body), (StatusCode::OK, &json!({ "outcome": "unknown", "reason": "restarted" })));
    let (status, body) = read(&server, &resolution("x", &e, &instance, at).replace("sessions", "sessions")).await;
    assert_eq!((status, body["outcome"].as_str()), (StatusCode::OK, Some("open")));
    for (path, expected) in [
        (format!("/api/v2/targets/global/ops/x?instance={instance}&at={at}"), StatusCode::BAD_REQUEST),
        (format!("/api/v2/targets/global/ops/x?expect={e}&at={at}"), StatusCode::BAD_REQUEST),
        (format!("/api/v2/targets/global/ops/x?expect={e}&instance={instance}&at=zero"), StatusCode::BAD_REQUEST),
        (format!("/api/v2/targets/global/ops/x?expect={e}&instance={instance}&at=1&bogus=1"), StatusCode::BAD_REQUEST),
        ("/api/v2/targets/nowhere/ops/x?expect=e&instance=I&at=1".to_string(), StatusCode::BAD_REQUEST),
        ("/api/v2/targets/global/state/x?expect=e&instance=I&at=1".to_string(), StatusCode::NOT_FOUND),
    ] {
        let (status, body) = read(&server, &path).await;
        assert_eq!(status, expected, "{path}: {body}");
    }
}

#[tokio::test]
async fn a_write_against_an_operation_that_is_not_the_current_one_is_stale_through_a_target() {
    let (server, _link) = Server::start(json!({})).await;
    let (status, body) = post(
        &server,
        "/api/v2/targets/global/ops",
        json!({"id": CREATE, "expect": "NotTheCurrentOp0000000", "kind": "create", "args": {"cwd": CWD}}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "stale");
    let (status, _) = post(&server, "/api/v2/targets/nowhere/ops", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_delayed_prompt_is_open_then_a_repeat_runs_it_once_and_the_delayed_copy_is_a_no_op() {
    let (server, mut link) = Server::start(json!({})).await;
    server.create(&mut link, "s1").await;
    let path = "/sessions/s1/op";
    let (_, mut sse) = open(&server, &format!("follow={path}")).await;
    let first = next(&mut sse).await;
    let instance = first["instance"].as_str().unwrap().to_string();
    let snapshot = &change(&first, path)["snapshot"];
    let (at, e) = (snapshot["at"].as_u64().unwrap(), snapshot["node"]["v"]["id"].as_str().unwrap().to_string());
    let ask = |id: &str| format!("/api/v2/targets/sessions/s1/ops/{id}?expect={e}&instance={instance}&at={at}");

    // The write is delayed: nothing admitted it yet, so the outcome is open and only a repeat of
    // the same request is allowed.
    let (_, body) = read(&server, &ask(CREATE)).await;
    assert_eq!(body, json!({ "outcome": "open" }));

    let write = json!({"id": CREATE, "expect": e, "kind": "prompt", "args": {"prompt": [text("work")]}});
    let uri = "/api/v2/targets/sessions/s1/ops";
    let (status, _) = post(&server, uri, write.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let prompt = link.request("session/prompt").await;
    // The delayed copy arrives while the prompt runs: it is the same operation, and no second
    // prompt reaches the Agent.
    let (status, body) = post(&server, uri, write).await;
    assert_eq!((status, &body), (StatusCode::ACCEPTED, &json!({"op": CREATE})));
    link.quiet(Duration::from_millis(300)).await;

    let (_, body) = read(&server, &ask(CREATE)).await;
    assert_eq!(body["outcome"], "admitted", "{body}");
    assert_eq!(body["record"]["kind"], "prompt");
    let (_, body) = read(&server, &ask(OTHER)).await;
    assert_eq!(body, json!({ "outcome": "superseded" }), "a new identity for the same input is never open now");
    link.reply(&prompt, json!({"stopReason": "end_turn"})).await;
}
