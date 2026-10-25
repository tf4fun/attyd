//! The views of a session (docs/protocol-r4-design.md §4.5): what a device follows instead of the
//! whole history. They are read-only and derived from the tree a commit makes, and exist only
//! while something follows or reads them.
//!
//! ```text
//! /views/<id>/output/latest                              the output of the current turn (moves)
//! /views/<id>/history/<instance>/<historyRev>/output/<start>    the output of the turn at <start>
//! /views/<id>/history/<instance>/<historyRev>/turn/<start>      that turn's entries
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::hash::TextRule;
use super::tree::{Node, Spec, Tree, escape, unescape};

/// A view, and the session it is of.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum View {
    /// `/views/<id>/output/latest`.
    Latest { session: String },
    /// `/views/<id>/history/<instance>/<historyRev>/output/<start>`.
    Output { session: String, instance: String, history_rev: u64, start: usize },
    /// `/views/<id>/history/<instance>/<historyRev>/turn/<start>`.
    Turn { session: String, instance: String, history_rev: u64, start: usize },
}

impl View {
    pub(crate) fn session(&self) -> &str {
        match self {
            Self::Latest { session } | Self::Output { session, .. } | Self::Turn { session, .. } => session,
        }
    }

    /// The view `path` is, or is inside (a followed `…/turn/<start>/entries` is in the turn view).
    pub(crate) fn of(path: &str) -> Option<Self> {
        let tokens: Vec<String> = path.strip_prefix("/views/")?.split('/').map(unescape).collect();
        let [session, rest @ ..] = tokens.as_slice() else { return None };
        let session = session.clone();
        match rest {
            [output, latest, ..] if output.as_str() == "output" && latest.as_str() == "latest" => Some(Self::Latest { session }),
            [history, instance, revision, kind, start, ..] if history.as_str() == "history" => {
                let (history_rev, start) = (revision.parse().ok()?, start.parse().ok()?);
                let instance = instance.clone();
                match kind.as_str() {
                    "output" => Some(Self::Output { session, instance, history_rev, start }),
                    "turn" => Some(Self::Turn { session, instance, history_rev, start }),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// The tokens of the view's path below `/views`.
    fn tokens(&self) -> Vec<String> {
        match self {
            Self::Latest { session } => vec![session.clone(), "output".into(), "latest".into()],
            Self::Output { session, instance, history_rev, start } => vec![
                session.clone(), "history".into(), instance.clone(), history_rev.to_string(),
                "output".into(), start.to_string(),
            ],
            Self::Turn { session, instance, history_rev, start } => vec![
                session.clone(), "history".into(), instance.clone(), history_rev.to_string(),
                "turn".into(), start.to_string(),
            ],
        }
    }

    /// The path of the view.
    #[cfg(test)]
    pub(crate) fn path(&self) -> String {
        self.tokens().iter().fold("/views".to_string(), |path, token| format!("{path}/{}", escape(token)))
    }
}

/// Why a fixed view is over, if it is (design §4.5): no turn starts at its `<start>` (`Absent`),
/// or the instance or the session's history is not the one it names (`Replaced`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gone {
    Absent,
    Replaced,
}

fn session_node<'a>(tree: &'a Tree, session: &str) -> Option<&'a Node> {
    tree.get(&format!("/sessions/{}", escape(session)))
}

/// The turn heads of a session; none while its history is unknown.
fn turns<'a>(tree: &'a Tree, session: &str) -> Option<&'a [Node]> {
    let node = tree.get(&format!("/sessions/{}/turns", escape(session)))?;
    Some(node.items())
}

static NULL: Value = Value::Null;

/// The value of a turn head, borrowed: it is read for every turn, on every commit.
fn head(node: &Node) -> &Value {
    node.as_value().map_or(&NULL, |(value, _)| value)
}

/// Whether a fixed `view` is over now. Following or reading it is over for good: with `have` or
/// without, the path itself names the selection.
pub(crate) fn gone(tree: &Tree, view: &View) -> Option<Gone> {
    let (session, instance, history_rev, start) = match view {
        View::Latest { .. } => return None,
        View::Output { session, instance, history_rev, start }
        | View::Turn { session, instance, history_rev, start } => (session, instance, *history_rev, *start),
    };
    let node = session_node(tree, session)?;
    if instance != tree.instance() || node.field("historyRev").and_then(Value::as_u64) != Some(history_rev) {
        return Some(Gone::Replaced);
    }
    let found = turns(tree, session)?
        .iter()
        .any(|turn| turn.as_value().is_some_and(|(head, _)| head["start"].as_u64() == Some(start as u64)));
    (!found).then_some(Gone::Absent)
}

