//! The commit step (docs/state-sync.md §2.3): the only writer of published
//! documents and events (I1).
//!
//! Each commit compares the projected documents with the published ones and
//! emits one event per changed scope. A log change and the same scope's live
//! patch are one event (S2). The Hub (migration step 5) delivers the events.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::Value;

use super::canon::{Attachments, ChainHash, LogEntry};
use super::diff::{self, Text};
use super::documents::{AgentInfo, End, Event, LiveChange, LogChange, Scope};
use super::project::{
    SessionDocument, held_sessions, project_catalog, project_global, project_session,
    project_update,
};
use super::registry::SessionLog;
use super::{AppendTail, Op};
use crate::runtime_state::SharedTurnUpdates;
use crate::session_registry::SessionRegistry;

/// A published document, as a one-off read returns it (protocol §8).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Document {
    pub live: Value,
    /// `None` while the session's log is unknown.
    pub log: Option<Vec<Arc<LogEntry>>>,
}

#[derive(Clone, Default)]
pub(crate) struct Publisher {
    sessions: BTreeMap<String, PublishedSession>,
    global: Option<Value>,
    catalog: Option<Value>,
}

#[derive(Clone)]
struct PublishedSession {
    log: SessionLog,
    /// `live` without `op.process`.
    live: Value,
    input_attachments: Attachments,
    process: Option<Process>,
}

/// A published `op.process`: each update of the running prompt as projected,
/// with the source it was projected from. The turn's updates are shared and an
/// update folded in place is a new value, so pointer identity tells which
/// updates changed (the identity diff); an unchanged update keeps its
/// projection.
#[derive(Clone)]
struct Process {
    op_id: String,
    source: SharedTurnUpdates,
    items: Vec<Arc<ProcessItem>>,
}

/// One projected update and the blocks its references name.
struct ProcessItem {
    value: Value,
    attachments: Attachments,
}

impl Process {
    fn project(op_id: &str, source: SharedTurnUpdates, previous: Option<&Process>) -> Self {
        let previous = previous.filter(|previous| previous.op_id == op_id);
        let items = source
            .iter()
            .enumerate()
            .map(|(index, update)| {
                if let Some(previous) = previous
                    && let Some(old) = previous.source.get(index)
                    && Arc::ptr_eq(old, update)
                {
                    return Arc::clone(&previous.items[index]);
                }
                let mut attachments = Attachments::default();
                let value = project_update(update, &mut attachments);
                Arc::new(ProcessItem { value, attachments })
            })
            .collect();
        Self {
            op_id: op_id.to_string(),
            source,
            items,
        }
    }

    fn values(&self) -> Value {
        Value::Array(self.items.iter().map(|item| item.value.clone()).collect())
    }

    /// The ops that turn `old`, of the same operation, into `self`.
    fn ops(&self, old: &Process, ops: &mut Vec<Op>) {
        if Arc::ptr_eq(&old.source, &self.source) {
            return;
        }
        if self.items.len() < old.items.len() {
            ops.push(Op::Set {
                path: PROCESS.to_string(),
                value: self.values(),
            });
            return;
        }
        // A changed update is patched in place: streamed text appends.
        for (index, (new, previous)) in self.items.iter().zip(&old.items).enumerate() {
            if !Arc::ptr_eq(new, previous) {
                diff::diff(
                    &diff::child(PROCESS, &index.to_string()),
                    &previous.value,
                    &new.value,
                    &text_mode,
                    ops,
                );
            }
        }
        if self.items.len() > old.items.len() {
            ops.push(Op::Append {
                path: PROCESS.to_string(),
                tail: AppendTail::Values {
                    values: self.items[old.items.len()..]
                        .iter()
                        .map(|item| item.value.clone())
                        .collect(),
                },
            });
        }
    }
}

const PROCESS: &str = "/op/process";

/// Terminal output is a bounded tail; every other string grows or is set.
fn text_mode(path: &str) -> Text {
    if path.starts_with("/terminals/") && path.ends_with("/output") {
        Text::Tail
    } else {
        Text::Grow
    }
}

fn wire(scope: &Scope) -> String {
    scope.to_string()
}

fn entries(log: &SessionLog) -> Option<&[Arc<LogEntry>]> {
    match log {
        SessionLog::Unknown => None,
        SessionLog::Known(log) => Some(log.entries()),
    }
}

