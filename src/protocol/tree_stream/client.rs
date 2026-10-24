//! A model of a client of the revision 4 stream, for the tests: it holds the JSON of each
//! followed path and its path version and applies events by the rules of design §6.2.
use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::super::diff;
use super::wire::{Change, Event, SNode};

/// What a client holding `held` has after installing `node` (design §6.3): it keeps every child
/// the structure names and the snapshot omits, drops the rest, and replaces the listed.
pub(crate) fn install(held: Option<&Value>, node: &SNode) -> Value {
    let child = |held: Option<&Value>, key: &str, c: &BTreeMap<String, SNode>| match c.get(key) {
        Some(node) => install(held.and_then(|held| held.get(key)), node),
        None => held.and_then(|held| held.get(key)).cloned().expect("a kept child is held"),
    };
    match node {
        SNode::Same { .. } => held.cloned().expect("the client holds what it says it holds"),
        SNode::Value { v } => v.clone(),
        SNode::Object { o, k, c } => {
            let mut object: Map<String, Value> = o.as_object().unwrap().clone();
            for key in k {
                object.insert(key.clone(), child(held, key, c));
            }
            Value::Object(object)
        }
        SNode::Map { m, c } => {
            Value::Object(m.iter().map(|key| (key.clone(), child(held, key, c))).collect())
        }
        SNode::List { l, c } => Value::Array((0..*l).map(|i| child(held, &i.to_string(), c)).collect()),
    }
}

/// The replica of one followed path.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Held {
    /// `None` while the path does not exist for the client.
    pub value: Option<Value>,
    /// `v_p`.
    pub version: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Client {
    pub instance: Option<String>,
    pub paths: BTreeMap<String, Held>,
    /// The `rev` of the last event.
    pub rev: u64,
}

impl Client {
    /// A client that follows `paths` and holds nothing.
    pub(crate) fn following(paths: &[&str]) -> Self {
        Self {
            paths: paths.iter().map(|path| (path.to_string(), Held::default())).collect(),
            ..Self::default()
        }
    }

    pub(crate) fn value(&self, path: &str) -> Option<&Value> {
        self.paths.get(path)?.value.as_ref()
    }

    pub(crate) fn version(&self, path: &str) -> u64 {
        self.paths[path].version
    }

    /// Applies one event; an error is a gap, and the client would resubscribe with its `have`s.
    pub(crate) fn apply(&mut self, event: &Event) -> Result<(), String> {
        if let Some(instance) = &event.instance
            && self.instance.as_ref() != Some(instance)
        {
            for held in self.paths.values_mut() {
                *held = Held::default();
            }
            self.instance = Some(instance.clone());
        }
        // All changes of one event are applied together: on an error nothing is.
        let mut next = self.paths.clone();
        for change in event.changes.iter().flatten() {
            let held = next
                .get_mut(change.path())
                .ok_or_else(|| format!("a change of a path not followed: {}", change.path()))?;
            match change {
                Change::Snapshot { snapshot, .. } => {
                    if snapshot.base.is_none() && snapshot.at <= held.version && held.value.is_some() {
                        continue;
                    }
                    if snapshot.base.is_some() && snapshot.base != Some(held.version) && held.value.is_some() {
                        return Err(format!("snapshot builds on {:?}, held {}", snapshot.base, held.version));
                    }
                    held.value = Some(install(held.value.as_ref(), &snapshot.node));
                    held.version = snapshot.at;
                }
                Change::Ops { from, ops, path } => {
                    if event.rev <= held.version {
                        continue;
                    }
                    if *from != held.version || held.value.is_none() {
                        return Err(format!("{path}: ops from {from}, held {}", held.version));
                    }
                    let value = held.value.as_mut().expect("held");
                    for op in ops {
                        diff::apply(value, op).map_err(|error| format!("{path}: {op:?}: {error:?}"))?;
                    }
                    held.version = event.rev;
                }
                Change::End { .. } => held.value = None,
            }
        }
        self.paths = next;
        self.rev = event.rev;
        Ok(())
    }
}
