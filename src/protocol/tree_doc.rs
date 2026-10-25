//! The revision 4 tree as the bridge's state projects it (docs/protocol-r4-design.md §4.2).
//!
//! A projection of the registry into the `Spec` the tree is committed from, keeping unchanged
//! entry values and the attachment blocks their references name. The operation is split into
//! its head and its detail (§4.4).
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;
#[cfg(test)]
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::documents::{
    AgentInfo, ErrorCode, Operation, OperationKind, OperationStatus, ProtocolError, Scope,
};
use super::hash::TextRule;
use super::project;
use super::timeline::{Entry, Prompt, Role, Timeline};
use super::timeline_doc::{AttachmentIndex, publish_entry_referencing};
use super::tree::Spec;
use crate::session_registry::SessionRegistry;

/// The operation without its input, result and the body of its error (§4.4).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OpHead {
    pub id: String,
    pub scope: String,
    pub after: Option<String>,
    pub kind: OperationKind,
    pub status: OperationStatus,
    pub cancel_requested: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorSummary>,
}

/// What a writer needs to know about an error: whether it failed and how.
#[cfg_attr(test, derive(Deserialize))]
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ErrorSummary {
    pub code: ErrorCode,
    /// For `agent_error`: the ACP error's integer `code`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_code: Option<i64>,
}

impl ErrorSummary {
    pub(crate) fn of(error: &ProtocolError) -> Self {
        Self {
            code: error.code,
            agent_code: if error.code == ErrorCode::AgentError {
                error
                    .data
                    .as_ref()
                    .and_then(|data| data.get("code"))
                    .and_then(Value::as_i64)
            } else {
                None
            },
        }
    }
}

/// The head of `operation` and its detail `{ input, result?, error? }` (§4.4).
pub(crate) fn split_operation(operation: &Operation) -> (OpHead, Value) {
    let head = OpHead {
        id: operation.id.clone(),
        scope: operation.scope.clone(),
        after: operation.after.clone(),
        kind: operation.kind,
        status: operation.status,
        cancel_requested: operation.cancel_requested,
        error: operation.error.as_ref().map(ErrorSummary::of),
    };
    let mut detail = Map::new();
    detail.insert("input".into(), operation.input.clone());
    if let Some(result) = &operation.result {
        detail.insert("result".into(), result.clone());
    }
    if let Some(error) = &operation.error {
        detail.insert("error".into(), json!(error));
    }
    (head, Value::Object(detail))
}

fn value(value: impl Serialize) -> Spec {
    Spec::value(serde_json::to_value(value).expect("a node serializes"))
}

/// A map node with one value node per entry.
fn values<T: Serialize>(entries: &BTreeMap<String, T>) -> Spec {
    Spec::map(entries.iter().map(|(key, entry)| (key.clone(), value(entry))))
}

/// A map of terminals: their `output` is a text position (§4.6).
fn terminals<T: Serialize>(entries: &BTreeMap<String, T>) -> Spec {
    Spec::map(entries.iter().map(|(key, entry)| {
        let terminal = serde_json::to_value(entry).expect("a terminal serializes");
        (key.clone(), Spec::text_value(Arc::new(terminal), TextRule::Terminal))
    }))
}

fn fields(members: Value) -> Map<String, Value> {
    match members {
        Value::Object(members) => members,
        _ => unreachable!("fields are an object"),
    }
}

/// The nodes every top-level object has (§4.2): `op`, `opDetail`, `interactions`, `urlFlows`.
fn common(
    op: &Operation,
    interactions: &BTreeMap<String, super::documents::Interaction>,
    url_flows: &BTreeMap<String, super::documents::UrlFlow>,
) -> Vec<(&'static str, Spec)> {
    let (head, detail) = split_operation(op);
    vec![
        ("op", value(head)),
        ("opDetail", Spec::value(detail)),
        ("interactions", values(interactions)),
        ("urlFlows", values(url_flows)),
    ]
}

/// `/global`.
pub(crate) fn global(registry: &SessionRegistry, agent: &AgentInfo) -> Spec {
    let live = project::project_global(registry, agent);
    let mut children = common(&live.op, &live.interactions, &live.url_flows);
    children.push(("sessions", values(&live.sessions)));
    children.push(("terminals", terminals(&live.terminals)));
    Spec::object(fields(json!({ "agent": live.agent })), children)
}

/// `/catalog`.
pub(crate) fn catalog(registry: &SessionRegistry, revision: u64) -> Spec {
    let live = project::project_catalog(registry, revision);
    let mut children = common(&live.op, &live.interactions, &live.url_flows);
    children.push((
        "pages",
        Spec::List(live.pages.iter().map(|page| Spec::value(page.clone())).collect()),
    ));
    Spec::object(fields(json!({ "revision": live.revision })), children)
}

