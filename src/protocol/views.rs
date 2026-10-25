//! The views of a session (docs/protocol-r4-design.md §4.5): what a device follows instead of the
//! whole history. They are read-only and derived from the tree a commit makes, and exist only
//! while something follows or reads them.
//!
//! ```text
//! /views/<id>/output/latest                              the output of the current turn (moves)
//! /views/<id>/history/<instance>/<historyRev>/output/<start>    the output of the turn at <start>
//! /views/<id>/history/<instance>/<historyRev>/turn/<start>      that turn's entries
//! ```
#![cfg_attr(not(test), allow(dead_code))]

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

fn head(node: &Node) -> Value {
    node.as_value().map(|(value, _)| value.clone()).unwrap_or(Value::Null)
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
    let head = head(turn);
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
    let (text, other) = entry
        .and_then(|index| tree.get(&format!("/sessions/{}/entries/{index}", escape(session))))
        .and_then(Node::as_value)
        .map_or((String::new(), 0), |(entry, _)| output_text(entry));
    output_spec(json!({ "turn": summary, "entry": entry, "other": other }), text)
}

/// An output view: the fixed members and `text`, a text position.
fn output_spec(mut members: Value, text: String) -> Spec {
    members["text"] = Value::String(text);
    Spec::text_value(Arc::new(members), TextRule::OutputView)
}

fn turn_view(tree: &Tree, session: &str, turn: &Node) -> Spec {
    let head = head(turn);
    let (start, length) = (head["start"].as_u64().unwrap_or(0), head["length"].as_u64().unwrap_or(0));
    let entries = (start..start + length)
        .filter_map(|index| tree.get(&format!("/sessions/{}/entries/{index}", escape(session)))?.value_arc())
        .map(|entry| Spec::text_value(entry, TextRule::Entry))
        .collect();
    let Value::Object(fields) = head else { return Spec::Map(BTreeMap::new()) };
    Spec::object(fields, [("entries", Spec::List(entries))])
}

/// The spec of `view` in `tree`; none while it does not exist (over, or the history unknown).
fn materialize(tree: &Tree, view: &View) -> Option<Spec> {
    if gone(tree, view).is_some() {
        return None;
    }
    let session = view.session();
    let heads = turns(tree, session)?;
    match view {
        View::Latest { .. } => Some(output_view(tree, session, heads.last())),
        View::Output { start, .. } | View::Turn { start, .. } => {
            let turn = heads.iter().find(|turn| {
                turn.as_value().is_some_and(|(head, _)| head["start"].as_u64() == Some(*start as u64))
            })?;
            Some(match view {
                View::Turn { .. } => turn_view(tree, session, turn),
                _ => output_view(tree, session, Some(turn)),
            })
        }
    }
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
pub(crate) fn derive(tree: &Tree, wanted: &BTreeSet<View>) -> Spec {
    let mut root = BTreeMap::new();
    for view in wanted {
        let Some(spec) = materialize(tree, view) else { continue };
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
