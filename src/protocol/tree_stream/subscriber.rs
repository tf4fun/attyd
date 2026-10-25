//! What a stream of the revision 4 feed has yet to write (design §6.2): the first event, then
//! one event per commit that changed a followed path.
use std::collections::{BTreeMap, VecDeque};

use tokio::time::Instant;

use super::super::Op;
use super::super::digest;
use super::super::pacing::Pacer;
use super::super::tree::{Committed, Tree};
use super::super::views::{self, Gone, View};
use super::params::{StreamQuery, session_of};
use super::snapshot::snapshot_windowed;
use super::windowed_ops::windowed;
use super::QUEUE_BUDGET;
use super::wire::{Change, Check, End, EndReason, Event};

/// A queued event and the bytes it counts against the budget.
struct Queued {
    event: Event,
    counted: usize,
}

/// What the stream holds on behalf of the subscriber for one followed path.
struct PathState {
    /// Hold the text positions under the path as windows of this many bytes (design §6.4).
    window: Option<usize>,
    /// The path version the client is known to hold: what the client said it held, then what
    /// was last written to its stream. An event still in the queue is not delivered.
    written: Option<u64>,
    /// The path version after everything queued for the path: what the next ops `from`.
    queued: Option<u64>,
    /// Whether the path existed at the last event queued for it.
    present: bool,
    /// A fixed view that ended: nothing follows its end.
    over: bool,
    /// Whether the path asks for its session to be held and loaded. An end for a reason (deleted,
    /// closed, failed to load) says there is no point in asking again: the subscription stays, to
    /// deliver the path if it is ever there again, and asks for nothing.
    demand: bool,
}

/// Why `path`, if it is a fixed view, is over: the end it is finished by (design §4.5).
fn fixed_end(tree: &Tree, path: &str) -> Option<End> {
    let reason = match views::gone(tree, &View::of(path)?)? {
        Gone::Absent => EndReason::Absent,
        Gone::Replaced => EndReason::Replaced,
    };
    Some(End { reason, error: None })
}

pub(crate) struct Subscriber {
    paths: BTreeMap<String, PathState>,
    cwd: BTreeMap<String, String>,
    queue: VecDeque<Queued>,
    /// The bytes of the queued events that carry ops. Snapshots are bounded by the tree and
    /// do not count: coalescing them again would only build the same snapshots anew.
    queued_bytes: usize,
    /// Whether the event that names the instance was written.
    instance_written: bool,
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
    /// One final read after shutdown: deliver the existing cut without holding a session,
    /// requesting a load, or tracking subsequent publication.
    pub(crate) fn final_snapshot(query: &StreamQuery, tree: &Tree) -> Self {
        let mut subscriber = Self::new(query, tree);
        for state in subscriber.paths.values_mut() {
            state.demand = false;
            state.over = true;
        }
        subscriber
    }

    /// A subscriber for `query`, aligned with `tree`: its first event names the instance and
    /// carries, per followed path, a snapshot from the version the client holds when it holds
    /// the tree's instance, else a whole one.
    pub(crate) fn new(query: &StreamQuery, tree: &Tree) -> Self {
        let same_instance = query.instance.as_deref() == Some(tree.instance());
        let mut paths = BTreeMap::new();
        let mut changes = Vec::new();
        for follow in &query.follow {
            let have = follow.have.filter(|_| same_instance);
            let mut state = PathState { window: follow.window, written: have, queued: None, present: false, over: false, demand: true };
            match snapshot_windowed(tree, &follow.path, have, follow.window) {
                Some(snapshot) => {
                    state.queued = Some(snapshot.at);
                    state.present = true;
                    changes.push(Change::Snapshot { path: follow.path.clone(), snapshot });
                }
                None => {
                    let fixed = fixed_end(tree, &follow.path);
                    state.over = fixed.is_some();
                    changes.push(Change::End {
                        path: follow.path.clone(),
                        end: fixed.unwrap_or(End { reason: EndReason::Absent, error: None }),
                    });
                }
            }
            paths.insert(follow.path.clone(), state);
        }
        let first = Event {
            instance: Some(tree.instance().to_string()),
            rev: tree.rev(),
            changes: Some(changes),
            check: None,
        };
        let mut subscriber = Self {
            paths,
            cwd: query.cwd.clone(),
            queue: VecDeque::new(),
            queued_bytes: 0,
            instance_written: false,
            pacer: Pacer::default(),
        };
        subscriber.push(first);
        subscriber
    }

