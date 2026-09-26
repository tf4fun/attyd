use super::*;
use clap::Parser;
use tower::ServiceExt;

fn block(mime: &str) -> serde_json::Value {
    json!({"type":"resource","resource":{"uri":"attyd://attachment/%E4%B8%AD%E6%96%87.md","mimeType":mime,"text":"# 中文正文 📝"}})
}

fn raw_view(block: serde_json::Value) -> serde_json::Value {
    json!({"bridgeEpoch":"epoch","session":{"sessionId":"session","incarnation":3,
        "viewRevision":5,"historyRevision":"history","phase":"ready","activeTurn":null},
        "baseline":{"updates":[{"sessionUpdate":"user_message_chunk","content":block}]}})
}

fn projected(block: serde_json::Value) -> serde_json::Value {
    let mut view = business_session_view(&raw_view(block)).unwrap();
    crate::attachments::project(&mut view);
    view["timeline"][0]["content"].clone()
}

fn request(uri: &str) -> Request {
    Request::builder()
        .uri(uri)
        .header(header::HOST, "localhost:7331")
        .body(Body::empty())
        .unwrap()
}

async fn bytes(response: Response) -> axum::body::Bytes {
    axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap()
}

#[tokio::test]
async fn attachment_http_serves_cached_bytes_with_native_headers_and_an_owner_fence() {
    let (hub, mut commands) = tests::observation_hub(4).await;
    let options = Options::try_parse_from(["attyd", "--", "fake-agent"]).unwrap();
    let app = app_router(&options, hub);
    for (mime, served_mime, active, binary) in [
        ("text/markdown", "text/markdown;charset=utf-8", false, false),
        ("text/html", "text/html;charset=utf-8", true, false),
        ("TEXT/HTML", "TEXT/HTML;charset=utf-8", true, false),
        (
            "TEXT/HTML \t; CHARSET=\"iso-8859-1\"",
            "TEXT/HTML;charset=utf-8",
            true,
            false,
        ),
        (
            "image/svg+xml ; charset=\"utf-8\"",
            "image/svg+xml ; charset=\"utf-8\"",
            true,
            true,
        ),
    ] {
        let mut original = block(mime);
        if binary {
            let resource = original["resource"].as_object_mut().unwrap();
            resource.remove("text");
            resource.insert("blob".into(), json!("AQID"));
        }
        let reference = crate::attachments::reference(&projected(original.clone())).unwrap();
        let uri = format!(
            "/api/v1/sessions/session/attachments/{}?expectedEpoch=epoch&expectedIncarnation=3",
            reference.id
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
                panic!("expected cached view")
            };
            assert_eq!(
                expected_owner,
                Some(SessionResourceOwner::new("epoch", "session", 3))
            );
            assert!(cwd.is_none());
            response.send(Ok(raw_view(original))).unwrap();
        };
        let (response, ()) = tokio::join!(pending, reply);
        let response = response.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], served_mime);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert_eq!(
            response.headers()[header::CONTENT_DISPOSITION],
            "inline; filename*=UTF-8''%E4%B8%AD%E6%96%87.md"
        );
        if active {
            assert_eq!(
                response.headers()[header::CONTENT_SECURITY_POLICY],
                "sandbox allow-downloads"
            );
        }
        let expected: &[u8] = if binary {
            &[1, 2, 3]
        } else {
            "# 中文正文 📝".as_bytes()
        };
        assert_eq!(bytes(response).await.as_ref(), expected);
    }
}

