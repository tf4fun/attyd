//! The events of a subscription (design §6.2), checked by a client model that applies them by
//! the design's rules and must hold, after each event, what the tree holds at its paths.
use std::collections::BTreeMap;

use serde_json::json;

use super::*;
use crate::protocol::tree::Committed;
use crate::protocol::tree::Spec;
use crate::protocol::tree_stream::client::Client;
use crate::protocol::tree_stream::params::{Follow, StreamQuery};

fn session(title: &str, entries: &[&str]) -> Spec {
    Spec::object(
        json!({ "title": title }).as_object().unwrap().clone(),
        [(
            "entries",
            Spec::List(entries.iter().map(|text| Spec::value(json!({ "text": text }))).collect()),
        )],
    )
}

fn root(global: i64, sessions: &[(&str, Spec)]) -> BTreeMap<String, Spec> {
    BTreeMap::from([
        ("global".to_string(), Spec::value(json!({ "n": global }))),
        ("sessions".to_string(), Spec::map(sessions.iter().map(|(id, spec)| (*id, spec.clone())))),
    ])
}

fn query(paths: &[&str]) -> StreamQuery {
    StreamQuery {
        instance: None,
        follow: paths
            .iter()
            .map(|path| Follow { path: path.to_string(), have: None, window: None })
            .collect(),
        cwd: BTreeMap::new(),
    }
}

/// Drains the subscriber into the client, checking each event.
fn pump(subscriber: &mut Subscriber, client: &mut Client) -> usize {
    let mut events = 0;
    while let Some(event) = subscriber.next() {
        client.apply(&event).unwrap_or_else(|gap| panic!("{gap}: {event:?}"));
        events += 1;
    }
    events
}

fn holds(client: &Client, tree: &Tree, paths: &[&str]) {
    for path in paths {
        assert_eq!(
            client.value(path).cloned(),
            tree.get(path).map(|node| node.to_value()),
            "{path}"
        );
        if let Some(node) = tree.get(path) {
            assert_eq!(client.version(path), node.srev(), "{path} is held at its path version");
        }
    }
}

#[test]
fn the_first_event_names_the_instance_and_snapshots_every_followed_path() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[("s", session("t", &["a"]))]));
    let mut subscriber = Subscriber::new(&query(&["/sessions/s/entries", "/global"]), &tree);
    let event = subscriber.next().unwrap();
    assert_eq!(event.instance.as_deref(), Some("I"));
    assert_eq!(event.rev, 1);
    assert_eq!(event.changes.as_ref().unwrap().len(), 2);
    assert!(subscriber.next().is_none());
}

#[test]
fn a_followed_path_is_kept_in_step_with_the_tree_by_ops() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[("s", session("t", &["a"]))]));
    let paths = ["/sessions/s/entries", "/sessions/s", "/global"];
    let (mut client, mut subscriber) = (Client::following(&paths[..1]), Subscriber::new(&query(&paths[..1]), &tree));
    pump(&mut subscriber, &mut client);
    for step in [
        root(0, &[("s", session("t", &["a", "b"]))]),
        root(1, &[("s", session("t", &["a", "bb"]))]),
        root(1, &[("s", session("u", &["a", "bb"]))]),
        root(2, &[("s", session("u", &["a", "bb", "c"]))]),
    ] {
        let committed = tree.commit_with_ops(step).unwrap();
        subscriber.publish(&tree, &committed);
        pump(&mut subscriber, &mut client);
        holds(&client, &tree, &paths[..1]);
    }
}

#[test]
fn a_commit_that_touched_only_other_paths_queues_nothing() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[("s", session("t", &["a"]))]));
    let mut subscriber = Subscriber::new(&query(&["/sessions/s/entries"]), &tree);
    subscriber.next();
    let committed = tree.commit_with_ops(root(1, &[("s", session("t", &["a"]))])).unwrap();
    subscriber.publish(&tree, &committed);
    assert_eq!(subscriber.queued(), 0);
}

