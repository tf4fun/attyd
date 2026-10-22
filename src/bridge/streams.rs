//! Protocol v2 streams in the bridge actor (protocol §5.1, S6): the loads a
//! subscription starts, the need that holds a session, and the held sessions
//! one Agent connection leaves for the next.
//!
//! The Hub only delivers. What a stream sees of a load, a reload or a release
//! is written by the bridge's own transitions and published by the commit; the
//! actor only ends a scope on one subscription when no transition can, as when
//! the subscription gave no directory or its session was deleted unheld.
use std::collections::BTreeMap;

use super::*;
use crate::protocol::documents::{End, EndReason, ErrorCode, ProtocolError, Scope};
use crate::protocol::hub::{Publication, Subscribed, Subscription};

/// Registers a subscription under the bridge guard, from the cut the guard
/// sees, and starts what it waits for. The guard's commit, on its release,
/// publishes the start after the cut.
pub(super) fn subscribe(
    state: &mut BridgeState,
    scopes: &[Subscribed],
) -> (Subscription, Option<Value>) {
    let subscription = register(state, scopes);
    let command = serve(state);
    refresh_observer_timers(state);
    (subscription, command)
}

/// Registers a subscription under the bridge guard, from the cut the guard
/// sees, without starting a load: while the Agent initializes, its
/// capabilities are not known yet. Its readiness serves the streams.
pub(super) fn register(state: &BridgeState, scopes: &[Subscribed]) -> Subscription {
    let published = &mut *state.protocol.publication.lock();
    published.hub.subscribe(&published.publisher, scopes)
}

/// Asks the actor to serve the streams again.
pub(super) fn wake(state: &BridgeState) {
    if let Some(events) = &state.observer_events {
        let _ = events.send(BridgeInput::ServeStreams);
    }
}

/// Starts the next load the streams wait for: first a session the previous
/// connection held, then an unheld session a subscription follows. Starts one
/// load a pass, the command the caller runs, and wakes itself for the rest.
pub(super) fn serve(state: &mut BridgeState) -> Option<Value> {
    while let Some(session_id) = state.stream_reloads.pop_front() {
        let Some(cwd) = state
            .sessions
            .active_session(&session_id)
            .map(|session| session.live.cwd.to_string_lossy().into_owned())
        else {
            continue;
        };
        let Some(method) = reattach_method(state.agent_capabilities.as_ref()) else {
            state.sessions.end_unloadable(&session_id);
            continue;
        };
        // The next pass serves the rest: the other held sessions, then the
        // subscriptions registered while the Agent initialized.
        wake(state);
        return Some(json!({
            "type": method, "requestId": format!("bridge-reload-{}", Uuid::new_v4()),
            "sessionId": session_id, "cwd": cwd,
        }));
    }
    let mut command = None;
    for (session_id, waiting) in waiting_for_load(&state.protocol.publication) {
        // An operation occupies the session from its admission, before its
        // ACP request (S5): the subscription waits for it.
        if waiting.is_empty()
            || state.sessions.scopes.occupied.contains_key(&session_id)
            || state.catalog_deletions.contains_key(&session_id)
            || session_materialization(state, &session_id).is_some()
            || published(&state.protocol.publication, &session_id)
        {
            continue;
        }
        if command.is_some() {
            wake(state);
            break;
        }
        // A held session ignores the directory; an unheld one needs it, and a
        // missing one is never replaced by a default (protocol §5.1).
        let Some(cwd) = waiting.iter().find_map(|cwd| cwd.clone()) else {
            let error = ProtocolError::new(
                ErrorCode::InvalidRequest,
                format!("the session {session_id} is not held: its subscription needs a cwd"),
            );
            end_waiting(state, &session_id, EndReason::LoadFailed, Some(error));
            continue;
        };
        command = prepare_session_load(state, session_id, Some(cwd));
    }
    command
}

/// The sessions some subscription, of either stream, waits to have published,
/// with the directories the subscriptions gave for their load.
fn waiting_for_load(publication: &Publication) -> BTreeMap<String, Vec<Option<String>>> {
    let published = publication.lock();
    let mut sessions: BTreeMap<String, Vec<Option<String>>> = BTreeMap::new();
    for (session_id, waiting) in published.hub.followed_sessions() {
        sessions
            .entry(session_id)
            .or_default()
            .extend(waiting.into_iter().map(|waiting| waiting.cwd));
    }
    for (session_id, waiting) in published.feed.followed_sessions() {
        sessions
            .entry(session_id)
            .or_default()
            .extend(waiting.into_iter().map(|waiting| waiting.cwd));
    }
    sessions
}

fn published(publication: &Publication, session_id: &str) -> bool {
    publication
        .lock()
        .feed
        .live(&Scope::Session(session_id.to_string()))
        .is_some()
}

