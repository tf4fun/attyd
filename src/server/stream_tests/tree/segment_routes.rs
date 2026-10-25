//! `segments` is a legal session ID, independently of the text-page suffix.
use super::*;
use crate::protocol::hash::{self, TextRule};
use crate::protocol::tree_stream::client::Client;
use crate::protocol::window::state_wire;

const SESSION: &str = "segments";
const OP: &str = "/sessions/segments/op";
const ENTRIES: &str = "/sessions/segments/entries";
const OUTPUT: &str = "/views/segments/output/latest";

/// The ACP fixture publishes one conversation. SSE supplies the held versions without
/// depending on the node GET routing under test.
async fn populated() -> (Server, Link, String, Client, Sse) {
    let (server, mut link) = Server::start(json!({})).await;
    server.create(&mut link, SESSION).await;
    let (status, mut sse) = open(&server, &format!("follow={ENTRIES}&follow={OUTPUT}")).await;
    assert_eq!(status, StatusCode::OK);
    let mut client = Client::following(&[ENTRIES, OUTPUT]);
    client
        .apply(&serde_json::from_value(next(&mut sse).await).unwrap())
        .unwrap();
    // Three UTF-8 segments, including a code point that would cross a 4096-byte cut.
    let long = format!(
        "{}{}{}",
        "a".repeat(5001),
        "日".repeat(2000),
        "z".repeat(100)
    );
    link.update(
        SESSION,
        json!({"sessionUpdate": "user_message_chunk", "content": text("hello")}),
    )
    .await;
    link.update(
        SESSION,
        json!({
            "sessionUpdate": "agent_message_chunk", "messageId": "reply", "content": text(&long),
        }),
    )
    .await;
    tokio::time::timeout(WAIT, async {
        loop {
            let entries = client.value(ENTRIES);
            let output = client.value(OUTPUT);
            if entries.is_some_and(|entries| {
                entries.as_array().is_some_and(|entries| entries.len() == 2)
                    && entries[1]["chunks"][0]["content"]["text"] == long
            }) && output.is_some_and(|output| output["text"] == long)
            {
                break;
            }
            client
                .apply(&serde_json::from_value(next(&mut sse).await).unwrap())
                .unwrap();
        }
    })
    .await
    .expect("the ACP conversation did not reach the stream");
    // Keep the output view followed while its text segments are read. A segment
    // read observes the existing view; it does not materialize an unfollowed one.
    (server, link, long, client, sse)
}

/// Apply the actual HTTP snapshot and verify the value-node hash it advertises.
async fn node(server: &Server, path: &str, rule: TextRule) -> (Value, Value) {
    let (status, body) = read(server, &format!("/api/v2/nodes{path}")).await;
    assert_eq!(status, StatusCode::OK, "{path}: {body}");
    assert_eq!(body["change"]["path"], path);
    assert_eq!(body["change"]["snapshot"]["base"], Value::Null);
    let mut client = Client::following(&[path]);
    client
        .apply(
            &serde_json::from_value(json!({
                "instance": body["instance"], "rev": body["rev"], "changes": [body["change"]],
            }))
            .unwrap(),
        )
        .unwrap();
    let value = client
        .value(path)
        .expect("the GET installed a node")
        .clone();
    assert_eq!(
        body["change"]["snapshot"]["hash"],
        crate::protocol::digest::wire(&hash::value_hash(rule, &value)),
        "the node's advertised hash verifies"
    );
    (value, body)
}

#[tokio::test]
async fn a_session_named_segments_serves_its_operation_as_a_node() {
    let (server, _link, _, _, sse) = populated().await;
    let (op, body) = node(&server, OP, TextRule::None).await;
    assert_eq!(op["kind"], "open");
    assert_eq!(op["status"], "completed");
    assert_eq!(op["scope"], "session/segments");
    assert!(op["id"].as_str().is_some_and(|id| !id.is_empty()));

    // The same route accepts node-read parameters, not a segment's required `at`.
    let instance = body["instance"].as_str().unwrap();
    let have = body["change"]["snapshot"]["at"].as_u64().unwrap();
    let (status, unchanged) = read(
        &server,
        &format!("/api/v2/nodes{OP}?instance={instance}&have={have}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{unchanged}");
    assert_eq!(unchanged["unchanged"], true);
    assert_eq!(unchanged["instance"], instance);
    drop(sse);
    server.hub.shutdown().await;
}

#[tokio::test]
async fn a_session_named_segments_serves_its_latest_output_as_a_node() {
    let (server, _link, long, streamed, sse) = populated().await;
    let (output, body) = node(&server, OUTPUT, TextRule::OutputView).await;
    assert_eq!(output["text"], long);
    assert_eq!(Some(&output), streamed.value(OUTPUT));
    assert_eq!(body["instance"].as_str(), streamed.instance.as_deref());
    drop(sse);
    server.hub.shutdown().await;
}

#[tokio::test]
async fn a_session_named_segments_pages_text_and_still_refuses_bad_segment_reads() {
    let (server, _link, long, streamed, sse) = populated().await;
    for (position, held_path) in [
        (
            "/sessions/segments/entries/1/chunks/0/content/text",
            ENTRIES,
        ),
        ("/views/segments/output/latest/text", OUTPUT),
    ] {
        let base = format!("/api/v2/nodes{position}/segments");
        let at = streamed.version(held_path);
        let mut paged = String::new();
        let mut chain = hash::ZERO;
        for index in 0..3 {
            let (status, body) = read(&server, &format!("{base}/{index}?at={at}")).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "{position}, segment {index}: {body}"
            );
            assert_eq!(body["before"], state_wire(&chain));
            let segment = body["segment"].as_str().expect("a text segment");
            assert!(!segment.is_empty() && segment.len() <= 4096);
            chain = hash::chain_step(&chain, segment);
            paged.push_str(segment);
        }
        assert_eq!(paged, long);
        assert_eq!(
            hash::finish_for_tests(paged.len() as u64, &chain),
            hash::text_hash(&long)
        );
        let (status, body) = read(&server, &format!("{base}/3?at={at}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"]["code"], "not_found");

        for suffix in [
            format!("x?at={at}"),
            format!("-1?at={at}"),
            "0".to_string(),
            "0?at=x".to_string(),
            "0?at=-1".to_string(),
            format!("0?at={}", u64::MAX),
            format!("0?at={at}&at={at}"),
            format!("0?at={at}&unknown=1"),
        ] {
            let (status, body) = read(&server, &format!("{base}/{suffix}")).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{position}/{suffix}: {body}"
            );
            assert_eq!(body["error"]["code"], "invalid_request");
        }
    }
    drop(sse);
    server.hub.shutdown().await;
}
