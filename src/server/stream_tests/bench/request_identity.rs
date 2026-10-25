//! The public operation identity contract, through raw HTTP bodies and real ACP dispatch.
use tokio::io::AsyncWriteExt;

use super::*;

const CLIENT_ID: &str = "Zm9yLXRoZS1jbGllbnQtMQ";

#[tokio::test]
async fn new_operations_require_a_client_id_without_changing_any_target() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create_with(&mut link, "s", json!({})).await;
    let global = bench.document("global").await;
    let session = bench.document("session/s").await;
    let catalog = bench.document("catalog").await;

    for (scope, kind, args) in [
        ("global", "create", json!({"cwd": CWD})),
        ("global", "delete", json!({"sessionId": "s"})),
        ("global", "authenticate", json!({"methodId": "login"})),
        ("global", "logout", json!({})),
        ("global", "terminal_login", json!({"methodId": "login", "cols": 80, "rows": 24})),
        ("session/s", "prompt", json!({"prompt": [text("never sent")]})),
        ("session/s", "fork", json!({})),
        ("catalog", "list", json!({})),
    ] {
        let expect = bench.expect_of(scope).await;
        for null_id in [false, true] {
            let mut write = json!({"expect": expect, "kind": kind, "args": args});
            if null_id {
                write["id"] = Value::Null;
            }
            assert!(serde_json::from_value::<crate::protocol::wire::OpRequest>(write.clone()).is_err());
            // Keep this raw: the convenience op helper supplies a client identity.
            let (status, body) = bench.call(Method::POST, &ops_path(scope), Some(write)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{kind}, null={null_id}: {body}");
            assert_eq!(body["error"]["code"], "invalid_request", "{body}");
        }
    }
    link.quiet(QUIET).await;
    assert_eq!(bench.document("global").await, global);
    assert_eq!(bench.document("session/s").await, session);
    assert_eq!(bench.document("catalog").await, catalog);
}

