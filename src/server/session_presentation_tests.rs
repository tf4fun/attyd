use super::*;
use clap::Parser;
use tower::ServiceExt;

fn raw_view() -> serde_json::Value {
    let mut updates = vec![
        json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "prompt"}}),
    ];
    for index in 0..23 {
        updates.push(json!({"sessionUpdate": "tool_call", "toolCallId": format!("t{index}"),
            "title": format!("hidden-process-{index}"), "content": [{"type": "terminal", "terminalId": format!("terminal-{index}")}]}));
    }
    updates.push(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "final answer"}}));
    json!({
        "bridgeEpoch": "epoch", "session": {"sessionId": "session", "incarnation": 3,
            "viewRevision": 5, "historyRevision": "history", "phase": "ready", "activeTurn": null,
            "turnOutcomes": [{"operationId": "op", "afterUpdate": updates.len(), "response": {"stopReason": "end_turn"}}]},
        "baseline": {"updates": updates},
        "live": {"terminals": {"live": {"output": "live output"}}},
        "terminals": {"terminal-0": {"output": "hidden terminal output"}}
    })
}

fn request(uri: &str) -> Request {
    Request::builder()
        .uri(uri)
        .header(header::HOST, "localhost:7331")
        .body(Body::empty())
        .unwrap()
}

async fn body(response: Response) -> serde_json::Value {
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn compact_http_view_defers_process_and_http_pages_deliver_ten_ten_three() {
    let (hub, mut commands) = tests::observation_hub(4).await;
    let options = Options::try_parse_from(["attyd", "--", "fake-agent"]).unwrap();
    let app = app_router(&options, hub.clone());
    let pending = app
        .clone()
        .oneshot(request("/api/v1/sessions/session?presentation=compact"));
    let reply = async {
        let bridge::BridgeInput::SessionViewRequest {
            expected_owner,
            response,
            ..
        } = commands.recv().await.unwrap()
        else {
            panic!("expected view request")
        };
        assert!(expected_owner.is_none());
        response.send(Ok(raw_view())).unwrap();
    };
    let (response, ()) = tokio::join!(pending, reply);
    let response = response.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()[header::ETAG], "\"history\"");
    let compact = body(response).await;
    assert!(!compact.to_string().contains("hidden"));
    assert_eq!(compact["timeline"][1]["content"]["text"], "final answer");
    assert_eq!(compact["collapsedTurns"][0]["processCount"], 23);
    assert_eq!(
        compact["terminals"],
        json!({"live": {"output": "live output"}})
    );
    let turn_id = compact["collapsedTurns"][0]["turnId"].as_str().unwrap();
    for (offset, expected_count, next) in [(0, 10, Some(10)), (10, 10, Some(20)), (20, 3, None)] {
        let uri = format!(
            "/api/v1/sessions/session/turns/{turn_id}/process?expectedEpoch=epoch&expectedIncarnation=3&historyRevision=history&offset={offset}"
        );
        let pending = app.clone().oneshot(request(&uri));
        let reply = async {
            let bridge::BridgeInput::SessionViewRequest {
                expected_owner,
                cwd,
                response,
                ..
            } = commands.recv().await.unwrap()
            else {
                panic!("expected fenced view")
            };
            assert_eq!(
                expected_owner,
                Some(SessionResourceOwner::new("epoch", "session", 3))
            );
            assert!(cwd.is_none());
            response.send(Ok(raw_view())).unwrap();
        };
        let (response, ()) = tokio::join!(pending, reply);
        let response = response.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let page = body(response).await;
        assert_eq!(page["items"].as_array().unwrap().len(), expected_count);
        assert_eq!(page["nextOffset"], json!(next));
        assert_eq!(page["offset"], offset);
        assert_eq!(page["response"]["stopReason"], "end_turn");
        assert_eq!(
            page["terminals"].as_object().unwrap().len(),
            usize::from(offset == 0)
        );
    }
    assert!(commands.try_recv().is_err());
}

