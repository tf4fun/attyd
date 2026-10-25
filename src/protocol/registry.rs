//! Operation slots and occupation in bridge state
//! (docs/state-sync.md §2.1).
//!
//! A held session keeps its `OpSlot` and reduced controls in its live state and
//! its timeline in its history state; `global` and `catalog` keep their slots
//! here. The registry's transitions maintain them through the functions below;
//! the commit step publishes them.
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use serde_json::{Value, json};

use super::SessionControls;
use super::timeline::{History, Timeline};
use super::documents::{
    End, EndReason, ErrorCode, ExitStatus, LoginTerminal, OperationKind, ProtocolError, Scope,
};
use super::op::OpSlot;
use super::wire::{self, Empty, PromptResult, SessionCreated};
use crate::runtime_state::{
    SessionLiveState, SessionOperationKind, UrlFlow, UrlFlowStatus,
};
use crate::session_registry::SessionRegistry;

/// The connection ended: the flows still waiting stay, `ended`, until a client dismisses them
/// (memory model §8.3).
fn mark_ended(flows: &mut BTreeMap<String, UrlFlow>) {
    for flow in flows.values_mut() {
        if flow.status == UrlFlowStatus::Waiting {
            flow.status = UrlFlowStatus::Ended;
        }
    }
}

/// A held session the next Agent connection loads again: bridge memory
/// that outlives the connection (protocol S6, §5.4). It keeps the controls
/// and the timeline the session had, with its attachments, and the waiting `load`.
#[derive(Debug, Clone)]
pub(crate) struct WaitingSession {
    pub session_id: String,
    pub cwd: std::path::PathBuf,
    pub controls: SessionControls,
    pub timeline: Option<Arc<Timeline>>,
    pub history: Option<History>,
    pub timeline_epoch: Option<String>,
    pub op: OpSlot,
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
    /// Request-scoped elicitations, by interaction ID, and the operation they
    /// joined: that of the scope whose ACP request they name (protocol §6.2).
    pub joined: BTreeMap<String, Joined>,
    /// Accepted request-scoped URL flows, by interaction ID, and their scope.
    pub flow_scopes: BTreeMap<String, Scope>,
    /// Terminal logins, by operation ID (protocol §4.6).
    pub login_terminals: BTreeMap<String, LoginTerminal>,
    /// The pages `list` produced since the catalog revision last changed.
    pub catalog_pages: CatalogPages,
    /// The latest requests of the bridge whose responses arrived (by `request_key`): a
    /// request-scoped interaction ends with the response of the request it names.
    pub completed_parents: VecDeque<String>,
}

/// How many completed requests are remembered for an interaction that arrives after its
/// parent's response.
const COMPLETED_PARENTS: usize = 256;

/// The text a request ID is compared by: its JSON form.
pub(crate) fn request_key(id: &Value) -> String {
    id.to_string()
}

/// The pages of a listing and the catalog revision they were listed at; they
/// are shown only while the catalog is still at that revision (protocol §4.3).
#[derive(Debug, Clone, Default)]
pub(crate) struct CatalogPages {
    pub revision: u64,
    pub pages: Vec<Value>,
}

/// The operation a request-scoped interaction joined.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Joined {
    pub scope: Scope,
    pub op: String,
    /// The request a request-scoped interaction names (`request_key`); its response ends it.
    pub parent: Option<String>,
}

/// The bound of a login terminal's output tail, in bytes.
const LOGIN_OUTPUT_LIMIT: usize = 1 << 20;

