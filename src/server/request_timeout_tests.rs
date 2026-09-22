//! HTTP waiting has a deadline; accepted bridge work and event streams do not.
use super::*;
use tower::ServiceExt;

fn request(method: &str, uri: &str, body: Body) -> Request {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "localhost:7331")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::IF_MATCH, "\"history\"")
        .header("idempotency-key", "intent")
        .body(body)
        .unwrap()
}

async fn json_body(response: Response) -> serde_json::Value {
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test(start_paused = true)]
async fn slow_json_response_can_complete_before_the_deadline() {
    let (hub, mut commands) = tests::observation_hub(1).await;
    let app = app_router(&hub.options, hub.clone());
    let mut pending = Box::pin(app.oneshot(request("GET", "/api/v1/sessions", Body::empty())));
    assert!(futures::poll!(pending.as_mut()).is_pending());
    let bridge::BridgeInput::BusinessRequest { response, .. } = commands.recv().await.unwrap()
    else {
        panic!("expected list request")
    };
    tokio::time::advance(Duration::from_secs(59)).await;
    assert!(futures::poll!(pending.as_mut()).is_pending());
    response.send(Ok(json!({"sessions": []}))).unwrap();
    let response = pending.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await, json!({"sessions": []}));
}

#[tokio::test(start_paused = true)]
async fn json_read_times_out_with_a_retryable_structured_error() {
    let (hub, mut commands) = tests::observation_hub(1).await;
    let app = app_router(&hub.options, hub.clone());
    let mut pending = Box::pin(app.oneshot(request("GET", "/api/v1/sessions", Body::empty())));
    assert!(futures::poll!(pending.as_mut()).is_pending());
    let bridge::BridgeInput::BusinessRequest { response, .. } = commands.recv().await.unwrap()
    else {
        panic!("expected list request")
    };
    tokio::time::advance(Duration::from_secs(60)).await;
    let completed = futures::poll!(pending.as_mut());
    assert!(
        completed.is_ready(),
        "ordinary JSON requests need a bounded wait"
    );
    let std::task::Poll::Ready(Ok(result)) = completed else {
        unreachable!()
    };
    assert_eq!(result.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(result.headers()[header::CACHE_CONTROL], "no-store");
    let error = json_body(result).await;
    assert_eq!(error["code"], "request_timeout");
    assert_eq!(error["timeoutMs"], 60_000);
    assert_eq!(error["operationMayContinue"], false);
    assert!(error["error"].as_str().unwrap().contains("retry"));
    assert!(response.is_closed());
}

#[tokio::test(start_paused = true)]
async fn process_page_wait_is_bounded_and_drops_only_its_view_waiter() {
    let (hub, mut commands) = tests::observation_hub(1).await;
    let app = app_router(&hub.options, hub.clone());
    let mut pending = Box::pin(app.oneshot(request(
        "GET",
        "/api/v1/sessions/session/turns/op/process?expectedEpoch=epoch&expectedIncarnation=1&historyRevision=history&offset=0",
        Body::empty(),
    )));
    assert!(futures::poll!(pending.as_mut()).is_pending());
    let bridge::BridgeInput::SessionViewRequest { response, .. } = commands.recv().await.unwrap()
    else {
        panic!("expected process view query")
    };
    tokio::time::advance(Duration::from_secs(60)).await;
    let completed = futures::poll!(pending.as_mut());
    assert!(
        completed.is_ready(),
        "process pages must stop waiting at their deadline"
    );
    let std::task::Poll::Ready(Ok(result)) = completed else {
        unreachable!()
    };
    assert_eq!(result.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(response.is_closed());
    assert!(
        commands.try_recv().is_err(),
        "timeout must not send turn cancellation"
    );
}

#[tokio::test(start_paused = true)]
async fn mutation_timeout_does_not_cancel_or_redispatch_accepted_bridge_commands() {
    for (method, uri, body) in [
        (
            "POST",
            "/api/v1/sessions/session/turns",
            json!({"prompt": [{"type": "text", "text": "work"}]}),
        ),
        (
            "DELETE",
            "/api/v1/sessions/session",
            serde_json::Value::Null,
        ),
    ] {
        let (hub, mut commands) = tests::observation_hub(1).await;
        let app = app_router(&hub.options, hub.clone());
        let mut pending = Box::pin(app.oneshot(request(method, uri, Body::from(body.to_string()))));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        let accepted = commands.recv().await.unwrap();
        assert!(matches!(
            accepted,
            bridge::BridgeInput::TurnRequest { .. } | bridge::BridgeInput::BusinessRequest { .. }
        ));
        tokio::time::advance(Duration::from_secs(60)).await;
        let completed = futures::poll!(pending.as_mut());
        assert!(
            completed.is_ready(),
            "mutation responses also need a bounded wait"
        );
        let std::task::Poll::Ready(Ok(result)) = completed else {
            unreachable!()
        };
        assert_eq!(result.status(), StatusCode::GATEWAY_TIMEOUT);
        let error = json_body(result).await;
        assert_eq!(error["code"], "request_timeout");
        assert_eq!(error["operationMayContinue"], true);
        assert!(error["error"].as_str().unwrap().contains("before retrying"));
        assert!(
            commands.try_recv().is_err(),
            "timeout must not cancel or repeat accepted work"
        );
        match accepted {
            bridge::BridgeInput::TurnRequest { response, .. } => assert!(response.is_closed()),
            bridge::BridgeInput::BusinessRequest { response, .. } => assert!(response.is_closed()),
            _ => unreachable!(),
        }
    }
}

#[tokio::test(start_paused = true)]
async fn deadline_also_bounds_waiting_for_an_incomplete_json_body() {
    let (hub, mut commands) = tests::observation_hub(1).await;
    let app = app_router(&hub.options, hub.clone());
    let body = Body::from_stream(futures::stream::pending::<Result<String, Infallible>>());
    let mut pending = Box::pin(app.oneshot(request("POST", "/api/v1/sessions", body)));
    assert!(futures::poll!(pending.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(60)).await;
    let completed = futures::poll!(pending.as_mut());
    assert!(
        completed.is_ready(),
        "the deadline starts before body extraction"
    );
    let std::task::Poll::Ready(Ok(result)) = completed else {
        unreachable!()
    };
    assert_eq!(result.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(commands.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn session_sse_handshake_and_stream_outlive_the_json_deadline() {
    let (hub, mut commands) = tests::observation_hub(1).await;
    let app = app_router(&hub.options, hub.clone());
    let mut pending = Box::pin(app.oneshot(request(
        "GET",
        "/api/v1/sessions/session/events",
        Body::empty(),
    )));
    assert!(futures::poll!(pending.as_mut()).is_pending());
    let bridge::BridgeInput::ObserveSession {
        observer_id,
        lease,
        reply,
        ..
    } = commands.recv().await.unwrap()
    else {
        panic!("expected observer")
    };
    tokio::time::advance(Duration::from_secs(61)).await;
    assert!(futures::poll!(pending.as_mut()).is_pending());
    assert!(!lease.is_cancelled());
    hub.publish(1, tests::observer_ready(observer_id, 10)).await;
    reply.send(Ok(())).unwrap();
    let response = pending.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let mut body = response.into_body().into_data_stream();
    let reset = futures::StreamExt::next(&mut body).await.unwrap().unwrap();
    assert!(
        std::str::from_utf8(&reset)
            .unwrap()
            .contains("bridge/session_reset")
    );
    tokio::time::advance(Duration::from_secs(61)).await;
    assert!(!lease.is_cancelled());
    let keepalive = futures::StreamExt::next(&mut body).await.unwrap().unwrap();
    assert!(!keepalive.is_empty());
    drop(body);
    assert!(lease.is_cancelled());
}

#[tokio::test(start_paused = true)]
async fn global_sse_handshake_is_exempt_from_the_json_deadline() {
    let (hub, _commands) = tests::observation_hub(1).await;
    let app = app_router(&hub.options, hub.clone());
    let state = hub.state.lock().await;
    let mut pending = Box::pin(app.oneshot(request("GET", "/api/v1/events", Body::empty())));
    assert!(futures::poll!(pending.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(61)).await;
    assert!(futures::poll!(pending.as_mut()).is_pending());
    drop(state);
    let response = pending.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
}