#[tokio::test]
async fn process_http_rejects_missing_owner_negative_offsets_and_unknown_query_fields_before_lookup()
 {
    let (hub, mut commands) = tests::observation_hub(4).await;
    let options = Options::try_parse_from(["attyd", "--", "fake-agent"]).unwrap();
    let app = app_router(&options, hub);
    for query in [
        "historyRevision=history",
        "expectedEpoch=epoch&historyRevision=history",
        "expectedEpoch=epoch&expectedIncarnation=0&historyRevision=history",
        "expectedEpoch=epoch&expectedIncarnation=3&historyRevision=history&offset=-1",
        "expectedEpoch=epoch&expectedIncarnation=3&historyRevision=history&limit=1000",
    ] {
        let response = app
            .clone()
            .oneshot(request(&format!(
                "/api/v1/sessions/session/turns/history-0-0/process?{query}"
            )))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        assert!(commands.try_recv().is_err());
    }
}

#[tokio::test]
async fn process_http_fences_stale_history_and_retired_connection_without_unscoped_reload() {
    let (hub, mut commands) = tests::observation_hub(4).await;
    let options = Options::try_parse_from(["attyd", "--", "fake-agent"]).unwrap();
    let app = app_router(&options, hub);
    for (result, code) in [
        (Ok(raw_view()), "history_revision_changed"),
        (
            Err(bridge::SessionViewError::Retired(
                SessionResourceOwner::new("epoch", "session", 3),
            )),
            "session_retired",
        ),
        (
            Err(bridge::SessionViewError::ConnectionReplaced {
                owner: SessionResourceOwner::new("epoch", "session", 3),
                current_epoch: "new".into(),
            }),
            "bridge_replaced",
        ),
    ] {
        let pending = app.clone().oneshot(request("/api/v1/sessions/session/turns/history-0-0/process?expectedEpoch=epoch&expectedIncarnation=3&historyRevision=old&offset=0"));
        let reply = async {
            let bridge::BridgeInput::SessionViewRequest {
                expected_owner,
                response,
                ..
            } = commands.recv().await.unwrap()
            else {
                panic!("expected fenced view")
            };
            assert_eq!(
                expected_owner,
                Some(SessionResourceOwner::new("epoch", "session", 3))
            );
            response.send(result).unwrap();
        };
        let (response, ()) = tokio::join!(pending, reply);
        let response = response.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(body(response).await["code"], code);
        assert!(commands.try_recv().is_err());
    }
}

#[tokio::test]
async fn default_http_and_explicit_full_view_keep_complete_export_compatibility() {
    let (hub, mut commands) = tests::observation_hub(4).await;
    let options = Options::try_parse_from(["attyd", "--", "fake-agent"]).unwrap();
    let app = app_router(&options, hub);
    for query in ["", "?presentation=full"] {
        let pending = app
            .clone()
            .oneshot(request(&format!("/api/v1/sessions/session{query}")));
        let reply = async {
            let bridge::BridgeInput::SessionViewRequest { response, .. } =
                commands.recv().await.unwrap()
            else {
                panic!("expected view")
            };
            response.send(Ok(raw_view())).unwrap();
        };
        let (response, ()) = tokio::join!(pending, reply);
        let view = body(response.unwrap()).await;
        assert!(view.to_string().contains("hidden-process-22"));
        assert!(view.get("collapsedTurns").is_none());
    }
    let embedded = normalize_embedded_session_view(json!({"view": raw_view()})).unwrap();
    assert!(!embedded.to_string().contains("hidden-process"));
}

