//! The state tree of protocol revision 4 (docs/protocol-r4-design.md §4.1, §4.2, §4.7).
//!
//! The bridge's published state is one tree of nodes. A commit hands `Tree::commit` the
//! complete tree the bridge now publishes, as a `Spec`; the tree compares it with what it
//! holds, gives every node whose value changed the new revision and leaves the others
//! alone. Nothing changed: no new revision. So a node's `rev` is the revision at which the
//! value at its path last changed, and its `srev` is the largest `rev` in its subtree:
//! if `srev(N) ≤ r`, the value at `N`'s path is the same now as at `r` (§4.7).
//!
//! Values are compared by `Arc` first, so an entry the bridge did not touch costs a
//! pointer comparison, not a walk of its JSON.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use serde_json::{Map, Value};

use super::hash::{self, Hash, TextRule};

/// What a node holds (§4.2).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Spec {
    /// Any JSON, with the text positions it hashes through chains; it has no child nodes.
    Value(Arc<Value>, TextRule),
    /// Its own fields, and named child nodes.
    Object {
        fields: Map<String, Value>,
        children: BTreeMap<String, Spec>,
    },
    /// The set of keys, and one node per key.
    Map(BTreeMap<String, Spec>),
    /// The length, and one node per index.
    List(Vec<Spec>),
}

impl Spec {
    pub(crate) fn value(value: Value) -> Self {
        Self::Value(Arc::new(value), TextRule::None)
    }

    /// A value whose text positions `rule` names (design §4.6).
    pub(crate) fn text_value(value: Arc<Value>, rule: TextRule) -> Self {
        Self::Value(value, rule)
    }

    pub(crate) fn object(
        fields: Map<String, Value>,
        children: impl IntoIterator<Item = (impl Into<String>, Spec)>,
    ) -> Self {
        Self::Object {
            fields,
            children: children.into_iter().map(|(k, v)| (k.into(), v)).collect(),
        }
    }

    pub(crate) fn map(children: impl IntoIterator<Item = (impl Into<String>, Spec)>) -> Self {
        Self::Map(children.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }
}

#[derive(Debug, Clone)]
enum Body {
    Value(Arc<Value>, TextRule),
    Object {
        fields: Map<String, Value>,
        children: BTreeMap<String, Node>,
    },
    Map(BTreeMap<String, Node>),
    List(Vec<Node>),
}

/// A node with its revisions.
#[derive(Debug, Clone)]
pub(crate) struct Node {
    rev: u64,
    srev: u64,
    body: Body,
    /// Computed when first asked for and kept while the node is.
    hash: OnceLock<Hash>,
}

impl Node {
    /// The revision at which the value at this node's path last changed.
    pub(crate) fn rev(&self) -> u64 {
        self.rev
    }

    /// The largest `rev` in this node's subtree: the node's path version (§4.7).
    pub(crate) fn srev(&self) -> u64 {
        self.srev
    }

    /// The child `key` of an object or a map, or the index `key` of a list.
    pub(crate) fn child(&self, key: &str) -> Option<&Node> {
        match &self.body {
            Body::Value(..) => None,
            Body::Object { children, .. } | Body::Map(children) => children.get(key),
            Body::List(items) => items.get(key.parse::<usize>().ok().filter(|i| i.to_string() == key)?),
        }
    }

    /// The node's subtree as one JSON value: an object is its fields and children, a map its
    /// keys, a list its items.
    pub(crate) fn to_value(&self) -> Value {
        match &self.body {
            Body::Value(value, _) => (**value).clone(),
            Body::Object { fields, children } => {
                let mut object = fields.clone();
                for (key, child) in children {
                    object.insert(key.clone(), child.to_value());
                }
                Value::Object(object)
            }
            Body::Map(children) => Value::Object(
                children
                    .iter()
                    .map(|(key, child)| (key.clone(), child.to_value()))
                    .collect(),
            ),
            Body::List(items) => Value::Array(items.iter().map(Node::to_value).collect()),
        }
    }

    /// Whether the node holds exactly `spec` at its own level: the value, the fields, the keys,
    /// the length. Children are compared when they are reconciled.
    fn same_level(&self, spec: &Spec) -> bool {
        match (&self.body, spec) {
            (Body::Value(old, old_rule), Spec::Value(new, new_rule)) => {
                old_rule == new_rule && (Arc::ptr_eq(old, new) || old == new)
            }
            (
                Body::Object { fields: old, children: old_children },
                Spec::Object { fields: new, children: new_children },
            ) => old == new && old_children.keys().eq(new_children.keys()),
            (Body::Map(old), Spec::Map(new)) => old.keys().eq(new.keys()),
            (Body::List(old), Spec::List(new)) => old.len() == new.len(),
            _ => false,
        }
    }