#[test]
fn one_commit_is_one_event_with_every_followed_path_it_changed() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[("s", session("t", &["a"]))]));
    let mut subscriber = Subscriber::new(&query(&["/sessions/s/entries", "/global"]), &tree);
    subscriber.next();
    let committed = tree.commit_with_ops(root(1, &[("s", session("t", &["a", "b"]))])).unwrap();
    subscriber.publish(&tree, &committed);
    assert_eq!(subscriber.queued(), 1);
    let event = subscriber.next().unwrap();
    assert_eq!(event.rev, 2);
    let paths: Vec<&str> = event.changes.as_ref().unwrap().iter().map(Change::path).collect();
    assert_eq!(paths, ["/global", "/sessions/s/entries"]);
}

#[test]
fn ops_are_relative_to_the_followed_path_and_chain_from_its_version() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[("s", session("t", &["a"]))]));
    tree.commit(root(1, &[("s", session("t", &["a"]))]));
    tree.commit(root(2, &[("s", session("t", &["a"]))]));
    let mut subscriber = Subscriber::new(&query(&["/sessions/s/entries"]), &tree);
    subscriber.next();
    let committed = tree.commit_with_ops(root(2, &[("s", session("t", &["a", "b"]))])).unwrap();
    subscriber.publish(&tree, &committed);
    let event = subscriber.next().unwrap();
    let Change::Ops { from, ops, .. } = &event.changes.unwrap()[0] else { panic!() };
    assert_eq!(*from, 1, "the path last changed at 1, not at the commit before");
    assert_eq!(ops, &[Op::Append {
        path: String::new(),
        tail: crate::protocol::AppendTail::Values { values: vec![json!({ "text": "b" })] },
    }]);
}

#[test]
fn a_session_installed_later_is_snapshotted_and_one_removed_ends_absent() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[]));
    let mut subscriber = Subscriber::new(&query(&["/sessions/s"]), &tree);
    let mut client = Client::following(&["/sessions/s"]);
    let first = subscriber.next().unwrap();
    assert!(matches!(first.changes.as_ref().unwrap()[0], Change::End { .. }), "absent at the start");
    client.apply(&first).unwrap();

    for (step, present) in [
        (root(0, &[("s", session("t", &["a"]))]), true),
        (root(0, &[("s", session("t", &["a", "b"]))]), true),
        (root(0, &[]), false),
        (root(0, &[("s", session("t", &[]))]), true),
    ] {
        let committed = tree.commit_with_ops(step).unwrap();
        subscriber.publish(&tree, &committed);
        pump(&mut subscriber, &mut client);
        assert_eq!(client.value("/sessions/s").is_some(), present);
        holds(&client, &tree, &["/sessions/s"]);
    }
}

#[test]
fn a_followed_path_replaced_whole_by_an_ancestor_op_is_sent_as_a_snapshot() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[("s", session("t", &["a", "b"]))]));
    let mut subscriber = Subscriber::new(&query(&["/sessions/s/entries/0"]), &tree);
    let mut client = Client::following(&["/sessions/s/entries/0"]);
    pump(&mut subscriber, &mut client);
    // A shrinking list is a whole `set` of the list, above the followed item.
    let committed = tree.commit_with_ops(root(0, &[("s", session("t", &["z"]))])).unwrap();
    subscriber.publish(&tree, &committed);
    let event = subscriber.next().unwrap();
    assert!(matches!(event.changes.as_ref().unwrap()[0], Change::Snapshot { .. }));
    client.apply(&event).unwrap();
    holds(&client, &tree, &["/sessions/s/entries/0"]);
}

#[test]
fn a_client_holding_the_instance_and_a_version_is_sent_only_what_changed() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[("s", session("t", &["a"]))]));
    let held = tree.get("/sessions/s/entries").unwrap();
    let (value, version) = (held.to_value(), held.srev());
    tree.commit(root(0, &[("s", session("t", &["a", "b"]))]));

    let mut q = query(&["/sessions/s/entries"]);
    q.instance = Some("I".into());
    q.follow[0].have = Some(version);
    let mut client = Client::following(&["/sessions/s/entries"]);
    client.instance = Some("I".into());
    *client.paths.get_mut("/sessions/s/entries").unwrap() =
        crate::protocol::tree_stream::client::Held { value: Some(value), version };
    let mut subscriber = Subscriber::new(&q, &tree);
    pump(&mut subscriber, &mut client);
    holds(&client, &tree, &["/sessions/s/entries"]);
}

