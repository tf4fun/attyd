//! The revision 4 tree as the bridge's state projects it (docs/protocol-r4-design.md §4.2).
//!
//! A pure projection of the registry into the `Spec` the tree is committed from, built from
//! the same projections as the revision 3 documents (`project`), so the two never disagree
//! about what the bridge holds. The operation is split into its head and its detail (§4.4).
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::documents::{
    AgentInfo, ErrorCode, Operation, OperationKind, OperationStatus, ProtocolError,
};
use super::project;
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

/// The complete tree the bridge now publishes, to commit.
pub(crate) fn project_tree(
    registry: &SessionRegistry,
    agent: &AgentInfo,
    catalog_revision: u64,
) -> BTreeMap<String, Spec> {
    BTreeMap::from([
        ("global".to_string(), global(registry, agent)),
        ("catalog".to_string(), catalog(registry, catalog_revision)),
        (
            "sessions".to_string(),
            Spec::map(
                project::held_sessions(registry)
                    .into_iter()
                    .filter_map(|id| session(registry, &id).map(|spec| (id, spec))),
            ),
        ),
    ])
}

#[cfg(test)]
mod tests;
