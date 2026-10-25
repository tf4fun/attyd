//! The ops of a commit as a client that holds text positions as windows needs them (design
//! §6.4): no event carries `N + L` bytes or more of one window's text.
use std::collections::BTreeSet;

use serde_json::Value;

use super::super::diff;
use super::super::hash::{self, TextChain};
use super::super::tree::{Node, Tree, escape, unescape};
use super::super::window::{self, appends, state_wire};
use super::super::{AppendTail, Op};

fn op_path(op: &Op) -> &str {
    match op {
        Op::Set { path, .. } | Op::Remove { path } | Op::Append { path, .. } | Op::Trim { path, .. } => path,
    }
}

/// The deepest node on `path` and the tokens below it, which address into a value node.
pub(super) fn locate<'a>(tree: &'a Tree, path: &str) -> (&'a Node, Vec<String>) {
    let tokens: Vec<String> = path.strip_prefix('/').map(|rest| rest.split('/').map(unescape).collect()).unwrap_or_default();
    let mut node = tree.root();
    for (index, token) in tokens.iter().enumerate() {
        match node.child(token) {
            Some(child) => node = child,
            None => return (node, tokens[index..].to_vec()),
        }
    }
    (node, Vec::new())
}

pub(super) fn pointer(tokens: &[String]) -> String {
    tokens.iter().map(|token| format!("/{}", escape(token))).collect()
}

/// `value` (the new content at pointer `at` of a value node) with the text positions under it
/// held as windows. `positions` are the text pointers of the whole value node.
fn window_inside(node: &Node, positions: &[String], at: &str, value: &Value, bytes: usize) -> Value {
    let mut out = value.clone();
    for position in positions {
        let Some(relative) = position.strip_prefix(at) else { continue };
        if !(relative.is_empty() || relative.starts_with('/')) {
            continue;
        }
        let Some(text) = out.pointer_mut(relative) else { continue };
        if let Some(string) = text.as_str() {
            *text = serde_json::to_value(window::window_cached(string, bytes, node.text_chain(position).expect("a text chain")))
                .expect("a window serializes");
        }
    }
    out
}

/// The ops a client holding windows of `bytes` applies for the committed `op`; `tree` is the tree
/// after the commit. `replaced` holds the text positions already sent whole in this commit.
pub(crate) fn windowed(tree: &Tree, op: &Op, bytes: usize, replaced: &mut BTreeSet<String>) -> Vec<Op> {
    let path = op_path(op);
    let (node, rest) = locate(tree, path);
    match op {
        Op::Remove { .. } => vec![op.clone()],
        Op::Set { path, value } => {
            if rest.is_empty() {
                return vec![Op::Set { path: path.clone(), value: node.to_value_windowed(bytes) }];
            }
            let Some((whole, rule)) = node.as_value() else { return vec![op.clone()] };
            let positions = hash::text_pointers(rule, whole);
            vec![Op::Set { path: path.clone(), value: window_inside(node, &positions, &pointer(&rest), value, bytes) }]
        }
        Op::Append { path, tail: AppendTail::Values { values } } => {
            if rest.is_empty() {
                // A list of nodes: the items appended are the last of the list.
                let appended = values.len();
                let items: Vec<Value> = (0..appended)
                    .filter_map(|n| {
                        let index = node.list_len()?.checked_sub(appended)? + n;
                        Some(node.child(&index.to_string())?.to_value_windowed(bytes))
                    })
                    .collect();
                if items.len() == appended {
                    return vec![Op::Append { path: path.clone(), tail: AppendTail::Values { values: items } }];
                }
                return vec![op.clone()];
            }
            let Some((whole, rule)) = node.as_value() else { return vec![op.clone()] };
            let positions = hash::text_pointers(rule, whole);
            let at = pointer(&rest);
            let length = whole.pointer(&at).and_then(Value::as_array).map_or(0, Vec::len);
            let first = length.saturating_sub(values.len());
            let values = values
                .iter()
                .enumerate()
                .map(|(n, value)| window_inside(node, &positions, &format!("{at}/{}", first + n), value, bytes))
                .collect::<Vec<_>>();
            vec![Op::Append { path: path.clone(), tail: AppendTail::Values { values } }]
        }
        Op::Append { path, tail: AppendTail::Text { text } } => {
            let Some((whole, rule)) = node.as_value() else { return vec![op.clone()] };
            let at = pointer(&rest);
            if !hash::text_pointers(rule, whole).contains(&at) {
                return vec![op.clone()];
            }
            let full = whole.pointer(&at).and_then(Value::as_str).expect("a text position");
            text_append(path, full, text.len(), bytes, node.text_chain(&at).expect("a text chain"), replaced)
        }
        Op::Trim { path, .. } => {
            let Some((whole, rule)) = node.as_value() else { return vec![op.clone()] };
            let at = pointer(&rest);
            if !hash::text_pointers(rule, whole).contains(&at) {
                return vec![op.clone()];
            }
            let full = whole.pointer(&at).and_then(Value::as_str).expect("a text position");
            whole_window(path, full, bytes, node.text_chain(&at).expect("a text chain"), replaced)
        }
    }
}

/// The text position at `path` sent whole, once per commit.
fn whole_window(path: &str, full: &str, bytes: usize, chain: &TextChain, replaced: &mut BTreeSet<String>) -> Vec<Op> {
    if !replaced.insert(path.to_string()) {
        return Vec::new();
    }
    vec![Op::Set {
        path: path.to_string(),
        value: serde_json::to_value(window::window_cached(full, bytes, chain)).expect("a window serializes"),
    }]
}

/// `appended` bytes were appended to the text position at `path`, now `full`. Below `N + L` bytes
/// it is an `append` and a new `length`, then, if the window grew past the rule, the trim of
/// whole leading segments with the new `offset` and `before`; otherwise the window is sent whole.
fn text_append(path: &str, full: &str, appended: usize, bytes: usize, chain: &TextChain, replaced: &mut BTreeSet<String>) -> Vec<Op> {
    if replaced.contains(path) {
        return Vec::new();
    }
    if !appends(appended, bytes) {
        return whole_window(path, full, bytes, chain, replaced);
    }
    let (old_offset, new_offset) = (chain.window_offset(full.len() - appended, bytes), chain.window_offset(full.len(), bytes));
    let held = |member: &str| diff::child(path, member);
    let mut ops = vec![
        Op::Append { path: held("text"), tail: AppendTail::Text { text: full[full.len() - appended..].to_string() } },
        Op::Set { path: held("length"), value: Value::from(full.len() as u64) },
    ];
    if new_offset > old_offset {
        let units: usize = full[old_offset..new_offset].chars().map(char::len_utf16).sum();
        ops.push(Op::Trim { path: held("text"), count: units });
        ops.push(Op::Set { path: held("offset"), value: Value::from(new_offset as u64) });
        ops.push(Op::Set {
            path: held("before"),
            value: Value::String(state_wire(&chain.before(new_offset))),
        });
    }
    ops
}

#[cfg(test)]
mod tests;
