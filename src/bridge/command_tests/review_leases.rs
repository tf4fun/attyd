//! A same-key retry must protect the owner it originally returned, including
//! after its public record has been acknowledged (protocol §4.2 and §5.2).
use super::*;

#[tokio::test]
async fn a_retried_fork_grants_a_new_first_subscription_lease() {
    retry_restores_observation(true, false).await;
}

#[tokio::test]
async fn an_acknowledged_fork_retry_still_grants_its_owner_a_lease() {
    retry_restores_observation(true, true).await;
}

#[tokio::test]
async fn an_acknowledged_open_retry_still_grants_its_owner_a_lease() {
    retry_restores_observation(false, true).await;
}

async fn retry_restores_observation(fork: bool, acknowledge: bool) {
    let mut peer = Peer::start(-1, json!({"sessionCapabilities": {"fork": {}}})).await;
    create(&mut peer).await;
    let epoch = epoch(&peer).await;
    let source_tag = tag(&mut peer).await;
    // Keep the source separate from the unobserved target in the fork case.
    // An open's source is its target, so it must have no extra observer.
    let source = if fork {
        let source_owner = owner(&mut peer, SESSION).await;
        Some(HttpStream::open(&peer.router, &[source_owner]).await)
    } else {
        None
    };
    let (path, target, body) = if fork {
        (
            format!("/api/v2/sessions/{SESSION}/fork"),
            "retried-fork",
            json!({}),
        )
    } else {
        (
            format!("/api/v2/sessions/{SESSION}/open"),
            SESSION,
            json!({"cwd": "/workspace"}),
        )
    };
    let tag = fork.then_some(source_tag.as_str());
    let (status, accepted) =
        command(&peer, "POST", &path, Some("lease-retry"), tag, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    if fork {
        let request = peer.next_request().await;
        assert_eq!(request["method"], "session/fork");
        peer.reply(&request, json!({"sessionId": target})).await;
    }
    let record = finished(&mut peer, "lease-retry").await;
    assert_eq!(record["status"], "succeeded");
    assert_eq!(record["result"]["sessionId"], target);
    let incarnation = record["result"]["incarnation"].as_u64().unwrap();
    let target_owner = owner_scope(target, &epoch, incarnation);

    // Consume the original grace lease through a real subscription, then
    // disconnect while automatic retirement is disabled. This removes the
    // original protection without depending on a short timer boundary.
    let initial = HttpStream::open(&peer.router, &[target_owner.clone()]).await;
    drop(initial);
    peer.no_request().await;
    if acknowledge {
        let (status, response) = send(
            &peer.router,
            "DELETE",
            &format!("/api/v2/requests/{}", record["requestId"].as_str().unwrap()),
            &[],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert!(find(&peer.check().await, "lease-retry").is_none());
    }

    let (status, retried) = command(&peer, "POST", &path, Some("lease-retry"), tag, body).await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    assert_eq!(retried["result"]["disposition"], "duplicate");
    assert_eq!(retried["result"]["requestId"], record["requestId"]);
    if acknowledge {
        assert_eq!(retried["result"]["resultExpired"], true);
    }
    {
        let state = peer.state.upgrade().unwrap();
        let mut state = state.lock().await;
        state.observer_timeout = Some(0);
        refresh_observer_timers(&mut state);
    }
    // A duplicate dispatch would be another defect; a lease renewal should
    // produce no ACP request and keep this idle owner held at timeout zero.
    peer.no_request().await;
    let documents = peer.check().await;
    assert!(
        documents.contains_key(&session_scope(target)),
        "a duplicate {}{} must grant a fresh lease on its original owner",
        if acknowledge { "acknowledged " } else { "" },
        if fork { "fork" } else { "open" },
    );
    assert_eq!(
        documents[&session_scope(target)]["incarnation"],
        incarnation
    );

    // The new stream takes the renewed lease over; disconnecting it must
    // again permit retirement, so the retry cannot leave a permanent lease.
    let subscribed = HttpStream::open(&peer.router, &[target_owner]).await;
    drop(subscribed);
    peer.until("the retried owner's new stream releases it", |documents| {
        !documents.contains_key(&session_scope(target))
    })
    .await;
    drop(source);
    peer.stop().await;
}