/// `/sessions/<id>` without its history: identity, controls, operation, interactions and
/// terminals. `None` when the bridge does not hold the session.
pub(crate) fn session(registry: &SessionRegistry, session_id: &str) -> Option<Spec> {
    let live = project::project_session_live(registry, session_id)?;
    let mut children = common(&live.op, &live.interactions, &live.url_flows);
    children.push(("controls", value(&live.controls)));
    children.push(("terminals", terminals(&live.terminals)));
    Some(Spec::object(
        fields(json!({
            "sessionId": live.session_id,
            "cwd": live.cwd,
            "closing": live.closing,
        })),
        children,
    ))
}

/// A turn of the flat entries (§4.3).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TurnHead {
    /// The session's `historyRev`: which history `start` indexes.
    pub history_rev: u64,
    pub start: usize,
    pub length: usize,
    /// The opening user message's `messageId`, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    pub state: TurnState,
    /// Index of the turn's last agent message entry, `null` when it has none.
    pub output: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TurnState {
    Running,
    Ended,
}

/// The turns of `timeline` (memory model §5) as the index publishes them. A turn is
/// `running` exactly while the prompt that started it has no `end`; anything else is `ended`.
pub(crate) fn turn_heads(timeline: &Timeline, history_rev: u64) -> Vec<TurnHead> {
    let entries = timeline.entries();
    timeline
        .turns()
        .into_iter()
        .map(|turn| {
            let message_id = match &*entries[turn.start] {
                Entry::Message(message) if message.role == Role::User => message.message_id.clone(),
                _ => None,
            };
            let running = turn
                .prompt
                .is_some_and(|prompt| timeline.prompts()[prompt].end.is_none());
            let output = (turn.start..turn.end).rev().find(|&index| {
                matches!(&*entries[index], Entry::Message(message) if message.role == Role::Agent)
            });
            TurnHead {
                history_rev,
                start: turn.start,
                length: turn.end - turn.start,
                message_id,
                state: if running { TurnState::Running } else { TurnState::Ended },
                output,
            }
        })
        .collect()
}

/// What the projection remembers between commits: a session's published entries, so that an
/// entry the memory did not touch keeps its value (and the tree compares it by pointer).
/// The record of an operation that occupied a target (design §9.3).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct OpRecord {
    pub id: String,
    /// This target's `op.id` it replaced; none for the first operation seen of the target.
    pub after: Option<String>,
    pub kind: OperationKind,
    /// The commit that admitted it.
    pub rev: u64,
    /// Updated until it ends.
    pub status: OperationStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorSummary>,
}

/// How many operations a target remembers (design §9.3).
pub(crate) const OP_LOG: usize = 64;

/// What is remembered of the operations of one target.
#[cfg_attr(test, derive(Clone))]
#[derive(Default)]
struct TargetLog {
    /// The `op.id` the target showed at the last commit; none before the first.
    last: Option<String>,
    records: std::collections::VecDeque<(OpRecord, Arc<Value>)>,
}

/// How many failed loads `/global/loadFailures` keeps (design §6.2).
pub(crate) const LOAD_FAILURES: usize = 64;

#[cfg_attr(test, derive(Clone))]
#[derive(Default)]
pub(crate) struct Projector {
    sessions: BTreeMap<String, Memo>,
    /// The last failed loads, oldest first, with the whole error. A stream's `end` carries only
    /// its summary; a client that shows the message reads it here.
    failures: std::collections::VecDeque<(String, ProtocolError)>,
    /// The operations each target showed, by target (`global`, `catalog`, `sessions/<id>`).
    logs: BTreeMap<String, TargetLog>,
}

/// What is remembered of one held session.
#[cfg_attr(test, derive(Clone))]
#[derive(Default)]
struct Memo {
    entries: Vec<PublishedEntry>,
    /// The blocks referenced by this session's published entries, counted per reference.
    attachments: AttachmentIndex,
    /// The revision at which the bridge started holding the session this time (§4.2).
    held_since: u64,
    /// The timeline the history was last installed as, and the revision it was.
    timeline: Option<String>,
    history_rev: u64,
    /// The turn heads and prompts as last published, so that those that did not change keep the
    /// value they were published as (and the tree compares them by pointer).
    turns: Vec<(TurnHead, Arc<Value>)>,
    prompts: Vec<(Prompt, Arc<Value>)>,
}

