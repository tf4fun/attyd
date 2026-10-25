//! Feed-level guarantees over real revision 4 events: demand, release, checkpoints and
//! independent consumers that fall behind or reconnect. Path snapshots and view lifecycles
//! are covered by tree_stream_tests; the client applies the wire events here too.

use serde_json::json;

use super::*;
use crate::protocol::documents::{AgentInfo, AgentLaunch, AgentPhase, ErrorCode, ProtocolError};
use crate::protocol::tree_stream::client::Client;
use crate::protocol::tree_stream::params::{Follow, parse_stream_query};
use crate::protocol::tree_stream::wire::Change;
use crate::protocol::tree_stream::QUEUE_BUDGET;
use crate::runtime_state::SessionLifecycle;
use crate::session_registry::SessionRegistry;

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

fn registry() -> SessionRegistry {
    let mut registry = SessionRegistry::new("epoch");
    registry.open_new("epoch", "s", "/work", json!({ "sessionId": "s" })).unwrap();
    registry
}

fn query(query: &str) -> StreamQuery {
    parse_stream_query(query).unwrap()
}

fn changed(registry: &mut SessionRegistry, text: &str) {
    registry.append_session_update("s", json!({
        "sessionUpdate": "agent_message_chunk", "messageId": "m",
        "content": { "type": "text", "text": text },
    })).unwrap();
}

fn after(base: Instant, millis: u64) -> Instant {
    base + std::time::Duration::from_millis(millis)
}

fn take(feed: &mut Feed, id: SubscriptionId, client: &mut Client) -> Vec<TreeEvent> {
    std::iter::from_fn(|| feed.next_tree(id)).map(|event| {
        client.apply(&event).unwrap_or_else(|error| panic!("{error}: {event:?}"));
        event
    }).collect()
}

#[test]
fn unchanged_commits_and_other_paths_publish_nothing_to_a_subscriber() {
    let mut registry = registry();
    let mut feed = Feed::new();
    feed.commit(&registry, &agent(), 0);
    let session = feed.subscribe_tree(&query("follow=/sessions/s"));
    let global = feed.subscribe_tree(&query("follow=/global"));
    let mut client = Client::following(&[SESSION]);
    take(&mut feed, session, &mut client);
    feed.next_tree(global).unwrap();
    feed.commit(&registry, &agent(), 0);
    feed.commit(&registry, &agent(), 0);
    assert!(feed.next_tree(session).is_none());
    assert!(feed.next_tree(global).is_none());
    registry.start_login_terminal("login", "agent-login");
    feed.commit(&registry, &agent(), 0);
    assert!(feed.next_tree(global).is_some());
    assert!(feed.next_tree(session).is_none());
}

#[test]
fn replacing_history_changes_its_revision_and_removes_the_old_entries() {
    let mut registry = registry();
    changed(&mut registry, "old history");
    let mut feed = Feed::new();
    feed.commit(&registry, &agent(), 0);
    let id = feed.subscribe_tree(&query("follow=/sessions/s"));
    let mut client = Client::following(&[SESSION]);
    take(&mut feed, id, &mut client);
    let instance = client.instance.clone();
    let history = client.value(SESSION).unwrap()["historyRev"].as_u64().unwrap();

    registry.replace_session_timeline("s", crate::protocol::timeline::Timeline::default());
    feed.commit(&registry, &agent(), 0);
    assert_eq!(take(&mut feed, id, &mut client).len(), 1);
    let held = client.value(SESSION).unwrap();
    assert!(held["historyRev"].as_u64().unwrap() > history);
    assert_eq!(held["entries"], json!([]));
    assert_eq!(client.instance, instance, "replacing one history does not replace the tree");
    assert_eq!(held, &feed.tree().get(SESSION).unwrap().to_value());
}

