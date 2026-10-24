//! Snapshots (design §6.3). A client is simulated by `install`, which follows the design's rule:
//! keep every child the structure names and the snapshot omits, drop the rest, replace the
//! listed; the result must be what the tree holds and hash as the snapshot says.
use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::*;
use crate::protocol::tree::Spec;
use crate::protocol::tree_stream::client::install;

fn session(title: &str, entries: &[&str]) -> Spec {
    Spec::object(
        json!({ "title": title }).as_object().unwrap().clone(),
        [(
            "entries",
            Spec::List(entries.iter().map(|text| Spec::value(json!({ "text": text }))).collect()),
        )],
    )
}

fn root(sessions: &[(&str, Spec)]) -> BTreeMap<String, Spec> {
    BTreeMap::from([
        ("global".to_string(), Spec::value(json!({ "agent": "a" }))),
        ("sessions".to_string(), Spec::map(sessions.iter().map(|(id, spec)| (*id, spec.clone())))),
    ])
}

fn hash_of(value: &Value, tree: &Tree, path: &str) -> String {
    let _ = value;
    digest::wire(&tree.get(path).unwrap().hash())
}

#[test]
fn a_full_snapshot_is_the_whole_subtree_at_its_path_version() {
    let mut tree = Tree::new("i");
    tree.commit(root(&[("s", session("t", &["a", "b"]))]));
    tree.commit(root(&[("s", session("t", &["a", "c"]))]));
    let snapshot = snapshot(&tree, "/sessions/s", None).unwrap();
    assert_eq!(snapshot.base, None);
    assert_eq!(snapshot.at, 2, "the path last changed at 2");
    assert_eq!(snapshot.node, SNode::Value { v: tree.get("/sessions/s").unwrap().to_value() });
    assert_eq!(snapshot.hash, hash_of(&Value::Null, &tree, "/sessions/s"));
    assert_eq!(snapshot.hash.len(), 22);
}

#[test]
fn a_path_last_changed_long_ago_is_held_at_that_version_not_at_the_commit() {
    let mut tree = Tree::new("i");
    tree.commit(root(&[("s", session("t", &["a"]))]));
    for n in 0..5 {
        let mut next = root(&[("s", session("t", &["a"]))]);
        next.insert("global".into(), Spec::value(json!({ "agent": n })));
        tree.commit(next);
    }
    assert_eq!(tree.rev(), 6);
    assert_eq!(snapshot(&tree, "/sessions/s", None).unwrap().at, 1);
}

#[test]
fn a_missing_path_has_no_snapshot() {
    let tree = Tree::new("i");
    assert!(snapshot(&tree, "/sessions/s", None).is_none());
}

#[test]
fn a_differential_snapshot_sends_the_structure_and_only_what_changed() {
    let mut tree = Tree::new("i");
    tree.commit(root(&[("s", session("t", &["a", "b", "c"]))]));
    let held = tree.get("/sessions/s").unwrap().to_value();
    let have = tree.get("/sessions/s").unwrap().srev();
    tree.commit(root(&[("s", session("t", &["a", "B", "c", "d"]))]));
    let snapshot = snapshot(&tree, "/sessions/s", Some(have)).unwrap();
    assert_eq!(snapshot.base, Some(have));
    assert_eq!(snapshot.at, 2);
    let SNode::Object { o, k, c } = &snapshot.node else { panic!("{:?}", snapshot.node) };
    assert_eq!(o, &json!({ "title": "t" }));
    assert_eq!(k, &vec!["entries".to_string()]);
    let SNode::List { l, c } = &c["entries"] else { panic!() };
    assert_eq!(*l, 4);
    assert_eq!(c.keys().map(String::as_str).collect::<Vec<_>>(), ["1", "3"], "items 0 and 2 are kept");
    assert_eq!(install(Some(&held), &snapshot.node), tree.get("/sessions/s").unwrap().to_value());
}

#[test]
fn a_path_that_did_not_change_is_the_same() {
    let mut tree = Tree::new("i");
    tree.commit(root(&[("s", session("t", &["a"]))]));
    tree.commit(root(&[("s", session("t", &["a"]))]));
    let have = tree.get("/sessions/s").unwrap().srev();
    let snapshot = snapshot(&tree, "/sessions/s", Some(have)).unwrap();
    assert_eq!((snapshot.base, snapshot.at), (Some(have), have));
    assert_eq!(snapshot.node, SNode::Same { same: true });
}

#[test]
fn removed_children_and_keys_are_dropped_by_the_structure() {
    let mut tree = Tree::new("i");
    tree.commit(root(&[("s", session("t", &["a"])), ("u", session("t", &[]))]));
    let held = tree.get("/sessions").unwrap().to_value();
    let have = tree.get("/sessions").unwrap().srev();
    tree.commit(root(&[("s", session("t", &["a"]))]));
    let snapshot = snapshot(&tree, "/sessions", Some(have)).unwrap();
    assert_eq!(install(Some(&held), &snapshot.node), tree.get("/sessions").unwrap().to_value());
    let SNode::Map { m, c } = &snapshot.node else { panic!() };
    assert_eq!(m, &vec!["s".to_string()]);
    assert!(c.is_empty());
}

#[test]
fn a_child_that_changed_kind_is_replaced() {
    let mut tree = Tree::new("i");
    tree.commit(BTreeMap::from([("x".to_string(), Spec::map([("a", Spec::value(json!(1)))]))]));
    let held = tree.root().to_value();
    let have = tree.root().srev();
    tree.commit(BTreeMap::from([("x".to_string(), Spec::List(vec![Spec::value(json!(1))]))]));
    let snapshot = snapshot(&tree, "/x", Some(have)).unwrap();
    assert_eq!(install(held.get("x"), &snapshot.node), json!([1]));
}

/// Over a sequence of commits, a client that installs the differential snapshot from the
/// version it holds always ends up with the tree's value and the tree's hash.
#[test]
fn installing_differential_snapshots_always_reproduces_the_tree() {
    let steps = [
        root(&[("s", session("t", &["a"]))]),
        root(&[("s", session("t", &["a", "b"]))]),
        root(&[("s", session("t", &["a", "b"])), ("u", session("u", &["x"]))]),
        root(&[("s", session("v", &["a", "c"])), ("u", session("u", &["x"]))]),
        root(&[("u", session("u", &["x", "y"]))]),
        root(&[("s", session("t", &["z"])), ("u", session("u", &["x", "y"]))]),
        root(&[("s", session("t", &["z"]))]),
    ];
    let mut tree = Tree::new("i");
    let mut held: Option<(Value, u64)> = None;
    for step in steps {
        tree.commit(step);
        let snapshot = snapshot(&tree, "/sessions", held.as_ref().map(|(_, version)| *version)).unwrap();
        let value = install(held.as_ref().map(|(value, _)| value), &snapshot.node);
        assert_eq!(value, tree.get("/sessions").unwrap().to_value());
        held = Some((value, snapshot.at));
    }
}