/// The text blocks of an entry concatenated, nothing inserted, and how many blocks are not text.
fn output_text(entry: &Value) -> (String, u64) {
    let (mut text, mut other) = (String::new(), 0);
    for chunk in entry["chunks"].as_array().into_iter().flatten() {
        let content = &chunk["content"];
        match content["text"].as_str() {
            Some(held) if content["type"] == "text" => text.push_str(held),
            _ => other += 1,
        }
    }
    (text, other)
}

fn output_view(tree: &Tree, session: &str, turn: Option<&Node>) -> Spec {
    let Some(turn) = turn else {
        return output_spec(json!({ "turn": null, "entry": null, "other": 0 }), String::new());
    };
    let (text, other) = output_entry(tree, session, turn)
        .and_then(Node::as_value)
        .map_or((String::new(), 0), |(entry, _)| output_text(entry));
    output_from_head(head(turn), text, other)
}

fn output_from_head(head: &Value, text: String, other: u64) -> Spec {
    let summary = {
        let mut summary = Map::new();
        for key in ["historyRev", "start", "messageId", "state"] {
            if let Some(held) = head.get(key) {
                summary.insert(key.to_string(), held.clone());
            }
        }
        Value::Object(summary)
    };
    let entry = head["output"].as_u64();
    output_spec(json!({ "turn": summary, "entry": entry, "other": other }), text)
}

/// An output view: the fixed members and `text`, a text position.
fn output_spec(mut members: Value, text: String) -> Spec {
    members["text"] = Value::String(text);
    Spec::text_value(Arc::new(members), TextRule::OutputView)
}

fn turn_view(tree: &Tree, session: &str, turn: &Node) -> Spec {
    let head = head(turn).clone();
    let (start, length) = (head["start"].as_u64().unwrap_or(0), head["length"].as_u64().unwrap_or(0));
    let entries = (start..start + length)
        .filter_map(|index| tree.get(&format!("/sessions/{}/entries/{index}", escape(session)))?.value_arc())
        .map(|entry| Spec::text_value(entry, TextRule::Entry))
        .collect();
    let Value::Object(fields) = head else { return Spec::Map(BTreeMap::new()) };
    Spec::object(fields, [("entries", Spec::List(entries))])
}

/// What an output view was built from, and what it was built as: while the turn head and the
/// output entry are the very values they were, the view is the same and is not built again.
struct Built {
    head: Arc<Value>,
    entry: Option<Arc<Value>>,
    entry_rev: u64,
    spec: Spec,
}

/// The output views derived so far, by view.
#[derive(Default)]
pub(crate) struct ViewCache(BTreeMap<View, Built>);

/// The spec of `view` in `tree`; none while it does not exist (over, or the history unknown).
fn materialize(tree: &Tree, view: &View, cache: &mut ViewCache) -> Option<Spec> {
    if gone(tree, view).is_some() {
        return None;
    }
    let session = view.session();
    let heads = turns(tree, session)?;
    match view {
        View::Latest { .. } => Some(cached_output(tree, session, view, heads.last(), cache)),
        View::Output { start, .. } | View::Turn { start, .. } => {
            let turn = heads.iter().find(|turn| {
                turn.as_value().is_some_and(|(head, _)| head["start"].as_u64() == Some(*start as u64))
            })?;
            Some(match view {
                View::Turn { .. } => turn_view(tree, session, turn),
                _ => cached_output(tree, session, view, Some(turn), cache),
            })
        }
    }
}

/// `output_view`, kept while its turn head and output entry are the values it was built from.
fn cached_output(tree: &Tree, session: &str, view: &View, turn: Option<&Node>, cache: &mut ViewCache) -> Spec {
    let Some(turn) = turn else {
        cache.0.remove(view);
        return output_view(tree, session, None);
    };
    let Some(head_arc) = turn.value_arc() else {
        return output_view(tree, session, Some(turn));
    };
    let entry = output_entry(tree, session, turn);
    let entry_arc = entry.and_then(Node::value_arc);
    if let Some(built) = cache.0.get(view)
        && (Arc::ptr_eq(&built.head, &head_arc) || same_output_head(&built.head, &head_arc))
        && match (&built.entry, &entry_arc) {
            (Some(held), Some(now)) => Arc::ptr_eq(held, now),
            (None, None) => true,
            _ => false,
        }
    {
        return built.spec.clone();
    }
    let spec = cache.0.get(view).and_then(|built| {
        // A new output or installed history starts afresh. For appends to the same entry,
        // the tree's text history proves which suffix grew; do not concatenate all blocks again.
        if built.head["historyRev"] != head_arc["historyRev"] || built.head["output"] != head_arc["output"] {
            return None;
        }
        let (text, other) = appended_output(built, entry?)?;
        Some(output_from_head(&head_arc, text, other))
    }).unwrap_or_else(|| output_view(tree, session, Some(turn)));
    cache.0.insert(view.clone(), Built { head: head_arc, entry: entry_arc, entry_rev: entry.map_or(0, Node::rev), spec: spec.clone() });
    spec
}