#[test]
fn a_session_released_without_a_reason_still_has_followers_and_keeps_its_load_directory() {
    let mut registry = registry();
    let mut feed = Feed::new();
    feed.commit(&registry, &agent(), 0);
    let id = feed.subscribe_tree(&query("follow=/sessions/s"));
    let mut client = Client::following(&[SESSION]);
    take(&mut feed, id, &mut client);
    registry.sessions.get_mut("s").unwrap().state.live.as_mut().unwrap().lifecycle = SessionLifecycle::Closed;
    feed.commit(&registry, &agent(), 0);
    let events = take(&mut feed, id, &mut client);
    assert!(matches!(
        &events[0].changes.as_ref().unwrap()[0],
        Change::End { end, .. } if end.reason == TreeEndReason::Absent
    ));
    assert!(client.value(SESSION).is_none());
    assert!(feed.tree().get(SESSION).is_none());
    let waiting = feed.followed_sessions();
    assert_eq!(waiting["s"], [Waiting { id, cwd: Some("/work".into()) }]);

    // A later load starts this same path again and has a new held/history revision.
    let mut reloaded = SessionRegistry::new("epoch");
    reloaded.open_new("epoch", "s", "/work", json!({ "sessionId": "s" })).unwrap();
    feed.commit(&reloaded, &agent(), 0);
    assert_eq!(take(&mut feed, id, &mut client).len(), 1);
    assert_eq!(client.value(SESSION), feed.tree().get(SESSION).map(|node| node.to_value()).as_ref());
    assert!(feed.followed_sessions()["s"].is_empty());
}

#[test]
fn each_waiting_subscriber_keeps_its_directory_and_dropping_them_removes_the_demand() {
    let mut registry = SessionRegistry::new("epoch");
    let mut feed = Feed::new();
    feed.commit(&registry, &agent(), 0);
    let first = feed.subscribe_tree(&query("follow=/sessions/s&cwd=s:/work"));
    let second = feed.subscribe_tree(&query("follow=/sessions/s"));
    let global = feed.subscribe_tree(&query("follow=/global"));
    let followed = feed.followed_sessions();
    assert_eq!(followed.keys().collect::<Vec<_>>(), ["s"]);
    assert_eq!(followed["s"], [
        Waiting { id: first, cwd: Some("/work".into()) },
        Waiting { id: second, cwd: None },
    ]);
    assert!(!followed["s"].iter().any(|waiting| waiting.id == global));
    registry.open_new("epoch", "s", "/work", json!({ "sessionId": "s" })).unwrap();
    feed.commit(&registry, &agent(), 0);
    assert!(feed.followed_sessions()["s"].is_empty(), "held but still followed");
    feed.unsubscribe(first);
    feed.unsubscribe(second);
    assert!(feed.followed_sessions().is_empty());
    assert!(feed.next_tree(first).is_none());
    assert_eq!(feed.subscribers(), 1, "the global subscription remains");
}

#[test]
fn a_failure_before_publication_ends_only_the_matching_wait_and_does_not_end_a_held_session() {
    let mut registry = SessionRegistry::new("epoch");
    let mut feed = Feed::new();
    feed.commit(&registry, &agent(), 0);
    let waiting = feed.subscribe_tree(&query("follow=/sessions/s"));
    let other = feed.subscribe_tree(&query("follow=/sessions/other"));
    let mut client = Client::following(&[SESSION]);
    take(&mut feed, waiting, &mut client);
    feed.next_tree(other).unwrap();
    let end = End {
        reason: EndReason::LoadFailed,
        error: Some(ProtocolError::new(ErrorCode::NotFound, "no such session")),
    };
    feed.commit_ending(&registry, &agent(), 0, Instant::now(), &[("s".into(), end.clone())]);
    let events = take(&mut feed, waiting, &mut client);
    assert!(matches!(
        &events.last().unwrap().changes.as_ref().unwrap()[0],
        Change::End { end, .. } if end.reason == TreeEndReason::LoadFailed
    ));
    assert!(feed.next_tree(other).is_none());
    assert!(!feed.followed_sessions().contains_key("s"));

    // Unlike a fixed view, a session path may appear later without another subscription.
    registry.open_new("epoch", "s", "/work", json!({ "sessionId": "s" })).unwrap();
    feed.commit(&registry, &agent(), 0);
    assert_eq!(take(&mut feed, waiting, &mut client).len(), 1);
    assert!(client.value(SESSION).is_some());
    feed.end_waiting("s", &end);
    assert!(feed.next_tree(waiting).is_none(), "an already held path is not ended");
}

