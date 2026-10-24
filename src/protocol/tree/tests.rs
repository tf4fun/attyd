use std::collections::BTreeMap;

use serde_json::{Map, json};

use super::*;

fn fields(value: Value) -> Map<String, Value> {
    value.as_object().expect("an object").clone()
}

fn session(title: &str, entries: &[&str]) -> Spec {
    Spec::object(
        fields(json!({ "title": title })),
        [(
            "entries",
            Spec::List(entries.iter().map(|text| Spec::value(json!({ "text": text }))).collect()),
        )],
    )
}

fn root(sessions: &[(&str, Spec)]) -> BTreeMap<String, Spec> {
    BTreeMap::from([
        ("global".to_string(), Spec::value(json!({ "agent": "a" }))),
        (
            "sessions".to_string(),
            Spec::map(sessions.iter().map(|(id, spec)| (*id, spec.clone()))),
        ),
    ])
}

#[test]
fn the_first_commit_is_revision_one_and_an_unchanged_one_is_none() {
    let mut tree = Tree::new("i");
    assert_eq!((tree.instance(), tree.rev()), ("i", 0));
    let first = root(&[("s", session("t", &["a"]))]);
    assert_eq!(tree.commit(first.clone()), Some(1));
    assert_eq!(tree.commit(first), None);
    assert_eq!(tree.rev(), 1);
}

#[test]
fn one_commit_changing_several_paths_is_one_revision() {
    let mut tree = Tree::new("i");
    tree.commit(root(&[("s", session("t", &["a"]))]));
    let next = BTreeMap::from([
        ("global".to_string(), Spec::value(json!({ "agent": "b" }))),
        (
            "sessions".to_string(),
            Spec::map([("s", session("t", &["a", "b"])), ("u", session("u", &[]))]),
        ),
    ]);
    assert_eq!(tree.commit(next), Some(2));
    assert_eq!(tree.get("/global").unwrap().rev(), 2);
    assert_eq!(tree.get("/sessions/u").unwrap().rev(), 2);
    assert_eq!(tree.get("/sessions/s/entries/1").unwrap().rev(), 2);
}

#[test]
fn only_the_changed_path_takes_the_new_revision() {
    let mut tree = Tree::new("i");
    tree.commit(root(&[("s", session("t", &["a", "b"]))]));
    tree.commit(root(&[("s", session("t", &["a", "c"]))]));
    assert_eq!(tree.get("/sessions/s/entries/0").unwrap().rev(), 1);
    assert_eq!(tree.get("/sessions/s/entries/1").unwrap().rev(), 2);
    // The list itself did not change: its length and keys are the same.
    assert_eq!(tree.get("/sessions/s/entries").unwrap().rev(), 1);
    assert_eq!(tree.get("/sessions/s/entries").unwrap().srev(), 2);
    assert_eq!(tree.get("/sessions/s").unwrap().rev(), 1);
    assert_eq!(tree.get("/sessions/s").unwrap().srev(), 2);
    assert_eq!(tree.get("/global").unwrap().srev(), 1);
}

#[test]
fn an_append_changes_the_list_and_the_new_index() {
    let mut tree = Tree::new("i");
    tree.commit(root(&[("s", session("t", &["a"]))]));
    tree.commit(root(&[("s", session("t", &["a", "b"]))]));
    assert_eq!(tree.get("/sessions/s/entries").unwrap().rev(), 2);
    assert_eq!(tree.get("/sessions/s/entries/0").unwrap().rev(), 1);
    assert_eq!(tree.get("/sessions/s/entries/1").unwrap().rev(), 2);
}

#[test]
fn an_object_changes_with_its_fields_and_not_with_its_children() {
    let mut tree = Tree::new("i");
    tree.commit(root(&[("s", session("t", &["a"]))]));
    tree.commit(root(&[("s", session("t", &["b"]))]));
    assert_eq!(tree.get("/sessions/s").unwrap().rev(), 1);
    tree.commit(root(&[("s", session("u", &["b"]))]));
    assert_eq!(tree.get("/sessions/s").unwrap().rev(), 3);
    assert_eq!(tree.get("/sessions/s/entries/0").unwrap().rev(), 2);
}

