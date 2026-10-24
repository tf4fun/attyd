//! The feed serves revision 4 streams beside revision 3's: each commit reaches the streams that
//! follow a path it changed, and a stream's session is held like a scope subscription's.
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::timeout;

use super::*;
use crate::protocol::documents::{AgentInfo, AgentLaunch, AgentPhase, ProtocolError};
use crate::protocol::tree_stream::client::Client;
use crate::protocol::tree_stream::params::parse_stream_query;
use crate::protocol::tree_stream::wire::Change;
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

fn registry() -> SessionRegistry {
    let mut registry = SessionRegistry::new("epoch");
    registry
        .open_new("epoch", "s", "/work", json!({ "sessionId": "s" }))
        .unwrap();
    registry
}

fn chunk(registry: &mut SessionRegistry, text: &str) {
    registry
        .append_session_update(
            "s",
            json!({ "sessionUpdate": "agent_message_chunk", "messageId": "m",
                    "content": { "type": "text", "text": text } }),
        )
        .unwrap();
}

fn query(text: &str) -> StreamQuery {
    parse_stream_query(text).unwrap()
}

/// Everything queued for `id`, applied to `client`.
fn pump(feed: &mut Feed, id: SubscriptionId, client: &mut Client) -> usize {
    let mut events = 0;
    while let Some(event) = feed.next_tree(id) {
        client.apply(&event).unwrap_or_else(|gap| panic!("{gap}: {event:?}"));
        events += 1;
    }
    events
}

#[test]
fn a_stream_is_aligned_with_the_tree_and_follows_its_commits() {
    let mut feed = Feed::new("process");
    let mut registry = registry();
    chunk(&mut registry, "hi");
    feed.commit(&registry, &agent(), 1);

    let path = "/sessions/s/entries";
    let id = feed.subscribe_tree(&query(&format!("follow={path}")));
    let mut client = Client::following(&[path]);
    assert_eq!(pump(&mut feed, id, &mut client), 1);
    assert_eq!(client.instance.as_deref(), Some(feed.tree().instance()));
    assert_eq!(client.value(path).unwrap()[0]["chunks"][0]["content"]["text"], "hi");

    chunk(&mut registry, " there");
    feed.commit(&registry, &agent(), 1);
    assert_eq!(pump(&mut feed, id, &mut client), 1);
    assert_eq!(client.value(path).unwrap()[0]["chunks"][0]["content"]["text"], "hi there");
    assert_eq!(client.value(path), feed.tree().get(path).map(|node| node.to_value()).as_ref());

    // A commit that changed nothing the stream follows queues nothing.
    registry.start_login_terminal("login", "m");
    feed.commit(&registry, &agent(), 1);
    assert_eq!(pump(&mut feed, id, &mut client), 0);
}

#[test]
fn revision_3_and_revision_4_streams_follow_the_same_commits() {
    let mut feed = Feed::new("process");
    let mut registry = registry();
    feed.commit(&registry, &agent(), 1);
    let scopes = feed.subscribe(&[crate::protocol::subscription::Subscribed {
        scope: Scope::Session("s".into()),
        have: None,
        cwd: None,
    }]);
    let tree = feed.subscribe_tree(&query("follow=/sessions/s/entries"));
    assert_eq!(feed.subscribers(), 2);
    chunk(&mut registry, "x");
    feed.commit(&registry, &agent(), 1);
    assert!(feed.next(scopes).is_some());
    assert!(feed.next_tree(tree).is_some());
    feed.unsubscribe(tree);
    feed.unsubscribe(scopes);
    assert_eq!(feed.subscribers(), 0);
}

