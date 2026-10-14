//! Operation slots, occupation and the turn log in bridge state
//! (docs/state-sync.md §2.1).
//!
//! A held session keeps its `OpSlot` and reduced controls in its live state and
//! its `TurnLog` in its history state; `global` and `catalog` keep their slots
//! here. The registry's transitions maintain them through the functions below;
//! the commit step publishes them.
use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use super::SessionControls;
use super::canon::TurnLog;
use super::documents::{End, EndReason, ErrorCode, OperationKind, ProtocolError, Scope};
use super::op::OpSlot;
use crate::runtime_state::{PendingInteraction, SessionLiveState, SessionOperationKind};
use crate::session_registry::SessionRegistry;

/// A session's log: unknown until the bridge holds its history (protocol S1).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) enum SessionLog {
    #[default]
    Unknown,
    Known(Arc<TurnLog>),
}

impl SessionLog {
    pub(crate) fn empty() -> Self {
        Self::Known(Arc::new(TurnLog::default()))
    }
}

/// The slots of `global` and `catalog`, and the scopes operations occupy
/// besides their own.
#[derive(Debug, Clone)]
pub(crate) struct ScopeOps {
    pub global: OpSlot,
    pub catalog: OpSlot,
    /// Occupied sessions, by session ID, and the scope whose operation
    /// occupies them.
    pub occupied: BTreeMap<String, Scope>,
    /// Session scopes that ended since the last commit, and why.
    pub ended: Vec<(String, End)>,
}

impl Default for ScopeOps {
    fn default() -> Self {
        Self {
            global: OpSlot::completed(OperationKind::None, Value::Null),
            catalog: OpSlot::completed(OperationKind::None, Value::Null),
            occupied: BTreeMap::new(),
            ended: Vec::new(),
        }
    }
}

/// Why an operation could not be installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstallError {
    /// No such held scope.
    NotFound,
    /// The scope's operation runs, or a scope it acts on is not free.
    NotFree,
}

/// The v2 kind of a v1 session operation, where the protocol has one. A close
/// releases a session and a delete belongs to `global`.
fn session_kind(kind: SessionOperationKind) -> Option<OperationKind> {
    match kind {
        SessionOperationKind::Load | SessionOperationKind::Resume => Some(OperationKind::Load),
        SessionOperationKind::Fork => Some(OperationKind::Fork),
        SessionOperationKind::SetMode => Some(OperationKind::SetMode),
        SessionOperationKind::SetConfig => Some(OperationKind::SetConfig),
        SessionOperationKind::Close | SessionOperationKind::Delete => None,
    }
}

/// A v1 operation error `{ code?, message, data? }`.
pub(crate) fn agent_error(error: &Value) -> ProtocolError {
    ProtocolError::agent(error)
}

/// The pending interactions of a scope that no answer is on its way to.
pub(crate) fn unanswered<'a>(
    interactions: impl IntoIterator<Item = &'a PendingInteraction>,
) -> impl Iterator<Item = &'a PendingInteraction> {
    interactions
        .into_iter()
        .filter(|interaction| interaction.responding_operation_id.is_none())
}

fn has_unanswered(live: &SessionLiveState) -> bool {
    unanswered(live.permissions.values().chain(live.elicitations.values()))
        .next()
        .is_some()
}

/// Starts an `agent_request` for interactions no operation runs for, and
/// completes one whose interactions are gone (protocol §6.2).
fn settle_agent_request(slot: &mut OpSlot, pending: bool) {
    if pending && !slot.is_running() {
        *slot = OpSlot::running(OperationKind::AgentRequest, json!({}));
    } else if !pending && slot.is_running_kind(OperationKind::AgentRequest) {
        slot.complete(json!({}));
    }
}

impl SessionRegistry {
    fn held_live(&self, session_id: &str) -> Option<&SessionLiveState> {
        self.sessions.get(session_id)?.state.live.as_ref()
    }

    fn held_live_mut(&mut self, session_id: &str) -> Option<&mut SessionLiveState> {
        self.sessions.get_mut(session_id)?.state.live.as_mut()
    }

    fn scope_free(&self, scope: &Scope) -> Result<(), InstallError> {
        let slot = match scope {
            Scope::Global => &self.scopes.global,
            Scope::Catalog => &self.scopes.catalog,
            Scope::Session(id) => {
                if self.scopes.occupied.contains_key(id) {
                    return Err(InstallError::NotFree);
                }
                &self.held_live(id).ok_or(InstallError::NotFound)?.op
            }
        };
        if slot.is_running() {
            return Err(InstallError::NotFree);
        }
        Ok(())
    }

