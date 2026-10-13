//! The commit step (plan §2.2): project each scope, diff it against the
//! document last published for that scope, and append the ops to the log.
//!
//! Published documents keep only what diffing needs. Large sequences are
//! compared by identity: the history baseline by its `Arc`, active-turn
//! updates element by element by pointer, and the projected timeline by
//! per-element digests, since collapsing completed turns renumbers it.
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Weak};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::diff::{self, Text};
use super::hub::{Documents, Hub, ScopeRequest, StreamQueue};
use super::log::{Change, Log};
use super::project::{HistorySection, SessionSource};
use super::{
    AgentState, AppendTail, EndReason, GlobalState, Op, Position, SessionState, Snapshot,
    project_global, project_session,
};
use crate::history_cache::HistorySnapshot;
use crate::runtime_state::SharedTurnUpdates;
use crate::session_presentation::terminal_references;
use crate::session_registry::SessionRegistry;
use crate::session_state::CompletedTurnOutcome;

pub(crate) const GLOBAL: &str = "global";

pub(crate) fn session_scope(session_id: &str) -> String {
    format!("session/{session_id}")
}

/// Ended incarnations kept per epoch, so a subscription to one gets its
/// `end` reason after the log compacted its `End` (protocol §4.2).
pub(crate) const RETIRED_OWNERS: usize = 1024;

/// The commit step's state: the log, the last published documents, and the
/// Hub that delivers the log to streams.
#[derive(Debug, Default)]
pub(crate) struct Replication {
    log: Log,
    global: Option<PublishedGlobal>,
    sessions: BTreeMap<String, PublishedSession>,
    /// Why an incarnation is retiring, noted by the transition that retires
    /// it and consumed by its scope's `End`.
    retiring: HashMap<(String, u64), EndReason>,
    retired: VecDeque<(String, u64, EndReason)>,
    hub: Option<Hub>,
    #[cfg(test)]
    pub(super) history_projections: usize,
}

/// The Hub's view of the published state at the current position.
struct Published<'a> {
    sessions: &'a BTreeMap<String, PublishedSession>,
    retired: &'a VecDeque<(String, u64, EndReason)>,
    agent: &'a AgentState,
    catalog_revision: u64,
    registry: &'a SessionRegistry,
}

impl Documents for Published<'_> {
    fn held_incarnation(&self, session_id: &str) -> Option<u64> {
        self.sessions
            .get(session_id)
            .map(|session| session.incarnation)
    }

    fn retired(&self, session_id: &str, incarnation: u64) -> Option<EndReason> {
        retired_reason(self.retired, session_id, incarnation)
    }

    fn global(&self) -> GlobalState {
        project_global(self.agent, self.catalog_revision, self.registry)
    }
}

fn retired_reason(
    retired: &VecDeque<(String, u64, EndReason)>,
    session_id: &str,
    incarnation: u64,
) -> Option<EndReason> {
    retired
        .iter()
        .rev()
        .find(|(id, retired, _)| id == session_id && *retired == incarnation)
        .map(|(_, _, reason)| *reason)
}

#[derive(Debug)]
struct PublishedGlobal {
    seq: u64,
    document: Value,
}

#[derive(Debug)]
struct PublishedSession {
    incarnation: u64,
    seq: u64,
    /// The document with `history.timeline`, `history.collapsedTurns`,
    /// `outcomes` and `turn.updates` left empty; those are diffed by identity.
    body: Value,
    history: PublishedHistory,
    turn: Option<PublishedTurn>,
}

#[derive(Debug)]
struct PublishedHistory {
    snapshot: Option<Weak<HistorySnapshot>>,
    revision: Option<String>,
    outcomes: Vec<CompletedTurnOutcome>,
    /// One digest per projected timeline element.
    timeline: Vec<[u8; 16]>,
    collapsed_turns: Value,
    projected_outcomes: Value,
    section: HistorySection,
}

#[derive(Debug)]
struct PublishedTurn {
    operation_id: String,
    prompt: Weak<Vec<Value>>,
    projected_prompt: Vec<Value>,
    prompt_refs: HashSet<String>,
    /// Shared with the bridge state: a changed element has a new pointer.
    updates: SharedTurnUpdates,
    update_refs: Vec<Vec<String>>,
}

