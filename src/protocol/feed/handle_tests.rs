//! The shared tree feed: waiting, wakeups and closing a stream.

use std::time::Duration;

use serde_json::json;
use tokio::time::timeout;

use super::*;
use crate::protocol::documents::{AgentInfo, AgentLaunch, AgentPhase};
use crate::protocol::tree_stream::client::Client;
use crate::protocol::tree_stream::params::parse_stream_query;
use crate::protocol::tree_stream::wire::Change;
use crate::session_registry::SessionRegistry;

const WAIT: Duration = Duration::from_secs(5);
const SESSION: &str = "/sessions/s";

fn agent() -> AgentInfo {
    AgentInfo {
        phase: AgentPhase::Ready,
        error: None,
        launch: AgentLaunch::default(),
        initialize: None,
        authenticated_method_id: None,
    }
}

fn follow(path: &str) -> StreamQuery {
    parse_stream_query(&format!("follow={path}")).unwrap()
}

fn registry() -> SessionRegistry {
    let mut registry = SessionRegistry::new("epoch");
    registry
        .open_new("epoch", "s", "/work", json!({ "sessionId": "s" }))
        .unwrap();
    registry
}

fn changed(registry: &mut SessionRegistry) {
    registry
        .append_session_update(
            "s",
            json!({ "sessionUpdate": "agent_message_chunk", "messageId": "m",
                    "content": { "type": "text", "text": "hi" } }),
        )
        .unwrap();
}

#[tokio::test]
async fn a_subscription_waits_for_the_next_event_and_a_commit_wakes_it() {
    let handle = FeedHandle::new();
    let mut registry = registry();
    handle.commit(&registry, &agent(), 0);
    let subscription = handle.subscribe_tree(&follow(SESSION));
    let mut client = Client::following(&[SESSION]);
    let aligned = timeout(WAIT, subscription.next()).await.unwrap().unwrap();
    client.apply(&aligned).unwrap();
    assert!(client.value(SESSION).is_some());

    // Nothing is queued: the next event is waited for, not polled.
    let waiting = tokio::spawn({
        let subscription = subscription.clone();
        async move { subscription.next().await }
    });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    changed(&mut registry);
    handle.commit(&registry, &agent(), 0);
    let event = timeout(WAIT, waiting).await.unwrap().unwrap().unwrap();
    client.apply(&event).unwrap();
    assert_eq!(event.rev, 2);
    assert_eq!(client.value(SESSION).unwrap()["entries"][0]["chunks"][0]["content"]["text"], "hi");
    handle.with_tree(|tree| assert_eq!(client.value(SESSION), tree.get(SESSION).map(|node| node.to_value()).as_ref()));
}

#[tokio::test]
async fn dropping_a_subscription_forgets_it() {
    let handle = FeedHandle::new();
    handle.commit(&registry(), &agent(), 0);
    let subscription = handle.subscribe_tree(&follow("/global"));
    assert_eq!(handle.subscribers(), 1);
    drop(subscription);
    assert_eq!(handle.subscribers(), 0);
}

#[tokio::test]
async fn closing_the_feed_ends_every_subscription() {
    let handle = FeedHandle::new();
    handle.commit(&registry(), &agent(), 0);
    let subscription = handle.subscribe_tree(&follow("/global"));
    let _ = subscription.next().await;
    let waiting = tokio::spawn({
        let subscription = subscription.clone();
        async move { subscription.next().await }
    });
    tokio::task::yield_now().await;
    handle.close();
    assert_eq!(timeout(WAIT, waiting).await.unwrap().unwrap(), None);
    // A subscription opened after the close has nothing to wait for or load.
    let late = handle.subscribe_tree(&follow("/sessions/missing"));
    assert_eq!(timeout(WAIT, late.next()).await.unwrap(), None);
    assert!(!handle.followed_sessions().contains_key("missing"));
    let read = handle.subscribe_tree_reading(&follow("/global"));
    let mut client = Client::following(&["/global"]);
    client.apply(&read.next().await.expect("the final cut remains readable")).unwrap();
    handle.with_tree(|tree| assert_eq!(client.value("/global"), tree.get("/global").map(|node| node.to_value()).as_ref()));
    assert!(read.next().await.is_none());
    assert!(handle.take_touched().is_empty());
    let missing = handle.subscribe_tree_reading(&parse_stream_query(
        "follow=/views/missing/output/latest&cwd=missing:/work",
    ).unwrap());
    assert!(missing.next().await.is_some(), "the final cut reports absence");
    assert!(missing.next().await.is_none());
    assert!(handle.followed_sessions().is_empty(), "a final read requests no load");
    assert!(handle.take_touched().is_empty());
    handle.with_tree(|tree| assert!(tree.get("/views/missing").is_none()));
}

