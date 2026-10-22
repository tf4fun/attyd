//! The feed (docs/protocol.md §5): after each committed change of the bridge's
//! state it publishes every scope that changed and queues the events of the
//! subscribers that follow them, so that no transition goes unpublished.

use serde_json::json;

use super::*;
use crate::protocol::documents::{AgentInfo, AgentLaunch, AgentPhase};
use crate::protocol::engine::StreamEvent;
use crate::protocol::subscription::Subscribed;
use crate::runtime_state::SessionLifecycle;
use crate::session_registry::SessionRegistry;

fn agent() -> AgentInfo {
    AgentInfo {
        phase: AgentPhase::Ready,
        error: None,
        launch: AgentLaunch::default(),
        initialize: None,
        authenticated_method_id: None,
    }
}

fn registry() -> SessionRegistry {
    let mut registry = SessionRegistry::new("epoch");
    registry
        .open_new("epoch", "s", "/work", json!({ "sessionId": "s" }))
        .unwrap();
    registry
}

fn follow(scopes: &[Scope]) -> Vec<Subscribed> {
    scopes
        .iter()
        .map(|scope| Subscribed {
            scope: scope.clone(),
            have: None,
            cwd: None,
        })
        .collect()
}

fn session() -> Scope {
    Scope::Session("s".into())
}

/// What a subscriber was queued, as "<kind> <scope> [rev]".
fn drain(feed: &mut Feed, id: SubscriptionId) -> Vec<String> {
    std::iter::from_fn(|| feed.next(id))
        .map(|event| match event {
            StreamEvent::Snapshot { scope, snapshot } => format!("snapshot {scope} rev {}", snapshot.rev),
            StreamEvent::Ops { scope, rev, .. } => format!("ops {scope} rev {rev}"),
            StreamEvent::Checkpoint { scope, rev, .. } => format!("checkpoint {scope} rev {rev}"),
            StreamEvent::End { scope, end } => format!("end {scope} {:?}", end.reason),
        })
        .collect()
}

#[test]
fn a_subscriber_is_aligned_with_everything_published_and_then_follows_commits() {
    let mut registry = registry();
    let mut feed = Feed::new("process");
    feed.commit(&registry, &agent(), 0);
    let id = feed.subscribe(&follow(&[Scope::Global, session()]));
    assert_eq!(
        drain(&mut feed, id),
        ["snapshot global rev 0", "snapshot session/s rev 0"]
    );

    registry
        .append_session_update(
            "s",
            json!({ "sessionUpdate": "agent_message_chunk", "messageId": "m",
                    "content": { "type": "text", "text": "hi" } }),
        )
        .unwrap();
    feed.commit(&registry, &agent(), 0);
    assert_eq!(drain(&mut feed, id), ["ops session/s rev 1"]);
}

#[test]
fn a_commit_that_changes_nothing_publishes_nothing() {
    let registry = registry();
    let mut feed = Feed::new("process");
    feed.commit(&registry, &agent(), 0);
    let id = feed.subscribe(&follow(&[session()]));
    drain(&mut feed, id);
    feed.commit(&registry, &agent(), 0);
    feed.commit(&registry, &agent(), 0);
    assert_eq!(drain(&mut feed, id), Vec::<String>::new());
}

#[test]
fn a_change_of_global_reaches_its_subscribers_only() {
    let mut registry = registry();
    let mut feed = Feed::new("process");
    feed.commit(&registry, &agent(), 0);
    let global = feed.subscribe(&follow(&[Scope::Global]));
    let only_session = feed.subscribe(&follow(&[session()]));
    drain(&mut feed, global);
    drain(&mut feed, only_session);
    registry.start_login_terminal("login", "agent-login");
    feed.commit(&registry, &agent(), 0);
    assert_eq!(drain(&mut feed, global), ["ops global rev 1"]);
    assert_eq!(drain(&mut feed, only_session), Vec::<String>::new());
}