#[test]
fn a_stream_of_an_unheld_session_waits_for_it_and_asks_for_its_load_with_the_cwd() {
    let mut feed = Feed::new("process");
    feed.commit(&SessionRegistry::new("epoch"), &agent(), 1);
    let id = feed.subscribe_tree(&query("follow=/sessions/s/entries&cwd=s:/work"));
    let followed = feed.followed_sessions();
    assert_eq!(followed["s"], vec![Waiting { id, cwd: Some("/work".into()) }]);
    let mut client = Client::following(&["/sessions/s/entries"]);
    pump(&mut feed, id, &mut client);
    assert_eq!(client.value("/sessions/s/entries"), None);

    // The session is installed: the stream starts with a snapshot and is no longer waiting.
    feed.commit(&registry(), &agent(), 1);
    assert_eq!(pump(&mut feed, id, &mut client), 1);
    assert_eq!(client.value("/sessions/s/entries"), Some(&json!([])));
    assert_eq!(feed.followed_sessions()["s"], vec![]);
}

#[test]
fn a_view_or_session_path_both_hold_the_session() {
    let mut feed = Feed::new("process");
    feed.commit(&SessionRegistry::new("epoch"), &agent(), 1);
    feed.subscribe_tree(&query("follow=/views/a~1b/output/latest&follow=/global"));
    assert_eq!(feed.followed_sessions().keys().collect::<Vec<_>>(), ["a/b"]);
}

#[test]
fn a_failed_load_ends_the_waiting_paths_with_an_error_summary_and_the_stream_stays() {
    let mut feed = Feed::new("process");
    feed.commit(&SessionRegistry::new("epoch"), &agent(), 1);
    let id = feed.subscribe_tree(&query("follow=/sessions/s/entries&cwd=s:/work"));
    let mut client = Client::following(&["/sessions/s/entries"]);
    pump(&mut feed, id, &mut client);
    feed.end_waiting(
        "s",
        &End {
            reason: EndReason::LoadFailed,
            error: Some(ProtocolError::agent(&json!({ "code": -32000, "message": "login" }))),
        },
    );
    let event = feed.next_tree(id).unwrap();
    let Change::End { end, .. } = &event.changes.as_ref().unwrap()[0] else { panic!() };
    assert_eq!(end.reason, TreeEndReason::LoadFailed);
    assert_eq!(
        serde_json::to_value(end).unwrap(),
        json!({ "reason": "load_failed", "error": { "code": "agent_error", "agentCode": -32000 } })
    );
    // The subscription stays: the session installed later starts it.
    feed.commit(&registry(), &agent(), 1);
    client.apply(&event).unwrap();
    assert_eq!(pump(&mut feed, id, &mut client), 1);
}

#[test]
fn a_session_that_went_with_a_reason_ends_its_paths_with_it() {
    let mut feed = Feed::new("process");
    feed.commit(&registry(), &agent(), 1);
    let id = feed.subscribe_tree(&query("follow=/sessions/s/entries"));
    let mut client = Client::following(&["/sessions/s/entries"]);
    pump(&mut feed, id, &mut client);
    let end = End { reason: EndReason::Deleted, error: None };
    feed.commit_ending(
        &SessionRegistry::new("epoch"),
        &agent(),
        1,
        Instant::now(),
        &[("s".to_string(), end)],
    );
    let event = feed.next_tree(id).unwrap();
    let Change::End { end, .. } = &event.changes.as_ref().unwrap()[0] else { panic!() };
    assert_eq!(end.reason, TreeEndReason::Deleted);
    client.apply(&event).unwrap();
    assert_eq!(client.value("/sessions/s/entries"), None);
}