impl Default for ScopeOps {
    fn default() -> Self {
        Self {
            global: OpSlot::completed(OperationKind::None, Value::Null),
            catalog: OpSlot::completed(OperationKind::None, Value::Null),
            occupied: BTreeMap::new(),
            ended: Vec::new(),
            joined: BTreeMap::new(),
            flow_scopes: BTreeMap::new(),
            login_terminals: BTreeMap::new(),
            catalog_pages: CatalogPages::default(),
            completed_parents: VecDeque::new(),
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

/// The protocol kind of a session operation of the bridge, where the protocol has one.
fn session_kind(kind: SessionOperationKind) -> Option<OperationKind> {
    match kind {
        SessionOperationKind::Load | SessionOperationKind::Resume => Some(OperationKind::Load),
        SessionOperationKind::Fork => Some(OperationKind::Fork),
        // A control is an action, and a close and a delete are not operations of the session.
        SessionOperationKind::SetMode
        | SessionOperationKind::SetConfig
        | SessionOperationKind::Close
        | SessionOperationKind::Delete => None,
    }
}

/// A v1 operation error `{ code?, message, data? }`.
pub(crate) fn agent_error(error: &Value) -> ProtocolError {
    ProtocolError::agent(error)
}

/// The ACP method of a request names the scope it belongs to: a list is the
/// catalog's, a session's request is the session's, the rest are global.
pub(crate) fn request_scope(session_id: Option<&str>, method: &str) -> Scope {
    match (method, session_id) {
        ("session/list", _) => Scope::Catalog,
        (_, Some(session_id)) => Scope::Session(session_id.to_string()),
        (_, None) => Scope::Global,
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
        self.install_operation_as(scope, kind, input, None, None)
    }

    /// Installs a running operation under `id`, the one its client chose
    /// (protocol §6.5), or a generated one; `after` is the operation it replaces.
    pub(crate) fn install_operation_as(
        &mut self,
        scope: &Scope,
        kind: OperationKind,
        input: Value,
        id: Option<String>,
        after: Option<String>,
    ) -> Result<String, InstallError> {
        let mut slot = OpSlot::running(kind, input);
        if let Some(id) = id {
            slot.id = id;
        }
        slot.after = after;
        self.install(scope, slot)
    }

    pub(crate) fn install_prompt_as(
        &mut self,
        session_id: &str,
        prompt: &[Value],
        id: Option<String>,
        after: Option<String>,
    ) -> Result<String, InstallError> {
        let mut slot = OpSlot::prompt(prompt);
        if let Some(id) = id {
            slot.id = id;
        }
        slot.after = after;
        self.install(&Scope::Session(session_id.to_string()), slot)
    }

    fn install(&mut self, scope: &Scope, mut slot: OpSlot) -> Result<String, InstallError> {
        self.scope_free(scope)?;
        slot.scope = Some(scope.to_string());
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

    /// The held response of the running operation `op` in `scope` takes its
    /// end (protocol §6.3).
    pub(crate) fn hold_response(
        &mut self,
        scope: &Scope,
        op: &str,
        sender: tokio::sync::oneshot::Sender<super::op::OpEnd>,
    ) {
        if self.held_scope(scope) {
            let slot = self.slot_mut(scope);
            if slot.id == op && slot.is_running() {
                slot.completion = super::op::Completion::new(sender);
            }
        }
    }

    /// The running operation `op`, wherever it is, failed: it frees the
    /// sessions it occupies. Another operation is never touched, whatever its
    /// kind.
    pub(crate) fn fail_protocol_operation(&mut self, op: &str, error: ProtocolError) {
        let scope = [Scope::Global, Scope::Catalog]
            .into_iter()
            .find(|scope| self.slot_mut(scope).id == op)
            .or_else(|| {
                self.sessions.iter().find_map(|(session_id, entry)| {
                    let live = entry.state.live.as_ref()?;
                    (live.op.id == op).then(|| Scope::Session(session_id.clone()))
                })
            });
        let Some(scope) = scope else {
            return;
        };
        if !self.slot_mut(&scope).is_running() {
            return;
        }
        let freed = self.take_occupied_by(&scope);
        self.slot_mut(&scope).fail(error);
        self.leave_op_in(&freed, &scope);
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

    /// An interaction arrived in `scope`: it joins the operation shown there
    /// unless it already joined one.
    pub(crate) fn bind_interaction(&mut self, interaction_id: &str, scope: Scope) {
        if !self.scopes.joined.contains_key(interaction_id) {
            self.join_interaction(interaction_id, scope);
        }
    }

    /// A request-scoped interaction naming the request `parent` (its `request_key`) arrived
    /// for `scope`: it ends with that request's response.
    pub(crate) fn bind_interaction_to_parent(&mut self, interaction_id: &str, scope: Scope, parent: String) {
        self.bind_interaction(interaction_id, scope);
        if let Some(joined) = self.scopes.joined.get_mut(interaction_id) {
            joined.parent = Some(parent);
        }
    }

    /// The response of the bridge's request `key` arrived: the interactions that name it end at
    /// the next commit, also one that arrives after it.
    pub(crate) fn parent_completed(&mut self, key: String) {
        let completed = &mut self.scopes.completed_parents;
        if completed.len() == COMPLETED_PARENTS {
            completed.pop_front();
        }
        completed.push_back(key);
    }

    /// A request-scoped interaction arrived for `scope`: it joins the operation shown there,
    /// which its parent request was sent for (protocol §6.2).
    /// An unheld session's request falls back to `global`.
    pub(crate) fn join_interaction(&mut self, interaction_id: &str, scope: Scope) {
        let scope = match &scope {
            Scope::Session(session_id) if self.held_live(session_id).is_none() => Scope::Global,
            _ => scope,
        };
        let owner = match &scope {
            Scope::Session(session_id) => self
                .scopes
                .occupied
                .get(session_id)
                .cloned()
                .unwrap_or_else(|| scope.clone()),
            _ => scope.clone(),
        };
        let op = self.slot_mut(&owner).id.clone();
        self.scopes
            .joined
            .insert(interaction_id.to_string(), Joined { scope, op, parent: None });
    }

    /// An accepted request-scoped URL elicitation became a flow: it stays in
    /// the scope of the interaction it came from.
    pub(crate) fn accept_joined_flow(&mut self, interaction_id: &str) {
        if let Some(joined) = self.scopes.joined.remove(interaction_id) {
            self.scopes
                .flow_scopes
                .insert(interaction_id.to_string(), joined.scope);
        }
    }

    /// Request-scoped interactions whose parent request has been answered (memory model §8.2).
    /// One that names no request (none is admitted since Q6) ends with the operation it joined.
    /// A session-scoped interaction is never one of them: an operation's end ends no
    /// interaction.
    pub(crate) fn orphaned_interactions(&self) -> Vec<String> {
        self.request_elicitations
            .values()
            .filter(|pending| {
                self.scopes
                    .joined
                    .get(&pending.interaction_id)
                    .is_some_and(|joined| match &joined.parent {
                        Some(parent) => self.scopes.completed_parents.contains(parent),
                        None => self
                            .shown_slot(&joined.scope)
                            .is_none_or(|slot| slot.id != joined.op || !slot.is_running()),
                    })
            })
            .map(|pending| pending.interaction_id.clone())
            .collect()
    }

    /// Forgets joins and flow scopes whose interaction or flow is gone.
    pub(crate) fn prune_joined(&mut self) {
        let pending: std::collections::BTreeSet<String> = self
            .request_elicitations
            .keys()
            .cloned()
            .chain(
                self.sessions
                    .values()
                    .filter_map(|entry| entry.state.live.as_ref())
                    .flat_map(|live| {
                        live.permissions
                            .keys()
                            .chain(live.elicitations.keys())
                            .cloned()
                    }),
            )
            .collect();
        self.scopes
            .joined
            .retain(|interaction_id, _| pending.contains(interaction_id));
        let flows: std::collections::BTreeSet<&str> = self
            .request_url_flows
            .values()
            .map(|flow| flow.registration_id.as_str())
            .collect();
        self.scopes
            .flow_scopes
            .retain(|interaction_id, _| flows.contains(interaction_id.as_str()));
    }

    /// The running operation of `kind` in `scope` completed with `result`.
    pub(crate) fn complete_scope_operation(
        &mut self,
        scope: &Scope,
        kind: OperationKind,
        result: Value,
    ) {
        if self.held_scope(scope) {
            let slot = self.slot_mut(scope);
            if slot.is_running_kind(kind) {
                let id = slot.id.clone();
                let continued = slot.input.get("cursor").is_some_and(|cursor| !cursor.is_null());
                if kind == OperationKind::List {
                    self.record_page(&result, continued);
                }
                let slot = self.slot_mut(scope);
                let created = (kind == OperationKind::Create)
                    .then(|| result["sessionId"].as_str().map(String::from))
                    .flatten();
                slot.complete(result);
                if let Some(session_id) = created {
                    self.set_origin(&session_id, id);
                }
            }
        }
    }

    /// A listed page: the first page of a new listing replaces the pages, a
    /// page after a cursor joins those of the same revision (protocol §6.2).
    fn record_page(&mut self, result: &Value, continued: bool) {
        let Some(revision) = result["revision"].as_str().and_then(|r| r.parse::<u64>().ok()) else {
            return;
        };
        let mut page = json!({ "sessions": result["sessions"], "nextCursor": result["nextCursor"] });
        if let Some(meta) = result.get("_meta").filter(|meta| !meta.is_null()) {
            page["_meta"] = meta.clone();
        }
        let pages = &mut self.scopes.catalog_pages;
        if continued && pages.revision == revision {
            pages.pages.push(page);
        } else {
            *pages = CatalogPages {
                revision,
                pages: vec![page],
            };
        }
    }

    /// The session `create` or `fork` `origin` installed names that operation.
    fn set_origin(&mut self, session_id: &str, origin: String) {
        if let Some(live) = self.held_live_mut(session_id) {
            live.origin = Some(origin);
        }
    }

    /// The running operation of `kind` in `scope` failed. A failed delete
    /// frees the session it occupied.
    pub(crate) fn fail_scope_operation(
        &mut self,
        scope: &Scope,
        kind: OperationKind,
        error: ProtocolError,
    ) {
        if !self.held_scope(scope) || !self.slot_mut(scope).is_running_kind(kind) {
            return;
        }
        let freed = if kind == OperationKind::Delete {
            self.take_occupied_by(scope)
        } else {
            Vec::new()
        };
        self.slot_mut(scope).fail(error);
        self.leave_op_in(&freed, scope);
    }

    /// A delete of a session the bridge does not hold completed.
    pub(crate) fn complete_catalog_delete(&mut self, session_id: &str) {
        if let Some(occupier) = self.free_occupation(session_id) {
            self.slot_mut(&occupier).complete(wire::value(&Empty {}));
        }
    }

    fn held_scope(&self, scope: &Scope) -> bool {
        match scope {
            Scope::Session(session_id) => self.held_live(session_id).is_some(),
            _ => true,
        }
    }

    // -----------------------------------------------------------------------
    // Terminal logins (protocol §4.6)

    /// A `terminal_login` operation started its terminal.
    pub(crate) fn start_login_terminal(&mut self, op: &str, method_id: &str) {
        self.scopes.login_terminals.insert(
            op.to_string(),
            LoginTerminal {
                terminal_id: op.to_string(),
                output: String::new(),
                truncated: false,
                exit_status: None,
                method_id: method_id.to_string(),
                released: false,
            },
        );
    }

    /// A login terminal could not start: it never existed.
    pub(crate) fn discard_login_terminal(&mut self, op: &str) {
        self.scopes.login_terminals.remove(op);
    }

    /// A login terminal printed `data`; its output keeps a bounded tail.
    pub(crate) fn login_output(&mut self, op: &str, data: &str) {
        let Some(terminal) = self.scopes.login_terminals.get_mut(op) else {
            return;
        };
        terminal.output.push_str(data);
        if terminal.output.len() > LOGIN_OUTPUT_LIMIT {
            let mut cut = terminal.output.len() - LOGIN_OUTPUT_LIMIT;
            while !terminal.output.is_char_boundary(cut) {
                cut += 1;
            }
            terminal.output.drain(..cut);
            terminal.truncated = true;
        }
    }

    /// A login terminal's process exited: the terminal stays until dismissed,
    /// and its operation ends with the exit (protocol §6.2).
    pub(crate) fn login_exited(&mut self, op: &str, exit: LoginExit) {
        if let Some(terminal) = self.scopes.login_terminals.get_mut(op) {
            terminal.exit_status = Some(ExitStatus {
                exit_code: exit.exit_code,
                signal: exit.signal.clone(),
            });
        }
        let slot = &mut self.scopes.global;
        if slot.id != op || !slot.is_running_kind(OperationKind::TerminalLogin) {
            return;
        }
        if let Some(failure) = exit.failure {
            slot.fail(ProtocolError::new(ErrorCode::Unavailable, failure));
        } else if exit.cancelled {
            slot.status = super::documents::OperationStatus::Cancelled;
        } else {
            slot.complete(wire::value(&ExitStatus {
                exit_code: exit.exit_code,
                signal: exit.signal,
            }));
        }
    }

    // -----------------------------------------------------------------------
    // The Agent connection

    /// The Agent connection ended (protocol §4.4, §4.6): every running
    /// operation fails with `error`, pending interactions end with it, and the
    /// connection's resources go.
    pub(crate) fn end_connection(&mut self, error: ProtocolError) {
        for slot in [&mut self.scopes.global, &mut self.scopes.catalog] {
            if slot.is_running() {
                slot.fail(error.clone());
            }
        }
        for entry in self.sessions.values_mut() {
            let timeline = entry.state.timeline.clone();
            if let Some(live) = entry.state.live.as_mut() {
                if live.op.is_running() {
                    live.op.fail(error.clone());
                }
                live.permissions.clear();
                live.elicitations.clear();
                mark_ended(&mut live.url_flows);
                // Every handle ends with the connection; the output an entry
                // embeds stays frozen with the session that remains held.
                for (terminal_id, mut snapshot) in std::mem::take(&mut live.terminals) {
                    if timeline
                        .as_ref()
                        .is_some_and(|timeline| timeline.embeds_terminal(&terminal_id))
                    {
                        if let Some(fields) = snapshot.as_object_mut() {
                            fields.insert("released".to_owned(), Value::Bool(true));
                        }
                        live.frozen_terminals.insert(terminal_id, snapshot);
                    }
                }
            }
        }
        for (session_id, occupier) in std::mem::take(&mut self.scopes.occupied) {
            self.leave_op_in(&[session_id], &occupier);
        }
        self.scopes.completed_parents.clear();
        self.request_elicitations.clear();
        mark_ended(&mut self.request_url_flows);
        self.scopes.joined.clear();
        self.scopes.login_terminals.clear();
    }

    /// The connection ended while a subscription follows the held session
    /// and the Agent can load it again: it shows a running `load`, which the
    /// next connection runs, and keeps its log (protocol S6, §5.4).
    pub(crate) fn wait_for_next_connection(&mut self, session_id: &str) -> Option<WaitingSession> {
        let entry = self.sessions.get_mut(session_id)?;
        let live = entry.state.live.as_mut()?;
        live.op = OpSlot::running(
            OperationKind::Load,
            json!({ "cwd": live.cwd.to_string_lossy() }),
        );
        Some(WaitingSession {
            session_id: session_id.to_string(),
            cwd: live.cwd.clone(),
            controls: live.controls.clone(),
            timeline: entry.state.timeline.clone(),
            history: entry.state.history,
            timeline_epoch: entry.state.timeline_epoch.clone(),
            op: live.op.clone(),
        })
    }

    /// The connection ended while a subscription follows the held session
    /// and the Agent cannot load it again: its scope ends with `load_failed`.
    pub(crate) fn end_unloadable(&mut self, session_id: &str) {
        self.sessions.remove(session_id);
        self.scopes.ended.push((
            session_id.to_string(),
            End {
                reason: EndReason::LoadFailed,
                error: Some(ProtocolError::new(
                    ErrorCode::Unavailable,
                    "the Agent connection ended and the Agent cannot load the session again",
                )),
            },
        ));
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
        let mut forked = None;
        if let Some(live) = self.held_live_mut(session_id)
            && live.op.is_running_kind(kind)
        {
            let origin = live.op.id.clone();
            live.op.complete(match kind {
                OperationKind::Fork => {
                    let new = result["sessionId"]
                        .as_str()
                        .expect("a fork result is the Agent's ForkSessionResponse")
                        .to_string();
                    forked = Some((new.clone(), origin));
                    wire::value(&SessionCreated { session_id: new })
                }
                _ => wire::value(&Empty {}),
            });
        }
        if let Some((new, origin)) = forked {
            self.set_origin(&new, origin);
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
                self.leave_op_in(&[session_id.to_string()], &occupier);
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
            self.slot_mut(&occupier).complete(wire::value(&Empty {}));
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

    /// Ends the occupation `scope` holds and returns the sessions it freed.
    fn take_occupied_by(&mut self, scope: &Scope) -> Vec<String> {
        let freed: Vec<String> = self
            .scopes
            .occupied
            .iter()
            .filter(|(_, occupier)| *occupier == scope)
            .map(|(session_id, _)| session_id.clone())
            .collect();
        for session_id in &freed {
            self.scopes.occupied.remove(session_id);
        }
        freed
    }

    /// The occupying operation of `occupier` ended without removing the sessions it occupied: they
    /// keep showing it. A session's `op.id` never returns to an ID it showed before, so a client
    /// that expected the operation it replaced can never be admitted (design §9.2, §9.3).
    fn leave_op_in(&mut self, sessions: &[String], occupier: &Scope) {
        let mut shown = match occupier {
            Scope::Global => self.scopes.global.clone(),
            Scope::Catalog => self.scopes.catalog.clone(),
            Scope::Session(id) => match self.held_live(id) {
                Some(live) => live.op.clone(),
                None => return,
            },
        };
        shown.completion = super::op::Completion::default();
        for session_id in sessions {
            if let Some(live) = self.held_live_mut(session_id) {
                live.op = shown.clone();
            }
        }
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

    /// The running prompt's turn ended; the prompt operation ends.
    pub(crate) fn end_session_turn(&mut self, session_id: &str, outcome: TurnEnd<'_>) {
        let Some(entry) = self.sessions.get_mut(session_id) else {
            return;
        };
        let state = &mut entry.state;
        let Some(live) = state.live.as_mut() else {
            return;
        };
        if !live.op.is_running_kind(OperationKind::Prompt) {
            return;
        }
        match outcome {
            TurnEnd::Completed(response) => {
                live.op.complete(wire::value(&PromptResult {
                    stop_reason: response["stopReason"].clone(),
                    usage: response
                        .get("usage")
                        .filter(|usage| !usage.is_null())
                        .cloned(),
                }));
            }
            TurnEnd::Failed(error) => live.op.fail(agent_error(error)),
            TurnEnd::Uncertain(reason) => live
                .op
                .fail(ProtocolError::new(ErrorCode::Unavailable, reason)),
        }
    }

    /// A cancel of the running operation `op` that `scope` shows was
    /// requested: it sets `cancelRequested` once. Whether this is the first.
    pub(crate) fn request_operation_cancel(&mut self, scope: &Scope, op: &str) -> bool {
        let owner = match scope {
            Scope::Session(session_id) => self
                .scopes
                .occupied
                .get(session_id)
                .cloned()
                .unwrap_or_else(|| scope.clone()),
            _ => scope.clone(),
        };
        if !self.held_scope(&owner) {
            return false;
        }
        let slot = self.slot_mut(&owner);
        if slot.id != op || !slot.is_running() || slot.cancel_requested {
            return false;
        }
        slot.cancel_requested = true;
        true
    }

    /// Whether a cancel of the running operation `op` of `scope` was
    /// requested.
    pub(crate) fn operation_cancel_requested(&self, scope: &Scope, op: &str) -> bool {
        self.shown_slot(scope)
            .is_some_and(|slot| slot.id == op && slot.is_running() && slot.cancel_requested)
    }

    /// A cancel of the running turn was requested.
    pub(crate) fn request_session_cancel(&mut self, session_id: &str) {
        if let Some(live) = self.held_live_mut(session_id)
            && live.op.is_running_kind(OperationKind::Prompt)
        {
            live.op.cancel_requested = true;
        }
    }

    /// A load completed: the session's timeline is the replay's, replacing the
    /// retained one in one step (bridge memory model §6.2).
    pub(crate) fn replace_session_timeline(&mut self, session_id: &str, timeline: Timeline) {
        if let Some(entry) = self.sessions.get_mut(session_id) {
            let timeline = Arc::new(timeline);
            // Frozen output nothing in the new timeline embeds is freed.
            if let Some(live) = entry.state.live.as_mut() {
                live.frozen_terminals
                    .retain(|terminal_id, _| timeline.embeds_terminal(terminal_id));
            }
            entry.state.timeline = Some(timeline);
            entry.state.history = Some(History::Complete);
            entry.state.timeline_epoch = Some(super::op::operation_id());
        }
    }

    /// A resume gave the session's context, which is no replay: it is the
    /// timeline only of a session whose history is unknown (§6.1).
    pub(crate) fn adopt_session_timeline(&mut self, session_id: &str, timeline: Timeline) {
        if let Some(entry) = self.sessions.get_mut(session_id)
            && entry.state.timeline.is_none()
        {
            entry.state.timeline = Some(Arc::new(timeline));
            entry.state.history = Some(History::Partial);
            entry.state.timeline_epoch = Some(super::op::operation_id());
        }
    }

    /// Whether the held session shows the `load` the previous connection left
    /// it waiting with, and nothing runs it yet (protocol S6, §5.4).
    pub(crate) fn waits_for_load(&self, session_id: &str) -> bool {
        self.sessions.get(session_id).is_some_and(|entry| {
            entry.state.operation.is_none()
                && entry
                    .state
                    .live
                    .as_ref()
                    .is_some_and(|live| live.op.is_running_kind(OperationKind::Load))
        })
    }
}

/// How a login terminal's process ended.
#[derive(Debug, Clone, Default)]
pub(crate) struct LoginExit {
    pub exit_code: Option<i64>,
    pub signal: Option<String>,
    /// A cancel stopped it.
    pub cancelled: bool,
    /// Waiting for it failed.
    pub failure: Option<String>,
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