/// Ends `session/<session_id>` on every subscription still waiting for it.
pub(super) fn end_waiting(
    state: &BridgeState,
    session_id: &str,
    reason: EndReason,
    error: Option<ProtocolError>,
) {
    let published = &mut *state.protocol.publication.lock();
    published.feed.end_waiting(
        session_id,
        &End {
            reason,
            error: error.clone(),
        },
    );
    let waiting = published
        .hub
        .followed_sessions()
        .remove(session_id)
        .unwrap_or_default();
    let scope = Scope::Session(session_id.to_string()).to_string();
    for waiting in waiting {
        published.hub.end(
            waiting.id,
            &scope,
            End {
                reason,
                error: error.clone(),
            },
        );
    }
}

/// A load a subscription started ended. A failure the registry published
/// ends the scope through the commit; any other ends it here.
pub(super) fn finish_load(
    state: &mut BridgeState,
    session_id: &str,
    result: Result<(), SessionViewError>,
) {
    let Err(error) = result else {
        return;
    };
    if state
        .sessions
        .scopes
        .ended
        .iter()
        .any(|(ended, _)| ended == session_id)
    {
        return;
    }
    let error = match error {
        SessionViewError::NotFound => {
            ProtocolError::new(ErrorCode::NotFound, format!("no session {session_id}"))
        }
        SessionViewError::Unavailable(message) => {
            ProtocolError::new(ErrorCode::Unavailable, message)
        }
    };
    end_waiting(state, session_id, EndReason::LoadFailed, Some(error));
}

/// How the Agent brings a held session back, if it can: `session/load`
/// replays its history; `session/resume` replays none, and the session keeps
/// the log it has (§5.4).
fn reattach_method(capabilities: Option<&AgentCapabilities>) -> Option<&'static str> {
    let capabilities = capabilities?;
    if capabilities.load_session {
        Some("session/load")
    } else if capabilities.session_capabilities.resume.is_some() {
        Some("session/resume")
    } else {
        None
    }
}

/// Whether the Agent can bring a released session back: only such a session
/// is released (S6).
pub(super) fn releasable(state: &BridgeState) -> bool {
    reattach_method(state.agent_capabilities.as_ref()).is_some()
}

/// The sessions some subscription follows.
pub(super) fn followed(state: &BridgeState) -> std::collections::BTreeSet<String> {
    let published = state.protocol.publication.lock();
    published
        .hub
        .followed_sessions()
        .into_keys()
        .chain(published.feed.followed_sessions().into_keys())
        .collect()
}

/// Whether a held session is needed (S6): a subscription follows it, an
/// operation runs in it or occupies it, from its admission on, or a resource
/// of its own is still there, an accepted URL flow or a terminal not released
/// yet, even an exited one.
pub(super) fn needed(
    state: &BridgeState,
    followed: &std::collections::BTreeSet<String>,
    session_id: &str,
) -> bool {
    let registry = &state.sessions;
    let scope = Scope::Session(session_id.to_string());
    followed.contains(session_id)
        || registry.scopes.occupied.contains_key(session_id)
        || registry.session_ref(session_id).is_some_and(|session| {
            session.live.op.is_running()
                || !session.live.url_flows.is_empty()
                || !session.live.terminals.is_empty()
        })
        || registry
            .request_url_flows
            .values()
            .any(|flow| registry.scopes.flow_scopes.get(&flow.registration_id) == Some(&scope))
}

impl BridgeState {
    /// At the connection's end, after its operations failed and its resources
    /// went: a held session a subscription follows waits for the next
    /// connection to load it again, with its log; one the Agent cannot load
    /// again ends with `load_failed` (S6, §5.4). Nothing else needs any other
    /// held session, which is released now, before the final commit.
    pub(super) fn hold_for_next_connection(&mut self) {
        let reloadable = self.agent_capabilities.is_none() || releasable(self);
        let followed = followed(self);
        for session_id in crate::protocol::project::held_sessions(&self.sessions) {
            if !followed.contains(&session_id) {
                self.sessions.retire_entry(&session_id);
            } else if !reloadable {
                self.sessions.end_unloadable(&session_id);
            } else if let Some(waiting) = self.sessions.wait_for_next_connection(&session_id) {
                self.protocol.publication.lock().waiting.push(waiting);
            }
        }
    }

    /// Before the connection's first commit: the sessions the previous one
    /// left start held, with their logs, and are loaded again once the Agent
    /// is ready.
    pub(super) fn seed_from_previous_connection(&mut self) {
        let waiting = std::mem::take(&mut self.protocol.publication.lock().waiting);
        for waiting in waiting {
            let session_id = waiting.session_id.clone();
            if self.sessions.seed_waiting(waiting).is_ok() {
                self.stream_reloads.push_back(session_id);
            }
        }
    }
}