#[test]
fn a_removed_key_changes_its_map_and_a_replaced_session_restarts_its_subtree() {
    let mut tree = Tree::new("i");
    tree.commit(root(&[("s", session("t", &["a"])), ("u", session("t", &["a"]))]));
    tree.commit(root(&[("u", session("t", &["a"]))]));
    assert!(tree.get("/sessions/s").is_none());
    assert_eq!(tree.get("/sessions").unwrap().rev(), 2);
    assert_eq!(tree.get("/sessions/u").unwrap().srev(), 1);
    // The session comes back: a new node, at the revision it came back.
    tree.commit(root(&[("s", session("t", &["a"])), ("u", session("t", &["a"]))]));
    assert_eq!(tree.get("/sessions/s").unwrap().srev(), 3);
}

#[test]
fn a_changed_kind_is_a_changed_node() {
    let mut tree = Tree::new("i");
    tree.commit(BTreeMap::from([("x".to_string(), Spec::value(json!([])))]));
    tree.commit(BTreeMap::from([("x".to_string(), Spec::List(Vec::new()))]));
    assert_eq!(tree.get("/x").unwrap().rev(), 2);
}

#[test]
fn paths_unescape_tokens_and_index_lists_canonically() {
    let mut tree = Tree::new("i");
    tree.commit(BTreeMap::from([(
        "sessions".to_string(),
        Spec::map([("a/b~c", Spec::List(vec![Spec::value(json!(1))]))]),
    )]));
    let id = escape("a/b~c");
    assert_eq!(id, "a~1b~0c");
    assert!(tree.get(&format!("/sessions/{id}/0")).is_some());
    assert!(tree.get(&format!("/sessions/{id}/00")).is_none());
    assert!(tree.get(&format!("/sessions/{id}/1")).is_none());
    assert!(tree.get("sessions").is_none());
    assert!(tree.get("").is_some());
}

#[test]
fn an_unchanged_value_keeps_its_identity() {
    let mut tree = Tree::new("i");
    let shared = Arc::new(json!({ "text": "a" }));
    let spec = |value: &Arc<Value>| {
        BTreeMap::from([("x".to_string(), Spec::List(vec![Spec::Value(value.clone())]))])
    };
    tree.commit(spec(&shared));
    tree.commit(spec(&Arc::new(json!({ "text": "a" }))));
    assert_eq!(tree.rev(), 1, "an equal value is not a change");
    tree.commit(spec(&Arc::new(json!({ "text": "b" }))));
    assert_eq!(tree.get("/x/0").unwrap().rev(), 2);
}

/// The path revision invariant (§4.7), checked over a sequence of commits: for every path
/// that exists at both `r` and now, `srev ≤ r` means its value did not change since `r`.
#[test]
fn the_path_revision_invariant_holds_over_a_sequence_of_commits() {
    let steps = [
        root(&[("s", session("t", &["a"]))]),
        root(&[("s", session("t", &["a", "b"]))]),
        root(&[("s", session("t", &["a", "b"])), ("u", session("t", &[]))]),
        root(&[("s", session("v", &["a", "c"])), ("u", session("t", &[]))]),
        root(&[("u", session("t", &[]))]),
        root(&[("u", session("t", &["z"]))]),
        root(&[("s", session("t", &["a"])), ("u", session("t", &["z"]))]),
    ];
    let mut tree = Tree::new("i");
    let mut history: Vec<(u64, Value)> = Vec::new();
    for step in steps {
        if let Some(rev) = tree.commit(step) {
            history.push((rev, tree.root().to_value()));
        }
        let now = tree.root().to_value();
        for (rev, then) in &history {
            check(&tree, "", then, &now, *rev);
        }
    }
    assert_eq!(tree.rev(), history.len() as u64);
}

fn check(tree: &Tree, path: &str, then: &Value, now: &Value, rev: u64) {
    if let Some(node) = tree.get(path)
        && node.srev() <= rev
    {
        assert_eq!(then, now, "{path} changed after {rev} but srev is {}", node.srev());
    }
    if let (Value::Object(then), Value::Object(now)) = (then, now) {
        for (key, then) in then {
            if let Some(now) = now.get(key) {
                check(tree, &format!("{path}/{}", escape(key)), then, now, rev);
            }
        }
    }
    if let (Value::Array(then), Value::Array(now)) = (then, now) {
        for (index, (then, now)) in then.iter().zip(now).enumerate() {
            check(tree, &format!("{path}/{index}"), then, now, rev);
        }
    }
}
