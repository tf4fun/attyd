//! The events of the revision 4 stream (design §6.2, §6.3).
use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::super::Op;
use super::super::tree_doc::ErrorSummary;

/// One event: the first of a stream names its `instance`; a commit or a catch-up carries
/// `changes`; a checkpoint carries `check`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Event {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    pub rev: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<Vec<Change>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<Vec<Check>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum Change {
    Snapshot { path: String, snapshot: Snapshot },
    /// Ops relative to `path`, applied when the client holds `from`.
    Ops { path: String, from: u64, ops: Vec<Op> },
    End { path: String, end: End },
}

impl Change {
    pub(crate) fn path(&self) -> &str {
        match self {
            Self::Snapshot { path, .. } | Self::Ops { path, .. } | Self::End { path, .. } => path,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Check {
    pub path: String,
    pub at: u64,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct End {
    pub reason: EndReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorSummary>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EndReason {
    Deleted,
    Closed,
    LoadFailed,
    Absent,
    Replaced,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Snapshot {
    /// The `have` it builds on; `null` replaces everything.
    pub base: Option<u64>,
    /// The path version after installing it.
    pub at: u64,
    pub hash: String,
    pub node: SNode,
}

/// A node of a snapshot (§6.3).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum SNode {
    /// The whole subtree as JSON.
    Value { v: Value },
    /// An object: its own fields, the names of all its children and those that changed.
    Object { o: Value, k: Vec<String>, c: BTreeMap<String, SNode> },
    /// A map: all its keys and the children that changed.
    Map { m: Vec<String>, c: BTreeMap<String, SNode> },
    /// A list: its length and the items that changed, by index.
    List { l: usize, c: BTreeMap<String, SNode> },
    /// The followed path is as the client holds it.
    Same { same: bool },
}