#[tokio::test]
async fn cancel_without_a_new_id_keeps_the_prompt_identity_and_is_repeatable() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create_with(&mut link, "s", json!({})).await;
    let expect = bench.expect_of("session/s").await;
    let (status, body) = bench.op_with_id(
        "session/s", &expect, CLIENT_ID, "prompt", json!({"prompt": [text("work")]}),
    ).await;
    assert_eq!((status, body), (StatusCode::ACCEPTED, json!({"op": CLIENT_ID})));
    let prompt = link.request("session/prompt").await;
    let cancel = json!({"expect": CLIENT_ID, "kind": "cancel", "args": {}});
    let running = bench.document("session/s").await;
    for id in [json!(7), json!("short"), json!({}), json!([])] {
        let mut malformed = cancel.clone();
        malformed["id"] = id;
        let (status, body) = bench.call(Method::POST, &ops_path("session/s"), Some(malformed)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    assert_eq!(bench.document("session/s").await, running);
    let (status, body) = bench.call(Method::POST, &ops_path("session/s"), Some(cancel.clone())).await;
    assert_eq!((status, body), (StatusCode::ACCEPTED, json!({"op": CLIENT_ID})));
    let notification = link.request("session/cancel").await;
    assert!(notification.get("id").is_none(), "{notification}");
    let before = bench.document("session/s").await;
    assert_eq!(before["live"]["op"]["id"], CLIENT_ID);
    assert_eq!(before["live"]["op"]["cancelRequested"], true);
    let (status, body) = bench.call(Method::POST, &ops_path("session/s"), Some(cancel)).await;
    assert_eq!((status, body), (StatusCode::ACCEPTED, json!({"op": CLIENT_ID})));
    link.quiet(QUIET).await;
    assert_eq!(bench.document("session/s").await, before);
    link.reply(&prompt, json!({"stopReason": "cancelled"})).await;
}

#[tokio::test]
async fn losing_the_http_response_preserves_the_client_identity_and_queryable_result() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create_with(&mut link, "s", json!({})).await;
    let (_, held) = bench.call(Method::GET, "/api/v2/nodes/sessions/s/op", None).await;
    let snapshot = &held["change"]["snapshot"];
    let expect = snapshot["node"]["v"]["id"].as_str().unwrap();
    let instance = held["instance"].as_str().unwrap();
    let at = snapshot["at"].as_u64().unwrap();
    let write = json!({"id": CLIENT_ID, "expect": expect, "kind": "prompt", "args": {"prompt": [text("once")]}});
    let path = ops_path("session/s");
    let body = write.to_string();
    let mut connection = TcpStream::connect(bench.address).await.unwrap();
    connection.write_all(format!(
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nOrigin: http://{address}\r\nContent-Type: application/json\r\nContent-Length: {length}\r\n\r\n{body}",
        address = bench.address, length = body.len(),
    ).as_bytes()).await.unwrap();
    let prompt = link.request("session/prompt").await;
    // No response bytes were read. The client only retains its original request identity.
    drop(connection);
    let outcome_url = format!(
        "/api/v2/targets/sessions/s/ops/{CLIENT_ID}?expect={expect}&instance={instance}&at={at}"
    );
    let (status, admitted) = bench.call(Method::GET, &outcome_url, None).await;
    assert_eq!(status, StatusCode::OK, "{admitted}");
    assert_eq!(admitted["outcome"], "admitted");
    assert_eq!(admitted["record"]["id"], CLIENT_ID);
    assert_eq!(admitted["record"]["after"], expect);
    assert_eq!(admitted["record"]["status"], "running");
    assert!(admitted["record"]["rev"].as_u64().unwrap() > at);
    let (status, body) = bench.call(Method::POST, &path, Some(write.clone())).await;
    assert_eq!((status, body), (StatusCode::ACCEPTED, json!({"op": CLIENT_ID})));
    link.quiet(QUIET).await;
    link.refuse(&prompt).await;
    let mut watch = bench.watch(&scopes(&["session/s"])).await;
    watch.until_op("session/s", &json!(CLIENT_ID), "failed").await;
    let (_, failed) = bench.call(Method::GET, &outcome_url, None).await;
    assert_eq!(failed["outcome"], "admitted");
    assert_eq!(failed["record"]["id"], CLIENT_ID);
    assert_eq!(failed["record"]["status"], "failed");
    assert_eq!(failed["record"]["error"], json!({"code": "agent_error", "agentCode": -32600}));
    let (status, body) = bench.call(Method::POST, &path, Some(write)).await;
    assert_eq!((status, body), (StatusCode::ACCEPTED, json!({"op": CLIENT_ID})));
    link.quiet(QUIET).await;
}

#[tokio::test]
async fn generated_request_dtos_serialize_to_the_public_http_contract() {
    use crate::protocol::wire::{Empty, OpRequest, PromptArgs};

    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create_with(&mut link, "s", json!({})).await;
    let expect = bench.expect_of("session/s").await.as_str().unwrap().to_string();
    let request = OpRequest::Prompt {
        id: CLIENT_ID.to_string(),
        expect,
        args: PromptArgs { prompt: vec![serde_json::from_value(text("typed request")).unwrap()] },
    };
    let body = serde_json::to_value(&request).unwrap();
    assert_eq!(serde_json::from_value::<OpRequest>(body.clone()).unwrap(), request);
    let (status, body) = bench.call(Method::POST, &ops_path("session/s"), Some(body)).await;
    assert_eq!((status, body), (StatusCode::ACCEPTED, json!({"op": CLIENT_ID})));
    let prompt = link.request("session/prompt").await;
    assert_eq!(prompt["params"]["prompt"], json!([text("typed request")]));

    let cancel = serde_json::to_value(OpRequest::Cancel {
        expect: CLIENT_ID.to_string(), args: Empty {},
    }).unwrap();
    assert!(cancel.get("id").is_none());
    let (status, body) = bench.call(Method::POST, &ops_path("session/s"), Some(cancel)).await;
    assert_eq!((status, body), (StatusCode::ACCEPTED, json!({"op": CLIENT_ID})));
    link.request("session/cancel").await;
    link.reply(&prompt, json!({"stopReason": "cancelled"})).await;
}