#[tokio::test(start_paused = true)]
async fn a_changed_path_is_checked_after_the_pause_by_a_waiting_stream() {
    let handle = FeedHandle::new("process");
    let mut registry = registry();
    handle.commit(&registry, &agent(), 1);
    let subscription = handle.subscribe_tree(&query("follow=/sessions/s/entries"));
    let first: Value = serde_json::from_str(&subscription.next_json().await.unwrap()).unwrap();
    assert!(first["instance"].is_string() && first["changes"].is_array());
    chunk(&mut registry, "x");
    handle.commit(&registry, &agent(), 1);
    let changes: Value = serde_json::from_str(&subscription.next_json().await.unwrap()).unwrap();
    assert_eq!(changes["changes"][0]["from"], 1);
    let check: Value = timeout(WAIT, subscription.next_json()).await.unwrap().map(|json| serde_json::from_str(&json).unwrap()).unwrap();
    assert_eq!(check["check"][0]["path"], "/sessions/s/entries");
    assert_eq!(check["check"][0]["at"], 2);
    handle.close();
    assert!(subscription.next_json().await.is_none());
}

// --- /global/loadFailures (design §6.2) -----------------------------------------------

fn failure(message: &str) -> End {
    End {
        reason: EndReason::LoadFailed,
        error: Some(ProtocolError::agent(&json!({ "code": -32000, "message": message, "data": { "big": "x".repeat(10_000) } }))),
    }
}

#[test]
fn a_failed_load_leaves_its_whole_error_in_global_and_a_stream_of_global_sees_it() {
    let mut feed = Feed::new("process");
    feed.commit(&SessionRegistry::new("epoch"), &agent(), 1);
    let id = feed.subscribe_tree(&query("follow=/global/loadFailures"));
    let mut client = Client::following(&["/global/loadFailures"]);
    pump(&mut feed, id, &mut client);
    assert_eq!(client.value("/global/loadFailures"), Some(&json!({})));

    feed.end_waiting("s", &failure("login required"));
    assert_eq!(pump(&mut feed, id, &mut client), 1);
    let kept = &client.value("/global/loadFailures").unwrap()["s"];
    assert_eq!(kept["code"], "agent_error");
    assert_eq!(kept["message"], "login required");
    assert_eq!(kept["data"]["code"], -32000);
    assert_eq!(kept["data"]["data"]["big"].as_str().unwrap().len(), 10_000);
}

#[test]
fn a_failure_is_forgotten_when_the_session_is_held_or_deleted() {
    let mut feed = Feed::new("process");
    feed.commit(&SessionRegistry::new("epoch"), &agent(), 1);
    feed.end_waiting("s", &failure("a"));
    feed.end_waiting("t", &failure("b"));
    assert!(feed.tree().get("/global/loadFailures/s").is_some());
    feed.commit(&registry(), &agent(), 1);
    assert!(feed.tree().get("/global/loadFailures/s").is_none(), "s is held again");
    assert!(feed.tree().get("/global/loadFailures/t").is_some());
    feed.commit_ending(
        &SessionRegistry::new("epoch"),
        &agent(),
        1,
        Instant::now(),
        &[("t".to_string(), End { reason: EndReason::Deleted, error: None })],
    );
    assert!(feed.tree().get("/global/loadFailures/t").is_none(), "t is deleted");
}

#[test]
fn only_the_last_failures_are_kept() {
    let mut feed = Feed::new("process");
    feed.commit(&SessionRegistry::new("epoch"), &agent(), 1);
    for n in 0..70 {
        feed.end_waiting(&format!("s{n}"), &failure("x"));
    }
    let node = feed.tree().get("/global/loadFailures").unwrap().to_value();
    let kept = node.as_object().unwrap();
    assert_eq!(kept.len(), 64);
    assert!(kept.contains_key("s69") && !kept.contains_key("s5") && kept.contains_key("s6"));
}

#[test]
fn a_session_that_ends_with_a_failed_load_after_it_was_held_is_recorded_too() {
    let mut feed = Feed::new("process");
    feed.commit(&registry(), &agent(), 1);
    feed.commit_ending(
        &SessionRegistry::new("epoch"),
        &agent(),
        1,
        Instant::now(),
        &[("s".to_string(), failure("cannot reattach"))],
    );
    assert_eq!(feed.tree().get("/global/loadFailures/s").unwrap().to_value()["message"], "cannot reattach");
}

// --- views (design §4.5) ----------------------------------------------------------------

