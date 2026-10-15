//! `create`, `fork` and `list` hold their HTTP response until the operation
//! ends, past the JSON request deadline (protocol §6.3).
use super::*;

/// The JSON request deadline of the v1 routes, which these responses outlive.
const PAST_THE_DEADLINE: Duration = Duration::from_secs(61);

/// A write whose response is awaited without a deadline of the test's own.
fn untimed_op(
    bench: &Bench,
    scope: &str,
    expect: &Value,
    kind: &str,
    args: Value,
) -> JoinHandle<(StatusCode, Value)> {
    let address = bench.address;
    let path = ops_path(scope);
    let body = json!({ "expect": expect, "kind": kind, "args": args });
    tokio::spawn(async move {
        let stream = TcpStream::connect(address).await.unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(connection);
        let request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::HOST, address.to_string())
            .header(header::ORIGIN, format!("http://{address}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = sender.send_request(request).await.unwrap();
        body_of(response).await
    })
}

/// The `200` a held response ends with: it names `op`; its result.
async fn finished(what: &str, response: JoinHandle<(StatusCode, Value)>, op: &Value) -> Value {
    let (status, body) = tokio::time::timeout(WAIT, response)
        .await
        .unwrap_or_else(|_| panic!("the {what} response did not arrive"))
        .unwrap();
    assert_eq!(status, StatusCode::OK, "{what}: {body}");
    assert_eq!(&body["op"], op, "{what}: {body}");
    body["result"].clone()
}

/// A create and a fork whose Agent has not answered, and a list that waits
/// for an elicitation, are still waiting once the clock is past the JSON
/// deadline; each then answers `200` with its operation and result, and no
/// cancel reaches the Agent meanwhile.
#[tokio::test]
async fn create_fork_and_list_responses_outlive_the_json_deadline() {
    let (bench, mut link) =
        Bench::ws(json!({"sessionCapabilities": {"fork": {}, "list": {}}})).await;
    bench.create(&mut link, "s", json!({})).await;
    let mut watch = bench
        .watch(&scopes(&["global", "catalog", "session/s"]))
        .await;
    watch
        .until("the alignments", |watch| {
            watch.has("global") && watch.has("catalog") && watch.has("session/s")
        })
        .await;

    let expect = watch.live("global")["op"]["id"].clone();
    let created = untimed_op(&bench, "global", &expect, "create", json!({"cwd": CWD}));
    let new = link.request("session/new").await;
    watch
        .until_live("global", "the create runs", |live| {
            live["op"]["kind"] == "create" && live["op"]["status"] == "running"
        })
        .await;
    let create = watch.live("global")["op"]["id"].clone();

    let expect = watch.live("session/s")["op"]["id"].clone();
    let forked = untimed_op(&bench, "session/s", &expect, "fork", json!({}));
    let fork_request = link.request("session/fork").await;
    watch
        .until_live("session/s", "the fork runs", |live| {
            live["op"]["kind"] == "fork" && live["op"]["status"] == "running"
        })
        .await;
    let fork = watch.live("session/s")["op"]["id"].clone();

    let expect = watch.live("catalog")["op"]["id"].clone();
    let listed = untimed_op(&bench, "catalog", &expect, "list", json!({}));
    let list_request = link.request("session/list").await;
    link.agent_request(
        "list-question",
        "elicitation/create",
        form(json!({"requestId": list_request["id"]}), "list?"),
    )
    .await;
    watch
        .until_live("catalog", "the list waits for its answer", |live| {
            live["op"]["kind"] == "list" && !interaction_ids(live).is_empty()
        })
        .await;
    let live = watch.live("catalog");
    let list = live["op"]["id"].clone();
    let interaction = interaction_for(&live, "list?");

    // Only the clock moves while it is paused: the harness's own waits are
    // not running, so nothing but the server's deadlines can fire.
    tokio::time::pause();
    tokio::time::advance(PAST_THE_DEADLINE).await;
    tokio::time::resume();
    link.quiet(QUIET).await;
    for (what, response) in [("create", &created), ("fork", &forked), ("list", &listed)] {
        assert!(
            !response.is_finished(),
            "the {what} response is still waiting for its operation"
        );
    }

    let (status, body) = bench
        .answer(
            "catalog",
            &list,
            answers(&[(interaction.as_str(), accept())]),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "the list's answer: {body}");
    assert_eq!(
        link.response("list-question").await["result"]["action"],
        "accept"
    );
    // The list settles first, so its catalog revision check does not race
    // the create and fork that follow.
    link.reply(&list_request, json!({"sessions": [], "nextCursor": null}))
        .await;
    let listed = finished("list", listed, &list).await;
    assert_eq!(listed["sessions"], json!([]), "{listed}");
    assert_eq!(listed["nextCursor"], Value::Null, "{listed}");
    assert!(listed["revision"].is_string(), "{listed}");
    watch.until_op("catalog", &list, "completed").await;
    assert!(
        !created.is_finished() && !forked.is_finished(),
        "the create and fork responses still wait for their operations"
    );

    link.reply(&fork_request, json!({"sessionId": "forked"}))
        .await;
    link.reply(&new, json!({"sessionId": "created"})).await;
    let created = finished("create", created, &create).await;
    assert_eq!(created, json!({"sessionId": "created"}));
    let forked = finished("fork", forked, &fork).await;
    assert_eq!(forked, json!({"sessionId": "forked"}));
    watch.until_op("global", &create, "completed").await;
    watch.until_op("session/s", &fork, "completed").await;
    watch.converged(&bench).await;
}
