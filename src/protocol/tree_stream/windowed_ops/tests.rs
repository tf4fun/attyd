//! The ops of a commit for a client that holds windows (design §6.4). A client is simulated by
//! applying the translated ops to the windowed value it holds; it must then hold exactly the
//! windows of the new tree, and no event may carry `N + L` bytes of one window's text.
use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::protocol::hash::L;
use crate::protocol::tree::Spec;
use crate::protocol::window::windowed_value_hash;

const N: usize = 8192;

fn entry(text: &str) -> Spec {
    Spec::text_value(
        Arc::new(json!({ "kind": "message", "role": "agent", "chunks": [
            { "content": { "type": "text", "text": text } }] })),
        TextRule::Entry,
    )
}

fn terminal(output: &str) -> Spec {
    Spec::text_value(Arc::new(json!({ "terminalId": "t", "output": output })), TextRule::Terminal)
}

fn root(texts: &[String]) -> BTreeMap<String, Spec> {
    BTreeMap::from([(
        "s".to_string(),
        Spec::object(Default::default(), [
            ("entries", Spec::List(texts.iter().map(|text| entry(text)).collect())),
            ("terminals", Spec::map([("t", terminal(texts.first().map_or("", String::as_str)))])),
        ]),
    )])
}

/// The most bytes of one window's text an op carries.
fn carried(op: &Op) -> usize {
    fn windows(value: &Value) -> usize {
        match value {
            Value::Object(members) if members.contains_key("offset") && members.contains_key("before") => {
                members["text"].as_str().map_or(0, str::len)
            }
            Value::Object(members) => members.values().map(windows).max().unwrap_or(0),
            Value::Array(items) => items.iter().map(windows).max().unwrap_or(0),
            _ => 0,
        }
    }
    match op {
        Op::Set { value, .. } => windows(value),
        Op::Append { tail: AppendTail::Text { text }, .. } => text.len(),
        Op::Append { tail: AppendTail::Values { values }, .. } => values.iter().map(windows).max().unwrap_or(0),
        _ => 0,
    }
}

/// Commits each step, translating its ops for a client that holds windows of `N`; the client's
/// copy must equal the new tree's windows after each.
fn follow(steps: Vec<Vec<String>>) -> Vec<Vec<Op>> {
    let mut tree = Tree::new("i");
    let mut held = tree.root().to_value_windowed(N);
    let mut all = Vec::new();
    for (index, texts) in steps.into_iter().enumerate() {
        let Some(committed) = tree.commit_with_ops(root(&texts)) else {
            all.push(Vec::new());
            continue;
        };
        let mut replaced = BTreeSet::new();
        let ops: Vec<Op> = committed.ops.iter().flat_map(|op| windowed(&tree, op, N, &mut replaced)).collect();
        for op in &ops {
            diff::apply(&mut held, op).unwrap_or_else(|error| panic!("step {index}: {op:?}: {error:?}"));
            assert!(carried(op) < N + L, "step {index}: an op carries {} bytes of one window", carried(op));
        }
        assert_eq!(held, tree.root().to_value_windowed(N), "step {index}");
        all.push(ops);
    }
    all
}

fn grow(base: &str, by: &[usize]) -> Vec<Vec<String>> {
    let mut text = base.to_string();
    let mut steps = vec![vec![text.clone()]];
    for (n, size) in by.iter().enumerate() {
        text.push_str(&char::from(b'a' + (n % 26) as u8).to_string().repeat(*size));
        steps.push(vec![text.clone()]);
    }
    steps
}

#[test]
fn small_appends_are_appends_with_a_new_length_and_trim_when_the_window_passes_a_segment() {
    let ops = follow(grow("start ", &[1, 10, 100, 1000, 3000, 3000, 3000, 5000]));
    // The first small appends only append and set the length.
    let first = &ops[1];
    assert!(matches!(&first[0], Op::Append { path, tail: AppendTail::Text { .. } } if path.ends_with("/chunks/0/content/text/text")));
    assert!(matches!(&first[1], Op::Set { path, .. } if path.ends_with("/content/text/length")));
    // Somewhere the window passed a segment: trim, offset and before follow the append.
    let trimmed = ops.iter().flatten().filter(|op| matches!(op, Op::Trim { .. })).count();
    assert!(trimmed > 0);
    for step in &ops {
        for (n, op) in step.iter().enumerate() {
            if let Op::Trim { path, .. } = op {
                assert!(matches!(&step[n + 1], Op::Set { path, .. } if path.ends_with("/offset")));
                assert!(matches!(&step[n + 2], Op::Set { path, .. } if path.ends_with("/before")));
                assert!(path.ends_with("/text"));
            }
        }
    }
}