#[tokio::test]
async fn compact_http_observed_process_suffix_requires_owner_and_keeps_full_turn() {
    let (hub, mut commands) = tests::observation_hub(4).await;
    let options = Options::try_parse_from(["attyd", "--", "fake-agent"]).unwrap();
    let app = app_router(&options, hub);
    for query in [
        "presentation=compact&includeProcessFrom=op",
        "presentation=full&expectedEpoch=epoch&expectedIncarnation=3&includeProcessFrom=op",
        "presentation=compact&expectedEpoch=epoch&expectedIncarnation=3&includeProcessFrom=",
    ] {
        let response = app
            .clone()
            .oneshot(request(&format!("/api/v1/sessions/session?{query}")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(commands.try_recv().is_err());
    }
    let pending = app.oneshot(request("/api/v1/sessions/session?presentation=compact&expectedEpoch=epoch&expectedIncarnation=3&includeProcessFrom=op"));
    let reply = async {
        let bridge::BridgeInput::SessionViewRequest {
            expected_owner,
            response,
            ..
        } = commands.recv().await.unwrap()
        else {
            panic!("expected fenced view")
        };
        assert_eq!(
            expected_owner,
            Some(SessionResourceOwner::new("epoch", "session", 3))
        );
        response.send(Ok(raw_view())).unwrap();
    };
    let (response, ()) = tokio::join!(pending, reply);
    let response = response.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let compact = body(response).await;
    assert_eq!(compact["collapsedTurns"][0]["processIncluded"], true);
    assert_eq!(compact["collapsedTurns"][0]["processCount"], 23);
    assert_eq!(compact["timeline"].as_array().unwrap().len(), 25);
    assert_eq!(
        compact["terminals"]["terminal-0"]["output"],
        "hidden terminal output"
    );
    assert!(commands.try_recv().is_err());
}

#[tokio::test]
async fn compact_http_released_process_exclusions_require_owner_and_valid_operation_ids() {
    let (hub, mut commands) = tests::observation_hub(4).await;
    let options = Options::try_parse_from(["attyd", "--", "fake-agent"]).unwrap();
    let app = app_router(&options, hub);
    for query in [
        "presentation=compact&excludeProcessFor=%5B%22op%22%5D",
        "presentation=full&expectedEpoch=epoch&expectedIncarnation=3&excludeProcessFor=%5B%22op%22%5D",
        "presentation=compact&expectedEpoch=epoch&expectedIncarnation=3&excludeProcessFor=op",
        "presentation=compact&expectedEpoch=epoch&expectedIncarnation=3&excludeProcessFor=%7B%7D",
        "presentation=compact&expectedEpoch=epoch&expectedIncarnation=3&excludeProcessFor=%5B1%5D",
        "presentation=compact&expectedEpoch=epoch&expectedIncarnation=3&excludeProcessFor=%5B%22%22%5D",
        "presentation=compact&expectedEpoch=epoch&expectedIncarnation=3&excludeProcessFor=%5B%22%5Cu0000%22%5D",
    ] {
        let response = app
            .clone()
            .oneshot(request(&format!("/api/v1/sessions/session?{query}")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        assert!(commands.try_recv().is_err());
    }

    let pending = app.oneshot(request("/api/v1/sessions/session?presentation=compact&expectedEpoch=epoch&expectedIncarnation=3&includeProcessFrom=op&excludeProcessFor=%5B%22op%22%5D"));
    let reply = async {
        let bridge::BridgeInput::SessionViewRequest {
            expected_owner,
            response,
            ..
        } = commands.recv().await.unwrap()
        else {
            panic!("expected fenced view")
        };
        assert_eq!(
            expected_owner,
            Some(SessionResourceOwner::new("epoch", "session", 3))
        );
        response.send(Ok(raw_view())).unwrap();
    };
    let (response, ()) = tokio::join!(pending, reply);
    let response = response.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let compact = body(response).await;
    assert!(!compact.to_string().contains("hidden"));
    assert!(
        compact["collapsedTurns"][0]
            .get("processIncluded")
            .is_none()
    );
    assert_eq!(compact["collapsedTurns"][0]["operationId"], "op");
    assert_eq!(compact["collapsedTurns"][0]["processCount"], 23);
    assert_eq!(
        compact["collapsedTurns"][0]["visibleRanges"],
        json!([{"start": 0, "end": 1}, {"start": 1, "end": 2}])
    );
    assert_eq!(compact["timeline"].as_array().unwrap().len(), 2);
    assert!(commands.try_recv().is_err());
}
