//! Session demand that arrives after EOF, when there is no runtime left to wake.
use super::*;
use crate::protocol::tree_stream::client::Client;

const ENTRIES: &str = "/sessions/saved/entries";
const OUTPUT: &str = "/views/saved/output/latest";

/// A published stopped phase can precede the runtime's final cleanup. New demand must come
/// after the input is actually gone, or the old generation could arrange its recovery.
async fn runtime_ended(server: &Server) -> Duration {
    tokio::time::timeout(WAIT, async {
        loop {
            let state = server.hub.state.lock().await;
            if state.input.is_none() {
                return state.recovery_backoff;
            }
            drop(state);
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the old Agent runtime did not finish")
}

async fn after_unobserved_eof() -> Server {
    let (server, link) = Server::start(json!({"loadSession": true})).await;
    tokio::time::timeout(WAIT, async {
        loop {
            if server.hub.state.lock().await.initialized {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the first Agent did not initialize");
    assert!(
        server
            .hub
            .publication
            .lock()
            .feed
            .followed_sessions()
            .is_empty()
    );
    link.close().await;
    runtime_ended(&server).await;
    assert_eq!(server.agents.accepted(), 1);
    server
}

fn follow(path: &str) -> String {
    format!("follow={path}&cwd={}", query_scope(&format!("saved:{CWD}")))
}

fn node_read(path: &str) -> String {
    format!("/api/v2/nodes{path}?cwd={}", query_scope(CWD))
}

fn has_replay(path: &str, value: &Value) -> bool {
    match path {
        ENTRIES => {
            value.as_array().is_some_and(|entries| entries.len() == 2)
                && value[0]["chunks"][0]["content"]["text"] == "hello"
                && value[1]["chunks"][0]["content"]["text"] == "hi there"
        }
        OUTPUT => value["text"] == "hi there",
        _ => panic!("unexpected path {path}"),
    }
}

async fn stream_has_replay(sse: &mut Sse, path: &str, first: Value) {
    let mut client = Client::following(&[path]);
    client
        .apply(&serde_json::from_value(first).unwrap())
        .unwrap();
    tokio::time::timeout(WAIT, async {
        while !client
            .value(path)
            .is_some_and(|value| has_replay(path, value))
        {
            let event = next(sse).await;
            client
                .apply(&serde_json::from_value(event).unwrap())
                .unwrap();
        }
    })
    .await
    .expect("the resumed stream did not receive the replayed content");
}

async fn read_has_replay(server: &Server, path: &str) {
    tokio::time::timeout(WAIT, async {
        loop {
            let (status, body) = read(server, &node_read(path)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let mut client = Client::following(&[path]);
            client
                .apply(
                    &serde_json::from_value(json!({
                        "instance": body["instance"], "rev": body["rev"],
                        "changes": [body["change"]],
                    }))
                    .unwrap(),
                )
                .unwrap();
            if client
                .value(path)
                .is_some_and(|value| has_replay(path, value))
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a read did not receive the replayed content");
}

async fn resume_and_load(server: &mut Server) -> Link {
    let mut link = server.agents.accept(json!({"loadSession": true})).await;
    let load = link
        .serve_load("saved", &turns(&[("hello", "hi there")]))
        .await;
    assert_eq!(load["params"]["cwd"], CWD);
    assert_eq!(server.agents.accepted(), 2, "one next Agent connection");
    link
}

async fn late_stream(path: &str) {
    let mut server = after_unobserved_eof().await;
    let (status, mut sse) = open(&server, &follow(path)).await;
    assert_eq!(status, StatusCode::OK);
    let first = next(&mut sse).await;
    assert_eq!(change(&first, path)["end"]["reason"], "absent");
    let mut link = resume_and_load(&mut server).await;
    stream_has_replay(&mut sse, path, first).await;
    link.quiet(Duration::from_millis(100)).await;
    assert_eq!(server.agents.accepted(), 2);
    drop(sse);
    server.hub.shutdown().await;
}

async fn late_read(path: &str) {
    let mut server = after_unobserved_eof().await;
    let (status, body) = read(&server, &node_read(path)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["change"]["end"]["reason"], "absent");
    // The HTTP body has ended. No stream remains to make recovery happen on its behalf.
    assert_eq!(
        server
            .hub
            .publication
            .lock()
            .feed
            .followers("session/saved"),
        0
    );
    let mut link = resume_and_load(&mut server).await;
    read_has_replay(&server, path).await;
    link.quiet(Duration::from_millis(100)).await;
    assert_eq!(server.agents.accepted(), 2);
    server.hub.shutdown().await;
}

#[tokio::test]
async fn a_session_stream_after_unobserved_eof_starts_and_loads_the_next_connection() {
    late_stream(ENTRIES).await;
}

#[tokio::test]
async fn a_view_stream_after_unobserved_eof_starts_and_loads_the_next_connection() {
    late_stream(OUTPUT).await;
}

#[tokio::test]
async fn a_session_read_after_unobserved_eof_starts_and_loads_the_next_connection() {
    late_read(ENTRIES).await;
}

#[tokio::test]
async fn a_view_read_after_unobserved_eof_starts_and_loads_the_next_connection() {
    late_read(OUTPUT).await;
}

#[tokio::test]
async fn concurrent_late_streams_and_reads_share_one_connection_and_one_load() {
    let mut server = after_unobserved_eof().await;
    let session_query = follow(ENTRIES);
    let view_query = follow(OUTPUT);
    let session_read = node_read(ENTRIES);
    let view_read = node_read(OUTPUT);
    let ((session_status, mut session), (view_status, mut view), session_body, view_body) = tokio::join!(
        open(&server, &session_query),
        open(&server, &view_query),
        read(&server, &session_read),
        read(&server, &view_read),
    );
    assert_eq!(session_status, StatusCode::OK);
    assert_eq!(view_status, StatusCode::OK);
    for (status, body) in [session_body, view_body] {
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["change"]["end"]["reason"], "absent");
    }
    let session_first = next(&mut session).await;
    let view_first = next(&mut view).await;
    let mut link = resume_and_load(&mut server).await;
    tokio::join!(
        stream_has_replay(&mut session, ENTRIES, session_first),
        stream_has_replay(&mut view, OUTPUT, view_first),
        read_has_replay(&server, ENTRIES),
        read_has_replay(&server, OUTPUT),
    );
    // An extra load on the same connection is as wrong as an extra connection.
    link.quiet(Duration::from_millis(100)).await;
    assert_eq!(server.agents.accepted(), 2);
    drop((session, view));
    server.hub.shutdown().await;
}

#[tokio::test]
async fn late_short_reads_keep_the_first_directory_and_demand_through_connection_backoff() {
    let mut server = Server::launch().await;
    let mut first = server.agents.connect().await;
    first.request("initialize").await;
    first.close().await;
    let backoff = runtime_ended(&server).await;
    assert!(
        backoff > Duration::ZERO,
        "an uninitialized connection must back off"
    );

    let (status, body) = read(&server, &node_read(OUTPUT)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["change"]["end"]["reason"], "absent");
    assert_eq!(
        server
            .hub
            .publication
            .lock()
            .feed
            .followers("session/saved"),
        0
    );
    // The reference client's next poll reads the operation without repeating the cwd.
    // Both requests finish before initialize is answered: neither can erase the pending
    // directory, and no live HTTP subscriber is left to carry the demand.
    let (status, body) = read(&server, "/api/v2/nodes/sessions/saved/op").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["change"]["end"]["reason"], "absent");
    assert_eq!(
        server
            .hub
            .publication
            .lock()
            .feed
            .followers("session/saved"),
        0
    );
    let mut link = resume_and_load(&mut server).await;
    read_has_replay(&server, OUTPUT).await;
    link.quiet(Duration::from_millis(100)).await;
    assert_eq!(server.agents.accepted(), 2);
    server.hub.shutdown().await;
}

#[tokio::test]
async fn late_global_and_catalog_reads_and_streams_do_not_start_a_connection() {
    let server = after_unobserved_eof().await;
    let (status, mut sse) = open(&server, "follow=/global&follow=/catalog").await;
    assert_eq!(status, StatusCode::OK);
    let first = next(&mut sse).await;
    assert!(change(&first, "/global").get("snapshot").is_some());
    assert!(change(&first, "/catalog").get("snapshot").is_some());
    for path in ["/api/v2/nodes/global", "/api/v2/nodes/catalog"] {
        let (status, body) = read(&server, path).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["change"].get("snapshot").is_some(), "{body}");
    }
    // Every HTTP admission has completed. None is session demand or scheduled recovery.
    let state = server.hub.state.lock().await;
    assert!(state.input.is_none());
    assert!(state.recovery.is_none());
    assert_eq!(server.agents.accepted(), 1);
    drop(state);
    drop(sse);
    server.hub.shutdown().await;
}

#[tokio::test]
async fn a_late_session_stream_without_cwd_ends_instead_of_waiting_forever() {
    let mut server = after_unobserved_eof().await;
    let (status, mut sse) = open(&server, &format!("follow={ENTRIES}")).await;
    assert_eq!(status, StatusCode::OK);
    let first = next(&mut sse).await;
    assert_eq!(change(&first, ENTRIES)["end"]["reason"], "absent");
    let mut link = server.agents.accept(json!({"loadSession": true})).await;
    let end = end_of(&mut sse, ENTRIES).await;
    assert_eq!(end["end"]["reason"], "load_failed", "{end}");
    assert_eq!(end["end"]["error"]["code"], "invalid_request", "{end}");
    link.quiet(Duration::from_millis(100)).await;
    assert!(
        server
            .hub
            .publication
            .lock()
            .feed
            .followed_sessions()
            .is_empty()
    );
    assert_eq!(server.agents.accepted(), 2);
    drop(sse);
    server.hub.shutdown().await;
}
