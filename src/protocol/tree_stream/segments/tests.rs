//! Segment reads (design §7.1): a device pages through a text at the version it holds.
use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::json;

use super::*;
use crate::protocol::hash::{Hash, L};
use crate::protocol::tree::Spec;

const PATH: &str = "/s/entries/0/chunks/0/content/text";

fn root(text: &str) -> BTreeMap<String, Spec> {
    BTreeMap::from([(
        "s".to_string(),
        Spec::object(Default::default(), [(
            "entries",
            Spec::List(vec![Spec::text_value(
                Arc::new(json!({ "kind": "message", "role": "agent", "chunks": [
                    { "content": { "type": "text", "text": text } }] })),
                TextRule::Entry,
            )]),
        )]),
    )])
}

fn read(tree: &Tree, index: usize, at: u64) -> SegmentRead {
    read_segment(tree, PATH, index, at)
}

fn segment(read: SegmentRead) -> Segment {
    match read {
        SegmentRead::Segment(segment) => segment,
        other => panic!("{other:?}"),
    }
}

fn state(text: &str) -> String {
    let before: Hash = hash::segments(text).fold(ZERO, |state, segment| hash::chain_step(&state, segment));
    state_wire(&before)
}

#[test]
fn a_device_pages_through_a_text_and_the_chain_reproduces_its_hash() {
    let text = format!("{}{}{}", "a".repeat(5000), "日".repeat(2000), "z".repeat(100));
    let mut tree = Tree::new("i");
    tree.commit(root(&text));
    let at = tree.rev();
    let mut paged = String::new();
    let mut chain = ZERO;
    for index in 0.. {
        let SegmentRead::Segment(read) = read(&tree, index, at) else { break };
        assert_eq!(read.before, state_wire(&chain), "segment {index} starts from the chain so far");
        chain = hash::chain_step(&chain, &read.segment);
        paged.push_str(&read.segment);
    }
    assert_eq!(paged, text);
    let mut expected = tree.get("/s/entries/0").unwrap().to_value();
    assert_eq!(expected["chunks"][0]["content"]["text"], text.as_str());
    let _ = &mut expected;
    // The chain at the end is the text's, so its hash is the one the tree states.
    let hash = hash::finish_for_tests(text.len() as u64, &chain);
    assert_eq!(hash, hash::text_hash(&text));
}

#[test]
fn the_first_segment_starts_from_zero_and_a_missing_one_is_not_found() {
    let mut tree = Tree::new("i");
    tree.commit(root("hello"));
    assert_eq!(segment(read(&tree, 0, 1)).before, state_wire(&ZERO));
    assert_eq!(read(&tree, 1, 1), SegmentRead::NotFound);
    assert_eq!(read_segment(&tree, "/s/entries/0/role", 0, 1), SegmentRead::NotFound);
    assert_eq!(read_segment(&tree, "/s/entries/0", 0, 1), SegmentRead::NotFound);
    assert_eq!(read_segment(&tree, "/nowhere", 0, 1), SegmentRead::NotFound);
    assert_eq!(read(&tree, 0, 99), SegmentRead::Future);
}

#[test]
fn earlier_segments_are_served_while_the_text_grows() {
    let mut text = "p".repeat(10_000);
    let mut tree = Tree::new("i");
    tree.commit(root(&text));
    let held = tree.rev();
    let first = segment(read(&tree, 0, held));
    for _ in 0..3 {
        text.push_str(&"q".repeat(3000));
        tree.commit(root(&text));
        // Segments 0 and 1 were complete at `held` (10,000 bytes: 4096 + 4096 + 1808).
        assert_eq!(segment(read(&tree, 0, held)), first);
        assert_eq!(segment(read(&tree, 1, held)).segment.len(), 4096);
    }
}

#[test]
fn a_segment_that_was_not_complete_at_the_version_held_is_stale_once_the_text_grew() {
    let mut tree = Tree::new("i");
    tree.commit(root(&"p".repeat(10_000)));
    let held = tree.rev();
    assert_eq!(segment(read(&tree, 2, held)).segment.len(), 10_000 - 2 * L);
    tree.commit(root(&"p".repeat(10_001)));
    assert_eq!(read(&tree, 2, held), SegmentRead::Stale, "the last segment grew");
    assert!(matches!(read(&tree, 2, tree.rev()), SegmentRead::Segment(_)));
}

#[test]
fn a_text_replaced_after_the_version_held_makes_every_segment_stale() {
    let mut tree = Tree::new("i");
    tree.commit(root(&"a".repeat(10_000)));
    let held = tree.rev();
    tree.commit(root(&"b".repeat(10_000)));
    for index in 0..3 {
        assert_eq!(read(&tree, index, held), SegmentRead::Stale, "{index}");
        assert!(matches!(read(&tree, index, tree.rev()), SegmentRead::Segment(_)));
    }
}

#[test]
fn a_value_not_changed_since_serves_every_segment() {
    let mut tree = Tree::new("i");
    tree.commit(root(&"a".repeat(10_000)));
    let held = tree.rev();
    let mut other = root(&"a".repeat(10_000));
    other.insert("t".into(), Spec::value(json!(1)));
    tree.commit(other);
    for index in 0..3 {
        assert!(matches!(read(&tree, index, held), SegmentRead::Segment(_)));
    }
}

#[test]
fn a_version_older_than_the_lengths_remembered_is_stale() {
    let mut text = "a".repeat(5000);
    let mut tree = Tree::new("i");
    tree.commit(root(&text));
    let held = tree.rev();
    for _ in 0..12 {
        text.push('b');
        tree.commit(root(&text));
    }
    assert_eq!(read(&tree, 0, held), SegmentRead::Stale, "it cannot be told whether segment 0 was complete");
    let _ = URL_SAFE_NO_PAD.encode([0u8]);
}
