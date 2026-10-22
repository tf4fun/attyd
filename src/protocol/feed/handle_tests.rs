//! The shared handle of the feed: what a stream waits on (docs/protocol.md §5).

use std::time::Duration;

use serde_json::json;
use tokio::time::timeout;

use super::*;
use crate::protocol::documents::{AgentInfo, AgentLaunch, AgentPhase, Scope};
use crate::protocol::subscription::Subscribed;
use crate::session_registry::SessionRegistry;

const WAIT: Duration = Duration::from_secs(5);

fn agent() -> AgentInfo {
    AgentInfo {
        phase: AgentPhase::Ready,
        error: None,
        launch: AgentLaunch::default(),
        initialize: None,
        authenticated_method_id: None,
    }
}

fn follow(scope: Scope) -> Vec<Subscribed> {
    vec![Subscribed {
        scope,
        have: None,
        cwd: None,
    }]
}

fn registry() -> SessionRegistry {
    let mut registry = SessionRegistry::new("epoch");
    registry
        .open_new("epoch", "s", "/work", json!({ "sessionId": "s" }))
        .unwrap();
    registry
}

#[tokio::test]
async fn a_subscription_waits_for_the_next_event_and_a_commit_wakes_it() {
    let handle = FeedHandle::new("process");
    let mut registry = registry();
    handle.commit(&registry, &agent(), 0);
    let subscription = handle.subscribe(&follow(Scope::Session("s".into())));
    let aligned = timeout(WAIT, subscription.next()).await.unwrap().unwrap();
    assert!(matches!(aligned, StreamEvent::Snapshot { .. }));

    // Nothing is queued: the next event is waited for, not polled.
    let waiting = tokio::spawn({
        let subscription = subscription.clone();
        async move { subscription.next().await }
    });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    registry
        .append_session_update(
            "s",
            json!({ "sessionUpdate": "agent_message_chunk", "messageId": "m",
                    "content": { "type": "text", "text": "hi" } }),
        )
        .unwrap();
    handle.commit(&registry, &agent(), 0);
    let event = timeout(WAIT, waiting).await.unwrap().unwrap().unwrap();
    assert!(matches!(event, StreamEvent::Ops { rev: 1, .. }));
}

#[tokio::test]
async fn dropping_a_subscription_forgets_it() {
    let handle = FeedHandle::new("process");
    handle.commit(&registry(), &agent(), 0);
    let subscription = handle.subscribe(&follow(Scope::Global));
    assert_eq!(handle.subscribers(), 1);
    drop(subscription);
    assert_eq!(handle.subscribers(), 0);
}

#[tokio::test]
async fn closing_the_feed_ends_every_subscription() {
    let handle = FeedHandle::new("process");
    handle.commit(&registry(), &agent(), 0);
    let subscription = handle.subscribe(&follow(Scope::Global));
    let _ = subscription.next().await;
    let waiting = tokio::spawn({
        let subscription = subscription.clone();
        async move { subscription.next().await }
    });
    tokio::task::yield_now().await;
    handle.close();
    assert_eq!(timeout(WAIT, waiting).await.unwrap().unwrap(), None);
    // A subscription opened after the close has nothing to wait for.
    let late = handle.subscribe(&follow(Scope::Global));
    assert_eq!(timeout(WAIT, late.next()).await.unwrap(), None);
}

#[tokio::test]
async fn a_subscription_sees_only_what_it_follows() {
    let handle = FeedHandle::new("process");
    let mut registry = registry();
    handle.commit(&registry, &agent(), 0);
    let global = handle.subscribe(&follow(Scope::Global));
    let _ = global.next().await;
    registry.start_login_terminal("login", "agent-login");
    handle.commit(&registry, &agent(), 0);
    let event = timeout(WAIT, global.next()).await.unwrap().unwrap();
    assert_eq!(event.scope(), "global");
    assert!(timeout(Duration::from_millis(50), global.next()).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn a_waiting_subscription_is_sent_the_checkpoint_when_it_is_due() {
    let handle = FeedHandle::new("process");
    let mut registry = registry();
    handle.commit(&registry, &agent(), 0);
    let subscription = handle.subscribe(&follow(Scope::Session("s".into())));
    let _ = subscription.next().await;

    registry
        .append_session_update(
            "s",
            json!({ "sessionUpdate": "agent_message_chunk", "messageId": "m",
                    "content": { "type": "text", "text": "hi" } }),
        )
        .unwrap();
    handle.commit(&registry, &agent(), 0);
    assert!(matches!(
        subscription.next().await,
        Some(StreamEvent::Ops { rev: 1, .. })
    ));

    let waiting = tokio::spawn({
        let subscription = subscription.clone();
        async move { subscription.next().await }
    });
    tokio::time::advance(Duration::from_millis(499)).await;
    assert!(!waiting.is_finished(), "not yet: the scope changed 499 ms ago");
    tokio::time::advance(Duration::from_millis(2)).await;
    let event = waiting.await.unwrap();
    assert!(
        matches!(event, Some(StreamEvent::Checkpoint { rev: 1, .. })),
        "{event:?}"
    );
}