impl Replication {
    #[cfg(test)]
    pub(crate) fn with_log_budget(budget: usize) -> Self {
        Self {
            log: Log::with_budget(budget),
            ..Self::default()
        }
    }

    pub(crate) fn log(&self) -> &Log {
        &self.log
    }

    /// Allows convergence tests to inject malformed delivery without changing
    /// the production commit path.
    #[cfg(test)]
    pub(crate) fn log_mut_for_test(&mut self) -> &mut Log {
        &mut self.log
    }

    #[cfg(test)]
    pub(crate) fn with_budgets(log: usize, epoch: &str, subscriber: usize) -> Self {
        Self {
            log: Log::with_budget(log),
            hub: Some(Hub::with_budget(epoch, subscriber)),
            ..Self::default()
        }
    }

    /// Shrinks the Hub's per-subscriber budget so a test can make a stream lag.
    #[cfg(test)]
    pub(crate) fn set_subscriber_budget(&mut self, epoch: &str, budget: usize) {
        self.hub
            .get_or_insert_with(|| Hub::new(epoch))
            .set_budget(budget);
    }

    /// Records why `incarnation` is retiring; its scope's `End` carries it.
    /// A later note for the same incarnation replaces it.
    pub(crate) fn note_retirement(
        &mut self,
        session_id: &str,
        incarnation: u64,
        reason: EndReason,
    ) {
        self.retiring
            .insert((session_id.to_string(), incarnation), reason);
    }

    /// Why an ended incarnation of this epoch ended, while the bounded table
    /// still holds it.
    pub(crate) fn retired(&self, session_id: &str, incarnation: u64) -> Option<EndReason> {
        retired_reason(&self.retired, session_id, incarnation)
    }

    /// The incarnation of the held session's published document.
    pub(crate) fn held_incarnation(&self, session_id: &str) -> Option<u64> {
        self.sessions
            .get(session_id)
            .map(|session| session.incarnation)
    }

    /// `GET /api/v2/global`: the document at the current position.
    pub(crate) fn global_snapshot(
        &self,
        agent: &AgentState,
        catalog_revision: u64,
        registry: &SessionRegistry,
    ) -> Snapshot<GlobalState> {
        Snapshot {
            at: self.position(registry),
            state: project_global(agent, catalog_revision, registry),
        }
    }

    /// `GET /api/v2/sessions/{id}`: a held session's document at the current
    /// position. Never loads anything.
    pub(crate) fn session_snapshot(
        &self,
        registry: &SessionRegistry,
        session_id: &str,
    ) -> Option<Snapshot<SessionState>> {
        if !self.sessions.contains_key(session_id) {
            return None;
        }
        Some(Snapshot {
            at: self.position(registry),
            state: project_session(registry, session_id)?,
        })
    }

    fn position(&self, registry: &SessionRegistry) -> Position {
        Position {
            epoch: registry.epoch().to_string(),
            seq: self.log.seq(),
        }
    }

    /// Opens a stream at the current position (plan §2.3, "Cut").
    pub(crate) fn subscribe(
        &mut self,
        agent: &AgentState,
        catalog_revision: u64,
        registry: &SessionRegistry,
        scopes: Vec<ScopeRequest>,
        last_event: Option<&Position>,
    ) -> Arc<StreamQueue> {
        let hub = self.hub.get_or_insert_with(|| Hub::new(registry.epoch()));
        let published = Published {
            sessions: &self.sessions,
            retired: &self.retired,
            agent,
            catalog_revision,
            registry,
        };
        hub.subscribe(&self.log, &published, scopes, last_event)
    }

    /// The epoch ended: every stream drains and closes.
    pub(crate) fn finish(&mut self) {
        if let Some(hub) = &mut self.hub {
            hub.finish();
        }
    }

    pub(crate) fn subscribers(&self) -> usize {
        self.hub.as_ref().map_or(0, Hub::subscribers)
    }

    /// The `seq` of the scope's last entry, or `None` when the scope has no
    /// published document.
    pub(crate) fn scope_seq(&self, scope: &str) -> Option<u64> {
        if scope == GLOBAL {
            return self.global.as_ref().map(|global| global.seq);
        }
        let session_id = scope.strip_prefix("session/")?;
        self.sessions.get(session_id).map(|session| session.seq)
    }

