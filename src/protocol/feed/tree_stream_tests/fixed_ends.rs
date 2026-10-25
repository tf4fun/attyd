//! A slow stream still owes each fixed view its final end, even after tracking stops.
use super::*;

use crate::protocol::tree_stream::QUEUE_BUDGET;

const ENTRIES: &str = "/sessions/s/entries";

fn assert_end(event: &TreeEvent, path: &str, reason: Option<TreeEndReason>) {
    let changes: Vec<_> = event
        .changes
        .iter()
        .flatten()
        .filter(|change| change.path() == path)
        .collect();
    match reason {
        Some(reason) => {
            assert_eq!(changes.len(), 1, "one final change of {path}");
            let Change::End { end, .. } = changes[0] else {
                panic!("a finished view must end, not become a snapshot: {path}");
            };
            assert_eq!(end.reason, reason);
            assert!(end.error.is_none());
        }
        None => assert!(
            changes.is_empty(),
            "an already delivered end stays final: {path}"
        ),
    }
}

/// The ordinary entries path creates real queued ops beyond the feed's budget. The feed,
/// rather than the test, must replace them with a catch-up snapshot.
fn overflow(feed: &mut Feed, id: SubscriptionId, registry: &mut SessionRegistry) {
    chunk(registry, &"x".repeat(QUEUE_BUDGET + 1));
    feed.commit(registry, &agent(), 1);
    let subscriber = &feed.tree_subscribers[&id];
    assert_eq!(subscriber.queued(), 1, "the feed coalesced the queue");
    assert!(!subscriber.over_budget());
    assert!(
        subscriber
            .peek_first()
            .changes
            .iter()
            .flatten()
            .any(|change| { matches!(change, Change::Snapshot { path, .. } if path == ENTRIES) }),
        "the active path caught up by snapshot, not by the oversized ops"
    );
}

fn coalesce_again(feed: &mut Feed, id: SubscriptionId) {
    feed.tree_subscribers
        .get_mut(&id)
        .unwrap()
        .coalesce(&feed.tree);
    assert_eq!(feed.tree_subscribers[&id].queued(), 1);
}

fn assert_entries(client: &Client, feed: &Feed) {
    assert_eq!(
        client.value(ENTRIES),
        Some(&feed.tree().get(ENTRIES).unwrap().to_value())
    );
}

#[test]
fn initial_fixed_view_ends_survive_catch_up_only_until_they_are_written() {
    for first_written in [false, true] {
        let mut feed = Feed::new();
        let mut registry = registry();
        conversation(&mut registry, "q", "one");
        feed.commit(&registry, &agent(), 1);
        let instance = feed.tree().instance().to_string();
        let h = history_rev(&feed);
        // One missing turn and one replaced history: both are permanent ends, unlike
        // an absent session waiting to load. Cover both fixed view shapes as well.
        let absent = format!("/views/s/history/{instance}/{h}/output/1");
        let replaced = format!("/views/s/history/{instance}/{}/turn/0", h + 1);
        let id = feed.subscribe_tree(&query(&format!(
            "follow={ENTRIES}&follow={absent}&follow={replaced}"
        )));
        let mut client = Client::following(&[ENTRIES, &absent, &replaced]);
        if first_written {
            let first = feed.next_tree(id).unwrap();
            assert_end(&first, &absent, Some(TreeEndReason::Absent));
            assert_end(&first, &replaced, Some(TreeEndReason::Replaced));
            client.apply(&first).unwrap();
        }

        overflow(&mut feed, id, &mut registry);
        // A catch-up can itself be superseded before the transport takes it. The debt
        // must survive each replacement; over alone does not mean the end was sent.
        coalesce_again(&mut feed, id);
        coalesce_again(&mut feed, id);
        let caught_up = feed.next_tree(id).unwrap();
        assert_eq!(caught_up.instance.is_some(), !first_written);
        assert_end(
            &caught_up,
            &absent,
            (!first_written).then_some(TreeEndReason::Absent),
        );
        assert_end(
            &caught_up,
            &replaced,
            (!first_written).then_some(TreeEndReason::Replaced),
        );
        client.apply(&caught_up).unwrap();
        assert_entries(&client, &feed);
        assert!(client.value(&absent).is_none());
        assert!(client.value(&replaced).is_none());
        assert!(feed.next_tree(id).is_none());

        // After next() has delivered the ends, further traffic and catch-up must neither
        // resend them nor reconstruct the old selections from a later tree.
        overflow(&mut feed, id, &mut registry);
        coalesce_again(&mut feed, id);
        let later = feed.next_tree(id).unwrap();
        assert!(later.instance.is_none());
        assert_end(&later, &absent, None);
        assert_end(&later, &replaced, None);
        client.apply(&later).unwrap();
        assert_entries(&client, &feed);
        assert!(feed.next_tree(id).is_none());
    }
}

#[test]
fn a_fixed_view_replaced_after_alignment_keeps_its_end_while_session_paths_can_reopen() {
    for first_written in [false, true] {
        let mut feed = Feed::new();
        let mut registry = registry();
        conversation(&mut registry, "q", "one");
        feed.commit(&registry, &agent(), 1);
        let path = format!(
            "/views/s/history/{}/{}/output/0",
            feed.tree().instance(),
            history_rev(&feed)
        );
        let id = feed.subscribe_tree(&query(&format!("follow={ENTRIES}&follow={path}")));
        let mut client = Client::following(&[ENTRIES, &path]);
        if first_written {
            client.apply(&feed.next_tree(id).unwrap()).unwrap();
            assert_eq!(client.value(&path).unwrap()["text"], "one");
        }

        registry.register_cold("s");
        registry.register_new("s");
        feed.commit(&registry, &agent(), 1);
        // Leave the history replacement's end queued. A different, still active path
        // pushes this same stream over budget before the final end can be delivered.
        overflow(&mut feed, id, &mut registry);
        coalesce_again(&mut feed, id);
        coalesce_again(&mut feed, id);
        let replaced = feed.next_tree(id).unwrap();
        assert_eq!(replaced.instance.is_some(), !first_written);
        assert_end(&replaced, &path, Some(TreeEndReason::Replaced));
        client.apply(&replaced).unwrap();
        assert!(
            client.value(&path).is_none(),
            "the client's old output was removed"
        );
        assert_entries(&client, &feed);
        assert!(feed.next_tree(id).is_none());

        // The terminal state belongs only to the fixed selection. An ordinary session
        // path on the same stream can end, then start again with its new contents.
        // Keep the same registry: replacing it would also rewind global operation IDs.
        registry.retire_entry("s");
        feed.commit_ending(
            &registry,
            &agent(),
            1,
            Instant::now(),
            &[(
                "s".into(),
                End {
                    reason: EndReason::Closed,
                    error: None,
                },
            )],
        );
        coalesce_again(&mut feed, id);
        let closed = feed.next_tree(id).unwrap();
        assert_end(&closed, &path, None);
        assert_end(&closed, ENTRIES, Some(TreeEndReason::Closed));
        client.apply(&closed).unwrap();
        assert!(client.value(ENTRIES).is_none());

        registry
            .open_new("epoch", "s", "/work", json!({ "sessionId": "s" }))
            .unwrap();
        conversation(&mut registry, "again", "new output");
        feed.commit(&registry, &agent(), 1);
        coalesce_again(&mut feed, id);
        let reopened = feed.next_tree(id).unwrap();
        assert_end(&reopened, &path, None);
        client.apply(&reopened).unwrap();
        assert_entries(&client, &feed);
        assert!(client.value(&path).is_none());
        assert!(feed.next_tree(id).is_none());
    }
}