#[tokio::test]
async fn closing_drains_the_final_commit_before_returning_none() {
    let handle = FeedHandle::new();
    let mut registry = registry();
    handle.commit(&registry, &agent(), 0);
    let subscription = handle.subscribe_tree(&follow(SESSION));
    let mut client = Client::following(&[SESSION]);
    client.apply(&subscription.next().await.unwrap()).unwrap();
    let (armed, ready) = tokio::sync::oneshot::channel();
    let waiting = tokio::spawn(async move {
        armed.send(()).unwrap();
        let final_event = subscription.next().await;
        let closed = subscription.next().await;
        (final_event, closed)
    });
    ready.await.unwrap();
    // Publish and close without yielding: the final event remains owed to the waiting client.
    changed(&mut registry);
    handle.commit(&registry, &agent(), 0);
    handle.close();
    let (event, closed) = timeout(WAIT, waiting).await.unwrap().unwrap();
    client.apply(&event.expect("the final commit must drain before close")).unwrap();
    assert_eq!(client.value(SESSION).unwrap()["entries"][0]["chunks"][0]["content"]["text"], "hi");
    assert_eq!(closed, None);
}

#[tokio::test]
async fn a_subscription_sees_only_what_it_follows() {
    let handle = FeedHandle::new();
    let mut registry = registry();
    handle.commit(&registry, &agent(), 0);
    let global = handle.subscribe_tree(&follow("/global"));
    let mut client = Client::following(&["/global"]);
    client.apply(&global.next().await.unwrap()).unwrap();
    changed(&mut registry);
    handle.commit(&registry, &agent(), 0);
    assert!(global.try_next().is_none(), "session text does not change global");
    registry.start_login_terminal("login", "agent-login");
    handle.commit(&registry, &agent(), 0);
    let event = timeout(WAIT, global.next()).await.unwrap().unwrap();
    assert!(event.changes.as_ref().unwrap().iter().all(|change| change.path() == "/global"));
    client.apply(&event).unwrap();
    assert!(client.value("/global").unwrap()["terminals"].get("login").is_some());
    assert!(timeout(Duration::from_millis(50), global.next()).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn a_waiting_subscription_is_sent_the_checkpoint_when_it_is_due() {
    let handle = FeedHandle::new();
    let mut registry = registry();
    handle.commit(&registry, &agent(), 0);
    let subscription = handle.subscribe_tree(&follow(SESSION));
    let mut client = Client::following(&[SESSION]);
    client.apply(&subscription.next().await.unwrap()).unwrap();
    changed(&mut registry);
    handle.commit(&registry, &agent(), 0);
    client.apply(&subscription.next().await.unwrap()).unwrap();

    let waiting = tokio::spawn({
        let subscription = subscription.clone();
        async move { subscription.next().await }
    });
    tokio::time::advance(Duration::from_millis(499)).await;
    assert!(!waiting.is_finished(), "not yet: the path changed 499 ms ago");
    tokio::time::advance(Duration::from_millis(2)).await;
    let event = waiting.await.unwrap().unwrap();
    client.apply(&event).unwrap();
    let check = &event.check.as_ref().expect("a checkpoint")[0];
    assert_eq!((check.path.as_str(), check.at), (SESSION, 2));
    handle.with_tree(|tree| {
        assert_eq!(check.hash, crate::protocol::digest::wire(&tree.get(SESSION).unwrap().hash()));
    });
}

#[tokio::test]
async fn ending_the_wait_for_a_session_wakes_the_stream_that_waits() {
    let handle = FeedHandle::new();
    handle.commit(&SessionRegistry::new("epoch"), &agent(), 0);
    let subscription = handle.subscribe_tree(&follow(SESSION));
    let mut client = Client::following(&[SESSION]);
    client.apply(&subscription.next().await.unwrap()).unwrap();
    assert_eq!(client.value(SESSION), None, "initial absence is not a final end");
    assert_eq!(handle.followed_sessions()["s"].len(), 1);
    let waiting = tokio::spawn({
        let subscription = subscription.clone();
        async move { subscription.next().await }
    });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    handle.end_waiting("s", &End { reason: EndReason::LoadFailed, error: None });
    let event = timeout(WAIT, waiting).await.unwrap().unwrap().unwrap();
    assert!(matches!(
        &event.changes.as_ref().unwrap()[0],
        Change::End { end, .. } if end.reason == TreeEndReason::LoadFailed
    ), "{event:?}");
    client.apply(&event).unwrap();
    assert!(!handle.followed_sessions().contains_key("s"));
}

#[test]
fn admission_reads_only_the_committed_target() {
    let handle = FeedHandle::new();
    let registry = registry();
    handle.with_tree(|tree| assert!(tree.get(SESSION).is_none(), "nothing is committed yet"));
    handle.commit(&registry, &agent(), 0);
    handle.with_tree(|tree| {
        let session = tree.get(SESSION).expect("a held session");
        assert_eq!(session.field("sessionId"), Some(&json!("s")));
        assert_eq!(session.child("op").unwrap().as_value().unwrap().0["status"], "completed");
        assert_eq!(session.child("interactions").unwrap().kind_name(), "map");
        assert!(tree.get("/global").is_some());
        assert!(tree.get("/sessions/unheld").is_none());
    });
}