#[cfg_attr(test, derive(Clone))]
struct PublishedEntry {
    source: Arc<Entry>,
    value: Arc<Value>,
    attachments: Vec<String>,
}

/// `items` as value nodes, an item equal to the one at its index in `cache` keeping that one's
/// value; `cache` then holds `items`.
fn reuse<T: Clone + PartialEq + Serialize>(cache: &mut Vec<(T, Arc<Value>)>, items: &[T]) -> Vec<Spec> {
    cache.truncate(items.len());
    for (index, item) in items.iter().enumerate() {
        match cache.get(index) {
            Some((held, _)) if held == item => {}
            Some(_) => {
                cache[index] = (item.clone(), Arc::new(serde_json::to_value(item).expect("a node serializes")));
            }
            None => cache.push((item.clone(), Arc::new(serde_json::to_value(item).expect("a node serializes")))),
        }
    }
    cache.iter().map(|(_, value)| Spec::text_value(Arc::clone(value), TextRule::None)).collect()
}

impl Projector {
    /// The exact block referenced by a currently published entry of this held session.
    pub(crate) fn attachment(&self, session_id: &str, id: &str) -> Option<&Value> {
        self.sessions.get(session_id)?.attachments.get(id)
    }

    pub(crate) fn has_failure(&self, session_id: &str) -> bool {
        self.failures.iter().any(|(id, _)| id == session_id)
    }

    /// A load of `session_id` failed with `error`: kept until the session is held again or
    /// deleted, among the last `LOAD_FAILURES`.
    pub(crate) fn record_failure(&mut self, session_id: &str, error: ProtocolError) {
        self.forget_failure(session_id);
        self.failures.push_back((session_id.to_string(), error));
        while self.failures.len() > LOAD_FAILURES {
            self.failures.pop_front();
        }
    }

    /// The session is deleted: its failure is no longer kept.
    pub(crate) fn forget_failure(&mut self, session_id: &str) {
        self.failures.retain(|(id, _)| id != session_id);
    }

    /// A commit at `next_rev` shows `operation` as the `op` of `target`. The first operation seen
    /// of a target is the baseline; every change of `op.id` after it is an admission and is
    /// recorded with the id it replaced. The record of the operation shown follows its status.
    fn observe(&mut self, target: &str, operation: &Operation, next_rev: u64) {
        let log = self.logs.entry(target.to_string()).or_default();
        if let Some(last) = &log.last
            && *last != operation.id
        {
            debug_assert!(
                !log.records.iter().any(|(record, _)| {
                    record.id == operation.id
                        || record.after.as_deref() == Some(operation.id.as_str())
                }),
                "operation ID returned in {target}: {}",
                operation.id
            );
            let record = OpRecord {
                id: operation.id.clone(),
                after: Some(last.clone()),
                kind: operation.kind,
                rev: next_rev,
                status: operation.status,
                error: operation.error.as_ref().map(ErrorSummary::of),
            };
            let published = Arc::new(serde_json::to_value(&record).expect("an operation record serializes"));
            log.records.push_back((record, published));
            while log.records.len() > OP_LOG {
                log.records.pop_front();
            }
        }
        log.last = Some(operation.id.clone());
        if let Some((record, published)) = log.records.back_mut().filter(|(record, _)| record.id == operation.id) {
            let error = operation.error.as_ref().map(ErrorSummary::of);
            if record.status != operation.status || record.error != error {
                record.status = operation.status;
                record.error = error;
                *published = Arc::new(serde_json::to_value(&record).expect("an operation record serializes"));
            }
        }
    }

    /// `ops` of `target`.
    fn ops(&self, target: &str) -> Spec {
        Spec::List(
            self.logs
                .get(target)
                .into_iter()
                .flat_map(|log| log.records.iter())
                .map(|(_, published)| Spec::text_value(Arc::clone(published), TextRule::None))
                .collect(),
        )
    }

    /// `/global/loadFailures`.
    pub(crate) fn load_failures(&self) -> Spec {
        Spec::map(self.failures.iter().map(|(id, error)| (id.clone(), value(error))))
    }