    fn push(&mut self, event: Event) {
        // Only an event with ops counts: the rest is snapshots, ends and checks.
        let has_ops = event
            .changes
            .iter()
            .flatten()
            .any(|change| matches!(change, Change::Ops { .. }));
        let counted = if has_ops {
            serde_json::to_string(&event).expect("an event serializes").len()
        } else {
            0
        };
        self.queued_bytes += counted;
        self.queue.push_back(Queued { event, counted });
    }

    /// The paths followed and the directory given for the load of the session each is under.
    pub(crate) fn followed(&self) -> impl Iterator<Item = &str> {
        self.paths
            .iter()
            .filter(|(_, state)| !state.over && state.demand)
            .map(|(path, _)| path.as_str())
    }

    /// The paths still tracked, whether or not they ask for their session.
    pub(crate) fn tracked(&self) -> impl Iterator<Item = &str> {
        self.paths.iter().filter(|(_, state)| !state.over).map(|(path, _)| path.as_str())
    }

    pub(crate) fn cwd(&self) -> &BTreeMap<String, String> {
        &self.cwd
    }

    /// The bridge released `session_id`, which it knew in `cwd`: a path of it this subscriber
    /// follows asks for the load in that directory, whatever it supplied.
    pub(crate) fn remember_cwd(&mut self, session_id: &str, cwd: &str) {
        if self.paths.keys().any(|path| session_of(path).as_deref() == Some(session_id)) {
            self.cwd.insert(session_id.to_string(), cwd.to_string());
        }
    }

    /// A commit: queues one event with the change of every followed path it touched.
    pub(crate) fn publish(&mut self, tree: &Tree, committed: &Committed) {
        self.publish_at(tree, committed, Instant::now());
    }

    /// `publish` at `now`, which paces the checkpoints of what changed.
    pub(crate) fn publish_at(&mut self, tree: &Tree, committed: &Committed, now: Instant) {
        self.publish_ended(tree, committed, now, &BTreeMap::new());
    }