/// The hash a log holds after `index` entries.
fn hash_at(entries: &[Arc<LogEntry>], index: usize) -> String {
    match index {
        0 => ChainHash::ZERO.wire(),
        index => entries[index - 1].hash.clone(),
    }
}

/// The whole log, from `0`.
fn whole(entries: &[Arc<LogEntry>]) -> LogChange {
    LogChange {
        from: 0,
        prev: ChainHash::ZERO.wire(),
        entries: entries.to_vec(),
    }
}

/// The change from the published log to the new one: an append, or a rebuild
/// from the longest common prefix of the two chains (§2.3).
fn log_change(old: &SessionLog, new: &SessionLog) -> Option<LogChange> {
    match (old, new) {
        (_, SessionLog::Unknown) => None,
        (SessionLog::Unknown, SessionLog::Known(new)) => Some(whole(new.entries())),
        (SessionLog::Known(old), SessionLog::Known(new)) if Arc::ptr_eq(old, new) => None,
        (SessionLog::Known(old), SessionLog::Known(new)) => {
            let (old, new) = (old.entries(), new.entries());
            let common = old
                .iter()
                .zip(new)
                .take_while(|(old, new)| old.hash == new.hash)
                .count();
            (common < old.len() || common < new.len()).then(|| LogChange {
                from: common,
                prev: hash_at(new, common),
                entries: new[common..].to_vec(),
            })
        }
    }
}

fn change(log: Option<LogChange>, ops: Vec<Op>, scope: String) -> Option<Event> {
    let live = (!ops.is_empty()).then_some(LiveChange::Patch { patch: ops });
    match (log, live) {
        (Some(log), live) => Some(Event::Log { scope, log, live }),
        (None, Some(live)) => Some(Event::Live { scope, live }),
        (None, None) => None,
    }
}

impl PublishedSession {
    fn new(document: SessionDocument) -> (Self, Value) {
        let process = document
            .process
            .map(|source| Process::project(&document.live.op.id, source, None));
        let published = Self {
            log: document.log,
            live: serde_json::to_value(&document.live).expect("documents serialize"),
            input_attachments: document.input_attachments,
            process,
        };
        let live = published.full_live();
        (published, live)
    }

    fn full_live(&self) -> Value {
        let mut live = self.live.clone();
        if let Some(process) = &self.process {
            live["op"]["process"] = process.values();
        }
        live
    }

    /// Publishes `document` and returns the change, if any.
    fn update(&mut self, document: SessionDocument, scope: String) -> Option<Event> {
        let log = log_change(&self.log, &document.log);
        let process = document
            .process
            .map(|source| Process::project(&document.live.op.id, source, self.process.as_ref()));
        let live = serde_json::to_value(&document.live).expect("documents serialize");
        let mut ops = Vec::new();
        if self.process.is_some() && process.is_none() {
            ops.push(Op::Remove {
                path: PROCESS.to_string(),
            });
        }
        diff::diff("", &self.live, &live, &text_mode, &mut ops);
        match (&self.process, &process) {
            (Some(old), Some(new)) if old.op_id == new.op_id => new.ops(old, &mut ops),
            (_, Some(new)) => ops.push(Op::Set {
                path: PROCESS.to_string(),
                value: new.values(),
            }),
            (_, None) => {}
        }
        self.log = document.log;
        self.live = live;
        self.input_attachments = document.input_attachments;
        self.process = process;
        change(log, ops, scope)
    }

    fn attachment(&self, attachment_id: &str) -> Option<&Value> {
        let log = match &self.log {
            SessionLog::Known(log) => log.attachments().get(attachment_id),
            SessionLog::Unknown => None,
        };
        log.or_else(|| self.input_attachments.get(attachment_id))
            .or_else(|| {
                self.process.as_ref().and_then(|process| {
                    process
                        .items
                        .iter()
                        .find_map(|item| item.attachments.get(attachment_id))
                })
            })
    }
}

/// A scope whose log is always empty: `global` and `catalog`.
fn publish_unlogged(published: &mut Option<Value>, live: Value, scope: &Scope) -> Option<Event> {
    let scope = wire(scope);
    match published {
        None => {
            *published = Some(live.clone());
            Some(Event::Log {
                scope,
                log: whole(&[]),
                live: Some(LiveChange::Replace { replace: live }),
            })
        }
        Some(old) => {
            let mut ops = Vec::new();
            diff::diff("", old, &live, &text_mode, &mut ops);
            *old = live;
            change(None, ops, scope)
        }
    }
}