#[test]
fn a_have_of_another_instance_is_ignored_and_the_client_installs_whole_snapshots() {
    let mut tree = Tree::new("NEW");
    tree.commit(root(0, &[("s", session("t", &["a"]))]));
    let mut q = query(&["/sessions/s/entries"]);
    q.instance = Some("OLD".into());
    q.follow[0].have = Some(1);
    let mut subscriber = Subscriber::new(&q, &tree);
    let event = subscriber.next().unwrap();
    let Change::Snapshot { snapshot, .. } = &event.changes.as_ref().unwrap()[0] else { panic!() };
    assert_eq!(snapshot.base, None);
    let mut client = Client::following(&["/sessions/s/entries"]);
    client.instance = Some("OLD".into());
    client.paths.get_mut("/sessions/s/entries").unwrap().version = 50;
    client.apply(&event).unwrap();
    assert_eq!(client.instance.as_deref(), Some("NEW"));
    holds(&client, &tree, &["/sessions/s/entries"]);
}

#[test]
fn a_list_appended_above_a_followed_item_changes_nothing_for_it() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[("s", session("t", &["a"]))]));
    let mut subscriber = Subscriber::new(&query(&["/sessions/s/entries/0"]), &tree);
    let mut client = Client::following(&["/sessions/s/entries/0"]);
    pump(&mut subscriber, &mut client);
    let committed = tree.commit_with_ops(root(0, &[("s", session("t", &["a", "b"]))])).unwrap();
    subscriber.publish(&tree, &committed);
    assert_eq!(subscriber.queued(), 0);
    // The item that is new is a snapshot when it is followed.
    let mut subscriber = Subscriber::new(&query(&["/sessions/s/entries/2"]), &tree);
    let mut client = Client::following(&["/sessions/s/entries/2"]);
    pump(&mut subscriber, &mut client);
    assert_eq!(client.value("/sessions/s/entries/2"), None);
    let committed = tree.commit_with_ops(root(0, &[("s", session("t", &["a", "b", "c"]))])).unwrap();
    subscriber.publish(&tree, &committed);
    pump(&mut subscriber, &mut client);
    holds(&client, &tree, &["/sessions/s/entries/2"]);
}

#[test]
fn a_value_set_at_the_followed_path_is_a_set_of_the_whole_relative_path() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[]));
    let mut subscriber = Subscriber::new(&query(&["/global"]), &tree);
    let mut client = Client::following(&["/global"]);
    pump(&mut subscriber, &mut client);
    let committed = tree.commit_with_ops(root(5, &[])).unwrap();
    subscriber.publish(&tree, &committed);
    let event = subscriber.next().unwrap();
    let Change::Ops { ops, from, .. } = &event.changes.as_ref().unwrap()[0] else { panic!() };
    assert_eq!(*from, 1);
    assert_eq!(ops, &[Op::Set { path: "/n".into(), value: json!(5) }]);
    client.apply(&event).unwrap();
    holds(&client, &tree, &["/global"]);
}

// --- checkpoints (design §6.5) -------------------------------------------------------

use std::time::Duration;

use tokio::time::Instant;

use crate::protocol::digest;

fn committed(tree: &mut Tree, step: BTreeMap<String, Spec>) -> Committed {
    tree.commit_with_ops(step).unwrap()
}

