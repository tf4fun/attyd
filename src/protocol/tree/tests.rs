use std::collections::BTreeMap;

use serde_json::{Map, json};

use super::*;
use crate::protocol::hash;
use crate::protocol::{AppendTail, Op};

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
fn text_hashes_and_windows_stay_correct_across_append_batches_edits_and_retained_trees() {
    let mut tree = Tree::new("i");
    let mut text = format!("{}{}", "x".repeat(hash::L - 1), "日😀".repeat(2000));
    let commit = |tree: &mut Tree, text: &str| {
        tree.commit(BTreeMap::from([("output".into(), Spec::text_value(Arc::new(json!({ "text": text })), TextRule::OutputView))]));
    };
    commit(&mut tree, &text);
    let original = tree.clone();
    let original_hash = tree.get("/output").unwrap().hash();
    for suffix in ["one", "日", "😀", "two"] {
        text.push_str(suffix);
        commit(&mut tree, &text);
    }
    for replace in [false, true] {
        if replace {
            text.replace_range(0..1, "y");
            commit(&mut tree, &text);
            text.push_str(" after edit");
            commit(&mut tree, &text);
        }
        let node = tree.get("/output").unwrap();
        let value = json!({ "text": text });
        assert_eq!(node.hash(), hash::value_hash(TextRule::OutputView, &value));
        assert_eq!(node.to_value_windowed(hash::L), crate::protocol::window::window_value(TextRule::OutputView, &value, hash::L));
    }
    assert_eq!(original.get("/output").unwrap().hash(), original_hash);
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
        BTreeMap::from([("x".to_string(), Spec::List(vec![Spec::Value(value.clone(), TextRule::None)]))])
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

// --- hashes (design §5) --------------------------------------------------------------

#[test]
fn a_node_hashes_like_the_hash_functions_over_its_content() {
    let mut tree = Tree::new("i");
    let entry = json!({ "kind": "message", "role": "agent",
        "chunks": [{ "content": { "type": "text", "text": "hi" } }] });
    tree.commit(BTreeMap::from([(
        "s".to_string(),
        Spec::object(
            fields(json!({ "title": "t" })),
            [
                ("controls", Spec::value(json!({ "modes": null }))),
                ("entries", Spec::List(vec![Spec::text_value(Arc::new(entry.clone()), TextRule::Entry)])),
            ],
        ),
    )]));
    let entry_hash = hash::value_hash(TextRule::Entry, &entry);
    let controls = hash::value_hash(TextRule::None, &json!({ "modes": null }));
    let object = hash::object_hash(
        &fields(json!({ "title": "t" })),
        [("controls", controls), ("entries", hash::list_hash([&entry_hash]))],
    );
    assert_eq!(tree.get("/s/entries/0").unwrap().hash(), entry_hash);
    assert_eq!(tree.get("/s").unwrap().hash(), object);
    assert_eq!(tree.root().hash(), hash::map_hash([("s", object)]));
}

#[test]
fn a_hash_depends_on_content_and_not_on_revisions() {
    let (mut a, mut b) = (Tree::new("a"), Tree::new("b"));
    a.commit(root(&[("s", session("t", &["x"]))]));
    a.commit(root(&[("s", session("t", &["x", "y"]))]));
    b.commit(root(&[("s", session("t", &["x", "y"]))]));
    assert_ne!(a.rev(), b.rev());
    assert_eq!(a.root().hash(), b.root().hash());
    assert_eq!(a.get("/sessions/s").unwrap().hash(), b.get("/sessions/s").unwrap().hash());
}

#[test]
fn a_change_changes_the_hashes_of_its_path_and_no_other() {
    let mut tree = Tree::new("i");
    tree.commit(root(&[("s", session("t", &["a", "b"]))]));
    let before = |tree: &Tree, path: &str| tree.get(path).unwrap().hash();
    let (e0, e1, list, global) = (
        before(&tree, "/sessions/s/entries/0"),
        before(&tree, "/sessions/s/entries/1"),
        before(&tree, "/sessions/s/entries"),
        before(&tree, "/global"),
    );
    let root_before = tree.root().hash();
    tree.commit(root(&[("s", session("t", &["a", "c"]))]));
    assert_eq!(before(&tree, "/sessions/s/entries/0"), e0);
    assert_ne!(before(&tree, "/sessions/s/entries/1"), e1);
    assert_ne!(before(&tree, "/sessions/s/entries"), list);
    assert_eq!(before(&tree, "/global"), global);
    assert_ne!(tree.root().hash(), root_before);
}

#[test]
fn an_unchanged_value_keeps_the_hash_it_computed() {
    let mut tree = Tree::new("i");
    let value = Arc::new(json!({ "text": "a" }));
    let spec = |value: &Arc<Value>| BTreeMap::from([("x".to_string(), Spec::Value(value.clone(), TextRule::None))]);
    tree.commit(spec(&value));
    let first = tree.get("/x").unwrap();
    first.hash();
    assert!(first.hash.get().is_some());
    tree.commit(spec(&Arc::new(json!({ "text": "a" }))));
    tree.commit(BTreeMap::from([
        ("x".to_string(), Spec::Value(value.clone(), TextRule::None)),
        ("y".to_string(), Spec::value(json!(1))),
    ]));
    assert!(tree.get("/x").unwrap().hash.get().is_some(), "computed once for the content");
    assert!(tree.get("/y").unwrap().hash.get().is_none(), "computed when asked for");
}

// --- ops (design §6.2) ---------------------------------------------------------------

/// Commits `steps` and checks, after each, that its ops turn the value before into the value
/// after; returns the ops of each.
fn replay(steps: Vec<BTreeMap<String, Spec>>) -> Vec<Vec<Op>> {
    let mut tree = Tree::new("i");
    let mut all = Vec::new();
    for (index, step) in steps.into_iter().enumerate() {
        let mut document = tree.root().to_value();
        let Some(committed) = tree.commit_with_ops(step) else {
            all.push(Vec::new());
            continue;
        };
        for op in &committed.ops {
            crate::protocol::diff::apply(&mut document, op)
                .unwrap_or_else(|error| panic!("step {index}: {op:?} does not apply: {error:?}"));
        }
        assert_eq!(document, tree.root().to_value(), "step {index}: {:?}", committed.ops);
        all.push(committed.ops);
    }
    all
}

#[test]
fn the_ops_of_a_commit_turn_the_tree_before_into_the_tree_after() {
    let terminal = |output: &str| {
        Spec::map([("t", Spec::text_value(Arc::new(json!({ "terminalId": "t", "output": output })), TextRule::Terminal))])
    };
    let with = |sessions: &[(&str, Spec)], terminals: Spec| {
        let mut tree = root(sessions);
        tree.insert("terminals".into(), terminals);
        tree
    };
    replay(vec![
        root(&[]),
        root(&[("s", session("t", &["a"]))]),
        root(&[("s", session("t", &["a", "b", "c"]))]),
        root(&[("s", session("t", &["a", "bb", "c"]))]),
        root(&[("s", session("u", &["a", "bb", "c"]))]),
        root(&[("s", session("u", &["a"]))]),
        root(&[("s", session("u", &["z"])), ("n", session("n", &[]))]),
        root(&[("n", session("n", &[]))]),
        root(&[]),
        with(&[], terminal("abc")),
        with(&[], terminal("abcdef")),
        with(&[], terminal("def")),
        with(&[], Spec::Map(BTreeMap::new())),
    ]);
}

#[test]
fn a_node_that_changes_kind_is_set_whole() {
    let ops = replay(vec![
        BTreeMap::from([("x".to_string(), Spec::value(json!([1, 2])))]),
        BTreeMap::from([("x".to_string(), Spec::List(vec![Spec::value(json!(1))]))]),
    ]);
    assert_eq!(ops[1], vec![Op::Set { path: "/x".into(), value: json!([1]) }]);
}

#[test]
fn text_growing_in_a_value_is_an_append_and_a_terminal_tail_trims() {
    let value = |text: &str| BTreeMap::from([("x".to_string(), Spec::value(json!({ "text": text })))]);
    let ops = replay(vec![value("ab"), value("abcd")]);
    assert_eq!(
        ops[1],
        vec![Op::Append { path: "/x/text".into(), tail: AppendTail::Text { text: "cd".into() } }]
    );
}

#[test]
fn a_list_that_grows_appends_and_one_that_shrinks_is_set_whole() {
    let ops = replay(vec![
        root(&[("s", session("t", &["a"]))]),
        root(&[("s", session("t", &["a", "b"]))]),
        root(&[("s", session("t", &[]))]),
    ]);
    assert_eq!(
        ops[1],
        vec![Op::Append {
            path: "/sessions/s/entries".into(),
            tail: AppendTail::Values { values: vec![json!({ "text": "b" })] },
        }]
    );
    assert_eq!(
        ops[2],
        vec![Op::Set { path: "/sessions/s/entries".into(), value: json!([]) }]
    );
}

#[test]
fn an_added_or_removed_key_is_a_set_or_a_remove_and_paths_are_escaped() {
    let ops = replay(vec![root(&[]), root(&[("a/b", session("t", &[]))]), root(&[])]);
    assert_eq!(ops[1][0], Op::Set { path: "/sessions/a~1b".into(), value: json!({ "title": "t", "entries": [] }) });
    assert_eq!(ops[2], vec![Op::Remove { path: "/sessions/a~1b".into() }]);
}

#[test]
fn a_commit_without_a_change_has_no_ops() {
    let mut tree = Tree::new("i");
    tree.commit_with_ops(root(&[("s", session("t", &["a"]))])).unwrap();
    assert!(tree.commit_with_ops(root(&[("s", session("t", &["a"]))])).is_none());
}

// --- a commit of one subtree ---------------------------------------------------------

fn failures(entries: &[(&str, &str)]) -> Spec {
    Spec::map(entries.iter().map(|(id, message)| (*id, Spec::value(json!({ "message": message })))))
}

fn with_failures(entries: &[(&str, &str)]) -> BTreeMap<String, Spec> {
    BTreeMap::from([(
        "global".to_string(),
        Spec::object(fields(json!({ "agent": "a" })), [("loadFailures", failures(entries))]),
    )])
}

#[test]
fn a_subtree_is_committed_alone_with_its_ops_and_its_ancestors_revised() {
    let mut tree = Tree::new("i");
    tree.commit(root(&[("s", session("t", &["a"]))]));
    tree.commit(with_failures(&[]));
    let before = tree.root().to_value();
    let hash = tree.root().hash();
    let committed = tree
        .commit_at("/global/loadFailures", failures(&[("s", "boom")]))
        .expect("a change");
    assert_eq!(committed.rev, tree.rev());
    assert_eq!(
        committed.ops,
        vec![Op::Set { path: "/global/loadFailures/s".into(), value: json!({ "message": "boom" }) }]
    );
    let mut document = before;
    for op in &committed.ops {
        crate::protocol::diff::apply(&mut document, op).unwrap();
    }
    assert_eq!(document, tree.root().to_value());
    for path in ["/global/loadFailures/s", "/global/loadFailures", "/global", ""] {
        assert_eq!(tree.get(path).unwrap().srev(), committed.rev, "{path}");
    }
    assert_ne!(tree.root().hash(), hash, "the ancestors' hashes follow");
    assert_eq!(tree.get("/global").unwrap().rev(), 2, "the object's own fields did not change");
}

#[test]
fn a_subtree_that_does_not_change_or_has_no_parent_is_not_committed() {
    let mut tree = Tree::new("i");
    tree.commit(with_failures(&[("s", "boom")]));
    assert!(tree.commit_at("/global/loadFailures", failures(&[("s", "boom")])).is_none());
    assert_eq!(tree.rev(), 1);
    assert!(tree.commit_at("/nowhere/loadFailures", failures(&[])).is_none());
    assert!(tree.commit_at("/global/agent/x", failures(&[])).is_none());
    assert_eq!(tree.rev(), 1);
}

#[test]
fn a_subtree_removing_a_key_removes_it() {
    let mut tree = Tree::new("i");
    tree.commit(with_failures(&[("s", "boom"), ("u", "bang")]));
    let committed = tree.commit_at("/global/loadFailures", failures(&[("u", "bang")])).unwrap();
    assert_eq!(committed.ops, vec![Op::Remove { path: "/global/loadFailures/s".into() }]);
    assert!(tree.get("/global/loadFailures/s").is_none());
}

// --- the text history of a value node (design §7.1) ----------------------------------

fn text_entry(text: &str) -> BTreeMap<String, Spec> {
    BTreeMap::from([(
        "e".to_string(),
        Spec::text_value(
            Arc::new(json!({ "kind": "message", "role": "agent", "chunks": [
                { "content": { "type": "text", "text": text } }] })),
            TextRule::Entry,
        ),
    )])
}

const TEXT: &str = "/chunks/0/content/text";

fn position(tree: &Tree) -> PositionHistory {
    tree.get("/e").unwrap().text_history().positions[TEXT].clone()
}

#[test]
fn appends_to_a_text_do_not_replace_it_and_anything_else_does() {
    let mut tree = Tree::new("i");
    tree.commit(text_entry("ab"));
    assert_eq!(position(&tree).replaced, 1, "a new value is a new text");
    tree.commit(text_entry("abc"));
    tree.commit(text_entry("abcdef"));
    assert_eq!(position(&tree).replaced, 1);
    assert_eq!(*position(&tree).segments, [3]);
    tree.commit(text_entry("xbcdefg"));
    assert_eq!(position(&tree).replaced, 4, "an edit is a replacement");
    tree.commit(text_entry("xbcdef"));
    assert_eq!(position(&tree).replaced, 5, "so is a shortening");
}

#[test]
fn a_value_that_changes_besides_its_text_does_not_replace_the_text() {
    let mut tree = Tree::new("i");
    tree.commit(text_entry("ab"));
    let mut next = text_entry("abc");
    if let Some(Spec::Value(value, _)) = next.get_mut("e") {
        *value = Arc::new(json!({ "kind": "message", "role": "user", "chunks": [
            { "content": { "type": "text", "text": "abc" } }] }));
    }
    tree.commit(next);
    assert_eq!(position(&tree).replaced, 1, "the text only grew");
}

#[test]
fn text_history_keeps_one_revision_per_segment_and_plain_values_keep_none() {
    let mut tree = Tree::new("i");
    let mut text = String::from("a");
    for _ in 0..20 {
        tree.commit(text_entry(&text));
        text.push('b');
    }
    assert_eq!(*position(&tree).segments, [20]);
    tree.commit(BTreeMap::from([("p".to_string(), Spec::value(json!({ "text": "x" })))]));
    assert!(tree.get("/p").unwrap().text_history().positions.is_empty());
}

// --- derived views in the same revision (design §4.5) ---------------------------------

/// The `views` a derivation of `wanted` entries yields: each is `{ n: <the global value> }`.
fn views_of<'a>(wanted: &'a [&'a str]) -> impl Fn(&Tree) -> Spec + 'a {
    move |tree| {
        let n = tree.get("/global").map(|node| node.to_value()["n"].clone()).unwrap_or(Value::Null);
        Spec::map(wanted.iter().map(|name| (*name, Spec::value(json!({ "name": name, "n": n })))))
    }
}

fn global(n: i64) -> BTreeMap<String, Spec> {
    BTreeMap::from([("global".to_string(), Spec::value(json!({ "n": n })))])
}

#[test]
fn views_are_derived_from_the_tree_a_commit_makes_and_committed_with_it() {
    let mut tree = Tree::new("i");
    let committed = tree.commit_with_views(global(1), views_of(&["a"])).unwrap();
    assert_eq!(tree.get("/views/a").unwrap().to_value(), json!({ "name": "a", "n": 1 }));
    assert_eq!(tree.get("/views/a").unwrap().rev(), committed.rev);
    assert_eq!(tree.get("/global").unwrap().rev(), committed.rev, "one revision for both");
    let committed = tree.commit_with_views(global(2), views_of(&["a"])).unwrap();
    assert_eq!(tree.get("/views/a").unwrap().to_value()["n"], 2);
    assert_eq!(tree.get("/views/a").unwrap().rev(), committed.rev);
    assert!(committed.ops.iter().any(|op| matches!(op, Op::Set { path, .. } | Op::Append { path, .. } if path.starts_with("/views/a"))), "{:?}", committed.ops);
}

#[test]
fn a_view_that_did_not_change_keeps_its_revision_and_a_commit_without_a_change_is_none() {
    let mut tree = Tree::new("i");
    tree.commit_with_views(global(1), views_of(&["a"])).unwrap();
    tree.commit_with_views(global(1), views_of(&["a", "b"])).unwrap();
    assert_eq!(tree.get("/views/a").unwrap().rev(), 1);
    assert_eq!(tree.get("/views/b").unwrap().rev(), 2);
    assert!(tree.commit_with_views(global(1), views_of(&["a", "b"])).is_none());
    assert_eq!(tree.rev(), 2);
}

#[test]
fn a_view_no_longer_wanted_is_removed_and_one_wanted_again_takes_the_later_revision() {
    let mut tree = Tree::new("i");
    tree.commit_with_views(global(1), views_of(&["a"])).unwrap();
    let removed = tree.commit_with_views(global(1), views_of(&[])).unwrap();
    assert!(tree.get("/views/a").is_none());
    assert_eq!(removed.ops, vec![Op::Remove { path: "/views/a".into() }]);
    let again = tree.commit_with_views(global(1), views_of(&["a"])).unwrap();
    assert_eq!(tree.get("/views/a").unwrap().rev(), again.rev, "materialized later than its last change");
}

#[test]
fn only_the_views_can_be_committed_and_the_rest_stays() {
    let mut tree = Tree::new("i");
    tree.commit_with_views(global(1), views_of(&[])).unwrap();
    let before = tree.get("/global").unwrap().rev();
    let committed = tree.commit_views(views_of(&["a"])).unwrap();
    assert_eq!(tree.get("/global").unwrap().rev(), before);
    assert_eq!(tree.get("/views/a").unwrap().rev(), committed.rev);
    assert_eq!(committed.ops, vec![Op::Set { path: "/views/a".into(), value: json!({ "name": "a", "n": 1 }) }]);
    assert!(tree.commit_views(views_of(&["a"])).is_none());
}

#[test]
fn the_ops_of_a_commit_with_views_turn_the_tree_before_into_the_tree_after() {
    let mut tree = Tree::new("i");
    let mut document = tree.root().to_value();
    for (n, wanted) in [(1, vec!["a"]), (2, vec!["a", "b"]), (3, vec!["b"]), (3, vec![]), (4, vec!["a"])] {
        let committed = tree.commit_with_views(global(n), views_of(&wanted)).unwrap();
        for op in &committed.ops {
            crate::protocol::diff::apply(&mut document, op).unwrap_or_else(|e| panic!("{op:?}: {e:?}"));
        }
        assert_eq!(document, tree.root().to_value(), "n={n} {wanted:?}");
    }
}
