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

use super::diff::{self, Text};
use super::hash::{self, Hash, TextRule};
use super::{AppendTail, Op};

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
    fn kind(&self) -> u8 {
        match self {
            Spec::Value(..) => 0,
            Spec::Object { .. } => 1,
            Spec::Map(_) => 2,
            Spec::List(_) => 3,
        }
    }

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

    /// The length of a list node.
    pub(crate) fn list_len(&self) -> Option<usize> {
        match &self.body {
            Body::List(items) => Some(items.len()),
            _ => None,
        }
    }

    /// The value of a value node and the rule of its text positions.
    pub(crate) fn as_value(&self) -> Option<(&Value, TextRule)> {
        match &self.body {
            Body::Value(value, rule) => Some((value, *rule)),
            _ => None,
        }
    }

    /// The child `key` of an object or a map, or the index `key` of a list.
    pub(crate) fn child(&self, key: &str) -> Option<&Node> {
        match &self.body {
            Body::Value(..) => None,
            Body::Object { children, .. } | Body::Map(children) => children.get(key),
            Body::List(items) => items.get(key.parse::<usize>().ok().filter(|i| i.to_string() == key)?),
        }
    }

    /// The node's own level and its children, for the snapshot of a stream; with `window`, the
    /// text positions of a value are held as windows of that many bytes.
    pub(crate) fn shape(&self, window: Option<usize>) -> super::tree_stream::snapshot::Shape<'_> {
        use super::tree_stream::snapshot::Shape;
        match &self.body {
            Body::Value(value, rule) => Shape::Value(match window {
                Some(bytes) => super::window::window_value(*rule, value, bytes),
                None => (**value).clone(),
            }),
            Body::Object { fields, children } => Shape::Object(
                Value::Object(fields.clone()),
                children.iter().map(|(key, node)| (key.clone(), node)).collect(),
            ),
            Body::Map(children) => Shape::Map(
                children.keys().cloned().collect(),
                children.iter().map(|(key, node)| (key.clone(), node)).collect(),
            ),
            Body::List(items) => Shape::List(
                items.len(),
                items.iter().enumerate().map(|(index, node)| (index.to_string(), node)).collect(),
            ),
        }
    }

    /// `to_value`, with the text positions held as windows of `window` bytes.
    pub(crate) fn to_value_windowed(&self, window: usize) -> Value {
        match &self.body {
            Body::Value(value, rule) => super::window::window_value(*rule, value, window),
            Body::Object { fields, children } => {
                let mut object = fields.clone();
                for (key, child) in children {
                    object.insert(key.clone(), child.to_value_windowed(window));
                }
                Value::Object(object)
            }
            Body::Map(children) => Value::Object(
                children
                    .iter()
                    .map(|(key, child)| (key.clone(), child.to_value_windowed(window)))
                    .collect(),
            ),
            Body::List(items) => Value::Array(items.iter().map(|item| item.to_value_windowed(window)).collect()),
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

    /// Which kind of node this is, to tell a changed kind from a changed value.
    fn kind(&self) -> u8 {
        match self.body {
            Body::Value(..) => 0,
            Body::Object { .. } => 1,
            Body::Map(_) => 2,
            Body::List(_) => 3,
        }
    }

    /// The node for `spec` at revision `rev`, keeping the revisions of `old` where nothing
    /// changed. The ops that turn `old`'s value at `path` into the new one are added to `ops`,
    /// absolute from the root; a node that is new, or of another kind, is the `set` of its
    /// parent's op and adds none below it.
    fn reconcile(old: Option<&Node>, spec: Spec, rev: u64, path: &str, ops: &mut Vec<Op>) -> Node {
        let same = old.is_some_and(|old| old.same_level(&spec));
        let own = match old {
            Some(old) if same => old.rev,
            _ => rev,
        };
        let old_body = old.map(|old| &old.body);
        let body = match spec {
            Spec::Value(value, rule) => match old_body {
                Some(Body::Value(old, _)) if same => Body::Value(old.clone(), rule),
                Some(Body::Value(old, _)) => {
                    diff::diff(path, old, &value, &text_mode, ops);
                    Body::Value(value, rule)
                }
                _ => Body::Value(value, rule),
            },
            Spec::Object { fields, children } => {
                let (old_fields, old_children) = match old_body {
                    Some(Body::Object { fields, children }) => (Some(fields), Some(children)),
                    _ => (None, None),
                };
                if let Some(old_fields) = old_fields.filter(|old| **old != fields) {
                    diff::diff(
                        path,
                        &Value::Object(old_fields.clone()),
                        &Value::Object(fields.clone()),
                        &text_mode,
                        ops,
                    );
                }
                Body::Object {
                    fields,
                    children: Node::reconcile_children(old_children, children, rev, path, ops),
                }
            }
            Spec::Map(children) => {
                let old_children = match old_body {
                    Some(Body::Map(children)) => Some(children),
                    _ => None,
                };
                Body::Map(Node::reconcile_children(old_children, children, rev, path, ops))
            }
            Spec::List(items) => {
                let old_items = match old_body {
                    Some(Body::List(items)) => Some(items.as_slice()),
                    _ => None,
                };
                // A list that shrank is replaced whole; one that grew keeps its items and
                // appends the new ones.
                let shrank = old_items.is_some_and(|old| items.len() < old.len());
                let mut replaced = Vec::new();
                let item_ops = if shrank { &mut replaced } else { &mut *ops };
                let kept = old_items.map_or(0, <[Node]>::len).min(items.len());
                let mut nodes = Vec::with_capacity(items.len());
                for (index, spec) in items.into_iter().enumerate() {
                    let old_item = old_items.and_then(|old| old.get(index));
                    let item_path = diff::child(path, &index.to_string());
                    nodes.push(Node::reconcile_child(old_item, spec, rev, &item_path, item_ops, index < kept));
                }
                if shrank {
                    ops.push(Op::Set {
                        path: path.to_string(),
                        value: Value::Array(nodes.iter().map(Node::to_value).collect()),
                    });
                } else if old_items.is_some() && nodes.len() > kept {
                    ops.push(Op::Append {
                        path: path.to_string(),
                        tail: AppendTail::Values {
                            values: nodes[kept..].iter().map(Node::to_value).collect(),
                        },
                    });
                }
                Body::List(nodes)
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

    /// A child at `path`: reconciled with its old node when that is of the same kind, else
    /// built anew and, when `existing` (its parent holds it), added to `ops` as a `set`.
    fn reconcile_child(
        old: Option<&Node>,
        spec: Spec,
        rev: u64,
        path: &str,
        ops: &mut Vec<Op>,
        existing: bool,
    ) -> Node {
        let comparable = old.filter(|old| old.kind() == spec.kind());
        if comparable.is_some() {
            return Node::reconcile(comparable, spec, rev, path, ops);
        }
        let node = Node::reconcile(None, spec, rev, path, &mut Vec::new());
        if existing || old.is_some() {
            ops.push(Op::Set {
                path: path.to_string(),
                value: node.to_value(),
            });
        }
        node
    }

    /// Replaces the subtree at `tokens` below this node by `spec`, as a commit at `rev` does; the
    /// ancestors recompute their `srev` and forget their hash. A new child of an object or a map
    /// is added when its parent exists. `false` when the parent does not exist or holds no
    /// children by key.
    fn replace_at(
        &mut self,
        tokens: &[String],
        spec: Spec,
        rev: u64,
        path: &str,
        ops: &mut Vec<Op>,
    ) -> bool {
        let Some((key, rest)) = tokens.split_first() else {
            *self = Node::reconcile(Some(self), spec, rev, path, ops);
            return true;
        };
        let child_path = diff::child(path, key);
        let children = match &mut self.body {
            Body::Object { children, .. } | Body::Map(children) => children,
            _ => return false,
        };
        let mut own = self.rev;
        match children.get_mut(key) {
            Some(child) => {
                if !child.replace_at(rest, spec, rev, &child_path, ops) {
                    return false;
                }
            }
            None if rest.is_empty() => {
                let node = Node::reconcile(None, spec, rev, &child_path, &mut Vec::new());
                ops.push(Op::Set { path: child_path, value: node.to_value() });
                children.insert(key.clone(), node);
                own = rev;
            }
            None => return false,
        }
        self.rev = own;
        self.srev = own.max(children.values().map(|node| node.srev).max().unwrap_or(0));
        self.hash = OnceLock::new();
        true
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

    /// The children of an object or a map. A child the parent did not have, or of another
    /// kind, is a `set` at its key; one that is gone is a `remove`.
    fn reconcile_children(
        old: Option<&BTreeMap<String, Node>>,
        children: BTreeMap<String, Spec>,
        rev: u64,
        path: &str,
        ops: &mut Vec<Op>,
    ) -> BTreeMap<String, Node> {
        if let Some(old) = old {
            for key in old.keys().filter(|key| !children.contains_key(*key)) {
                ops.push(Op::Remove {
                    path: diff::child(path, key),
                });
            }
        }
        children
            .into_iter()
            .map(|(key, spec)| {
                let old_child = old.and_then(|old| old.get(&key));
                let child_path = diff::child(path, &key);
                let node = match old {
                    // The parent exists: a child new to it is a `set`.
                    Some(_) => {
                        Node::reconcile_child(old_child, spec, rev, &child_path, ops, true)
                    }
                    None => Node::reconcile(None, spec, rev, &child_path, &mut Vec::new()),
                };
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
        self.commit_with_ops(root).map(|committed| committed.rev)
    }

    /// `commit`, with the ops that turn the tree before into the tree after.
    pub(crate) fn commit_with_ops(&mut self, root: BTreeMap<String, Spec>) -> Option<Committed> {
        let next = self.rev + 1;
        let mut ops = Vec::new();
        let new = Node::reconcile(Some(&self.root), Spec::Map(root), next, "", &mut ops);
        if new.srev < next {
            return None;
        }
        self.root = new;
        self.rev = next;
        Some(Committed { rev: next, ops })
    }
}

impl Tree {
    /// Publishes `spec` as the subtree at `path`, leaving the rest of the tree as it is: a commit
    /// of one subtree, for state that changes outside the projection of the registry. `None`
    /// when it changes nothing, or when the path's parent does not exist.
    pub(crate) fn commit_at(&mut self, path: &str, spec: Spec) -> Option<Committed> {
        let tokens: Vec<String> = path.strip_prefix('/')?.split('/').map(unescape).collect();
        let next = self.rev + 1;
        let mut ops = Vec::new();
        let mut root = self.root.clone();
        if !root.replace_at(&tokens, spec, next, "", &mut ops) || root.srev < next {
            return None;
        }
        self.root = root;
        self.rev = next;
        Some(Committed { rev: next, ops })
    }
}

/// What a commit changed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Committed {
    pub rev: u64,
    /// JSON Pointer ops from the root; they turn `to_value()` of the tree before into that of
    /// the tree after.
    pub ops: Vec<Op>,
}

/// Terminal output is a bounded tail; every other string grows or is set.
fn text_mode(path: &str) -> Text {
    if path.contains("/terminals/") && path.ends_with("/output") {
        Text::Tail
    } else {
        Text::Grow
    }
}

/// A JSON Pointer token for `key` (RFC 6901).
pub(crate) fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// The key a JSON Pointer token stands for.
pub(crate) fn unescape(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}

#[cfg(test)]
mod tests;
