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
