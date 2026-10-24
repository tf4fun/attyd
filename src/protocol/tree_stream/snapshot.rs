//! The snapshot of a path, whole or from the version a client holds (design §6.3).
use std::collections::BTreeMap;

use super::super::digest;
use super::super::tree::{Node, Tree};
use super::wire::{SNode, Snapshot};

/// The snapshot of `path` for a client that holds version `have` of it, or holds nothing.
/// A node with `srev ≤ have` is omitted (the client keeps its own); a value node that changed
/// is sent whole; a container that changed is sent as its structure and the children that
/// changed. `None` when `path` is not in the tree.
pub(crate) fn snapshot(tree: &Tree, path: &str, have: Option<u64>) -> Option<Snapshot> {
    let node = tree.get(path)?;
    let at = node.srev();
    let node_of = match have {
        Some(have) if at <= have => SNode::Same { same: true },
        Some(have) => changed(node, have),
        None => SNode::Value { v: node.to_value() },
    };
    Some(Snapshot {
        base: have,
        at,
        hash: digest::wire(&node.hash()),
        node: node_of,
    })
}

fn changed(node: &Node, have: u64) -> SNode {
    let children = |all: Vec<(String, &Node)>| -> BTreeMap<String, SNode> {
        all.into_iter()
            .filter(|(_, child)| child.srev() > have)
            .map(|(key, child)| (key, changed(child, have)))
            .collect()
    };
    match node.shape() {
        Shape::Value(value) => SNode::Value { v: value },
        Shape::Object(fields, all) => SNode::Object {
            o: fields,
            k: all.iter().map(|(key, _)| key.clone()).collect(),
            c: children(all),
        },
        Shape::Map(keys, all) => SNode::Map { m: keys, c: children(all) },
        Shape::List(len, all) => SNode::List { l: len, c: children(all) },
    }
}

/// A node's own level and its children.
pub(crate) enum Shape<'a> {
    Value(serde_json::Value),
    Object(serde_json::Value, Vec<(String, &'a Node)>),
    Map(Vec<String>, Vec<(String, &'a Node)>),
    List(usize, Vec<(String, &'a Node)>),
}

#[cfg(test)]
mod tests;
