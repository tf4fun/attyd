//! Pure projections of bridge state into the published documents
//! (docs/state-sync.md §2.2). They are the only definition of what clients see.
use std::collections::BTreeMap;

use super::documents::{
    AgentInfo, CatalogLive, GlobalLive, Interaction, Operation, OperationSummary, PendingCounts,
    Scope, SessionSummary, UrlFlow, UrlFlowState,
};
use super::op::OpSlot;
use crate::runtime_state::{PendingInteraction, SessionLifecycle, SessionLiveState, UrlFlowStatus};
use crate::session_registry::SessionRegistry;

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

/// A session's pending interactions. An operation's end ends none of them (memory model §8.2).
fn session_interactions(live: &SessionLiveState, into: &mut BTreeMap<String, Interaction>) {
    into.extend(
        live.permissions
            .values()
            .map(|pending| interaction(pending, true)),
    );
    into.extend(
        live.elicitations
            .values()
            .map(|pending| interaction(pending, false)),
    );
}

/// The pending interactions `owner`'s slot shows: its own scope's, those of the scopes it
/// occupies, and the request-scoped ones that were sent for a write or a load of those scopes
/// (protocol §6.2). Whether an operation runs does not matter: an interaction is the scope's
/// until it is answered or ends by its own lifetime.
fn interactions(registry: &SessionRegistry, owner: &Scope) -> BTreeMap<String, Interaction> {
    let mut interactions = BTreeMap::new();
    let shown = |scope: &Scope| {
        scope == owner
            || matches!(scope, Scope::Session(session_id)
                if registry.scopes.occupied.get(session_id) == Some(owner))
    };
    let joined =
        |pending: &&PendingInteraction| registry.scopes.joined.get(&pending.interaction_id);
    interactions.extend(
        registry
            .request_elicitations
            .values()
            .filter(|pending| joined(pending).is_some_and(|joined| shown(&joined.scope)))
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
                session_interactions(live, &mut interactions);
            }
        }
    }
    for (session_id, occupier) in &registry.scopes.occupied {
        if occupier == owner
            && let Some(live) = held_live(registry, session_id)
        {
            session_interactions(live, &mut interactions);
        }
    }
    interactions
}

fn operation(
    slot: &OpSlot,
    shown_in: &Scope,
    interactions: BTreeMap<String, Interaction>,
) -> Operation {
    Operation {
        id: slot.id.clone(),
        scope: slot.scope.clone().unwrap_or_else(|| shown_in.to_string()),
        after: slot.after.clone(),
        kind: slot.kind,
        status: slot.status,
        input: slot.input.clone(),
        cancel_requested: slot.cancel_requested,
        interactions,
        result: slot.result.clone(),
        error: slot.error.clone(),
    }
}

/// The scope whose slot `scope` shows.
pub(super) fn owner(registry: &SessionRegistry, scope: &Scope) -> Scope {
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

pub(super) fn shown_operation(
    registry: &SessionRegistry,
    scope: &Scope,
) -> Option<(Operation, Scope)> {
    let owner = owner(registry, scope);
    let slot = registry.shown_slot(scope)?;
    Some((
        operation(slot, scope, interactions(registry, &owner)),
        owner,
    ))
}

/// The accepted request-scoped URL flows of `scope`; a flow of no known scope
/// is global's.
pub(super) fn request_flows<'a>(
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

pub(super) fn url_flows<'a>(
    flows: impl IntoIterator<Item = &'a crate::runtime_state::UrlFlow>,
) -> BTreeMap<String, UrlFlow> {
    flows
        .into_iter()
        .map(|flow| {
            (
                flow.registration_id.clone(),
                UrlFlow {
                    interaction_id: flow.registration_id.clone(),
                    elicitation_id: flow.elicitation_id.clone(),
                    request: flow.request.clone(),
                    status: match flow.status {
                        UrlFlowStatus::Waiting => UrlFlowState::Waiting,
                        UrlFlowStatus::Completed => UrlFlowState::Completed,
                        UrlFlowStatus::Ended => UrlFlowState::Ended,
                        UrlFlowStatus::Cancelled => unreachable!("a cancelled flow is removed"),
                    },
                },
            )
        })
        .collect()
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
        origin: live.origin.clone(),
    }
}

pub(crate) fn project_global(registry: &SessionRegistry, agent: &AgentInfo) -> GlobalLive {
    GlobalLive {
        op: operation(
            &registry.scopes.global,
            &Scope::Global,
            interactions(registry, &Scope::Global),
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
            &Scope::Catalog,
            interactions(registry, &Scope::Catalog),
        ),
        url_flows: url_flows(request_flows(registry, &Scope::Catalog)),
        revision: revision.to_string(),
    }
}
