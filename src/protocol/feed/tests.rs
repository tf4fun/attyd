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
fn a_session_that_went_with_a_reason_ends_its_scope_for_its_subscribers() {
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
    let closed = End {
        reason: EndReason::Closed,
        error: None,
    };
    feed.commit_ending(&registry, &agent(), 0, Instant::now(), &[("s".to_string(), closed)]);
    assert_eq!(drain(&mut feed, id), ["end session/s Closed"]);
    // Nothing is published of it again, and the end is only sent once.
    feed.commit(&registry, &agent(), 0);
    assert_eq!(drain(&mut feed, id), Vec::<String>::new());
    assert_eq!(feed.version("session/s"), None);
}

#[test]
fn a_session_released_with_no_reason_ends_nothing_and_its_followers_wait_for_it() {
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
    assert_eq!(drain(&mut feed, id), Vec::<String>::new(), "a release sends no end");
    assert_eq!(feed.version("session/s"), None);
    // The follower is still waiting for it, so the bridge loads it again.
    let waiting = feed.followed_sessions();
    assert_eq!(waiting["s"].len(), 1, "{waiting:?}");
    assert_eq!(
        waiting["s"][0].cwd.as_deref(),
        Some("/work"),
        "it is loaded in the directory the bridge knew it in"
    );

    // Published again, the session is sent whole.
    let mut registry = SessionRegistry::new("epoch");
    registry
        .open_new("epoch", "s", "/work", json!({ "sessionId": "s" }))
        .unwrap();
    feed.commit(&registry, &agent(), 0);
    assert_eq!(drain(&mut feed, id), ["snapshot session/s rev 0"]);
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

// -- The sessions subscriptions follow (protocol §5.1, S6) --------------------------------

fn follow_in(scope: &Scope, cwd: Option<&str>) -> Vec<Subscribed> {
    vec![Subscribed {
        scope: scope.clone(),
        have: None,
        cwd: cwd.map(String::from),
    }]
}

#[test]
fn a_session_a_subscription_follows_waits_for_its_load_until_it_is_published() {
    let mut registry = SessionRegistry::new("epoch");
    let mut feed = Feed::new("process");
    feed.commit(&registry, &agent(), 0);
    let first = feed.subscribe(&follow_in(&session(), Some("/work")));
    let second = feed.subscribe(&follow_in(&session(), None));
    let only_global = feed.subscribe(&follow(&[Scope::Global]));

    let followed = feed.followed_sessions();
    assert_eq!(followed.keys().collect::<Vec<_>>(), ["s"], "only sessions are followed");
    let waiting = &followed["s"];
    assert_eq!(
        waiting.iter().map(|w| (w.id, w.cwd.as_deref())).collect::<Vec<_>>(),
        [(first, Some("/work")), (second, None)]
    );
    assert!(!waiting.iter().any(|w| w.id == only_global));

    // Published, the session is still followed but nobody waits for it.
    registry
        .open_new("epoch", "s", "/work", json!({ "sessionId": "s" }))
        .unwrap();
    feed.commit(&registry, &agent(), 0);
    assert_eq!(feed.followed_sessions()["s"].len(), 0);

    // A subscription that is gone follows nothing.
    feed.unsubscribe(first);
    feed.unsubscribe(second);
    assert!(feed.followed_sessions().is_empty());
}

#[test]
fn ending_the_waiting_ends_the_scope_for_those_that_wait_only() {
    let mut registry = SessionRegistry::new("epoch");
    let mut feed = Feed::new("process");
    feed.commit(&registry, &agent(), 0);
    let waiting = feed.subscribe(&follow_in(&session(), None));
    let end = End {
        reason: EndReason::LoadFailed,
        error: Some(crate::protocol::documents::ProtocolError::new(
            crate::protocol::documents::ErrorCode::InvalidRequest,
            "needs a cwd",
        )),
    };
    feed.end_waiting("s", &end);
    assert_eq!(drain(&mut feed, waiting), ["end session/s LoadFailed"]);
    // The end is final: it is not waiting any more, and nothing follows.
    assert!(feed.followed_sessions().get("s").is_none_or(|w| w.is_empty()));
    registry
        .open_new("epoch", "s", "/work", json!({ "sessionId": "s" }))
        .unwrap();
    feed.commit(&registry, &agent(), 0);
    assert_eq!(drain(&mut feed, waiting), Vec::<String>::new());

    // A subscription that already has the session published is not ended.
    let aligned = feed.subscribe(&follow_in(&session(), None));
    drain(&mut feed, aligned);
    feed.end_waiting("s", &end);
    assert_eq!(drain(&mut feed, aligned), Vec::<String>::new());
}

// -- Why a scope ended -----------------------------------------------------------------------

fn failed_load() -> End {
    End {
        reason: EndReason::LoadFailed,
        error: Some(crate::protocol::documents::ProtocolError::new(
            crate::protocol::documents::ErrorCode::NotFound,
            "no such session",
        )),
    }
}

#[test]
fn a_session_gone_with_a_reason_ends_its_scope_with_that_reason() {
    let base = tokio::time::Instant::now();
    let mut registry = registry();
    let mut feed = Feed::new("process");
    feed.commit_at(&registry, &agent(), 0, base);
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
    feed.commit_ending(&registry, &agent(), 0, base, &[("s".to_string(), failed_load())]);
    assert_eq!(drain(&mut feed, id), ["end session/s LoadFailed"]);
}

#[test]
fn a_scope_that_ended_before_it_was_published_ends_for_the_subscriptions_that_wait() {
    let base = tokio::time::Instant::now();
    let registry = SessionRegistry::new("epoch");
    let mut feed = Feed::new("process");
    feed.commit_at(&registry, &agent(), 0, base);
    let waiting = feed.subscribe(&follow(&[session()]));
    let other = feed.subscribe(&follow(&[Scope::Session("other".into())]));
    feed.commit_ending(&registry, &agent(), 0, base, &[("s".to_string(), failed_load())]);
    assert_eq!(drain(&mut feed, waiting), ["end session/s LoadFailed"]);
    assert_eq!(drain(&mut feed, other), Vec::<String>::new());
    assert!(feed.followed_sessions().get("s").is_none());
}

// -- Consumers that converge (docs/state-sync.md §4.1) -----------------------------------------

/// A consumer of the feed: a subscription and the replica its events build.
struct Consumer {
    id: SubscriptionId,
    replica: crate::protocol::replica::Replica,
}

impl Consumer {
    fn new(feed: &mut Feed) -> Self {
        let id = feed.subscribe(&follow(&[session()]));
        Self {
            id,
            replica: Default::default(),
        }
    }

    /// Takes every event queued for it.
    fn take(&mut self, feed: &mut Feed) {
        while let Some(event) = feed.next(self.id) {
            let event = serde_json::to_value(event).unwrap();
            if event["scope"] == "session/s" {
                self.replica
                    .apply(&event)
                    .unwrap_or_else(|error| panic!("{error}: {event}"));
            }
        }
    }

    /// Closes the subscription and subscribes again with the version it holds.
    fn reconnect(&mut self, feed: &mut Feed) {
        feed.unsubscribe(self.id);
        let have = (!self.replica.epoch.is_empty()).then(|| crate::protocol::engine::Have {
            epoch: self.replica.epoch.clone(),
            rev: self.replica.rev,
        });
        self.id = feed.subscribe(&[Subscribed {
            scope: session(),
            have,
            cwd: None,
        }]);
    }
}

/// A deterministic generator for the reconnecting consumer.
struct Lcg(u64);

impl Lcg {
    fn draw(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

#[test]
fn four_consumers_converge_one_by_one_lagging_reconnecting_and_paused() {
    let mut registry = registry();
    let mut feed = Feed::new("process");
    feed.commit(&registry, &agent(), 0);
    let mut following = Consumer::new(&mut feed);
    let mut lagging = Consumer::new(&mut feed);
    let mut reconnecting = Consumer::new(&mut feed);
    let mut paused = Consumer::new(&mut feed);
    let mut random = Lcg(7);

    for step in 0..60u32 {
        // Every ninth update is big: two of them pass a queue's budget, so a consumer that
        // does not keep up is sent a snapshot instead of the events it missed.
        let text = if step % 9 == 8 {
            "x".repeat(crate::protocol::subscription::QUEUE_BUDGET * 3 / 5)
        } else {
            format!("update {step}")
        };
        registry
            .append_session_update(
                "s",
                json!({ "sessionUpdate": "agent_message_chunk",
                        "messageId": format!("m{}", step / 4),
                        "content": { "type": "text", "text": text } }),
            )
            .unwrap();
        feed.commit(&registry, &agent(), 0);
        following.take(&mut feed);
        if step % 7 == 6 {
            lagging.take(&mut feed);
        }
        if random.draw() % 3 == 0 {
            reconnecting.take(&mut feed);
            reconnecting.reconnect(&mut feed);
        }
    }

    for consumer in [&mut following, &mut lagging, &mut reconnecting, &mut paused] {
        consumer.take(&mut feed);
    }
    let published = crate::protocol::replica::Replica::from_snapshot(
        &serde_json::to_value(feed.snapshot("session/s").unwrap()).unwrap(),
    );
    for (name, consumer) in [
        ("following", &following),
        ("lagging", &lagging),
        ("reconnecting", &reconnecting),
        ("paused", &paused),
    ] {
        assert_eq!(consumer.replica.doc, published.doc, "{name}");
        assert_eq!(consumer.replica.rev, published.rev, "{name}");
    }
    assert!(published.entries().len() > 10, "the updates reached the document");
}
