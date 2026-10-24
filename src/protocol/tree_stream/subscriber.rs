//! What a stream of the revision 4 feed has yet to write (design §6.2): the first event, then
//! one event per commit that changed a followed path.
use std::collections::{BTreeMap, VecDeque};

use tokio::time::Instant;

use super::super::Op;
use super::super::digest;
use super::super::pacing::Pacer;
use super::super::tree::{Committed, Tree};
use super::params::StreamQuery;
use super::snapshot::snapshot;
use super::wire::{Change, Check, End, EndReason, Event};

/// What the stream holds on behalf of the subscriber for one followed path.
struct PathState {
    /// The path version after everything queued for the path: what the next ops `from`.
    queued: Option<u64>,
    /// Whether the path existed at the last event queued for it.
    present: bool,
}

pub(crate) struct Subscriber {
    paths: BTreeMap<String, PathState>,
    cwd: BTreeMap<String, String>,
    queue: VecDeque<Event>,
    /// When a path that changed is due its checkpoint (design §6.5).
    pacer: Pacer,
}

/// Whether `path` is below `ancestor`.
fn below(path: &str, ancestor: &str) -> bool {
    path.len() > ancestor.len()
        && path.starts_with(ancestor)
        && path.as_bytes()[ancestor.len()] == b'/'
}

fn rebased(op: &Op, prefix: usize) -> Op {
    let mut op = op.clone();
    let path = match &mut op {
        Op::Set { path, .. } | Op::Remove { path } | Op::Append { path, .. } | Op::Trim { path, .. } => path,
    };
    *path = path[prefix..].to_string();
    op
}

fn op_path(op: &Op) -> &str {
    match op {
        Op::Set { path, .. } | Op::Remove { path } | Op::Append { path, .. } | Op::Trim { path, .. } => path,
    }
}

impl Subscriber {
    /// A subscriber for `query`, aligned with `tree`: its first event names the instance and
    /// carries, per followed path, a snapshot from the version the client holds when it holds
    /// the tree's instance, else a whole one.
    pub(crate) fn new(query: &StreamQuery, tree: &Tree) -> Self {
        let same_instance = query.instance.as_deref() == Some(tree.instance());
        let mut paths = BTreeMap::new();
        let mut changes = Vec::new();
        for follow in &query.follow {
            let have = follow.have.filter(|_| same_instance);
            let mut state = PathState { queued: None, present: false };
            match snapshot(tree, &follow.path, have) {
                Some(snapshot) => {
                    state.queued = Some(snapshot.at);
                    state.present = true;
                    changes.push(Change::Snapshot { path: follow.path.clone(), snapshot });
                }
                None => changes.push(Change::End {
                    path: follow.path.clone(),
                    end: End { reason: EndReason::Absent, error: None },
                }),
            }
            paths.insert(follow.path.clone(), state);
        }
        let first = Event {
            instance: Some(tree.instance().to_string()),
            rev: tree.rev(),
            changes: Some(changes),
            check: None,
        };
        Self {
            paths,
            cwd: query.cwd.clone(),
            queue: VecDeque::from([first]),
            pacer: Pacer::default(),
        }
    }

    /// The paths followed and the directory given for the load of the session each is under.
    pub(crate) fn followed(&self) -> impl Iterator<Item = &str> {
        self.paths.keys().map(String::as_str)
    }

    pub(crate) fn cwd(&self) -> &BTreeMap<String, String> {
        &self.cwd
    }

    /// A commit: queues one event with the change of every followed path it touched.
    pub(crate) fn publish(&mut self, tree: &Tree, committed: &Committed) {
        self.publish_at(tree, committed, Instant::now());
    }

    /// `publish` at `now`, which paces the checkpoints of what changed.
    pub(crate) fn publish_at(&mut self, tree: &Tree, committed: &Committed, now: Instant) {
        let mut changes = Vec::new();
        for (path, state) in &mut self.paths {
            let direct: Vec<&Op> = committed
                .ops
                .iter()
                .filter(|op| op_path(op) == path || below(op_path(op), path))
                .collect();
            let above = |op: &&Op| below(path, op_path(op));
            // An op above the path that sets or removes replaced a subtree that holds it; one
            // that only appends to a list above it changed nothing in it, unless the path is
            // new. A path removed itself has no ops to send.
            let replaced = committed.ops.iter().filter(above).any(|op| !matches!(op, Op::Append { .. } | Op::Trim { .. }))
                || direct.iter().any(|op| matches!(op, Op::Remove { .. }) && op_path(op) == path);
            let appeared = !state.present && committed.ops.iter().any(|op| above(&op));
            if direct.is_empty() && !replaced && !appeared {
                continue;
            }
            if replaced || appeared || !state.present {
                match snapshot(tree, path, state.queued) {
                    Some(snapshot) => {
                        state.queued = Some(snapshot.at);
                        state.present = true;
                        // A snapshot carries the hash of its subtree.
                        self.pacer.settled(path);
                        changes.push(Change::Snapshot { path: path.clone(), snapshot });
                    }
                    None if state.present => {
                        state.present = false;
                        state.queued = None;
                        self.pacer.settled(path);
                        changes.push(Change::End {
                            path: path.clone(),
                            end: End { reason: EndReason::Absent, error: None },
                        });
                    }
                    None => {}
                }
                continue;
            }
            let from = state.queued.expect("a present path has a version");
            state.queued = Some(committed.rev);
            self.pacer.changed(path, now);
            changes.push(Change::Ops {
                path: path.clone(),
                from,
                ops: direct.iter().map(|op| rebased(op, path.len())).collect(),
            });
        }
        if !changes.is_empty() {
            self.queue.push_back(Event {
                instance: None,
                rev: committed.rev,
                changes: Some(changes),
                check: None,
            });
        }
    }

    /// Queues the checkpoints due at `now`, one event for all the paths: each states the path
    /// version and hash of the tree, which every event queued before it has been applied to.
    /// Whether one was queued.
    pub(crate) fn checkpoints(&mut self, tree: &Tree, now: Instant) -> bool {
        let check: Vec<Check> = self
            .pacer
            .due(now)
            .into_iter()
            .filter_map(|path| {
                let node = tree.get(&path)?;
                Some(Check {
                    at: node.srev(),
                    hash: digest::wire(&node.hash()),
                    path,
                })
            })
            .collect();
        if check.is_empty() {
            return false;
        }
        self.queue.push_back(Event { instance: None, rev: tree.rev(), changes: None, check: Some(check) });
        true
    }

    /// When the next checkpoint is due, if a path waits for one.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.pacer.deadline()
    }

    /// The next event to write.
    pub(crate) fn next(&mut self) -> Option<Event> {
        self.queue.pop_front()
    }

    pub(crate) fn queued(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests;