impl Publisher {
    /// Publishes every change since the last commit.
    pub(crate) fn commit(
        &mut self,
        registry: &mut SessionRegistry,
        agent: &AgentInfo,
        catalog_revision: u64,
    ) -> Vec<Event> {
        let ended = std::mem::take(&mut registry.scopes.ended);
        self.publish(registry, &ended, agent, catalog_revision)
    }

    /// Whether a commit now would publish anything: a transition that changed
    /// what clients see without committing it (the gate's check, §4.1).
    pub(crate) fn diverges(
        &self,
        registry: &SessionRegistry,
        agent: &AgentInfo,
        catalog_revision: u64,
    ) -> bool {
        !self
            .clone()
            .publish(registry, &registry.scopes.ended, agent, catalog_revision)
            .is_empty()
    }

    fn publish(
        &mut self,
        registry: &SessionRegistry,
        ended: &[(String, End)],
        agent: &AgentInfo,
        catalog_revision: u64,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        let held: BTreeSet<String> = held_sessions(registry).into_iter().collect();
        let gone: Vec<String> = self
            .sessions
            .keys()
            .filter(|session_id| !held.contains(*session_id))
            .cloned()
            .collect();
        for session_id in gone {
            self.sessions.remove(&session_id);
            // A released session ends nothing: a client keeps its copy and
            // subscribes again (S6).
            if let Some((_, end)) = ended.iter().rev().find(|(ended, _)| *ended == session_id) {
                events.push(Event::End {
                    scope: wire(&Scope::Session(session_id)),
                    end: end.clone(),
                });
            }
        }
        for session_id in held {
            let Some(document) = project_session(registry, &session_id) else {
                continue;
            };
            let scope = wire(&Scope::Session(session_id.clone()));
            match self.sessions.get_mut(&session_id) {
                Some(published) => events.extend(published.update(document, scope)),
                None => {
                    let (published, live) = PublishedSession::new(document);
                    let live = LiveChange::Replace { replace: live };
                    events.push(match entries(&published.log) {
                        Some(entries) => Event::Log {
                            scope,
                            log: whole(entries),
                            live: Some(live),
                        },
                        None => Event::Live { scope, live },
                    });
                    self.sessions.insert(session_id, published);
                }
            }
        }
        let global = serde_json::to_value(project_global(registry, agent)).expect("serializes");
        events.extend(publish_unlogged(&mut self.global, global, &Scope::Global));
        let catalog =
            serde_json::to_value(project_catalog(registry, catalog_revision)).expect("serializes");
        events.extend(publish_unlogged(
            &mut self.catalog,
            catalog,
            &Scope::Catalog,
        ));
        events
    }

    /// The published document of `scope`.
    pub(crate) fn document(&self, scope: &str) -> Option<Document> {
        match Scope::parse(scope)? {
            Scope::Global => self.global.clone().map(|live| Document {
                live,
                log: Some(Vec::new()),
            }),
            Scope::Catalog => self.catalog.clone().map(|live| Document {
                live,
                log: Some(Vec::new()),
            }),
            Scope::Session(session_id) => {
                let published = self.sessions.get(&session_id)?;
                Some(Document {
                    live: published.full_live(),
                    log: entries(&published.log).map(<[_]>::to_vec),
                })
            }
        }
    }

    /// The log of each published session, by session ID: `None` while it is
    /// unknown.
    pub(crate) fn session_logs(&self) -> impl Iterator<Item = (&str, Option<&[Arc<LogEntry>]>)> {
        self.sessions
            .iter()
            .map(|(session_id, published)| (session_id.as_str(), entries(&published.log)))
    }

    /// The published scopes.
    pub(crate) fn scopes(&self) -> Vec<String> {
        let mut scopes = Vec::new();
        if self.global.is_some() {
            scopes.push(wire(&Scope::Global));
        }
        if self.catalog.is_some() {
            scopes.push(wire(&Scope::Catalog));
        }
        scopes.extend(
            self.sessions
                .keys()
                .map(|session_id| wire(&Scope::Session(session_id.clone()))),
        );
        scopes
    }

    /// An attachment of a held session, while its published `log` or `live`
    /// references it (protocol §8).
    pub(crate) fn attachment(&self, session_id: &str, attachment_id: &str) -> Option<Value> {
        self.sessions
            .get(session_id)?
            .attachment(attachment_id)
            .cloned()
    }
}