#[test]
fn a_megabyte_appended_at_once_is_one_window_under_n_plus_l_bytes() {
    let ops = follow(grow("start ", &[1 << 20]));
    let step = &ops[1];
    assert_eq!(step.len(), 2, "the entry's window and the terminal's, each set whole: {step:?}");
    for op in step {
        let Op::Set { value, .. } = op else { panic!("{op:?}") };
        assert_eq!(value["length"], (1 << 20) + 6);
        assert!(value["text"].as_str().unwrap().len() < N + L);
    }
}

#[test]
fn the_decision_depends_on_the_appended_bytes_alone() {
    // One byte then 12,000 bytes (at least N + L = 12,288 would be a set; just below is an append).
    let below = follow(grow("x", &[1, N + L - 1]));
    assert!(matches!(&below[2][0], Op::Append { .. }), "{:?}", below[2][0]);
    let at = follow(grow("x", &[1, N + L]));
    assert!(matches!(&at[2][0], Op::Set { .. }), "{:?}", at[2][0]);
}

#[test]
fn multibyte_text_trims_in_utf16_units_and_the_client_keeps_equal_windows() {
    let mut steps = vec![vec![String::new()]];
    let mut text = String::new();
    for n in 0..30 {
        text.push_str(&if n % 2 == 0 { "日本語".repeat(300) } else { "😀é".repeat(200) });
        steps.push(vec![text.clone()]);
    }
    follow(steps);
}

#[test]
fn a_window_a_client_holds_verifies_as_the_whole_text_after_every_commit() {
    let mut tree = Tree::new("i");
    let mut held = tree.root().to_value_windowed(N);
    let mut text = String::new();
    for size in [3, 5000, 7000, 100, 9000, 20_000, 1, 4096, 4097] {
        text.push_str(&"q".repeat(size));
        let committed = tree.commit_with_ops(root(&[text.clone()])).unwrap();
        let mut replaced = BTreeSet::new();
        for op in committed.ops.iter().flat_map(|op| windowed(&tree, op, N, &mut replaced)) {
            diff::apply(&mut held, &op).unwrap();
        }
        let entry = &held["s"]["entries"][0];
        assert_eq!(windowed_value_hash(TextRule::Entry, entry), Some(tree.get("/s/entries/0").unwrap().hash()));
    }
}

#[test]
fn a_new_entry_arrives_with_its_text_as_a_window() {
    let big = "z".repeat(40_000);
    let ops = follow(vec![vec!["a".into()], vec!["a".into(), big.clone()]]);
    let Op::Append { tail: AppendTail::Values { values }, .. } = &ops[1][0] else { panic!("{:?}", ops[1]) };
    assert_eq!(values[0]["chunks"][0]["content"]["text"]["length"], 40_000);
    assert!(values[0]["chunks"][0]["content"]["text"]["text"].as_str().unwrap().len() < N + L);
}

#[test]
fn replacing_a_text_sets_its_window_and_a_terminal_tail_is_set_whole_once() {
    let ops = follow(vec![
        vec!["a".repeat(20_000)],
        vec!["b".repeat(20_000)],
        vec!["c".repeat(30_000)],
    ]);
    assert!(ops[1].iter().all(|op| matches!(op, Op::Set { .. })), "{:?}", ops[1]);
}

#[test]
fn what_is_not_a_text_position_is_left_alone() {
    let mut tree = Tree::new("i");
    let step = |title: &str| {
        BTreeMap::from([(
            "s".to_string(),
            Spec::object(json!({ "title": title }).as_object().unwrap().clone(), Vec::<(&str, Spec)>::new()),
        )])
    };
    tree.commit(step("ab"));
    let committed = tree.commit_with_ops(step("abcd")).unwrap();
    let mut replaced = BTreeSet::new();
    let ops: Vec<Op> = committed.ops.iter().flat_map(|op| windowed(&tree, op, N, &mut replaced)).collect();
    assert_eq!(ops, committed.ops);
}
