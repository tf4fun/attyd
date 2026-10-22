//! `GET /api/v3/stream` over real HTTP (docs/protocol.md §5): the stream of the
//! revision 3 feed, beside the revision 2 one until the cutover replaces it.

use super::*;

/// `GET /api/v3/stream?…`: the status and the SSE body.
async fn open(server: &Server, query: &str) -> (StatusCode, Sse) {
    let response = server.send(&format!("/api/v3/stream?{query}")).await;
    let status = response.status();
    if status == StatusCode::OK {
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/event-stream");
    }
    let sse = Sse {
        body: Body::new(response.into_body()).into_data_stream(),
        buffer: String::new(),
        replicas: BTreeMap::new(),
        aligned: BTreeSet::new(),
        ended: BTreeSet::new(),
    };
    (status, sse)
}

/// The next event of the stream, skipping comments (the heartbeat).
async fn event(sse: &mut Sse, period: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + period;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let message = sse.message(left).await?;
        assert!(
            message.lines().all(|line| !line.starts_with("id:")),
            "events carry no SSE id: {message:?}"
        );
        let data: Vec<&str> = message
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect();
        if !data.is_empty() {
            return Some(serde_json::from_str(&data.join("\n")).unwrap());
        }
    }
}

#[tokio::test]
async fn a_stream_starts_with_the_snapshot_of_every_scope_it_follows() {
    let (server, _link) = Server::start(json!({})).await;
    let (status, mut sse) = open(&server, "scope=global&scope=catalog").await;
    assert_eq!(status, StatusCode::OK);
    let mut scopes = BTreeMap::new();
    for _ in 0..2 {
        let event = event(&mut sse, WAIT).await.expect("an alignment");
        scopes.insert(event["scope"].as_str().unwrap().to_string(), event);
    }
    let global = &scopes["global"]["snapshot"];
    assert!(global["rev"].is_u64(), "{global}");
    assert!(global["epoch"].is_string() && global["digest"].is_string(), "{global}");
    assert!(global["doc"]["live"]["agent"].is_object(), "{global}");
    assert!(scopes["catalog"]["snapshot"]["doc"]["live"].is_object());
}

#[tokio::test]
async fn a_session_is_followed_through_its_changes_and_verified_by_a_checkpoint() {
    let (server, mut link) = Server::start(json!({})).await;
    server.create(&mut link, "s1").await;
    let (_, mut sse) = open(&server, "scope=session%2Fs1").await;
    let first = event(&mut sse, WAIT).await.expect("an alignment");
    assert_eq!(first["scope"], "session/s1");
    let snapshot = &first["snapshot"];
    let base = snapshot["rev"].as_u64().unwrap();

    link.update(
        "s1",
        json!({"sessionUpdate": "agent_message_chunk", "messageId": "m", "content": text("hi")}),
    )
    .await;
    let ops = event(&mut sse, WAIT).await.expect("the change");
    assert_eq!(ops["scope"], "session/s1");
    assert_eq!(ops["rev"], base + 1, "{ops}");
    assert!(ops["ops"].is_array());

    // The changes stopped: the checkpoint follows after the pause.
    let checkpoint = event(&mut sse, WAIT).await.expect("the checkpoint");
    assert_eq!(checkpoint["scope"], "session/s1");
    assert_eq!(checkpoint["rev"], base + 1, "{checkpoint}");
    assert_eq!(checkpoint["digest"].as_str().unwrap().len(), 22);
}

#[tokio::test]
async fn a_stream_with_a_query_that_is_not_one_is_refused() {
    let (server, _link) = Server::start(json!({})).await;
    for query in ["", "scope=nowhere", "scope=global&scope=global", "scope=global&x=1"] {
        let (status, _) = open(&server, query).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query:?}");
    }
}

#[tokio::test]
async fn shutting_the_server_down_ends_the_streams_after_what_is_queued() {
    let (server, _link) = Server::start(json!({})).await;
    let (_, mut sse) = open(&server, "scope=global").await;
    assert!(event(&mut sse, WAIT).await.is_some());
    server.hub.shutdown().await;
    // The final commit is delivered; then the stream ends.
    while let Some(event) = event(&mut sse, WAIT).await {
        assert_eq!(event["scope"], "global", "{event}");
    }
    assert!(sse.message(WAIT).await.is_none(), "the stream ended");
}

/// The next event of `scope`, skipping those of other scopes.
async fn event_of(sse: &mut Sse, scope: &str) -> Value {
    loop {
        let event = event(sse, WAIT)
            .await
            .unwrap_or_else(|| panic!("no event of {scope}"));
        if event["scope"] == scope {
            return event;
        }
    }
}