    /// The node for `spec` at revision `rev`, keeping the revisions of `old` where nothing
    /// changed.
    fn reconcile(old: Option<&Node>, spec: Spec, rev: u64) -> Node {
        let same = old.is_some_and(|old| old.same_level(&spec));
        let own = match old {
            Some(old) if same => old.rev,
            _ => rev,
        };
        let old_body = old.map(|old| &old.body);
        let body = match spec {
            Spec::Value(value, rule) => match old_body {
                Some(Body::Value(old, _)) if same => Body::Value(old.clone(), rule),
                _ => Body::Value(value, rule),
            },
            Spec::Object { fields, children } => {
                let old_children = match old_body {
                    Some(Body::Object { children, .. }) => Some(children),
                    _ => None,
                };
                Body::Object {
                    fields,
                    children: Node::reconcile_children(old_children, children, rev),
                }
            }
            Spec::Map(children) => {
                let old_children = match old_body {
                    Some(Body::Map(children)) => Some(children),
                    _ => None,
                };
                Body::Map(Node::reconcile_children(old_children, children, rev))
            }
            Spec::List(items) => {
                let old_items = match old_body {
                    Some(Body::List(items)) => items.as_slice(),
                    _ => &[],
                };
                Body::List(
                    items
                        .into_iter()
                        .enumerate()
                        .map(|(index, spec)| Node::reconcile(old_items.get(index), spec, rev))
                        .collect(),
                )
            }
        };
        let srev = own.max(match &body {
            Body::Value(..) => 0,
            Body::Object { children, .. } | Body::Map(children) => {
                children.values().map(|node| node.srev).max().unwrap_or(0)
            }
            Body::List(items) => items.iter().map(|node| node.srev).max().unwrap_or(0),
        });
        // A value node that did not change keeps the hash it had.
        let hash = match (old, &body) {
            (Some(old), Body::Value(..)) if same => old.hash.clone(),
            _ => OnceLock::new(),
        };
        Node { rev: own, srev, body, hash }
    }

    /// The node's content hash (design §5): of the value and its text chains, or of the object,
    /// map or list over its children's hashes.
    pub(crate) fn hash(&self) -> Hash {
        *self.hash.get_or_init(|| match &self.body {
            Body::Value(value, rule) => hash::value_hash(*rule, value),
            Body::Object { fields, children } => hash::object_hash(
                fields,
                children.iter().map(|(name, node)| (name.as_str(), node.hash())),
            ),
            Body::Map(children) => {
                hash::map_hash(children.iter().map(|(key, node)| (key.as_str(), node.hash())))
            }
            Body::List(items) => {
                let hashes: Vec<Hash> = items.iter().map(Node::hash).collect();
                hash::list_hash(&hashes)
            }
        })
    }

    fn reconcile_children(
        old: Option<&BTreeMap<String, Node>>,
        children: BTreeMap<String, Spec>,
        rev: u64,
    ) -> BTreeMap<String, Node> {
        children
            .into_iter()
            .map(|(key, spec)| {
                let node = Node::reconcile(old.and_then(|old| old.get(&key)), spec, rev);
                (key, node)
            })
            .collect()
    }
}

/// The published tree of one bridge process: its `instance`, its current `rev` and the root.
#[derive(Debug, Clone)]
pub(crate) struct Tree {
    instance: String,
    rev: u64,
    root: Node,
}

impl Tree {
    /// An empty tree of `instance`, at revision 0: nothing is published yet, so the first
    /// commit that changes anything is revision 1 (§4.1).
    pub(crate) fn new(instance: impl Into<String>) -> Self {
        Self {
            instance: instance.into(),
            rev: 0,
            root: Node {
                rev: 0,
                srev: 0,
                body: Body::Map(BTreeMap::new()),
                hash: OnceLock::new(),
            },
        }
    }

    pub(crate) fn instance(&self) -> &str {
        &self.instance
    }

    pub(crate) fn rev(&self) -> u64 {
        self.rev
    }

    pub(crate) fn root(&self) -> &Node {
        &self.root
    }

    /// The node at `path`, a JSON Pointer (RFC 6901; `""` is the root).
    pub(crate) fn get(&self, path: &str) -> Option<&Node> {
        let mut node = &self.root;
        if path.is_empty() {
            return Some(node);
        }
        for token in path.strip_prefix('/')?.split('/') {
            node = node.child(&unescape(token))?;
        }
        Some(node)
    }

    /// Publishes `root`, the complete tree. When it differs from the published one the
    /// revision rises by exactly one, however many paths changed, and the new revision is
    /// returned; otherwise nothing changes.
    pub(crate) fn commit(&mut self, root: BTreeMap<String, Spec>) -> Option<u64> {
        let next = self.rev + 1;
        let new = Node::reconcile(Some(&self.root), Spec::Map(root), next);
        if new.srev < next {
            return None;
        }
        self.root = new;
        self.rev = next;
        Some(next)
    }
}

/// A JSON Pointer token for `key` (RFC 6901).
pub(crate) fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn unescape(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}

#[cfg(test)]
mod tests;