#[test]
fn a_path_that_changed_is_checked_after_a_pause_and_the_check_states_its_version_and_hash() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[("s", session("t", &["a"]))]));
    let paths = ["/sessions/s/entries"];
    let (mut client, mut subscriber) = (Client::following(&paths), Subscriber::new(&query(&paths), &tree));
    pump(&mut subscriber, &mut client);
    let t0 = Instant::now();
    let change = committed(&mut tree, root(0, &[("s", session("t", &["a", "b"]))]));
    subscriber.publish_at(&tree, &change, t0);
    pump(&mut subscriber, &mut client);

    assert!(!subscriber.checkpoints(&tree, t0 + Duration::from_millis(499)));
    assert_eq!(subscriber.deadline(), Some(t0 + Duration::from_millis(500)));
    assert!(subscriber.checkpoints(&tree, t0 + Duration::from_millis(500)));
    pump(&mut subscriber, &mut client);
    let node = tree.get(paths[0]).unwrap();
    assert_eq!(client.checks, vec![(paths[0].to_string(), node.srev(), digest::wire(&node.hash()))]);
    assert!(!subscriber.checkpoints(&tree, t0 + Duration::from_secs(60)), "nothing changed since");
    assert_eq!(subscriber.deadline(), None);
}

#[test]
fn a_path_that_keeps_changing_is_checked_at_least_every_two_seconds() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[("s", session("t", &[]))]));
    let mut subscriber = Subscriber::new(&query(&["/sessions/s/entries"]), &tree);
    subscriber.next();
    let t0 = Instant::now();
    let mut entries = Vec::new();
    for n in 0..5 {
        entries.push("x");
        let at = t0 + Duration::from_millis(400 * n);
        let change = committed(&mut tree, root(0, &[("s", session("t", &entries))]));
        subscriber.publish_at(&tree, &change, at);
        assert!(!subscriber.checkpoints(&tree, at), "n={n}");
    }
    assert_eq!(subscriber.deadline(), Some(t0 + Duration::from_secs(2)));
    assert!(subscriber.checkpoints(&tree, t0 + Duration::from_secs(2)));
}

#[test]
fn a_snapshot_carries_its_hash_and_settles_the_path() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[("s", session("t", &["a", "b"]))]));
    let mut subscriber = Subscriber::new(&query(&["/sessions/s/entries/0"]), &tree);
    subscriber.next();
    let t0 = Instant::now();
    let change = committed(&mut tree, root(0, &[("s", session("t", &["A", "b"]))]));
    subscriber.publish_at(&tree, &change, t0);
    assert!(subscriber.deadline().is_some());
    let shrink = committed(&mut tree, root(0, &[("s", session("t", &["z"]))]));
    subscriber.publish_at(&tree, &shrink, t0 + Duration::from_millis(10));
    assert_eq!(subscriber.deadline(), None, "the whole path was sent, with its hash");
}

#[test]
fn a_path_that_is_gone_is_not_checked() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[("s", session("t", &["a"]))]));
    let mut subscriber = Subscriber::new(&query(&["/sessions/s"]), &tree);
    subscriber.next();
    let t0 = Instant::now();
    let change = committed(&mut tree, root(0, &[("s", session("u", &["a"]))]));
    subscriber.publish_at(&tree, &change, t0);
    let gone = committed(&mut tree, root(0, &[]));
    subscriber.publish_at(&tree, &gone, t0);
    assert!(!subscriber.checkpoints(&tree, t0 + Duration::from_secs(1)));
}

#[test]
fn a_check_of_a_later_version_than_the_client_holds_reveals_a_missed_change() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[("s", session("t", &["a"]))]));
    let paths = ["/sessions/s/entries"];
    let (mut client, mut subscriber) = (Client::following(&paths), Subscriber::new(&query(&paths), &tree));
    pump(&mut subscriber, &mut client);
    let t0 = Instant::now();
    let change = committed(&mut tree, root(0, &[("s", session("t", &["a", "b"]))]));
    subscriber.publish_at(&tree, &change, t0);
    subscriber.next().expect("the only change of the path is lost on the way");
    assert!(subscriber.checkpoints(&tree, t0 + Duration::from_millis(500)));
    let check = subscriber.next().unwrap();
    let gap = client.apply(&check).expect_err("the path went quiet, the check at > v_p says why");
    assert!(gap.contains("missed"), "{gap}");
}

