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
