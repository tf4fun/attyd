//! Reading one segment of a text position (design §7.1): a device pages through a long text
//! with memory for one segment, chaining the states and checking them against its window's
//! `before` or the text's hash.
use serde::Serialize;

use super::super::hash::{self, TextRule, ZERO};
use super::super::tree::Tree;
use super::super::window::state_wire;
use super::windowed_ops::{locate, pointer};

/// `{ before: c_{i-1}, segment: s_i }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Segment {
    /// The chain state before the segment: 32 bytes, unpadded base64url; all zeros for the first.
    pub before: String,
    pub segment: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SegmentRead {
    Segment(Segment),
    /// The path is not a text position, or the text has no such segment.
    NotFound,
    /// `at` is a version the tree has not reached.
    Future,
    /// The segment changed after `at`.
    Stale,
}

/// Segment `index` of the text position at `path`, as it was at version `at` (a version the
/// client holds the text at).
///
/// Earlier segments never change while a text only grows; a segment changes after `at` if the
/// text was replaced or edited after it, or if the segment was not complete at `at`.
pub(crate) fn read_segment(tree: &Tree, path: &str, index: usize, at: u64) -> SegmentRead {
    if at > tree.rev() {
        return SegmentRead::Future;
    }
    let (node, rest) = locate(tree, path);
    let Some((value, rule)) = node.as_value() else { return SegmentRead::NotFound };
    if rule == TextRule::None || rest.is_empty() {
        return SegmentRead::NotFound;
    }
    let position = pointer(&rest);
    if !hash::text_pointers(rule, value).contains(&position) {
        return SegmentRead::NotFound;
    }
    let text = value.pointer(&position).and_then(serde_json::Value::as_str).expect("a text position");
    let segments: Vec<&str> = hash::segments(text).collect();
    let Some(segment) = segments.get(index) else { return SegmentRead::NotFound };
    let history = node.text_history();
    // Only this position's own history counts: another position of the node, or a field beside
    // the text, changing leaves its segments as they were.
    let Some(held) = history.positions.get(&position) else { return SegmentRead::Stale };
    if held.replaced > at || held.segments[index] > at {
        return SegmentRead::Stale;
    }
    let before = segments[..index].iter().fold(ZERO, |state, segment| hash::chain_step(&state, segment));
    SegmentRead::Segment(Segment { before: state_wire(&before), segment: (*segment).to_string() })
}

#[cfg(test)]
mod tests;