fn conversation(registry: &mut SessionRegistry, user: &str, agent_text: &str) {
    for (kind, id, text) in [("user_message_chunk", "u", user), ("agent_message_chunk", "a", agent_text)] {
        registry
            .append_session_update(
                "s",
                json!({ "sessionUpdate": kind, "messageId": format!("{id}{user}"),
                        "content": { "type": "text", "text": text } }),
            )
            .unwrap();
    }
}

#[test]
fn a_followed_view_is_materialized_at_once_and_follows_the_session_in_the_same_revision() {
    let mut feed = Feed::new("process");
    let mut registry = registry();
    conversation(&mut registry, "q", "one");
    feed.commit(&registry, &agent(), 1);
    assert!(feed.tree().get("/views").map(|views| views.to_value()) == Some(json!({})), "no one follows a view yet");

    let path = "/views/s/output/latest";
    let id = feed.subscribe_tree(&query(&format!("follow={path}")));
    let mut client = Client::following(&[path]);
    pump(&mut feed, id, &mut client);
    assert_eq!(client.value(path).unwrap()["text"], "one");

    conversation(&mut registry, "r", "two");
    feed.commit(&registry, &agent(), 1);
    pump(&mut feed, id, &mut client);
    assert_eq!(client.value(path).unwrap()["text"], "two");
    // The view changed in the revision that changed the session's entries.
    let rev = feed.tree().rev();
    assert_eq!(feed.tree().get(path).unwrap().rev(), rev);
    assert_eq!(feed.tree().get("/sessions/s/entries").unwrap().srev(), rev);
}

#[test]
fn a_view_no_one_follows_is_removed_at_the_next_commit() {
    let mut feed = Feed::new("process");
    let mut registry = registry();
    conversation(&mut registry, "q", "one");
    feed.commit(&registry, &agent(), 1);
    let path = "/views/s/output/latest";
    let id = feed.subscribe_tree(&query(&format!("follow={path}")));
    assert!(feed.tree().get(path).is_some());
    feed.unsubscribe(id);
    conversation(&mut registry, "r", "two");
    feed.commit(&registry, &agent(), 1);
    assert!(feed.tree().get(path).is_none());
}

#[test]
fn a_read_keeps_its_view_for_a_while_so_a_poll_finds_the_same_version() {
    let mut feed = Feed::new("process");
    let mut registry = registry();
    conversation(&mut registry, "q", "one");
    feed.commit(&registry, &agent(), 1);
    let path = "/views/s/output/latest";
    let id = feed.subscribe_tree_at(&query(&format!("follow={path}")), true);
    let held = feed.tree().get(path).unwrap().srev();
    feed.unsubscribe(id);
    // An unrelated commit: the pinned view stays, at the same path version.
    registry.start_login_terminal("login", "m");
    feed.commit(&registry, &agent(), 1);
    assert_eq!(feed.tree().get(path).unwrap().srev(), held);
}

#[test]
fn a_stream_of_a_view_holds_the_session_like_the_session_does() {
    let mut feed = Feed::new("process");
    feed.commit(&SessionRegistry::new("epoch"), &agent(), 1);
    let id = feed.subscribe_tree(&query("follow=/views/s/output/latest&cwd=s:/work"));
    assert_eq!(feed.followed_sessions()["s"], vec![Waiting { id, cwd: Some("/work".into()) }]);
}

fn end_of(event: &TreeEvent, path: &str) -> Option<TreeEndReason> {
    event.changes.iter().flatten().find_map(|change| match change {
        Change::End { path: at, end } if at == path => Some(end.reason),
        _ => None,
    })
}

fn history_rev(feed: &Feed) -> u64 {
    feed.tree().get("/sessions/s").unwrap().field("historyRev").and_then(Value::as_u64).unwrap()
}