#[test]
fn continuous_changes_are_checked_at_two_seconds_and_only_for_followed_paths() {
    let base = Instant::now();
    let mut registry = registry();
    let mut feed = Feed::new();
    feed.commit_at(&registry, &agent(), 0, base);
    let id = feed.subscribe_tree(&query("follow=/sessions/s"));
    let other = feed.subscribe_tree(&query("follow=/global"));
    let mut client = Client::following(&[SESSION]);
    take(&mut feed, id, &mut client);
    feed.next_tree(other).unwrap();
    for step in 0..5u64 {
        changed(&mut registry, &format!("t{step}"));
        feed.commit_at(&registry, &agent(), 0, after(base, step * 400));
        assert!(!feed.checkpoints(after(base, step * 400)), "step {step}");
        take(&mut feed, id, &mut client);
    }
    assert_eq!(feed.deadline(), Some(after(base, 2_000)));
    assert!(feed.checkpoints(after(base, 2_000)));
    let events = take(&mut feed, id, &mut client);
    let check = &events[0].check.as_ref().expect("a checkpoint")[0];
    assert_eq!((check.path.as_str(), check.at), (SESSION, client.version(SESSION)));
    assert_eq!(check.hash, crate::protocol::digest::wire(&feed.tree().get(SESSION).unwrap().hash()));
    assert!(feed.next_tree(other).is_none());
    assert!(feed.deadline().is_none());
    assert!(!feed.checkpoints(after(base, 10_000)));
}

struct Consumer {
    id: SubscriptionId,
    client: Client,
}

impl Consumer {
    fn new(feed: &mut Feed) -> Self {
        Self {
            id: feed.subscribe_tree(&query("follow=/sessions/s")),
            client: Client::following(&[SESSION]),
        }
    }

    fn take(&mut self, feed: &mut Feed) -> Vec<TreeEvent> {
        take(feed, self.id, &mut self.client)
    }

    fn reconnect(&mut self, feed: &mut Feed) {
        feed.unsubscribe(self.id);
        self.id = feed.subscribe_tree(&StreamQuery {
            instance: self.client.instance.clone(),
            follow: vec![Follow {
                path: SESSION.into(),
                have: self.client.value(SESSION).map(|_| self.client.version(SESSION)),
                window: None,
            }],
            cwd: BTreeMap::new(),
        });
    }
}

/// A deterministic generator for the reconnecting consumer.
struct Lcg(u64);

impl Lcg {
    fn draw(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

#[test]
fn four_consumers_converge_one_by_one_lagging_reconnecting_and_paused() {
    let mut registry = registry();
    let mut feed = Feed::new();
    feed.commit(&registry, &agent(), 0);
    let mut following = Consumer::new(&mut feed);
    let mut lagging = Consumer::new(&mut feed);
    let mut reconnecting = Consumer::new(&mut feed);
    let mut paused = Consumer::new(&mut feed);
    let mut random = Lcg(7);
    for step in 0..60u32 {
        // Two large updates pass the queue budget; a slow consumer receives a catch-up.
        let text = if step % 9 == 8 { "x".repeat(QUEUE_BUDGET * 3 / 5) } else { format!("update {step}") };
        registry.append_session_update("s", json!({
            "sessionUpdate": "agent_message_chunk", "messageId": format!("m{}", step / 4),
            "content": { "type": "text", "text": text },
        })).unwrap();
        feed.commit(&registry, &agent(), 0);
        following.take(&mut feed);
        if step % 7 == 6 { lagging.take(&mut feed); }
        if random.draw() % 3 == 0 {
            reconnecting.take(&mut feed);
            reconnecting.reconnect(&mut feed);
        }
    }
    for consumer in [&mut following, &mut lagging, &mut reconnecting] {
        consumer.take(&mut feed);
    }
    let catch_up = paused.take(&mut feed);
    assert!(catch_up.iter().flat_map(|event| event.changes.iter().flatten()).any(|change| {
        matches!(change, Change::Snapshot { snapshot, .. } if snapshot.at > 1)
    }), "the paused subscriber was coalesced into a recent snapshot");
    let published = feed.tree().get(SESSION).unwrap();
    for (name, consumer) in [
        ("following", &following), ("lagging", &lagging),
        ("reconnecting", &reconnecting), ("paused", &paused),
    ] {
        assert_eq!(consumer.client.value(SESSION), Some(&published.to_value()), "{name}");
        assert_eq!(consumer.client.version(SESSION), published.srev(), "{name}");
    }
    assert!(published.child("entries").unwrap().items().len() > 10, "the updates reached the tree");
}