#[test]
fn a_session_published_after_a_subscription_is_aligned_when_it_appears() {
    let mut registry = SessionRegistry::new("epoch");
    let mut feed = Feed::new("process");
    feed.commit(&registry, &agent(), 0);
    let id = feed.subscribe(&follow(&[session()]));
    assert_eq!(drain(&mut feed, id), Vec::<String>::new(), "nothing is published of it yet");
    registry
        .open_new("epoch", "s", "/work", json!({ "sessionId": "s" }))
        .unwrap();
    feed.commit(&registry, &agent(), 0);
    assert_eq!(drain(&mut feed, id), ["snapshot session/s rev 0"]);
}

#[test]
fn a_replaced_timeline_is_a_new_epoch_and_a_snapshot_for_every_subscriber() {
    let mut registry = registry();
    let mut feed = Feed::new("process");
    feed.commit(&registry, &agent(), 0);
    let id = feed.subscribe(&follow(&[session()]));
    drain(&mut feed, id);
    registry.replace_session_timeline("s", crate::protocol::timeline::Timeline::default());
    feed.commit(&registry, &agent(), 0);
    assert_eq!(drain(&mut feed, id), ["snapshot session/s rev 0"]);
}

#[test]
fn a_session_the_bridge_no_longer_holds_ends_its_scope_for_its_subscribers() {
    let mut registry = registry();
    let mut feed = Feed::new("process");
    feed.commit(&registry, &agent(), 0);
    let id = feed.subscribe(&follow(&[session()]));
    drain(&mut feed, id);
    registry
        .sessions
        .get_mut("s")
        .unwrap()
        .state
        .live
        .as_mut()
        .unwrap()
        .lifecycle = SessionLifecycle::Closed;
    feed.commit(&registry, &agent(), 0);
    assert_eq!(drain(&mut feed, id), ["end session/s Closed"]);
    // Nothing is published of it again, and the end is only sent once.
    feed.commit(&registry, &agent(), 0);
    assert_eq!(drain(&mut feed, id), Vec::<String>::new());
    assert_eq!(feed.version("session/s"), None);
}

#[test]
fn a_slow_subscriber_is_sent_a_snapshot_instead_of_the_events_it_missed() {
    let mut registry = registry();
    let mut feed = Feed::new("process");
    feed.commit(&registry, &agent(), 0);
    let id = feed.subscribe(&follow(&[session()]));
    drain(&mut feed, id);
    let big = "x".repeat(crate::protocol::subscription::QUEUE_BUDGET);
    registry
        .append_session_update(
            "s",
            json!({ "sessionUpdate": "agent_message_chunk", "messageId": "m",
                    "content": { "type": "text", "text": big } }),
        )
        .unwrap();
    feed.commit(&registry, &agent(), 0);
    // The queue went over its budget: the subscriber holds rev 0 and is sent
    // what changed after it, not the event.
    assert_eq!(drain(&mut feed, id), ["snapshot session/s rev 1"]);
}

#[test]
fn a_closed_subscription_is_forgotten() {
    let registry = registry();
    let mut feed = Feed::new("process");
    feed.commit(&registry, &agent(), 0);
    let id = feed.subscribe(&follow(&[session()]));
    feed.unsubscribe(id);
    assert_eq!(feed.next(id), None);
    assert_eq!(feed.subscribers(), 0);
}

// -- Checkpoints (protocol §5.5) --------------------------------------------------------

fn changed(registry: &mut SessionRegistry, text: &str) {
    registry
        .append_session_update(
            "s",
            json!({ "sessionUpdate": "agent_message_chunk", "messageId": "m",
                    "content": { "type": "text", "text": text } }),
        )
        .unwrap();
}

fn after(base: tokio::time::Instant, millis: u64) -> tokio::time::Instant {
    base + std::time::Duration::from_millis(millis)
}

#[test]
fn a_scope_that_changed_is_checkpointed_for_its_subscribers_after_a_pause() {
    let base = tokio::time::Instant::now();
    let mut registry = registry();
    let mut feed = Feed::new("process");
    feed.commit_at(&registry, &agent(), 0, base);
    let id = feed.subscribe(&follow(&[session()]));
    drain(&mut feed, id);

    changed(&mut registry, "hi");
    feed.commit_at(&registry, &agent(), 0, after(base, 10));
    assert_eq!(drain(&mut feed, id), ["ops session/s rev 1"]);
    assert_eq!(feed.deadline(), Some(after(base, 510)));

    assert!(!feed.checkpoints(after(base, 509)));
    assert_eq!(drain(&mut feed, id), Vec::<String>::new());
    assert!(feed.checkpoints(after(base, 510)));
    assert_eq!(drain(&mut feed, id), ["checkpoint session/s rev 1"]);
    // One checkpoint covers the change: no more are due.
    assert_eq!(feed.deadline(), None);
    assert!(!feed.checkpoints(after(base, 10_000)));
}