/// The next event of `scope` that ends it.
async fn end_of(sse: &mut Sse, scope: &str) -> Value {
    loop {
        let event = event_of(sse, scope).await;
        if event.get("end").is_some() {
            return event;
        }
    }
}

#[tokio::test]
async fn a_stream_loads_a_session_the_bridge_does_not_hold_with_the_cwd_it_gave() {
    let (server, mut link) = Server::start(json!({"loadSession": true})).await;
    let (status, mut sse) = open(
        &server,
        &format!(
            "scope=global&scope=session%2Fsaved&cwd={}",
            query_scope(&format!("session/saved:{CWD}"))
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let load = link.serve_load("saved", &turns(&[("hello", "hi there")])).await;
    assert_eq!(load["params"]["cwd"], CWD);

    // The stream is aligned while the load runs.
    let first = event_of(&mut sse, "session/saved").await;
    assert_eq!(first["snapshot"]["doc"]["live"]["op"]["kind"], "load", "{first}");

    // Once the load ended the session is held with its replayed history: a
    // stream that follows it now is aligned with the whole of it.
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let (_, mut other) = open(&server, "scope=session%2Fsaved").await;
        let snapshot = event_of(&mut other, "session/saved").await;
        let doc = &snapshot["snapshot"]["doc"];
        if doc["history"] == "complete" {
            assert_eq!(snapshot["snapshot"]["entries"]["length"], 2, "{snapshot}");
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "never loaded: {snapshot}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_stream_that_gave_no_cwd_for_an_unheld_session_is_ended_for_it() {
    let (server, mut link) = Server::start(json!({"loadSession": true})).await;
    let (_, mut sse) = open(&server, "scope=global&scope=session%2Fsaved").await;
    let end = end_of(&mut sse, "session/saved").await;
    assert_eq!(end["end"]["reason"], "load_failed", "{end}");
    assert_eq!(end["end"]["error"]["code"], "invalid_request", "{end}");
    // The Agent is never asked to load a session the stream gave no directory for.
    link.quiet(Duration::from_millis(300)).await;
}

#[tokio::test]
async fn a_load_the_agent_refuses_ends_the_scope_for_the_stream() {
    let (server, mut link) = Server::start(json!({"loadSession": true})).await;
    let (_, mut sse) = open(
        &server,
        &format!(
            "scope=session%2Fsaved&cwd={}",
            query_scope(&format!("session/saved:{CWD}"))
        ),
    )
    .await;
    let load = link.request("session/load").await;
    link.send(json!({
        "jsonrpc": "2.0", "id": load["id"],
        "error": {"code": -32002, "message": "no such session"},
    }))
    .await;
    let end = end_of(&mut sse, "session/saved").await;
    assert_eq!(end["end"]["reason"], "load_failed", "{end}");
}

// -- Operations (protocol §6) ---------------------------------------------------------------

/// A client-chosen operation id: 22 characters of base64url (protocol §6.5).
const CREATE_ID: &str = "Cr3ateOperation_Id-001";

/// The status and the JSON body of `POST /api/v3/scopes/{scope}/ops`.
async fn write(server: &Server, scope: &str, body: Value) -> (StatusCode, Value) {
    let path = format!("/api/v3/scopes/{}/ops", scope.replace('/', "%2F"));
    let response = send(server.address, Method::POST, &path, Some(body)).await;
    let status = response.status();
    (status, json_of(response).await)
}

/// The document of `scope` as a new stream is aligned with it.
async fn document(server: &Server, scope: &str) -> Value {
    let (_, mut sse) = open(server, &format!("scope={}", scope.replace('/', "%2F"))).await;
    event_of(&mut sse, scope).await["snapshot"]["doc"].clone()
}

#[tokio::test]
async fn every_operation_is_answered_with_202_before_it_ends_and_its_result_is_in_the_document() {
    let (server, mut link) = Server::start(json!({})).await;
    let global = document(&server, "global").await;
    let expect = global["live"]["op"]["id"].clone();

    // The Agent has not answered `session/new`: the write is answered anyway.
    let (status, body) = write(
        &server,
        "global",
        json!({"id": CREATE_ID, "expect": expect, "kind": "create", "args": {"cwd": CWD}}),
    )
    .await;
    assert_eq!((status, &body), (StatusCode::ACCEPTED, &json!({"op": CREATE_ID})));

    // Repeating the write is harmless: it is the same operation.
    let (status, body) = write(
        &server,
        "global",
        json!({"id": CREATE_ID, "expect": expect, "kind": "create", "args": {"cwd": CWD}}),
    )
    .await;
    assert_eq!((status, &body), (StatusCode::ACCEPTED, &json!({"op": CREATE_ID})));

    let new = link.request("session/new").await;
    link.reply(&new, json!({"sessionId": "s1"})).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let op = document(&server, "global").await["live"]["op"].clone();
        if op["status"] == "completed" {
            assert_eq!(op["id"], CREATE_ID);
            assert_eq!(op["result"], json!({"sessionId": "s1"}), "{op}");
            assert_eq!(op["after"], expect);
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "never completed: {op}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    link.quiet(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn a_write_against_an_operation_that_is_not_the_current_one_is_stale() {
    let (server, _link) = Server::start(json!({})).await;
    let (status, body) = write(
        &server,
        "global",
        json!({"id": "AAAAAAAAAAAAAAAAAAAAAA", "expect": "not-the-current-op", "kind": "create", "args": {"cwd": CWD}}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "stale", "{body}");
}

// -- The connection a followed session needs (S6, Q2) ------------------------------------------

#[tokio::test]
async fn a_stream_that_follows_a_session_starts_the_next_connection_and_keeps_the_session() {
    let (mut server, mut link) = Server::start(json!({"loadSession": true})).await;
    let (_, mut sse) = open(
        &server,
        &format!(
            "scope=global&scope=session%2Fsaved&cwd={}",
            query_scope(&format!("session/saved:{CWD}"))
        ),
    )
    .await;
    link.serve_load("saved", &turns(&[("one", "1")])).await;
    // Wait until the session is held with its history.
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let (_, mut other) = open(&server, "scope=session%2Fsaved").await;
        let snapshot = event_of(&mut other, "session/saved").await;
        if snapshot["snapshot"]["doc"]["history"] == "complete" {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "never loaded");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The Agent connection ends: the stream needs the next one, and the session is loaded on it.
    link.close().await;
    let mut next = server.agents.accept(json!({"loadSession": true})).await;
    let load = next.serve_load("saved", &turns(&[("one", "1")])).await;
    assert_eq!(load["params"]["sessionId"], "saved");
    assert_eq!(server.agents.accepted(), 2);

    // The stream never ended the session; its document is back to a completed load.
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let (_, mut other) = open(&server, "scope=session%2Fsaved").await;
        let snapshot = event_of(&mut other, "session/saved").await;
        assert!(snapshot.get("end").is_none(), "{snapshot}");
        let op = &snapshot["snapshot"]["doc"]["live"]["op"];
        if op["kind"] == "load" && op["status"] == "completed" {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "never reattached: {snapshot}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(sse.body);
}

// -- Reads (protocol §8) ----------------------------------------------------------------------

/// `GET /api/v3/scopes/{scope}`: the status and the JSON body.
async fn read_snapshot(server: &Server, scope: &str) -> (StatusCode, Value) {
    let path = format!("/api/v3/scopes/{}", scope.replace('/', "%2F"));
    let response = server.send(&path).await;
    let status = response.status();
    (status, json_of(response).await)
}

#[tokio::test]
async fn a_scope_is_read_as_one_snapshot_that_is_the_one_a_stream_aligns_with() {
    let (server, mut link) = Server::start(json!({})).await;
    server.create(&mut link, "s1").await;

    let (status, global) = read_snapshot(&server, "global").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(global["scope"], "global");
    let snapshot = &global["snapshot"];
    assert!(snapshot["epoch"].is_string() && snapshot["digest"].is_string(), "{global}");
    assert!(snapshot["doc"]["live"]["sessions"].get("s1").is_some(), "{global}");

    let (status, session) = read_snapshot(&server, "session/s1").await;
    assert_eq!(status, StatusCode::OK, "{session}");
    assert_eq!(session["snapshot"]["entries"]["base"], "replace");

    // A stream that follows the session is aligned with the same document.
    let (_, mut sse) = open(&server, "scope=session%2Fs1").await;
    let aligned = event_of(&mut sse, "session/s1").await;
    let (_, again) = read_snapshot(&server, "session/s1").await;
    assert_eq!(aligned["snapshot"]["digest"], again["snapshot"]["digest"]);
    assert_eq!(aligned["snapshot"]["rev"], again["snapshot"]["rev"]);
}

#[tokio::test]
async fn reading_an_unheld_session_holds_nothing_and_a_bad_scope_is_refused() {
    let (server, mut link) = Server::start(json!({"loadSession": true})).await;
    let (status, body) = read_snapshot(&server, "session/unheld").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "not_found");
    // Nothing was loaded for it.
    link.quiet(Duration::from_millis(300)).await;
    assert_eq!(read_snapshot(&server, "session/unheld").await.0, StatusCode::NOT_FOUND);

    let (status, body) = read_snapshot(&server, "nowhere").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "invalid_request");
}