    /// One commit for the whole transition. Sessions commit before `global`,
    /// so a summary change follows its session's entry.
    pub(crate) fn commit(
        &mut self,
        agent: &AgentState,
        catalog_revision: u64,
        registry: &SessionRegistry,
    ) {
        let ended = self
            .sessions
            .iter()
            .filter(|(session_id, published)| {
                SessionSource::new(registry, session_id)
                    .is_none_or(|source| source.state.incarnation != published.incarnation)
            })
            .map(|(session_id, _)| session_id.clone())
            .collect::<Vec<_>>();
        for session_id in ended {
            let published = self.sessions.remove(&session_id).expect("listed above");
            let reason = self
                .retiring
                .remove(&(session_id.clone(), published.incarnation))
                .unwrap_or_else(|| {
                    if registry.sessions.contains_key(&session_id) {
                        EndReason::Replaced
                    } else {
                        EndReason::Gone
                    }
                });
            self.log.append(
                &session_scope(&session_id),
                published.seq,
                Change::End(reason),
            );
            self.retired
                .push_back((session_id, published.incarnation, reason));
            if self.retired.len() > RETIRED_OWNERS {
                self.retired.pop_front();
            }
            // A scope that ends during a turn releases that turn's process
            // content, as a turn that ends does.
            if published.turn.is_some() {
                self.log.compact_through(published.seq);
            }
        }
        for session_id in registry.sessions.keys() {
            if let Some(source) = SessionSource::new(registry, session_id) {
                self.commit_session(session_id, &source);
            }
        }

        let document = serde_json::to_value(project_global(agent, catalog_revision, registry))
            .unwrap_or(Value::Null);
        let (prev, ops) = match &self.global {
            Some(global) => {
                let mut ops = Vec::new();
                diff::diff("", &global.document, &document, &|_| Text::Grow, &mut ops);
                (global.seq, ops)
            }
            None => (0, create(&document)),
        };
        if !ops.is_empty() {
            let seq = self.log.append(GLOBAL, prev, Change::Ops(ops.into()));
            self.global = Some(PublishedGlobal { seq, document });
        }

        let hub = self.hub.get_or_insert_with(|| Hub::new(registry.epoch()));
        let published = Published {
            sessions: &self.sessions,
            retired: &self.retired,
            agent,
            catalog_revision,
            registry,
        };
        hub.deliver(&self.log, &published);
    }