    /// The complete tree the bridge now publishes, to commit as revision `next_rev`: a session
    /// held for the first time, or whose history was replaced, takes it for `heldSince` or
    /// `historyRev`. Each of those changes the tree, so `next_rev` is then the revision committed.
    pub(crate) fn project(
        &mut self,
        registry: &SessionRegistry,
        agent: &AgentInfo,
        catalog_revision: u64,
        next_rev: u64,
    ) -> BTreeMap<String, Spec> {
        let held = project::held_sessions(registry);
        self.sessions.retain(|id, _| held.contains(id));
        self.logs.retain(|target, _| match target.strip_prefix("sessions/") {
            Some(id) => held.iter().any(|held| held == id),
            None => true,
        });
        for (target, scope) in [("global", Scope::Global), ("catalog", Scope::Catalog)] {
            if let Some((operation, _)) = project::shown_operation(registry, &scope) {
                self.observe(target, &operation, next_rev);
            }
        }
        for id in &held {
            if let Some((operation, _)) = project::shown_operation(registry, &Scope::Session(id.clone())) {
                self.observe(&format!("sessions/{id}"), &operation, next_rev);
            }
        }
        // A session held again has no failure to explain.
        self.failures.retain(|(id, _)| !held.contains(id));
        let sessions: Vec<(String, Spec)> = held
            .into_iter()
            .filter_map(|id| {
                let spec = self.session_with_history(registry, &id, next_rev)?;
                Some((id, spec))
            })
            .collect();
        let mut global = global(registry, agent);
        if let Spec::Object { children, .. } = &mut global {
            children.insert("loadFailures".into(), self.load_failures());
            children.insert("ops".into(), self.ops("global"));
        }
        let mut catalog = catalog(registry, catalog_revision);
        if let Spec::Object { children, .. } = &mut catalog {
            children.insert("ops".into(), self.ops("catalog"));
        }
        BTreeMap::from([
            ("global".to_string(), global),
            ("catalog".to_string(), catalog),
            ("sessions".to_string(), Spec::map(sessions)),
        ])
    }

    /// `/sessions/<id>` with `history`, `entries` and `prompts` (§4.2). While the history is
    /// unknown there is no `history` field and no `entries` node.
    fn session_with_history(
        &mut self,
        registry: &SessionRegistry,
        session_id: &str,
        next_rev: u64,
    ) -> Option<Spec> {
        let Spec::Object { mut fields, mut children } = session(registry, session_id)? else {
            unreachable!("a session is an object");
        };
        children.insert("ops".into(), self.ops(&format!("sessions/{session_id}")));
        let state = &registry.sessions.get(session_id)?.state;
        let memo = self.sessions.entry(session_id.to_string()).or_insert_with(|| Memo {
            held_since: next_rev,
            ..Memo::default()
        });
        fields.insert("heldSince".into(), json!(memo.held_since));
        if memo.timeline != state.timeline_epoch {
            memo.history_rev = next_rev;
            memo.timeline = state.timeline_epoch.clone();
        }
        let cache = &mut memo.entries;
        match state.timeline.as_ref() {
            Some(timeline) => {
                fields.insert("historyRev".into(), json!(memo.history_rev));
                if let Some(history) = state.history {
                    fields.insert("history".into(), json!(history));
                }
                let source = timeline.entries();
                // Entries only change in place or grow; a shorter list is another history.
                for removed in cache.drain(source.len().min(cache.len())..) {
                    memo.attachments.release(&removed.attachments);
                }
                for (position, entry) in source.iter().enumerate() {
                    match cache.get(position) {
                        Some(cached) if Arc::ptr_eq(&cached.source, entry) => {}
                        _ => {
                            let (value, attachments) = publish_entry_referencing(entry, &mut memo.attachments);
                            let published = PublishedEntry { source: Arc::clone(entry), value: Arc::new(value), attachments };
                            if position < cache.len() {
                                // Publish first: a block both versions reference stays retained.
                                memo.attachments.release(&cache[position].attachments);
                                cache[position] = published;
                            } else {
                                cache.push(published);
                            }
                        }
                    }
                }
                children.insert(
                    "entries".into(),
                    Spec::List(cache.iter().map(|entry| Spec::text_value(Arc::clone(&entry.value), TextRule::Entry)).collect()),
                );
                children.insert(
                    "turns".into(),
                    Spec::List(reuse(&mut memo.turns, &turn_heads(timeline, memo.history_rev))),
                );
                children.insert("prompts".into(), Spec::List(reuse(&mut memo.prompts, timeline.prompts())));
            }
            None => {
                cache.clear();
                memo.attachments = AttachmentIndex::default();
                memo.turns.clear();
                memo.prompts.clear();
                children.insert("prompts".into(), Spec::List(Vec::new()));
            }
        }
        Some(Spec::Object { fields, children })
    }
}

/// `Projector::project` with nothing remembered.
#[cfg(test)]
pub(crate) fn project_tree(
    registry: &SessionRegistry,
    agent: &AgentInfo,
    catalog_revision: u64,
) -> BTreeMap<String, Spec> {
    Projector::default().project(registry, agent, catalog_revision, 1)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod perf_tests;
