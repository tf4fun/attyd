//! The events of a subscription (design §6.2), checked by a client model that applies them by
//! the design's rules and must hold, after each event, what the tree holds at its paths.
use std::collections::BTreeMap;

use serde_json::json;

use super::*;
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
    let mut subscriber = Subscriber::new(&query(&["/sessions/s/entries"]), &tree);
    let mut client = Client::following(&["/sessions/s/entries"]);
    pump(&mut subscriber, &mut client);
    // A shrinking list is a whole `set` of the list: the followed path itself.
    let committed = tree.commit_with_ops(root(0, &[("s", session("t", &["z"]))])).unwrap();
    subscriber.publish(&tree, &committed);
    let event = subscriber.next().unwrap();
    assert!(matches!(event.changes.as_ref().unwrap()[0], Change::Snapshot { .. }));
    client.apply(&event).unwrap();
    holds(&client, &tree, &["/sessions/s/entries"]);
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