fn same_output_head(before: &Value, after: &Value) -> bool {
    ["historyRev", "start", "messageId", "state", "output"].iter().all(|key| before[*key] == after[*key])
}

/// An unchanged prefix plus the appended suffix of the last text block. An append to an
/// earlier block, a new block, an edit or a replacement uses the ordinary full projection.
fn appended_output(built: &Built, entry: &Node) -> Option<(String, u64)> {
    let history = entry.text_history();
    let old = built.entry.as_ref()?;
    let (current, rule) = entry.as_value()?;
    let pointers = super::hash::text_pointers(rule, current);
    // Per-position append history alone says nothing about added non-text blocks. Keep both
    // the block count and text positions fixed before retaining `other` or concatenated text.
    if old["chunks"].as_array()?.len() != current["chunks"].as_array()?.len()
        || pointers != super::hash::text_pointers(rule, old)
        || pointers.iter().any(|pointer| history.positions.get(pointer).is_none_or(|position| position.replaced > built.entry_rev))
    {
        return None;
    }
    let (last, prefix) = pointers.split_last()?;
    if prefix.iter().any(|pointer| old.pointer(pointer).and_then(Value::as_str).map(str::len) != current.pointer(pointer).and_then(Value::as_str).map(str::len)) {
        return None;
    }
    let previous = old.pointer(last)?.as_str()?;
    let suffix = current.pointer(last)?.as_str()?.get(previous.len()..)?;
    let Spec::Value(value, _) = &built.spec else { return None };
    let held = value["text"].as_str()?;
    let mut text = String::with_capacity(held.len() + suffix.len());
    text.push_str(held);
    text.push_str(suffix);
    Some((text, value["other"].as_u64()?))
}

/// The node of the entry that holds the output of the turn with head `turn`, if it has one.
fn output_entry<'a>(tree: &'a Tree, session: &str, turn: &Node) -> Option<&'a Node> {
    let index = head(turn)["output"].as_u64()?;
    tree.get(&format!("/sessions/{}/entries/{index}", escape(session)))
}

/// A trie of the leaves, then nested maps.
enum Trie {
    Leaf(Spec),
    Branch(BTreeMap<String, Trie>),
}

fn nested(trie: Trie) -> Spec {
    match trie {
        Trie::Leaf(spec) => spec,
        Trie::Branch(children) => Spec::Map(children.into_iter().map(|(key, trie)| (key, nested(trie))).collect()),
    }
}

/// The `views` subtree for the views wanted, read from `tree` as a commit makes it.
#[cfg(test)]
pub(crate) fn derive(tree: &Tree, wanted: &BTreeSet<View>) -> Spec {
    derive_cached(tree, wanted, &mut ViewCache::default())
}

/// `derive`, keeping the output views that did not change from one commit to the next.
pub(crate) fn derive_cached(tree: &Tree, wanted: &BTreeSet<View>, cache: &mut ViewCache) -> Spec {
    cache.0.retain(|view, _| wanted.contains(view));
    let mut root = BTreeMap::new();
    for view in wanted {
        let Some(spec) = materialize(tree, view, cache) else {
            cache.0.remove(view);
            continue;
        };
        let tokens = view.tokens();
        let (leaf, branches) = tokens.split_last().expect("a view has a path");
        let mut level = &mut root;
        for token in branches {
            level = match level.entry(token.clone()).or_insert_with(|| Trie::Branch(BTreeMap::new())) {
                Trie::Branch(children) => children,
                Trie::Leaf(_) => unreachable!("a view is not inside another view"),
            };
        }
        level.insert(leaf.clone(), Trie::Leaf(spec));
    }
    nested(Trie::Branch(root))
}

#[cfg(test)]
mod tests;