#[tokio::test]
async fn attachment_http_never_reloads_a_retired_owner_or_reads_a_filesystem_path() {
    let (hub, mut commands) = tests::observation_hub(4).await;
    let options = Options::try_parse_from(["attyd", "--", "fake-agent"]).unwrap();
    let app = app_router(&options, hub);
    let id = "a".repeat(64);
    for uri in [
        format!("/api/v1/sessions/session/attachments/{id}"),
        "/api/v1/sessions/session/attachments/etc-passwd?expectedEpoch=epoch&expectedIncarnation=3"
            .into(),
        format!(
            "/api/v1/sessions/session/attachments/{id}?expectedEpoch=epoch&expectedIncarnation=0"
        ),
    ] {
        assert_eq!(
            app.clone().oneshot(request(&uri)).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
        assert!(commands.try_recv().is_err());
    }
    let uri = format!(
        "/api/v1/sessions/session/attachments/{id}?expectedEpoch=epoch&expectedIncarnation=3"
    );
    for answer in [
        Ok(raw_view(block("text/plain"))),
        Err(bridge::SessionViewError::Retired(
            SessionResourceOwner::new("epoch", "session", 3),
        )),
    ] {
        let expected = if answer.is_ok() {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::CONFLICT
        };
        let pending = app.clone().oneshot(request(&uri));
        let reply = async {
            let bridge::BridgeInput::SessionViewRequest {
                expected_owner,
                response,
                ..
            } = commands.recv().await.unwrap()
            else {
                panic!("expected fenced lookup")
            };
            assert!(expected_owner.is_some());
            response.send(answer).unwrap();
        };
        let (response, ()) = tokio::join!(pending, reply);
        assert_eq!(response.unwrap().status(), expected);
        assert!(commands.try_recv().is_err());
    }
}

#[tokio::test]
async fn prompt_reuse_resolves_original_acp_content_before_admission() {
    let (hub, mut commands) = tests::observation_hub(4).await;
    let options = Options::try_parse_from(["attyd", "--", "fake-agent"]).unwrap();
    let app = app_router(&options, hub);
    let original = block("text/markdown");
    let prompt = json!({"prompt":[projected(original.clone())]});
    let pending = app.oneshot(
        Request::builder()
            .method("POST")
            .uri("/api/v1/sessions/session/turns")
            .header(header::HOST, "localhost:7331")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::IF_MATCH, "\"history\"")
            .header("idempotency-key", "intent")
            .body(Body::from(prompt.to_string()))
            .unwrap(),
    );
    let reply = async {
        let bridge::BridgeInput::SessionViewRequest { response, .. } =
            commands.recv().await.unwrap()
        else {
            panic!("expected reference lookup")
        };
        response.send(Ok(raw_view(original.clone()))).unwrap();
        let bridge::BridgeInput::TurnRequest {
            prompt,
            history_revision,
            client_intent_id,
            response,
            ..
        } = commands.recv().await.unwrap()
        else {
            panic!("expected turn admission")
        };
        assert_eq!(prompt, vec![original]);
        assert_eq!(history_revision, "history");
        assert_eq!(client_intent_id, "intent");
        response.send(Ok(json!({"operationId":"op"}))).unwrap();
    };
    let (response, ()) = tokio::join!(pending, reply);
    assert_eq!(response.unwrap().status(), StatusCode::ACCEPTED);
}

#[test]
fn live_attachment_events_do_not_ship_inline_payloads() {
    let event = json!({"type":"bridge/session_delta","bridgeEpoch":"epoch","sessionId":"session","sessionIncarnation":3,
        "fromRevision":4,"viewRevision":5,"change":{"kind":"turn_update","update":{"sessionUpdate":"agent_message_chunk","content":block("text/plain")}}});
    let (_, encoded) = business_session_event(&event.to_string()).unwrap();
    assert!(!encoded.contains("中文正文"));
    let event: serde_json::Value = serde_json::from_str(&encoded).unwrap();
    assert!(crate::attachments::reference(&event["change"]["update"]["content"]).is_some());
}

#[tokio::test]
async fn ordinary_views_contain_references_and_explicit_export_gets_full_content() {
    let (hub, mut commands) = tests::observation_hub(4).await;
    let options = Options::try_parse_from(["attyd", "--", "fake-agent"]).unwrap();
    let app = app_router(&options, hub);
    for suffix in [
        "?presentation=compact",
        "",
        "?includeAttachmentContent=true",
    ] {
        let pending = app
            .clone()
            .oneshot(request(&format!("/api/v1/sessions/session{suffix}")));
        let reply = async {
            let bridge::BridgeInput::SessionViewRequest { response, .. } =
                commands.recv().await.unwrap()
            else {
                panic!("expected view");
            };
            response.send(Ok(raw_view(block("text/markdown")))).unwrap();
        };
        let (response, ()) = tokio::join!(pending, reply);
        let payload: serde_json::Value =
            serde_json::from_slice(&bytes(response.unwrap()).await).unwrap();
        let content = &payload["timeline"][0]["content"];
        if suffix.contains("includeAttachmentContent") {
            assert_eq!(content, &block("text/markdown"));
        } else {
            assert!(crate::attachments::reference(content).is_some());
            assert!(!payload.to_string().contains("中文正文"));
        }
    }
}
