//! Pure projections of bridge state into the published documents
//! (docs/state-sync.md §2.2). They are the only definition of what clients see.
use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use super::canon::{Attachments, Block, normalize};
use super::documents::{
    AgentInfo, CatalogLive, ExitStatus, GlobalLive, Interaction, Operation, OperationKind,
    OperationSummary, PendingCounts, Scope, SessionLive, SessionSummary, Terminal, UrlFlow,
};
use super::op::OpSlot;
use super::registry::SessionLog;
use crate::runtime_state::{
    PendingInteraction, SessionLifecycle, SessionLiveState, SharedTurnUpdates, UrlFlowStatus,
};
use crate::session_registry::SessionRegistry;

/// A held session's document. `live.op.process` is left out: the commit step
/// projects the running prompt's updates one by one (`project_update`), so an
/// update it already published is not projected again.
#[derive(Debug, Clone)]
pub(crate) struct SessionDocument {
    pub log: SessionLog,
    pub live: SessionLive,
    /// The running prompt's folded updates, when `op` is that prompt.
    pub process: Option<SharedTurnUpdates>,
    /// The blocks `op.input` references.
    pub input_attachments: Attachments,
}

fn held(live: &SessionLiveState) -> bool {
    live.lifecycle != SessionLifecycle::Closed
}

/// The IDs of the held sessions.
pub(crate) fn held_sessions(registry: &SessionRegistry) -> Vec<String> {
    registry
        .sessions
        .iter()
        .filter(|(_, entry)| entry.state.live.as_ref().is_some_and(held))
        .map(|(session_id, _)| session_id.clone())
        .collect()
}

fn held_live<'a>(registry: &'a SessionRegistry, session_id: &str) -> Option<&'a SessionLiveState> {
    registry
        .sessions
        .get(session_id)?
        .state
        .live
        .as_ref()
        .filter(|live| held(live))
}

fn interaction(pending: &PendingInteraction, permission: bool) -> (String, Interaction) {
    let interaction_id = pending.interaction_id.clone();
    let request = pending.request.clone();
    let interaction = if permission {
        Interaction::Permission {
            interaction_id: interaction_id.clone(),
            request,
        }
    } else {
        Interaction::Elicitation {
            interaction_id: interaction_id.clone(),
            request,
        }
    };
    (interaction_id, interaction)
}

/// A session's pending interactions that belong to `op`.
fn session_interactions(
    registry: &SessionRegistry,
    live: &SessionLiveState,
    op: &str,
    into: &mut BTreeMap<String, Interaction>,
) {
    let belongs = |pending: &&PendingInteraction| registry.belongs_to(&pending.interaction_id, op);
    into.extend(
        live.permissions
            .values()
            .filter(belongs)
            .map(|pending| interaction(pending, true)),
    );
    into.extend(
        live.elicitations
            .values()
            .filter(belongs)
            .map(|pending| interaction(pending, false)),
    );
}

/// The pending interactions of the operation `op` in `owner`'s slot: its own
/// scope's, those of the scopes it occupies, and the request-scoped ones that
/// joined it (protocol §6.2).
fn interactions(
    registry: &SessionRegistry,
    owner: &Scope,
    op: &str,
) -> BTreeMap<String, Interaction> {
    let mut interactions = BTreeMap::new();
    let joined =
        |pending: &&PendingInteraction| registry.scopes.joined.get(&pending.interaction_id);
    interactions.extend(
        registry
            .request_elicitations
            .values()
            .filter(|pending| joined(pending).is_some_and(|joined| joined.op == op))
            .map(|pending| interaction(pending, false)),
    );
    match owner {
        Scope::Global => interactions.extend(
            registry
                .request_elicitations
                .values()
                .filter(|pending| joined(pending).is_none())
                .map(|pending| interaction(pending, false)),
        ),
        Scope::Catalog => {}
        Scope::Session(session_id) => {
            if let Some(live) = held_live(registry, session_id) {
                session_interactions(registry, live, op, &mut interactions);
            }
        }
    }
    for (session_id, occupier) in &registry.scopes.occupied {
        if occupier == owner
            && let Some(live) = held_live(registry, session_id)
        {
            session_interactions(registry, live, op, &mut interactions);
        }
    }
    interactions
}

fn operation(slot: &OpSlot, interactions: BTreeMap<String, Interaction>) -> Operation {
    Operation {
        id: slot.id.clone(),
        kind: slot.kind,
        status: slot.status,
        input: slot.input.clone(),
        cancel_requested: slot.cancel_requested,
        // An operation's end clears its pending interactions (S7).
        interactions: if slot.is_running() {
            interactions
        } else {
            BTreeMap::new()
        },
        process: None,
        result: slot.result.clone(),
        error: slot.error.clone(),
    }
}

/// The scope whose slot `scope` shows.
fn owner(registry: &SessionRegistry, scope: &Scope) -> Scope {
    match scope {
        Scope::Session(session_id) => registry
            .scopes
            .occupied
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| scope.clone()),
        _ => scope.clone(),
    }
}

fn shown_operation(registry: &SessionRegistry, scope: &Scope) -> Option<(Operation, Scope)> {
    let owner = owner(registry, scope);
    let slot = registry.shown_slot(scope)?;
    Some((
        operation(slot, interactions(registry, &owner, &slot.id)),
        owner,
    ))
}

