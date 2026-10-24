//! The revision 4 tree as the bridge's state projects it (docs/protocol-r4-design.md §4.2).
//!
//! A pure projection of the registry into the `Spec` the tree is committed from, built from
//! the same projections as the revision 3 documents (`project`), so the two never disagree
//! about what the bridge holds. The operation is split into its head and its detail (§4.4).
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::documents::{
    AgentInfo, ErrorCode, Operation, OperationKind, OperationStatus, ProtocolError,
};
use super::project;
use super::timeline::{Entry, Role, Timeline};
use super::timeline_doc::{AttachmentIndex, publish_entry};
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
    children.push(("terminals", values(&live.terminals)));
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
    children.push(("terminals", values(&live.terminals)));
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
#[derive(Default)]
pub(crate) struct Projector {
    sessions: BTreeMap<String, Memo>,
}

/// What is remembered of one held session.
#[derive(Default)]
struct Memo {
    entries: Vec<(Arc<Entry>, Arc<Value>)>,
    /// The revision at which the bridge started holding the session this time (§4.2).
    held_since: u64,
    /// The timeline the history was last installed as, and the revision it was.
    timeline: Option<String>,
    history_rev: u64,
}

impl Projector {
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
        let sessions: Vec<(String, Spec)> = held
            .into_iter()
            .filter_map(|id| {
                let spec = self.session_with_history(registry, &id, next_rev)?;
                Some((id, spec))
            })
            .collect();
        BTreeMap::from([
            ("global".to_string(), global(registry, agent)),
            ("catalog".to_string(), catalog(registry, catalog_revision)),
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
                cache.truncate(source.len());
                let mut index = AttachmentIndex::default();
                for (position, entry) in source.iter().enumerate() {
                    match cache.get(position) {
                        Some((cached, _)) if Arc::ptr_eq(cached, entry) => {}
                        _ => {
                            let published = Arc::new(publish_entry(entry, &mut index));
                            if position < cache.len() {
                                cache[position] = (Arc::clone(entry), published);
                            } else {
                                cache.push((Arc::clone(entry), published));
                            }
                        }
                    }
                }
                children.insert(
                    "entries".into(),
                    Spec::List(cache.iter().map(|(_, value)| Spec::Value(Arc::clone(value))).collect()),
                );
                children.insert(
                    "turns".into(),
                    Spec::List(turn_heads(timeline, memo.history_rev).iter().map(value).collect()),
                );
                children.insert(
                    "prompts".into(),
                    Spec::List(timeline.prompts().iter().map(value).collect()),
                );
            }
            None => {
                cache.clear();
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