    fn commit_session(&mut self, session_id: &str, source: &SessionSource) {
        let mut previous = self.sessions.remove(session_id);
        let state = source.state;
        let mut history_ops = Vec::new();

        // History: re-projected only when its sources changed.
        let snapshot = source.history_snapshot();
        let old_history = previous
            .as_mut()
            .map(|previous| std::mem::take(&mut previous.history));
        let history = match old_history {
            Some(history)
                if history.snapshot.as_ref().map(Weak::as_ptr) == snapshot.map(Arc::as_ptr)
                    && history.revision == state.history_revision
                    && history.outcomes == state.turn_outcomes =>
            {
                history
            }
            old => {
                #[cfg(test)]
                {
                    self.history_projections += 1;
                }
                let (history, timeline) = PublishedHistory::project(source, snapshot);
                let old = old.unwrap_or_default();
                diff_timeline(
                    &old.timeline,
                    &history.timeline,
                    &timeline,
                    &mut history_ops,
                );
                diff::diff(
                    "/history/collapsedTurns",
                    &old.collapsed_turns,
                    &history.collapsed_turns,
                    &|_| Text::Grow,
                    &mut history_ops,
                );
                diff::diff(
                    "/outcomes",
                    &old.projected_outcomes,
                    &history.projected_outcomes,
                    &|_| Text::Grow,
                    &mut history_ops,
                );
                history
            }
        };

        // Active turn: prompt by identity, updates element by element. Both
        // stay out of the body, so an unchanged turn copies no content.
        let mut previous_turn = previous.as_mut().and_then(|previous| previous.turn.take());
        let had_turn = previous_turn.is_some();
        let continued = state
            .active_turn
            .as_ref()
            .zip(previous_turn.as_ref())
            .is_some_and(|(active, turn)| active.operation_id == turn.operation_id);
        let mut turn_ops = Vec::new();
        let mut head = None;
        let turn = state.active_turn.as_ref().map(|active| {
            let mut same = previous_turn.take().filter(|_| continued);
            let prompt_kept = same
                .as_ref()
                .is_some_and(|turn| Weak::as_ptr(&turn.prompt) == Arc::as_ptr(&active.prompt));
            let (projected_prompt, prompt_refs) = match same.as_mut().filter(|_| prompt_kept) {
                Some(turn) => (
                    std::mem::take(&mut turn.projected_prompt),
                    std::mem::take(&mut turn.prompt_refs),
                ),
                None => {
                    let mut refs = HashSet::new();
                    for block in active.prompt.iter() {
                        terminal_references(block, &mut refs);
                    }
                    let projected = source.prompt(&active.prompt);
                    // A new turn's body has an empty prompt; a replaced one
                    // still holds the previous turn's.
                    if had_turn || !projected.is_empty() {
                        turn_ops.push(Op::Set {
                            path: "/turn/prompt".into(),
                            value: Value::Array(projected.clone()),
                        });
                    }
                    (projected, refs)
                }
            };
            let updates = &active.updates;
            let update_refs = updates
                .iter()
                .enumerate()
                .map(|(index, update)| {
                    same.as_ref()
                        .and_then(|turn| {
                            turn.updates
                                .get(index)
                                .filter(|old| Arc::ptr_eq(old, update))
                                .map(|_| turn.update_refs[index].clone())
                        })
                        .unwrap_or_else(|| {
                            let mut refs = HashSet::new();
                            terminal_references(update, &mut refs);
                            refs.into_iter().collect()
                        })
                })
                .collect::<Vec<_>>();
            match same.as_ref() {
                Some(turn) if Arc::ptr_eq(&turn.updates, updates) => {}
                Some(turn) => diff_updates(source, &turn.updates, updates, &mut turn_ops),
                None => {
                    let projected = updates
                        .iter()
                        .map(|update| source.update(update))
                        .collect::<Vec<_>>();
                    if had_turn {
                        // The body diff keeps the previous turn's updates.
                        turn_ops.push(Op::Set {
                            path: "/turn/updates".into(),
                            value: Value::Array(projected),
                        });
                    } else if !projected.is_empty() {
                        turn_ops.push(Op::Append {
                            path: "/turn/updates".into(),
                            tail: AppendTail::Values { values: projected },
                        });
                    }
                }
            }
            head = source.turn_head_with(Vec::new());
            PublishedTurn {
                operation_id: active.operation_id.clone(),
                prompt: Arc::downgrade(&active.prompt),
                projected_prompt,
                prompt_refs,
                updates: updates.clone(),
                update_refs,
            }
        });
        // An ended turn releases its process content (S6): earlier entries
        // carried it, so the log drops them and lagging replicas reset.
        let released_turn = had_turn && !continued;

        let mut refs = history.section.terminal_refs.clone();
        if let Some(turn) = &turn {
            refs.extend(turn.prompt_refs.iter().cloned());
            refs.extend(turn.update_refs.iter().flatten().cloned());
        }
        let body = serde_json::to_value(source.assemble(
            HistorySection::default(),
            head.map(|head| head.with_updates(Vec::new())),
            refs,
        ))
        .unwrap_or(Value::Null);

        // The body first: it creates the containers the identity ops patch.
        let (prev, mut ops) = match &previous {
            Some(previous) => {
                let mut ops = Vec::new();
                diff::diff("", &previous.body, &body, &session_text, &mut ops);
                (previous.seq, ops)
            }
            None => (0, create(&body)),
        };
        ops.append(&mut history_ops);
        ops.append(&mut turn_ops);
        let seq = if ops.is_empty() {
            prev
        } else {
            self.log
                .append(&session_scope(session_id), prev, Change::Ops(ops.into()))
        };
        if released_turn {
            self.log.compact_through(prev);
        }
        self.sessions.insert(
            session_id.to_string(),
            PublishedSession {
                incarnation: state.incarnation,
                seq,
                body,
                history,
                turn,
            },
        );
    }
}