/// The accepted request-scoped URL flows of `scope`; a flow of no known scope
/// is global's.
fn request_flows<'a>(
    registry: &'a SessionRegistry,
    scope: &'a Scope,
) -> impl Iterator<Item = &'a crate::runtime_state::UrlFlow> {
    registry.request_url_flows.values().filter(move |flow| {
        registry
            .scopes
            .flow_scopes
            .get(&flow.registration_id)
            .unwrap_or(&Scope::Global)
            == scope
    })
}

fn url_flows<'a>(
    flows: impl IntoIterator<Item = &'a crate::runtime_state::UrlFlow>,
) -> BTreeMap<String, UrlFlow> {
    flows
        .into_iter()
        .filter(|flow| flow.status == UrlFlowStatus::Waiting)
        .map(|flow| {
            (
                flow.registration_id.clone(),
                UrlFlow {
                    interaction_id: flow.registration_id.clone(),
                    elicitation_id: flow.elicitation_id.clone(),
                    request: flow.request.clone(),
                },
            )
        })
        .collect()
}

fn terminal(terminal_id: &str, terminal: &Value) -> Terminal {
    let exit_status = terminal
        .get("exitStatus")
        .filter(|status| status.is_object())
        .map(|status| ExitStatus {
            exit_code: status.get("exitCode").and_then(Value::as_i64),
            signal: status
                .get("signal")
                .and_then(Value::as_str)
                .map(String::from),
        });
    Terminal {
        terminal_id: terminal_id.to_string(),
        output: terminal["output"].as_str().unwrap_or_default().to_string(),
        truncated: terminal["truncated"].as_bool().unwrap_or(false),
        exit_status,
    }
}

/// `session/<id>`, or `None` when the bridge does not hold the session.
pub(crate) fn project_session(
    registry: &SessionRegistry,
    session_id: &str,
) -> Option<SessionDocument> {
    let state = &registry.sessions.get(session_id)?.state;
    let live = held_live(registry, session_id)?;
    let scope = Scope::Session(session_id.to_string());
    let (op, owner) = shown_operation(registry, &scope)?;
    let slot = registry.shown_slot(&scope)?;
    let process = (owner == scope && slot.is_running_kind(OperationKind::Prompt)).then(|| {
        state
            .active_turn
            .as_ref()
            .filter(|turn| turn.execution.is_some())
            .map_or_else(|| Arc::new(Vec::new()), |turn| Arc::clone(&turn.updates))
    });
    Some(SessionDocument {
        log: state.log.clone(),
        live: SessionLive {
            op,
            url_flows: url_flows(
                live.url_flows
                    .values()
                    .chain(request_flows(registry, &scope)),
            ),
            session_id: session_id.to_string(),
            cwd: live.cwd.to_string_lossy().into_owned(),
            controls: live.controls.clone(),
            terminals: live
                .terminals
                .iter()
                .map(|(terminal_id, value)| (terminal_id.clone(), terminal(terminal_id, value)))
                .collect(),
        },
        process,
        input_attachments: slot.attachments.clone(),
    })
}

/// One process update: its image, audio and embedded resource blocks become
/// attachment references, kept in `attachments`.
pub(crate) fn project_update(update: &Value, attachments: &mut Attachments) -> Value {
    let mut update = update.clone();
    crate::attachments::update_blocks(&mut update, &mut |block: &mut Value| {
        if matches!(block["type"].as_str(), Some("image" | "audio" | "resource"))
            && let Some(Block::Attachment(reference)) =
                normalize(std::slice::from_ref(block), attachments).pop()
        {
            *block = serde_json::to_value(reference).expect("references serialize");
        }
    });
    update
}

fn summary(
    registry: &SessionRegistry,
    session_id: &str,
    live: &SessionLiveState,
) -> SessionSummary {
    let scope = Scope::Session(session_id.to_string());
    let (op, _) = shown_operation(registry, &scope).expect("held session has an op");
    let url_flows = live
        .url_flows
        .values()
        .chain(request_flows(registry, &scope))
        .filter(|flow| flow.status == UrlFlowStatus::Waiting)
        .count();
    SessionSummary {
        session_id: session_id.to_string(),
        cwd: live.cwd.to_string_lossy().into_owned(),
        title: live.controls.info.title.clone(),
        op: OperationSummary {
            kind: op.kind,
            status: op.status,
        },
        pending: PendingCounts {
            interactions: op.interactions.len(),
            url_flows,
        },
    }
}

pub(crate) fn project_global(registry: &SessionRegistry, agent: &AgentInfo) -> GlobalLive {
    GlobalLive {
        op: operation(
            &registry.scopes.global,
            interactions(registry, &Scope::Global, &registry.scopes.global.id),
        ),
        url_flows: url_flows(request_flows(registry, &Scope::Global)),
        agent: agent.clone(),
        sessions: held_sessions(registry)
            .into_iter()
            .filter_map(|session_id| {
                let live = held_live(registry, &session_id)?;
                let summary = summary(registry, &session_id, live);
                Some((session_id, summary))
            })
            .collect(),
        terminals: registry.scopes.login_terminals.clone(),
    }
}

pub(crate) fn project_catalog(registry: &SessionRegistry, revision: u64) -> CatalogLive {
    CatalogLive {
        op: operation(
            &registry.scopes.catalog,
            interactions(registry, &Scope::Catalog, &registry.scopes.catalog.id),
        ),
        url_flows: url_flows(request_flows(registry, &Scope::Catalog)),
        revision: revision.to_string(),
    }
}