#[test]
fn a_checkpoint_names_the_rev_it_covers_and_carries_a_digest() {
    let base = tokio::time::Instant::now();
    let mut registry = registry();
    let mut feed = Feed::new("process");
    feed.commit_at(&registry, &agent(), 0, base);
    let id = feed.subscribe(&follow(&[session()]));
    drain(&mut feed, id);
    changed(&mut registry, "hi");
    feed.commit_at(&registry, &agent(), 0, base);
    feed.checkpoints(after(base, 500));
    let events: Vec<StreamEvent> = std::iter::from_fn(|| feed.next(id)).collect();
    let Some(StreamEvent::Checkpoint { digest, rev, .. }) = events.last() else {
        panic!("a checkpoint is queued last: {events:?}");
    };
    assert_eq!(*rev, 1);
    assert_eq!(digest.len(), 22);
}

#[test]
fn a_scope_that_keeps_changing_is_checkpointed_every_two_seconds() {
    let base = tokio::time::Instant::now();
    let mut registry = registry();
    let mut feed = Feed::new("process");
    feed.commit_at(&registry, &agent(), 0, base);
    let id = feed.subscribe(&follow(&[session()]));
    drain(&mut feed, id);
    for step in 0..5u64 {
        changed(&mut registry, &format!("t{step}"));
        feed.commit_at(&registry, &agent(), 0, after(base, step * 400));
        assert!(!feed.checkpoints(after(base, step * 400)), "step {step}");
    }
    assert_eq!(feed.deadline(), Some(after(base, 2_000)));
    drain(&mut feed, id);
    assert!(feed.checkpoints(after(base, 2_000)));
    assert_eq!(drain(&mut feed, id), ["checkpoint session/s rev 5"]);
}

#[test]
fn a_snapshot_settles_the_scope_and_nothing_is_checkpointed_for_a_scope_that_ended() {
    let base = tokio::time::Instant::now();
    let mut registry = registry();
    let mut feed = Feed::new("process");
    feed.commit_at(&registry, &agent(), 0, base);
    let id = feed.subscribe(&follow(&[session()]));
    drain(&mut feed, id);

    changed(&mut registry, "hi");
    feed.commit_at(&registry, &agent(), 0, base);
    // A new epoch is a snapshot, which carries a digest of its own.
    registry.replace_session_timeline("s", crate::protocol::timeline::Timeline::default());
    feed.commit_at(&registry, &agent(), 0, after(base, 10));
    assert_eq!(feed.deadline(), None);

    changed(&mut registry, "again");
    feed.commit_at(&registry, &agent(), 0, after(base, 20));
    registry
        .sessions
        .get_mut("s")
        .unwrap()
        .state
        .live
        .as_mut()
        .unwrap()
        .lifecycle = SessionLifecycle::Closed;
    feed.commit_at(&registry, &agent(), 0, after(base, 30));
    drain(&mut feed, id);
    // `global` lists the session, so it changed too: its checkpoint is due, the
    // ended session's is not.
    feed.checkpoints(after(base, 10_000));
    assert_eq!(drain(&mut feed, id), Vec::<String>::new());
    assert_eq!(feed.deadline(), None);
}

#[test]
fn only_the_subscribers_that_follow_a_scope_are_sent_its_checkpoint() {
    let base = tokio::time::Instant::now();
    let mut registry = registry();
    let mut feed = Feed::new("process");
    feed.commit_at(&registry, &agent(), 0, base);
    let other = feed.subscribe(&follow(&[Scope::Global]));
    drain(&mut feed, other);
    changed(&mut registry, "hi");
    feed.commit_at(&registry, &agent(), 0, base);
    feed.checkpoints(after(base, 500));
    assert_eq!(drain(&mut feed, other), Vec::<String>::new());
}