impl PublishedHistory {
    /// The published history and the projected timeline it digests.
    fn project(
        source: &SessionSource,
        snapshot: Option<&Arc<HistorySnapshot>>,
    ) -> (Self, Vec<Value>) {
        let mut section = source.history();
        let timeline = std::mem::take(&mut section.timeline);
        let history = Self {
            snapshot: snapshot.map(Arc::downgrade),
            revision: source.state.history_revision.clone(),
            outcomes: source.state.turn_outcomes.clone(),
            timeline: timeline.iter().map(digest).collect(),
            collapsed_turns: json!(std::mem::take(&mut section.collapsed_turns)),
            projected_outcomes: json!(std::mem::take(&mut section.outcomes)),
            section,
        };
        (history, timeline)
    }
}

impl Default for PublishedHistory {
    /// The history of a document that does not exist yet: the body creates
    /// it with an empty timeline, no collapsed turns and no outcomes.
    fn default() -> Self {
        Self {
            snapshot: None,
            revision: None,
            outcomes: Vec::new(),
            timeline: Vec::new(),
            collapsed_turns: json!([]),
            projected_outcomes: json!({}),
            section: HistorySection::default(),
        }
    }
}

/// A new document: one `set` per top-level field of an empty root.
fn create(document: &Value) -> Vec<Op> {
    let mut ops = Vec::new();
    diff::diff("", &json!({}), document, &|_| Text::Grow, &mut ops);
    ops
}

/// Terminal output is a bounded tail; every other string grows or is set.
fn session_text(path: &str) -> Text {
    match path.strip_prefix("/terminals/") {
        Some(rest) if rest.split('/').count() == 2 && rest.ends_with("/output") => Text::Tail,
        _ => Text::Grow,
    }
}

fn digest(value: &Value) -> [u8; 16] {
    let encoded = serde_json::to_vec(value).unwrap_or_default();
    let hash = Sha256::digest(encoded);
    let mut digest = [0; 16];
    digest.copy_from_slice(&hash[..16]);
    digest
}

/// A grown timeline sets its changed indices (a background `tool_call_update`
/// folding into an earlier slot) and appends its tail; anything else, or a
/// mostly changed timeline, is one `set` of the field.
fn diff_timeline(old: &[[u8; 16]], digests: &[[u8; 16]], timeline: &[Value], ops: &mut Vec<Op>) {
    if old == digests {
        return;
    }
    let changed = old
        .iter()
        .zip(digests)
        .enumerate()
        .filter(|(_, (old, new))| old != new)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if old.is_empty() || digests.len() < old.len() || changed.len() * 2 > old.len() {
        ops.push(Op::Set {
            path: "/history/timeline".into(),
            value: Value::Array(timeline.to_vec()),
        });
        return;
    }
    for index in changed {
        ops.push(Op::Set {
            path: format!("/history/timeline/{index}"),
            value: timeline[index].clone(),
        });
    }
    if digests.len() > old.len() {
        ops.push(Op::Append {
            path: "/history/timeline".into(),
            tail: AppendTail::Values {
                values: timeline[old.len()..].to_vec(),
            },
        });
    }
}

/// Pointer comparison finds the folded and appended updates; only those are
/// projected and diffed.
fn diff_updates(
    source: &SessionSource,
    old: &SharedTurnUpdates,
    new: &SharedTurnUpdates,
    ops: &mut Vec<Op>,
) {
    if new.len() < old.len() {
        ops.push(Op::Set {
            path: "/turn/updates".into(),
            value: Value::Array(new.iter().map(|update| source.update(update)).collect()),
        });
        return;
    }
    for (index, (old, new)) in old.iter().zip(new.iter()).enumerate() {
        if !Arc::ptr_eq(old, new) {
            diff::diff(
                &format!("/turn/updates/{index}"),
                &source.update(old),
                &source.update(new),
                &|_| Text::Grow,
                ops,
            );
        }
    }
    if new.len() > old.len() {
        ops.push(Op::Append {
            path: "/turn/updates".into(),
            tail: AppendTail::Values {
                values: new[old.len()..]
                    .iter()
                    .map(|update| source.update(update))
                    .collect(),
            },
        });
    }
}