#[test]
fn a_fixed_view_ends_when_its_turn_is_gone_or_its_history_replaced_and_stays_ended() {
    let mut feed = Feed::new("process");
    let mut registry = registry();
    conversation(&mut registry, "q", "one");
    feed.commit(&registry, &agent(), 1);
    let (instance, h) = (feed.tree().instance().to_string(), history_rev(&feed));
    let output = format!("/views/s/history/{instance}/{h}/output/0");
    let missing = format!("/views/s/history/{instance}/{h}/output/1");
    let id = feed.subscribe_tree(&query(&format!("follow={output}&follow={missing}")));
    let mut client = Client::following(&[output.as_str(), missing.as_str()]);
    let first = feed.next_tree(id).unwrap();
    assert_eq!(end_of(&first, &missing), Some(TreeEndReason::Absent), "no turn starts at 1");
    assert_eq!(end_of(&first, &output), None);
    client.apply(&first).unwrap();
    assert_eq!(client.value(&output).unwrap()["text"], "one");
    assert_eq!(feed.followed_sessions()["s"], vec![], "the finished view holds nothing, the other does");

    // The history is replaced: the view is replaced, and nothing follows its end.
    registry.register_cold("s");
    registry.register_new("s");
    feed.commit(&registry, &agent(), 1);
    let replaced = feed.next_tree(id).unwrap();
    assert_eq!(end_of(&replaced, &output), Some(TreeEndReason::Replaced));
    assert!(feed.next_tree(id).is_none());
    conversation(&mut registry, "q", "again");
    feed.commit(&registry, &agent(), 1);
    assert!(feed.next_tree(id).is_none(), "a finished view does not come back");
}

#[test]
fn a_fixed_view_of_another_instance_or_history_is_replaced_whatever_the_client_holds() {
    let mut feed = Feed::new("process");
    let mut registry = registry();
    conversation(&mut registry, "q", "one");
    feed.commit(&registry, &agent(), 1);
    let h = history_rev(&feed);
    let old_instance = format!("/views/s/history/OLDINSTANCE0000000000/{h}/output/0");
    let id = feed.subscribe_tree(&query(&format!("follow={old_instance}")));
    let first = feed.next_tree(id).unwrap();
    assert_eq!(end_of(&first, &old_instance), Some(TreeEndReason::Replaced));
    // Resubscribing as a client does after a failed verification (no `have`) says the same.
    let again = feed.subscribe_tree(&query(&format!("follow={old_instance}")));
    assert_eq!(end_of(&feed.next_tree(again).unwrap(), &old_instance), Some(TreeEndReason::Replaced));
    let other_history = format!("/views/s/history/{}/{}/output/0", feed.tree().instance(), h + 5);
    let third = feed.subscribe_tree(&query(&format!("follow={other_history}")));
    assert_eq!(end_of(&feed.next_tree(third).unwrap(), &other_history), Some(TreeEndReason::Replaced));
}

#[test]
fn a_fixed_view_of_a_session_not_loaded_yet_waits_and_is_replaced_once_it_is_loaded_otherwise() {
    let mut feed = Feed::new("process");
    feed.commit(&SessionRegistry::new("epoch"), &agent(), 1);
    let path = format!("/views/s/history/{}/1/output/0", feed.tree().instance());
    let id = feed.subscribe_tree(&query(&format!("follow={path}&cwd=s:/work")));
    let first = feed.next_tree(id).unwrap();
    assert_eq!(end_of(&first, &path), Some(TreeEndReason::Absent), "not final: the session is not there");
    assert_eq!(feed.followed_sessions()["s"].len(), 1, "still waiting for the load");
    let mut registry = registry();
    conversation(&mut registry, "q", "one");
    feed.commit(&registry, &agent(), 1);
    // The load installed another history than the one the path names.
    let h = history_rev(&feed);
    if h != 1 {
        let event = feed.next_tree(id).unwrap();
        assert_eq!(end_of(&event, &path), Some(TreeEndReason::Replaced));
    }
}