    /// Installs a running operation in `scope` and occupies the other scopes it
    /// acts on (a delete occupies its session). Admission (protocol §6.4) is
    /// checked by the caller; this only refuses a scope that is not free.
    pub(crate) fn install_operation(
        &mut self,
        scope: &Scope,
        kind: OperationKind,
        input: Value,
    ) -> Result<String, InstallError> {
        self.install(scope, OpSlot::running(kind, input))
    }

    /// Installs a running prompt with its normalized input.
    pub(crate) fn install_prompt(
        &mut self,
        session_id: &str,
        prompt: &[Value],
    ) -> Result<String, InstallError> {
        self.install(
            &Scope::Session(session_id.to_string()),
            OpSlot::prompt(prompt),
        )
    }

    fn install(&mut self, scope: &Scope, slot: OpSlot) -> Result<String, InstallError> {
        self.scope_free(scope)?;
        let occupies = match (scope, slot.kind) {
            (Scope::Global, OperationKind::Delete) => {
                slot.input["sessionId"].as_str().map(String::from)
            }
            _ => None,
        };
        if let Some(session_id) = &occupies {
            let session = Scope::Session(session_id.clone());
            match self.scope_free(&session) {
                Ok(()) | Err(InstallError::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
        let id = slot.id.clone();
        match scope {
            Scope::Global => self.scopes.global = slot,
            Scope::Catalog => self.scopes.catalog = slot,
            Scope::Session(session_id) => {
                self.held_live_mut(session_id)
                    .expect("free scope is held")
                    .op = slot;
            }
        }
        if let Some(session_id) = occupies {
            self.scopes.occupied.insert(session_id, scope.clone());
        }
        Ok(id)
    }

    /// The slot a held scope shows: the occupying operation's, if any.
    pub(crate) fn shown_slot(&self, scope: &Scope) -> Option<&OpSlot> {
        match scope {
            Scope::Global => Some(&self.scopes.global),
            Scope::Catalog => Some(&self.scopes.catalog),
            Scope::Session(id) => {
                let live = self.held_live(id)?;
                match self.scopes.occupied.get(id) {
                    Some(occupier) if occupier != scope => self.shown_slot(occupier),
                    _ => Some(&live.op),
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Transitions, called by the registry's v1 transitions.

    /// A transition of `session_id` ended: settle its `agent_request`.
    pub(crate) fn settle_session_ops(&mut self, session_id: &str) {
        if self.scopes.occupied.contains_key(session_id) {
            return;
        }
        if let Some(live) = self.held_live_mut(session_id) {
            let pending = has_unanswered(live);
            settle_agent_request(&mut live.op, pending);
        }
    }

    /// A connection-level transition ended: settle `global`'s `agent_request`.
    pub(crate) fn settle_global_ops(&mut self) {
        let pending = unanswered(self.request_elicitations.values())
            .next()
            .is_some();
        settle_agent_request(&mut self.scopes.global, pending);
    }

    /// A v1 session operation started; a reload runs as a `load`.
    pub(crate) fn begin_session_operation(&mut self, session_id: &str, kind: SessionOperationKind) {
        if kind != SessionOperationKind::Load {
            return;
        }
        if let Some(live) = self.held_live_mut(session_id)
            && !live.op.is_running()
        {
            let cwd = live.cwd.to_string_lossy().into_owned();
            live.op = OpSlot::running(OperationKind::Load, json!({ "cwd": cwd }));
        }
    }

    /// A v1 session operation completed with `result`.
    pub(crate) fn complete_session_operation(
        &mut self,
        session_id: &str,
        kind: SessionOperationKind,
        result: &Value,
    ) {
        let Some(kind) = session_kind(kind) else {
            return;
        };
        let result = match kind {
            OperationKind::Fork => json!({ "sessionId": result["sessionId"] }),
            _ => json!({}),
        };
        if let Some(live) = self.held_live_mut(session_id)
            && live.op.is_running_kind(kind)
        {
            live.op.complete(result);
        }
    }

    /// A v1 session operation failed with `error`. A failed delete fails the
    /// `global` delete that occupies the session and frees it.
    pub(crate) fn fail_session_operation(
        &mut self,
        session_id: &str,
        kind: SessionOperationKind,
        error: &Value,
    ) {
        if kind == SessionOperationKind::Delete {
            if let Some(occupier) = self.free_occupation(session_id) {
                self.slot_mut(&occupier).fail(agent_error(error));
            }
            return;
        }
        let Some(kind) = session_kind(kind) else {
            return;
        };
        if let Some(live) = self.held_live_mut(session_id)
            && live.op.is_running_kind(kind)
        {
            live.op.fail(agent_error(error));
        }
    }

    /// A delete removed the session: its scope ends with `deleted`, and the
    /// occupying delete completes.
    pub(crate) fn complete_session_delete(&mut self, session_id: &str) {
        if let Some(occupier) = self.free_occupation(session_id) {
            self.slot_mut(&occupier).complete(json!({}));
        }
        self.scopes.ended.push((
            session_id.to_string(),
            End {
                reason: EndReason::Deleted,
                error: None,
            },
        ));
    }

    /// A first load failed: its scope ends with `load_failed`.
    pub(crate) fn fail_session_load(&mut self, session_id: &str, error: &Value) {
        self.scopes.ended.push((
            session_id.to_string(),
            End {
                reason: EndReason::LoadFailed,
                error: Some(agent_error(error)),
            },
        ));
    }

    fn free_occupation(&mut self, session_id: &str) -> Option<Scope> {
        let occupier = self.scopes.occupied.remove(session_id)?;
        self.slot_mut(&occupier).is_running().then_some(occupier)
    }

    fn slot_mut(&mut self, scope: &Scope) -> &mut OpSlot {
        match scope {
            Scope::Global => &mut self.scopes.global,
            Scope::Catalog => &mut self.scopes.catalog,
            Scope::Session(id) => &mut self.held_live_mut(id).expect("held occupier").op,
        }
    }

    /// The running prompt's turn ended. A completed turn is appended to the log
    /// (S2) whichever client started it; the prompt operation ends.
    pub(crate) fn end_session_turn(&mut self, session_id: &str, outcome: TurnEnd<'_>) {
        let Some(entry) = self.sessions.get_mut(session_id) else {
            return;
        };
        let state = &mut entry.state;
        if let TurnEnd::Completed(_) = outcome
            && let (Some(turn), SessionLog::Known(log)) = (&state.active_turn, &mut state.log)
        {
            let updates: Vec<Value> = turn
                .updates
                .iter()
                .map(|update| (**update).clone())
                .collect();
            Arc::make_mut(log).commit(&turn.prompt, &updates);
        }
        let Some(live) = state.live.as_mut() else {
            return;
        };
        if !live.op.is_running_kind(OperationKind::Prompt) {
            return;
        }
        match outcome {
            TurnEnd::Completed(response) => {
                let mut result = json!({ "stopReason": response["stopReason"] });
                if let Some(usage) = response.get("usage").filter(|usage| !usage.is_null()) {
                    result["usage"] = usage.clone();
                }
                live.op.complete(result);
            }
            TurnEnd::Failed(error) => live.op.fail(agent_error(error)),
            TurnEnd::Uncertain(reason) => live
                .op
                .fail(ProtocolError::new(ErrorCode::Unavailable, reason)),
        }
    }

    /// A cancel of the running turn was requested.
    pub(crate) fn request_session_cancel(&mut self, session_id: &str) {
        if let Some(live) = self.held_live_mut(session_id)
            && live.op.is_running_kind(OperationKind::Prompt)
        {
            live.op.cancel_requested = true;
        }
    }

    /// A load or reload replaced the session's history: its log is rebuilt from
    /// the replay.
    pub(crate) fn replace_session_log(&mut self, session_id: &str, replay: &[Value]) {
        if let Some(entry) = self.sessions.get_mut(session_id) {
            entry.state.log = SessionLog::Known(Arc::new(TurnLog::from_replay(replay)));
        }
    }

    /// A setup response arrived: controls settle from the earlier updates and
    /// the response (protocol §4.5).
    pub(crate) fn settle_session_controls(&mut self, session_id: &str) {
        if let Some(live) = self.held_live_mut(session_id) {
            live.controls = SessionControls::settle(&live.control_state, &live.session);
        }
    }

    /// A control update or a control operation's acknowledgement arrived.
    pub(crate) fn fold_session_controls(&mut self, session_id: &str, update: &Value) {
        if let Some(live) = self.held_live_mut(session_id) {
            live.controls.fold_update(update);
        }
    }
}

/// How a prompt's turn ended.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TurnEnd<'a> {
    /// The Agent answered `session/prompt` with this response.
    Completed(&'a Value),
    /// `session/prompt` failed with this error.
    Failed(&'a Value),
    /// The outcome is unknown: the transport closed.
    Uncertain(&'a str),
}