    /// `publish_at`, with the sessions that ended in this commit and why: a followed path of
    /// such a session that is gone ends with that reason instead of `absent`.
    pub(crate) fn publish_ended(
        &mut self,
        tree: &Tree,
        committed: &Committed,
        now: Instant,
        ended: &BTreeMap<String, End>,
    ) {
        let mut changes = Vec::new();
        for (path, state) in &mut self.paths {
            if state.over {
                continue;
            }
            // A fixed view that was not there yet may now be over for good.
            if !state.present
                && let Some(end) = fixed_end(tree, path)
            {
                state.over = true;
                changes.push(Change::End { path: path.clone(), end });
                continue;
            }
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
                match snapshot_windowed(tree, path, state.queued, state.window) {
                    Some(snapshot) => {
                        state.queued = Some(snapshot.at);
                        state.present = true;
                        state.demand = true;
                        // A snapshot carries the hash of its subtree.
                        self.pacer.settled(path);
                        changes.push(Change::Snapshot { path: path.clone(), snapshot });
                    }
                    None if state.present => {
                        state.present = false;
                        state.queued = None;
                        self.pacer.settled(path);
                        let fixed = fixed_end(tree, path);
                        state.over = fixed.is_some();
                        let end = fixed
                            .or_else(|| session_of(path).and_then(|session_id| ended.get(&session_id).cloned()))
                            .unwrap_or(End { reason: EndReason::Absent, error: None });
                        // An end for a reason is no reason to ask for the session again.
                        state.demand = end.reason == EndReason::Absent;
                        changes.push(Change::End { path: path.clone(), end });
                    }
                    None => {}
                }
                continue;
            }
            let from = state.queued.expect("a present path has a version");
            state.queued = Some(committed.rev);
            self.pacer.changed(path, now);
            let ops: Vec<Op> = match state.window {
                Some(bytes) => {
                    let mut replaced = std::collections::BTreeSet::new();
                    direct
                        .iter()
                        .flat_map(|op| windowed(tree, op, bytes, &mut replaced))
                        .map(|op| rebased(&op, path.len()))
                        .collect()
                }
                None => direct.iter().map(|op| rebased(op, path.len())).collect(),
            };
            changes.push(Change::Ops { path: path.clone(), from, ops });
        }
        if !changes.is_empty() {
            self.push(Event {
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
        self.push(Event { instance: None, rev: tree.rev(), changes: None, check: Some(check) });
        true
    }

    /// When the next checkpoint is due, if a path waits for one.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.pacer.deadline()
    }

    /// A load of `session_id` failed, or the session is over, before it was published: its
    /// followed paths end with `end`. The subscription stays (a later load starts it again).
    pub(crate) fn end_session(&mut self, tree: &Tree, session_id: &str, end: &End) {
        let changes: Vec<Change> = self
            .paths
            .iter_mut()
            .filter(|(path, state)| {
                session_of(path).as_deref() == Some(session_id)
                    && state.demand
                    && !state.present
                    && !state.over
                    && tree.get(path).is_none()
            })
            .map(|(path, state)| {
                state.demand = end.reason == EndReason::Absent;
                Change::End { path: path.clone(), end: end.clone() }
            })
            .collect();
        if !changes.is_empty() {
            self.push(Event { instance: None, rev: tree.rev(), changes: Some(changes), check: None });
        }
    }

    /// The next event to write; it advances what the client is known to hold.
    pub(crate) fn next(&mut self) -> Option<Event> {
        let Queued { event, counted } = self.queue.pop_front()?;
        self.queued_bytes -= counted;
        self.instance_written |= event.instance.is_some();
        for change in event.changes.iter().flatten() {
            if let Some(state) = self.paths.get_mut(change.path()) {
                state.written = match change {
                    Change::Snapshot { snapshot, .. } => Some(snapshot.at),
                    Change::Ops { .. } => Some(event.rev),
                    Change::End { .. } => None,
                };
            }
        }
        Some(event)
    }

    /// Whether the events queued exceed the budget.
    pub(crate) fn over_budget(&self) -> bool {
        self.queued_bytes > QUEUE_BUDGET
    }

    /// The subscriber could not keep up: what is queued is dropped and replaced by one
    /// catch-up event at the current revision, carrying for each followed path a differential
    /// snapshot from the version last written, or its end. It never changes what a path
    /// version contains (design §6.5).
    pub(crate) fn coalesce(&mut self, tree: &Tree) {
        // The ends queued are kept, with their reasons: a snapshot says nothing of why a path
        // is gone.
        let mut queued_ends: BTreeMap<String, End> = BTreeMap::new();
        for queued in &self.queue {
            for change in queued.event.changes.iter().flatten() {
                match change {
                    Change::End { path, end } => {
                        queued_ends.insert(path.clone(), end.clone());
                    }
                    other => {
                        queued_ends.remove(other.path());
                    }
                }
            }
        }
        self.queue.clear();
        self.queued_bytes = 0;
        let mut changes = Vec::new();
        for (path, state) in &mut self.paths {
            if state.over {
                // Tracking ended, but its final notification may still be waiting to be
                // written. Only the queued end is owed; never recreate a delivered one.
                if let Some(end) = queued_ends.remove(path.as_str()) {
                    changes.push(Change::End { path: path.clone(), end });
                }
                continue;
            }
            match snapshot_windowed(tree, path, state.written, state.window) {
                Some(snapshot) => {
                    state.queued = Some(snapshot.at);
                    state.present = true;
                    state.demand = true;
                    self.pacer.settled(path);
                    changes.push(Change::Snapshot { path: path.clone(), snapshot });
                }
                None => {
                    state.queued = None;
                    state.present = false;
                    self.pacer.settled(path);
                    let fixed = fixed_end(tree, path);
                    state.over = fixed.is_some();
                    let end = fixed.or_else(|| queued_ends.remove(path.as_str()));
                    if let Some(end) = &end {
                        state.demand = end.reason == EndReason::Absent;
                    }
                    if end.is_some() || state.written.is_some() {
                        changes.push(Change::End {
                            path: path.clone(),
                            end: end.unwrap_or(End { reason: EndReason::Absent, error: None }),
                        });
                    }
                }
            }
        }
        // A catch-up that replaces the first event names the instance too.
        let instance = (!self.instance_written).then(|| tree.instance().to_string());
        self.push(Event { instance, rev: tree.rev(), changes: Some(changes), check: None });
    }

    /// The event at the head of the queue.
    #[cfg(test)]
    pub(crate) fn peek_first(&self) -> &Event {
        &self.queue.front().expect("an event is queued").event
    }

    pub(crate) fn queued(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests;