#[test]
fn a_check_older_than_what_the_client_holds_is_ignored() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[("s", session("t", &["a"]))]));
    let paths = ["/sessions/s/entries"];
    let (mut client, mut subscriber) = (Client::following(&paths), Subscriber::new(&query(&paths), &tree));
    pump(&mut subscriber, &mut client);
    let t0 = Instant::now();
    let change = committed(&mut tree, root(0, &[("s", session("t", &["a", "b"]))]));
    subscriber.publish_at(&tree, &change, t0);
    assert!(subscriber.checkpoints(&tree, t0 + Duration::from_millis(500)));
    // The client has applied the change; the check is of the version before it.
    let ops = subscriber.next().unwrap();
    client.apply(&ops).unwrap();
    let mut old = subscriber.next().unwrap();
    old.check.as_mut().unwrap()[0].at = 1;
    client.apply(&old).unwrap();
    assert!(client.checks.is_empty());
}

// --- a subscriber that cannot keep up (design §6.5) -----------------------------------

fn big(n: usize) -> String {
    "x".repeat(n)
}

#[test]
fn a_queue_past_the_budget_is_replaced_by_one_catch_up_from_the_version_written() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[("s", session("t", &["a"]))]));
    let paths = ["/sessions/s/entries", "/global"];
    let (mut client, mut subscriber) = (Client::following(&paths), Subscriber::new(&query(&paths), &tree));
    pump(&mut subscriber, &mut client);
    let mut entries = vec!["a".to_string()];
    let mut n = 0;
    while !subscriber.over_budget() {
        n += 1;
        entries.push(big(100_000));
        let refs: Vec<&str> = entries.iter().map(String::as_str).collect();
        let change = committed(&mut tree, root(n, &[("s", session("t", &refs))]));
        subscriber.publish(&tree, &change);
        assert!(n < 100);
    }
    assert!(subscriber.queued() > 1);
    subscriber.coalesce(&tree);
    assert_eq!(subscriber.queued(), 1);
    assert!(!subscriber.over_budget());
    let catch_up = subscriber.next().unwrap();
    assert_eq!(catch_up.rev, tree.rev());
    assert_eq!(catch_up.instance, None, "the first event was written");
    client.apply(&catch_up).unwrap();
    holds(&client, &tree, &paths);
    // The catch-up carries only what changed since the version written: entry 0 is not in it.
    let json = serde_json::to_string(&catch_up).unwrap();
    assert!(json.contains("\"1\"") && !json.contains("\"text\":\"a\""), "{}", &json[..200.min(json.len())]);
}

#[test]
fn a_catch_up_goes_on_from_what_was_written_not_from_what_was_queued() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[("s", session("t", &["a"]))]));
    let paths = ["/sessions/s/entries"];
    let (mut client, mut subscriber) = (Client::following(&paths), Subscriber::new(&query(&paths), &tree));
    pump(&mut subscriber, &mut client);
    for entries in [&["a", "b"][..], &["a", "b", "c"], &["a", "b", "c", "d"]] {
        let change = committed(&mut tree, root(0, &[("s", session("t", entries))]));
        subscriber.publish(&tree, &change);
    }
    // The client got the first of them only.
    let first = subscriber.next().unwrap();
    client.apply(&first).unwrap();
    subscriber.coalesce(&tree);
    pump(&mut subscriber, &mut client);
    holds(&client, &tree, &paths);
    // And the stream goes on with ops from the version the catch-up installed.
    let change = committed(&mut tree, root(0, &[("s", session("t", &["a", "b", "c", "d", "e"]))]));
    subscriber.publish(&tree, &change);
    pump(&mut subscriber, &mut client);
    holds(&client, &tree, &paths);
}

#[test]
fn a_catch_up_ends_a_path_that_is_gone_and_keeps_a_path_never_held_quiet() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[("s", session("t", &["a"]))]));
    let paths = ["/sessions/s", "/sessions/u"];
    let (mut client, mut subscriber) = (Client::following(&paths), Subscriber::new(&query(&paths), &tree));
    pump(&mut subscriber, &mut client);
    let change = committed(&mut tree, root(0, &[]));
    subscriber.publish(&tree, &change);
    subscriber.coalesce(&tree);
    let catch_up = subscriber.next().unwrap();
    let changes = catch_up.changes.as_ref().unwrap();
    assert_eq!(changes.len(), 1, "/sessions/u was never held");
    assert!(matches!(&changes[0], Change::End { path, .. } if path == "/sessions/s"));
    client.apply(&catch_up).unwrap();
    assert_eq!(client.value("/sessions/s"), None);
}

#[test]
fn a_catch_up_that_replaces_the_first_event_names_the_instance() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[("s", session("t", &["a"]))]));
    let paths = ["/sessions/s/entries"];
    let (mut client, mut subscriber) = (Client::following(&paths), Subscriber::new(&query(&paths), &tree));
    let change = committed(&mut tree, root(0, &[("s", session("t", &["a", "b"]))]));
    subscriber.publish(&tree, &change);
    subscriber.coalesce(&tree);
    assert_eq!(subscriber.queued(), 1);
    let catch_up = subscriber.next().unwrap();
    assert_eq!(catch_up.instance.as_deref(), Some("I"));
    client.apply(&catch_up).unwrap();
    holds(&client, &tree, &paths);
}

#[test]
fn snapshots_do_not_count_against_the_budget() {
    let mut tree = Tree::new("I");
    let text = big(2_000_000);
    tree.commit(root(0, &[("s", session("t", &[text.as_str()]))]));
    let subscriber = Subscriber::new(&query(&["/sessions/s"]), &tree);
    assert!(!subscriber.over_budget(), "the first event is a 2 MB snapshot");
}

#[test]
fn an_end_queued_behind_events_reaches_a_slow_subscriber_with_its_reason() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[("s", session("t", &["a"]))]));
    let paths = ["/sessions/s", "/sessions/u"];
    let (mut client, mut subscriber) = (Client::following(&paths), Subscriber::new(&query(&paths), &tree));
    pump(&mut subscriber, &mut client);
    let ends = BTreeMap::from([(
        "s".to_string(),
        End { reason: EndReason::Deleted, error: None },
    )]);
    let change = committed(&mut tree, root(1, &[("s", session("u", &["a"]))]));
    subscriber.publish(&tree, &change);
    let gone = committed(&mut tree, root(1, &[]));
    subscriber.publish_ended(&tree, &gone, Instant::now(), &ends);
    subscriber.end_session(&tree, "u", &End { reason: EndReason::LoadFailed, error: None });
    subscriber.coalesce(&tree);
    let catch_up = subscriber.next().unwrap();
    let changes = catch_up.changes.as_ref().unwrap();
    let reason = |path: &str| match changes.iter().find(|c| c.path() == path) {
        Some(Change::End { end, .. }) => Some(end.reason),
        other => panic!("{path}: {other:?}"),
    };
    assert_eq!(reason("/sessions/s"), Some(EndReason::Deleted));
    assert_eq!(reason("/sessions/u"), Some(EndReason::LoadFailed), "never held, but its end is owed");
    assert!(changes.iter().all(|change| !matches!(change, Change::Snapshot { .. })), "no snapshot for an ended path");
}

#[test]
fn an_end_followed_by_the_path_coming_back_is_not_kept() {
    let mut tree = Tree::new("I");
    tree.commit(root(0, &[("s", session("t", &["a"]))]));
    let (mut client, mut subscriber) = (Client::following(&["/sessions/s"]), Subscriber::new(&query(&["/sessions/s"]), &tree));
    pump(&mut subscriber, &mut client);
    let gone = committed(&mut tree, root(0, &[]));
    subscriber.publish(&tree, &gone);
    let back = committed(&mut tree, root(0, &[("s", session("t", &["b"]))]));
    subscriber.publish(&tree, &back);
    subscriber.coalesce(&tree);
    pump(&mut subscriber, &mut client);
    holds(&client, &tree, &["/sessions/s"]);
}
