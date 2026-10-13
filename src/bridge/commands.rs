//! Protocol v2 commands (protocol §5, plan §2.4).
//!
//! A command is admitted in one critical section of the bridge lock: its
//! intent is looked up, its `If-Match` owner and its admission are checked,
//! and its record (or, for a turn, its intent) is committed before the HTTP
//! response. The command then runs through the same bridge input as v1, and
//! its result reaches the record when it finishes. A synchronous rejection
//! commits nothing. Reads never materialize a session.
use std::collections::BTreeMap;
use std::time::Instant;

use super::*;
use crate::protocol::GLOBAL;
use crate::protocol::commands::{
    AcknowledgeError, FinalStatus, Fingerprint, INTENT_KEY_LIMIT, IntentLookup, Subject,
};
use crate::protocol::{
    CommandResult, Disposition, EmptyBody, ErrorCode, InteractionResponseBody, Position,
    ProtocolError, Request, RequestAccepted, RequestOperation, RequestResult, RequestStatus,
    RequestTarget, TurnAccepted, TurnOutcomeStatus, session_scope,
};
use crate::runtime_state::UrlFlowAcknowledgement;

/// The grace period of a first-subscription lease (protocol §4.2). It is
/// independent of `--session-unobserved-timeout`.
pub(crate) const FIRST_SUBSCRIPTION_GRACE: Duration = Duration::from_secs(30);

/// First-subscription leases the bridge holds for owners a command returned.
pub(super) struct FirstLeases {
    pub(super) grace: Duration,
    next: u64,
    leases: Vec<FirstLease>,
}

struct FirstLease {
    session_id: String,
    incarnation: u64,
    observer_id: u64,
    lease: ObservationLease,
}

impl Default for FirstLeases {
    fn default() -> Self {
        Self {
            grace: FIRST_SUBSCRIPTION_GRACE,
            // Clear of the observer IDs the server allocates for streams.
            next: 1 << 62,
            leases: Vec::new(),
        }
    }
}

/// An `If-Match` owner tag (protocol §5.3): `"<epoch>"`,
/// `"<epoch>:<incarnation>"` or `"<epoch>:<incarnation>:<historyRevision>"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnerTag {
    pub(crate) epoch: String,
    pub(crate) incarnation: Option<u64>,
    pub(crate) history_revision: Option<String>,
}

impl OwnerTag {
    /// A strong entity tag; weak tags and malformed parts are rejected.
    pub(crate) fn parse(header: &str) -> Option<Self> {
        let tag = header.trim().strip_prefix('"')?.strip_suffix('"')?;
        let mut parts = tag.splitn(3, ':');
        let epoch = parts.next().filter(|epoch| !epoch.is_empty())?.to_string();
        let incarnation = match parts.next() {
            None => None,
            Some(incarnation) => Some(incarnation.parse::<u64>().ok()?),
        };
        let history_revision = match parts.next() {
            None => None,
            Some(revision) if !revision.is_empty() => Some(revision.to_string()),
            Some(_) => return None,
        };
        Some(Self {
            epoch,
            incarnation,
            history_revision,
        })
    }

    fn text(&self) -> String {
        let mut text = self.epoch.clone();
        if let Some(incarnation) = self.incarnation {
            text.push_str(&format!(":{incarnation}"));
        }
        if let Some(revision) = &self.history_revision {
            text.push_str(&format!(":{revision}"));
        }
        text
    }
}

/// A valid `Idempotency-Key`: visible ASCII, bounded so a tombstone stays
/// small.
pub(crate) fn valid_intent_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= INTENT_KEY_LIMIT
        && key.bytes().all(|byte| byte.is_ascii_graphic())
}

/// An operation command on a held session (protocol §5.5).
#[derive(Debug, Clone)]
pub(crate) enum HeldCommand {
    SetMode { mode_id: String },
    SetConfig { config_id: String, value: Value },
    Fork,
    Close,
    Delete,
}

impl HeldCommand {
    fn operation(&self) -> RequestOperation {
        match self {
            Self::SetMode { .. } => RequestOperation::SetMode,
            Self::SetConfig { .. } => RequestOperation::SetConfig,
            Self::Fork => RequestOperation::Fork,
            Self::Close => RequestOperation::Close,
            Self::Delete => RequestOperation::Delete,
        }
    }

    fn method(&self) -> &'static str {
        match self {
            Self::SetMode { .. } => "session/set_mode",
            Self::SetConfig { .. } => "session/set_config_option",
            Self::Fork => "session/fork",
            Self::Close => "session/close",
            Self::Delete => "session/delete",
        }
    }

    fn kind(&self) -> RuntimeSessionOperationKind {
        match self {
            Self::SetMode { .. } => RuntimeSessionOperationKind::SetMode,
            Self::SetConfig { .. } => RuntimeSessionOperationKind::SetConfig,
            Self::Fork => RuntimeSessionOperationKind::Fork,
            Self::Close => RuntimeSessionOperationKind::Close,
            Self::Delete => RuntimeSessionOperationKind::Delete,
        }
    }

    /// The protocol operation name, as fingerprints and records use it.
    fn name(&self) -> &'static str {
        match self {
            Self::SetMode { .. } => "set_mode",
            Self::SetConfig { .. } => "set_config",
            Self::Fork => "fork",
            Self::Close => "close",
            Self::Delete => "delete",
        }
    }

    fn params(&self) -> BTreeMap<String, Value> {
        match self {
            Self::SetMode { mode_id } => BTreeMap::from([("modeId".into(), json!(mode_id))]),
            Self::SetConfig { config_id, value } => BTreeMap::from([
                ("configId".into(), json!(config_id)),
                ("value".into(), value.clone()),
            ]),
            Self::Fork | Self::Close | Self::Delete => BTreeMap::new(),
        }
    }

    /// The fingerprint's body: everything but the path parameters.
    fn body(&self) -> Value {
        match self {
            Self::SetMode { mode_id } => json!({ "modeId": mode_id }),
            Self::SetConfig { value, .. } => json!({ "value": value }),
            Self::Fork | Self::Close | Self::Delete => json!({}),
        }
    }

    /// The v1 command, bound to the owner admission checked: the handler
    /// refuses it if that owner was replaced in the meantime.
    fn command(&self, request_id: &str, session_id: &str, incarnation: u64) -> Value {
        let mut command = json!({
            "type": self.method(), "requestId": request_id, "sessionId": session_id,
            "expectedOwnerIncarnation": incarnation,
        });
        match self {
            Self::SetMode { mode_id } => command["modeId"] = json!(mode_id),
            Self::SetConfig { config_id, value } => {
                command["configId"] = json!(config_id);
                command["value"] = value.clone();
            }
            Self::Fork | Self::Close | Self::Delete => {}
        }
        command
    }
}

/// A global operation command (protocol §5.4).
#[derive(Debug, Clone)]
pub(crate) enum GlobalCommand {
    Authenticate {
        method_id: String,
    },
    Logout,
    AuthTerminal {
        method_id: String,
        cols: u16,
        rows: u16,
    },
    Create {
        cwd: String,
    },
}

impl GlobalCommand {
    fn operation(&self) -> RequestOperation {
        match self {
            Self::Authenticate { .. } => RequestOperation::Authenticate,
            Self::Logout => RequestOperation::Logout,
            Self::AuthTerminal { .. } => RequestOperation::AuthTerminal,
            Self::Create { .. } => RequestOperation::Create,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Authenticate { .. } => "authenticate",
            Self::Logout => "logout",
            Self::AuthTerminal { .. } => "auth_terminal",
            Self::Create { .. } => "create",
        }
    }

    fn method(&self) -> &'static str {
        match self {
            Self::Authenticate { .. } => "authenticate",
            Self::Logout => "logout",
            Self::AuthTerminal { .. } => "attyd/auth_terminal",
            Self::Create { .. } => "session/new",
        }
    }

    /// The fingerprint's body: the command's whole body.
    fn body(&self) -> Value {
        match self {
            Self::Authenticate { method_id } => json!({ "methodId": method_id }),
            Self::Logout => json!({}),
            Self::AuthTerminal {
                method_id,
                cols,
                rows,
            } => json!({ "methodId": method_id, "cols": cols, "rows": rows }),
            Self::Create { cwd } => json!({ "cwd": cwd }),
        }
    }

    fn method_id(&self) -> Option<String> {
        match self {
            Self::Authenticate { method_id } | Self::AuthTerminal { method_id, .. } => {
                Some(method_id.clone())
            }
            Self::Logout | Self::Create { .. } => None,
        }
    }

    fn params(&self) -> BTreeMap<String, Value> {
        match self {
            Self::Create { cwd } => BTreeMap::from([("cwd".to_string(), json!(cwd))]),
            _ => BTreeMap::new(),
        }
    }

    fn command(&self, request_id: &str) -> Value {
        match self {
            Self::Authenticate { method_id } => json!({
                "type": "auth/authenticate", "requestId": request_id, "methodId": method_id,
            }),
            Self::Logout => json!({ "type": "auth/logout", "requestId": request_id }),
            Self::AuthTerminal {
                method_id,
                cols,
                rows,
            } => json!({
                "type": "auth/terminal_start", "requestId": request_id, "methodId": method_id,
                "cols": cols, "rows": rows,
            }),
            Self::Create { cwd } => json!({
                "type": "session/new", "requestId": request_id, "cwd": cwd,
            }),
        }
    }
}

/// An auth terminal control action (protocol §5.4).
#[derive(Debug, Clone)]
pub(crate) enum TerminalControl {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
    Cancel,
}

/// The global commands' own admission, checked under the lock that commits
/// the record.
fn admit_global(state: &BridgeState, command: &GlobalCommand) -> Result<(), ProtocolError> {
    let Some(capabilities) = state.agent_capabilities.as_ref() else {
        return Err(unavailable());
    };
    let offered = |method_id: &str| {
        state.auth_methods.get(method_id).ok_or_else(|| {
            rejection(
                ErrorCode::InvalidRequest,
                format!("authentication method was not offered: {method_id}"),
            )
        })
    };
    match command {
        GlobalCommand::Authenticate { method_id } => {
            if matches!(offered(method_id)?, AuthMethod::Terminal(_)) {
                return Err(rejection(
                    ErrorCode::InvalidRequest,
                    "a terminal authentication method starts an auth terminal",
                ));
            }
        }
        GlobalCommand::Logout => {
            if capabilities.auth.logout.is_none() {
                return Err(rejection(
                    ErrorCode::Unsupported,
                    "Agent did not advertise support for auth/logout",
                ));
            }
        }
        GlobalCommand::AuthTerminal {
            method_id,
            cols,
            rows,
        } => {
            if !matches!(offered(method_id)?, AuthMethod::Terminal(_)) {
                return Err(rejection(
                    ErrorCode::InvalidRequest,
                    "the authentication method is handled by the Agent, not a terminal",
                ));
            }
            if *cols == 0 || *rows == 0 {
                return Err(rejection(
                    ErrorCode::InvalidRequest,
                    "terminal dimensions must be positive",
                ));
            }
            if running_record(state, |request| {
                request.operation == RequestOperation::AuthTerminal
            }) {
                return Err(rejection(
                    ErrorCode::Conflict,
                    "an auth terminal is already running",
                ));
            }
        }
        GlobalCommand::Create { .. } => {}
    }
    Ok(())
}

/// The session a create or fork response returns.
fn returned_session(response: &Value) -> RequestResult {
    RequestResult::Session {
        session_id: response
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        incarnation: response
            .pointer("/view/session/incarnation")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
    }
}

/// A rejection of a control action or read from the v1 handler.
fn handler_rejection(error: &BridgeRequestError) -> ProtocolError {
    let message = error
        .data
        .as_ref()
        .and_then(Value::as_str)
        .map_or(error.message.clone(), str::to_string);
    let code = match error.code {
        Some(code) if code == i32::from(agent_client_protocol::ErrorCode::MethodNotFound) => {
            ErrorCode::Unsupported
        }
        Some(code) if code == i32::from(agent_client_protocol::ErrorCode::InvalidParams) => {
            ErrorCode::InvalidRequest
        }
        _ => ErrorCode::Conflict,
    };
    rejection(code, message)
}

pub(crate) type Outcome<T> = Result<CommandResult<T>, ProtocolError>;

fn rejection(code: ErrorCode, message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(code, message)
}

fn unavailable() -> ProtocolError {
    rejection(ErrorCode::Unavailable, "no Agent connection in this epoch")
}

/// The error a finished record carries for a failure the bridge reported.
fn record_error(error: &BridgeRequestError) -> ProtocolError {
    let code = if error.code == Some(i32::from(agent_client_protocol::ErrorCode::MethodNotFound)) {
        ErrorCode::Unsupported
    } else {
        ErrorCode::AgentError
    };
    ProtocolError {
        code,
        message: error.message.clone(),
        data: Some(json!({ "code": error.code, "message": error.message, "data": error.data })),
    }
}

/// A synchronous admission failure from the bridge's own checks.
fn admission_error(error: &Error) -> ProtocolError {
    let message = error
        .data
        .as_ref()
        .and_then(Value::as_str)
        .map_or_else(|| error.message.clone(), str::to_string);
    let code = if error.code == agent_client_protocol::ErrorCode::MethodNotFound {
        ErrorCode::Unsupported
    } else if error.code == agent_client_protocol::ErrorCode::InvalidParams {
        ErrorCode::InvalidRequest
    } else {
        ErrorCode::Conflict
    };
    rejection(code, message)
}

fn position(state: &BridgeState) -> Position {
    Position {
        epoch: state.sessions.epoch().to_string(),
        seq: state.replication.log().seq(),
    }
}

/// The position after this critical section's own transition.
fn committed(state: &mut BridgeState) -> Position {
    state.commit();
    position(state)
}

fn result<T>(at: Position, scopes: Vec<String>, result: T) -> CommandResult<T> {
    CommandResult { at, scopes, result }
}

/// The answer for a known key whose subject is a request record.
fn duplicate_request(
    state: &BridgeState,
    request_id: &str,
    status: Option<FinalStatus>,
) -> RequestAccepted {
    if state.sessions.commands.request(request_id).is_some() {
        return RequestAccepted {
            request_id: request_id.to_string(),
            disposition: Disposition::Duplicate,
            result_expired: None,
            status: None,
        };
    }
    RequestAccepted {
        request_id: request_id.to_string(),
        disposition: Disposition::Duplicate,
        result_expired: Some(true),
        status: Some(match status {
            Some(FinalStatus::Request(status)) => status,
            // A record only leaves once it finished; nothing else is final.
            _ => RequestStatus::Failed,
        }),
    }
}

/// Looks the key up; `Ok(Some)` is the answer to a resend.
fn known_request(
    state: &BridgeState,
    key: &str,
    fingerprint: Fingerprint,
) -> Result<Option<RequestAccepted>, ProtocolError> {
    match state.sessions.commands.lookup(key, fingerprint) {
        IntentLookup::Unknown => Ok(None),
        IntentLookup::Full => Err(rejection(
            ErrorCode::IntentCapacity,
            "the intent table is full for this epoch",
        )),
        IntentLookup::Conflict => Err(rejection(
            ErrorCode::IntentConflict,
            "Idempotency-Key was used for a different command",
        )),
        IntentLookup::Known(intent) => match &intent.subject {
            Subject::Request(request_id) => {
                Ok(Some(duplicate_request(state, request_id, intent.status)))
            }
            Subject::Turn { .. } => Err(rejection(
                ErrorCode::IntentConflict,
                "Idempotency-Key was used for a turn",
            )),
        },
    }
}

/// The incarnation the tag names, when the bridge holds exactly that owner.
fn held_owner(state: &BridgeState, session_id: &str, tag: &OwnerTag) -> Result<u64, ProtocolError> {
    let held = state.replication.held_incarnation(session_id);
    if tag.epoch != state.sessions.epoch() {
        return Err(rejection(
            ErrorCode::OwnerMismatch,
            "If-Match names another epoch",
        ));
    }
    match (held, tag.incarnation) {
        (None, _) => Err(rejection(
            ErrorCode::NotFound,
            format!("session {session_id} is not held"),
        )),
        (Some(held), Some(incarnation)) if held == incarnation => Ok(held),
        _ => Err(rejection(
            ErrorCode::OwnerMismatch,
            "If-Match does not name the held incarnation",
        )),
    }
}

/// A running interaction response or session operation record already owns
/// this target; a second one would only race it.
fn running_record(state: &BridgeState, matches: impl Fn(&Request) -> bool) -> bool {
    state
        .sessions
        .commands
        .requests()
        .values()
        .any(|request| request.status == RequestStatus::Running && matches(request))
}

fn targets(request: &Request, session_id: &str) -> bool {
    request
        .target
        .as_ref()
        .is_some_and(|target| target.session_id == session_id)
}

fn agent_method_supported(state: &BridgeState, method: &str) -> Result<(), ProtocolError> {
    let Some(capabilities) = state.agent_capabilities.as_ref() else {
        return Err(unavailable());
    };
    let supported = match method {
        "session/load" => capabilities.load_session,
        "session/resume" => capabilities.session_capabilities.resume.is_some(),
        "session/fork" => capabilities.session_capabilities.fork.is_some(),
        "session/close" => capabilities.session_capabilities.close.is_some(),
        "session/delete" => capabilities.session_capabilities.delete.is_some(),
        _ => true,
    };
    if supported {
        Ok(())
    } else {
        Err(rejection(
            ErrorCode::Unsupported,
            format!("Agent did not advertise support for {method}"),
        ))
    }
}

/// The session operation commands' own admission, checked under the lock
/// that commits the record.
fn admit_held(
    state: &BridgeState,
    session_id: &str,
    incarnation: u64,
    command: &HeldCommand,
) -> Result<(), ProtocolError> {
    agent_method_supported(state, command.method())?;
    let Some(session) = state
        .sessions
        .active_session(session_id)
        .filter(|session| session.state.incarnation == incarnation)
    else {
        return Err(rejection(ErrorCode::Conflict, "session is not active"));
    };
    match command {
        HeldCommand::SetMode { mode_id } => {
            validate_session_mode_reference(session.live.modes(), mode_id)
                .map_err(|error| rejection(ErrorCode::InvalidRequest, error))?;
        }
        HeldCommand::SetConfig { config_id, value } => {
            validate_session_config_reference(session.live.config_options(), config_id, value)
                .map_err(|error| rejection(ErrorCode::InvalidRequest, error))?;
        }
        HeldCommand::Fork | HeldCommand::Close | HeldCommand::Delete => {}
    }
    if running_record(state, |request| {
        targets(request, session_id) && request.operation != RequestOperation::InteractionResponse
    }) {
        return Err(rejection(
            ErrorCode::Conflict,
            "another session operation is in progress",
        ));
    }
    state
        .sessions
        .can_begin(session_id, incarnation, command.kind().admission())
        .map_err(|error| admission_error(&mirror_error(error)))
}

fn record(
    request_id: &str,
    operation: RequestOperation,
    method: &str,
    key: &str,
    target: Option<RequestTarget>,
    params: BTreeMap<String, Value>,
) -> Request {
    Request {
        request_id: request_id.to_string(),
        operation,
        method: method.to_string(),
        client_intent_id: Some(key.to_string()),
        target,
        interaction_id: None,
        method_id: None,
        params,
        status: RequestStatus::Running,
        result: None,
        error: None,
        terminal: None,
    }
}

/// Sends a v1 business command through the bridge input; `None` when the
/// connection is gone.
fn dispatch(
    state: &BridgeState,
    input: impl FnOnce(oneshot::Sender<Result<Value, BridgeRequestError>>) -> BridgeInput,
) -> Option<oneshot::Receiver<Result<Value, BridgeRequestError>>> {
    let events = state.observer_events.as_ref()?;
    let (response, received) = oneshot::channel();
    events.send(input(response)).ok()?;
    Some(received)
}

fn business(
    command: Value,
) -> impl FnOnce(oneshot::Sender<Result<Value, BridgeRequestError>>) -> BridgeInput {
    move |response| BridgeInput::BusinessRequest { command, response }
}

/// Runs `then` under the bridge lock once a command's response arrives,
/// even after the HTTP caller went away.
fn on_response(
    state: Weak<BridgeLock>,
    response: oneshot::Receiver<Result<Value, BridgeRequestError>>,
    then: impl FnOnce(&mut BridgeState, Result<Value, BridgeRequestError>) + Send + 'static,
) {
    tokio::spawn(async move {
        let response = response.await.unwrap_or_else(|_| {
            Err(BridgeRequestError::internal(
                "bridge stopped before answering the command",
            ))
        });
        let Some(state) = state.upgrade() else {
            return;
        };
        then(&mut *state.lock().await, response);
    });
}

/// Finishes a record when its command's response arrives. A record that
/// already finished (a fork whose target became held) stays as it is.
fn finish_on_response(
    state: Weak<BridgeLock>,
    request_id: String,
    response: oneshot::Receiver<Result<Value, BridgeRequestError>>,
    outcome: impl FnOnce(Result<Value, BridgeRequestError>) -> Result<RequestResult, ProtocolError>
    + Send
    + 'static,
) {
    on_response(state, response, move |state, response| {
        state
            .sessions
            .commands
            .finish(&request_id, outcome(response), Instant::now());
        end_request_children(state, &request_id);
    });
}

fn done(response: Result<Value, BridgeRequestError>) -> Result<RequestResult, ProtocolError> {
    response
        .map(|_| RequestResult::Done)
        .map_err(|error| record_error(&error))
}

/// A resend of a command that returned an owner (open, fork) renews that
/// owner's lease (protocol §4.2), from the tombstone once the record is gone.
fn renew_returned_owner(state: &mut BridgeState, key: &str) {
    let owner = state.sessions.commands.intent(key).and_then(|intent| {
        intent
            .owner
            .and_then(|incarnation| {
                // Incarnations are unique within the epoch.
                let session = state
                    .sessions
                    .iter_states()
                    .find(|session| session.incarnation == incarnation)?;
                Some((session.session_id.clone(), incarnation))
            })
            .or_else(|| {
                // An open still running names its owner in the record's target.
                let Subject::Request(request_id) = &intent.subject else {
                    return None;
                };
                let request = state.sessions.commands.request(request_id)?;
                let target = request
                    .target
                    .as_ref()
                    .filter(|_| request.operation == RequestOperation::Open)?;
                Some((target.session_id.clone(), target.incarnation?))
            })
    });
    if let Some((session_id, incarnation)) = owner
        && state.replication.held_incarnation(&session_id) == Some(incarnation)
    {
        grant_first_subscription_lease(state, &session_id, incarnation);
    }
}

/// The owner a v2 command was admitted for must still be the session's
/// owner when the handler runs; otherwise the command would act on a
/// replacement (a v1 close and a new session with the same ID).
pub(super) fn require_admitted_owner(
    state: &BridgeState,
    command: &Value,
    session_id: &str,
) -> Result<(), Error> {
    let Some(expected) = command
        .get("expectedOwnerIncarnation")
        .and_then(Value::as_u64)
    else {
        return Ok(());
    };
    // `0` is a cold delete's `"<epoch>:0"`: the bridge holds no incarnation,
    // which is exactly when deletion takes the catalog path.
    let admitted = state
        .sessions
        .state(session_id)
        .map_or(0, |session| session.incarnation)
        == expected;
    if admitted {
        Ok(())
    } else {
        Err(Error::invalid_params().data("the admitted session owner was replaced"))
    }
}

/// Grants a first-subscription lease on an owner a command returned
/// (protocol §4.2). A stream that subscribes to the owner takes it over;
/// otherwise it lapses after the grace period.
pub(super) fn grant_first_subscription_lease(
    state: &mut BridgeState,
    session_id: &str,
    incarnation: u64,
) {
    let observer_id = state.first_leases.next;
    state.first_leases.next += 1;
    let lease = ObservationLease::new();
    let Ok(resources) = state.sessions.resources_mut(session_id, incarnation) else {
        return;
    };
    if !resources.observers.observe(observer_id, lease.clone()) {
        return;
    }
    state.first_leases.leases.push(FirstLease {
        session_id: session_id.to_string(),
        incarnation,
        observer_id,
        lease: lease.clone(),
    });
    refresh_observer_timers(state);
    let (Some(events), grace) = (state.observer_events.clone(), state.first_leases.grace) else {
        return;
    };
    let session_id = session_id.to_string();
    let withdrawn = lease.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = withdrawn.cancelled() => {}
            _ = tokio::time::sleep(grace) => {
                lease.cancel();
                let _ = events.send(BridgeInput::UnobserveSession { session_id, observer_id, lease });
            }
        }
    });
}

/// A stream subscribed to the owner: its own lease replaces the leases
/// commands granted.
pub(super) fn take_over_first_subscription_leases(
    state: &mut BridgeState,
    session_id: &str,
    incarnation: u64,
) {
    let (taken, kept) = std::mem::take(&mut state.first_leases.leases)
        .into_iter()
        .partition::<Vec<_>, _>(|lease| {
            lease.session_id == session_id && lease.incarnation == incarnation
        });
    state.first_leases.leases = kept;
    for lease in taken {
        lease.lease.cancel();
        if let Ok(resources) = state.sessions.resources_mut(session_id, incarnation) {
            resources
                .observers
                .unobserve(lease.observer_id, &lease.lease);
        }
    }
}

/// A lapsed first-subscription lease leaves the table with its observer.
pub(super) fn forget_first_subscription_lease(state: &mut BridgeState, observer_id: u64) {
    state
        .first_leases
        .leases
        .retain(|lease| lease.observer_id != observer_id);
}

/// The owner a create or fork returns is held: its record succeeds with the
/// new owner, and the owner gets its lease in the same transition, before
/// anything can retire it.
pub(super) fn returned_owner_held(
    state: &mut BridgeState,
    request_id: &str,
    session_id: &str,
    incarnation: u64,
) {
    if state.sessions.commands.finish(
        request_id,
        Ok(RequestResult::Session {
            session_id: session_id.to_string(),
            incarnation,
        }),
        Instant::now(),
    ) {
        grant_first_subscription_lease(state, session_id, incarnation);
    }
}

/// Frozen process terminals live exactly as long as their owner's revision:
/// a retired owner or a replaced revision releases them in the same commit.
pub(super) fn release_process_terminals(state: &mut BridgeState) {
    if state.process_terminals.is_empty() {
        return;
    }
    let sessions = &state.sessions;
    state
        .process_terminals
        .retain(|session_id, (incarnation, revision, _)| {
            sessions.state(session_id).is_some_and(|session| {
                session.incarnation == *incarnation
                    && session.history_revision.as_deref() == Some(revision.as_str())
                    && session.live.is_some()
            })
        });
}

/// Turn intents settle once their turn leaves the session: completed when it
/// left an outcome, otherwise failed.
pub(super) fn settle_turn_intents(state: &mut BridgeState) {
    let settled = state
        .sessions
        .commands
        .running_turns()
        .filter_map(|(key, session_id, incarnation, operation_id)| {
            let operation_id = operation_id?;
            let session = state
                .sessions
                .state(session_id)
                .filter(|session| session.incarnation == incarnation);
            match session {
                Some(session)
                    if session
                        .active_turn
                        .as_ref()
                        .is_some_and(|turn| turn.operation_id == operation_id) =>
                {
                    None
                }
                Some(session)
                    if session
                        .turn_outcomes
                        .iter()
                        .any(|outcome| outcome.operation_id == operation_id) =>
                {
                    Some((key.to_string(), TurnOutcomeStatus::Completed))
                }
                _ => Some((key.to_string(), TurnOutcomeStatus::Failed)),
            }
        })
        .collect::<Vec<_>>();
    for (key, status) in settled {
        state.sessions.commands.settle_turn(&key, status);
    }
}

/// The record of an ACP request the bridge sends outside a held session
/// (plan §2.4), opened in the turn that sends it. A v2 command's request is
/// its own record; any other one, such as a v1 client's, gets an
/// `agent_request` record.
pub(super) fn out_of_session_request(
    state: &mut BridgeState,
    operation_id: &str,
    method: &str,
    target: Option<RequestTarget>,
) {
    if state.sessions.commands.request(operation_id).is_some()
        || state.agent_requests.contains_key(operation_id)
    {
        return;
    }
    let request_id = state.sessions.commands.allocate_request_id();
    state.sessions.commands.open(Request {
        request_id: request_id.clone(),
        operation: RequestOperation::AgentRequest,
        method: method.to_string(),
        client_intent_id: None,
        target,
        interaction_id: None,
        method_id: None,
        params: BTreeMap::new(),
        status: RequestStatus::Running,
        result: None,
        error: None,
        terminal: None,
    });
    state
        .agent_requests
        .insert(operation_id.to_string(), request_id);
}

/// The record a request-scoped elicitation belongs to, while the request it
/// names still runs.
pub(super) fn running_parent(state: &BridgeState, operation_id: &str) -> Option<String> {
    let request_id = state
        .agent_requests
        .get(operation_id)
        .map_or(operation_id, String::as_str);
    state
        .sessions
        .commands
        .request(request_id)
        .filter(|request| request.status == RequestStatus::Running)
        .map(|request| request.request_id.clone())
}

/// An out-of-session request ended. Its `agent_request` record finishes
/// with it, a read's record leaves at once (protocol §6), and the
/// request-scoped elicitations it parents end (protocol §3.4). Their URL
/// flows stay.
pub(super) fn out_of_session_request_ended(
    state: &mut BridgeState,
    operation_id: &str,
    result: &Result<(), Error>,
) {
    let agent_request = state.agent_requests.remove(operation_id);
    let request_id = agent_request
        .clone()
        .unwrap_or_else(|| operation_id.to_string());
    if let Some(request_id) = &agent_request {
        let outcome = result
            .as_ref()
            .map(|_| RequestResult::Done)
            .map_err(acp_error);
        state
            .sessions
            .commands
            .finish(request_id, outcome, Instant::now());
        if state
            .sessions
            .commands
            .request(request_id)
            .is_some_and(|request| request.method == "session/list")
        {
            let _ = state.sessions.commands.acknowledge(request_id);
        }
    }
    end_request_children(state, &request_id);
}

/// A request ended: the request-scoped elicitations it parents end with it
/// in the same transition, and the Agent gets `cancel` (protocol §3.4).
/// Their URL flows stay.
fn end_request_children(state: &mut BridgeState, request_id: &str) {
    let children = state
        .sessions
        .request_elicitations
        .values()
        .filter(|pending| pending.request_id.as_deref() == Some(request_id))
        .map(|pending| pending.interaction_id.clone())
        .collect::<Vec<_>>();
    let epoch = state.sessions.epoch().to_string();
    for interaction_id in children {
        if let Some(pending) = state.request_elicitations.remove(&interaction_id) {
            let _ = state
                .sessions
                .resolve_elicitation(&epoch, None, &interaction_id, None);
            let _ = pending
                .sender
                .send(CreateElicitationResponse::new(ElicitationAction::Cancel));
        }
    }
}

/// The bounded tail of an auth terminal record's output, in bytes.
pub(crate) const AUTH_TERMINAL_OUTPUT_LIMIT: usize = 64 * 1024;

/// An auth terminal event (`bridge/auth_terminal_*`) reaches its record:
/// output extends the bounded tail, and the exit finishes the record with its
/// status. `methodId` stays from start to end.
pub(super) fn auth_terminal_event(state: &mut BridgeState, event: &Value) {
    let Some(request_id) = event.get("requestId").and_then(Value::as_str) else {
        return;
    };
    let commands = &mut state.sessions.commands;
    if commands
        .request(request_id)
        .is_none_or(|request| request.operation != RequestOperation::AuthTerminal)
    {
        return;
    }
    match event.get("type").and_then(Value::as_str) {
        Some("bridge/auth_terminal_output") => {
            let data = event
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default();
            commands.update_terminal(request_id, |terminal| {
                terminal.output.push_str(data);
                if terminal.output.len() > AUTH_TERMINAL_OUTPUT_LIMIT {
                    let mut cut = terminal.output.len() - AUTH_TERMINAL_OUTPUT_LIMIT;
                    while !terminal.output.is_char_boundary(cut) {
                        cut += 1;
                    }
                    terminal.output.drain(..cut);
                    terminal.truncated = true;
                }
            });
        }
        Some("bridge/auth_terminal_exited") => {
            let message = event
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string);
            commands.update_terminal(request_id, |terminal| {
                terminal.exit_code = event
                    .get("exitCode")
                    .and_then(Value::as_i64)
                    .and_then(|code| i32::try_from(code).ok());
                terminal.signal = event
                    .get("signal")
                    .and_then(Value::as_i64)
                    .and_then(|signal| i32::try_from(signal).ok());
                terminal.message = message.clone();
            });
            let now = Instant::now();
            match event.get("status").and_then(Value::as_str) {
                Some("succeeded") => {
                    commands.finish(request_id, Ok(RequestResult::Done), now);
                }
                Some("cancelled") => {
                    commands.cancel(request_id, now);
                }
                _ => {
                    let error = ProtocolError::new(
                        ErrorCode::AgentError,
                        message.unwrap_or_else(|| "terminal authentication failed".to_string()),
                    );
                    commands.finish(request_id, Err(error), now);
                }
            }
        }
        _ => {}
    }
}

/// A finished record's error for an ACP error the Agent returned.
fn acp_error(error: &Error) -> ProtocolError {
    record_error(&BridgeRequestError::from_acp(error))
}

impl ReplicationSource {
    /// `set_mode`, `set_config`, `fork`, `close` and held `delete`.
    pub(crate) async fn held_command(
        &self,
        key: String,
        session_id: String,
        tag: OwnerTag,
        command: HeldCommand,
    ) -> Outcome<RequestAccepted> {
        let lock = self.live().ok_or_else(unavailable)?;
        let mut state = lock.lock().await;
        let mut target = vec![("sessionId", session_id.as_str())];
        if let HeldCommand::SetConfig { config_id, .. } = &command {
            target.push(("configId", config_id.as_str()));
        }
        let fingerprint = Fingerprint::of(command.name(), &target, &tag.text(), &command.body());
        let scopes = vec![GLOBAL.to_string(), session_scope(&session_id)];
        if let Some(duplicate) = known_request(&state, &key, fingerprint)? {
            renew_returned_owner(&mut state, &key);
            return Ok(result(committed(&mut state), scopes, duplicate));
        }
        let incarnation = held_owner(&state, &session_id, &tag)?;
        admit_held(&state, &session_id, incarnation, &command)?;
        let request_id = state.sessions.commands.allocate_request_id();
        let Some(response) = dispatch(
            &state,
            business(command.command(&request_id, &session_id, incarnation)),
        ) else {
            return Err(unavailable());
        };
        state.sessions.commands.open(record(
            &request_id,
            command.operation(),
            command.method(),
            &key,
            Some(RequestTarget {
                session_id: session_id.clone(),
                incarnation: Some(incarnation),
            }),
            command.params(),
        ));
        state
            .sessions
            .commands
            .admit(&key, fingerprint, Subject::Request(request_id.clone()));
        let at = committed(&mut state);
        finish_on_response(
            Arc::downgrade(&lock),
            request_id.clone(),
            response,
            move |response| match command {
                HeldCommand::Fork => response
                    .map(|response| returned_session(&response))
                    .map_err(|error| record_error(&error)),
                _ => done(response),
            },
        );
        Ok(result(
            at,
            scopes,
            RequestAccepted {
                request_id,
                disposition: Disposition::Accepted,
                result_expired: None,
                status: None,
            },
        ))
    }

    /// `POST /api/v2/sessions/{id}/open`: the only command that materializes
    /// a session (protocol §4.2).
    pub(crate) async fn open(
        &self,
        key: String,
        session_id: String,
        cwd: String,
    ) -> Outcome<RequestAccepted> {
        let lock = self.live().ok_or_else(unavailable)?;
        let mut state = lock.lock().await;
        let body = json!({ "cwd": cwd });
        let fingerprint = Fingerprint::of("open", &[("sessionId", session_id.as_str())], "", &body);
        let scopes = vec![GLOBAL.to_string(), session_scope(&session_id)];
        if let Some(duplicate) = known_request(&state, &key, fingerprint)? {
            renew_returned_owner(&mut state, &key);
            return Ok(result(committed(&mut state), scopes, duplicate));
        }
        let params = BTreeMap::from([("cwd".to_string(), json!(cwd))]);
        let request_id = state.sessions.commands.allocate_request_id();
        if let Some(incarnation) = state.replication.held_incarnation(&session_id) {
            let method = if state
                .agent_capabilities
                .as_ref()
                .is_some_and(|caps| caps.load_session)
            {
                "session/load"
            } else {
                "session/resume"
            };
            let target = RequestTarget {
                session_id: session_id.clone(),
                incarnation: Some(incarnation),
            };
            state.sessions.commands.open(record(
                &request_id,
                RequestOperation::Open,
                method,
                &key,
                Some(target),
                params,
            ));
            // Admitted before it finishes, so the tombstone keeps its status.
            state
                .sessions
                .commands
                .admit(&key, fingerprint, Subject::Request(request_id.clone()));
            state.sessions.commands.finish(
                &request_id,
                Ok(RequestResult::Session {
                    session_id: session_id.clone(),
                    incarnation,
                }),
                Instant::now(),
            );
            grant_first_subscription_lease(&mut state, &session_id, incarnation);
            return Ok(result(committed(&mut state), scopes, accepted(request_id)));
        }
        let kind = if state
            .agent_capabilities
            .as_ref()
            .is_some_and(|caps| caps.load_session)
        {
            RuntimeSessionOperationKind::Load
        } else {
            RuntimeSessionOperationKind::Resume
        };
        let method = if kind == RuntimeSessionOperationKind::Load {
            "session/load"
        } else {
            "session/resume"
        };
        agent_method_supported(&state, method)?;
        if state.catalog_deletions.contains_key(&session_id) {
            return Err(rejection(ErrorCode::Conflict, "session is being deleted"));
        }
        if state.observer_events.is_none() {
            return Err(unavailable());
        }
        let reservation = reserve_attachment_locked(
            &mut state,
            &session_id,
            kind,
            Some(cwd.as_str()),
            &request_id,
            None,
        )
        .map_err(|error| admission_error(&error))?;
        let incarnation = reservation.incarnation();
        let prepared = PreparedAttachment {
            operation_id: request_id.clone(),
            reservation,
        };
        let command = json!({
            "type": method, "requestId": request_id, "sessionId": session_id, "cwd": cwd,
        });
        let Some(response) = dispatch(&state, move |response| BridgeInput::PreparedAttachment {
            command,
            reservation: prepared,
            response: Some(response),
        }) else {
            return Err(unavailable());
        };
        state.sessions.commands.open(record(
            &request_id,
            RequestOperation::Open,
            method,
            &key,
            Some(RequestTarget {
                session_id: session_id.clone(),
                incarnation: Some(incarnation),
            }),
            params,
        ));
        state
            .sessions
            .commands
            .admit(&key, fingerprint, Subject::Request(request_id.clone()));
        grant_first_subscription_lease(&mut state, &session_id, incarnation);
        let at = committed(&mut state);
        finish_on_response(
            Arc::downgrade(&lock),
            request_id.clone(),
            response,
            move |response| {
                response
                    .map(|_| RequestResult::Session {
                        session_id,
                        incarnation,
                    })
                    .map_err(|error| record_error(&error))
            },
        );
        Ok(result(at, scopes, accepted(request_id)))
    }

    /// `POST /api/v2/sessions/{id}/turns` with the history CAS in `If-Match`.
    pub(crate) async fn start_turn(
        &self,
        key: String,
        session_id: String,
        tag: OwnerTag,
        prompt: Vec<Value>,
    ) -> Outcome<TurnAccepted> {
        let lock = self.live().ok_or_else(unavailable)?;
        let fingerprint = Fingerprint::of(
            "turn",
            &[("sessionId", session_id.as_str())],
            &tag.text(),
            &json!({ "prompt": prompt }),
        );
        let scopes = vec![session_scope(&session_id)];
        let response = {
            let mut state = lock.lock().await;
            match state.sessions.commands.lookup(&key, fingerprint) {
                IntentLookup::Full => {
                    return Err(rejection(
                        ErrorCode::IntentCapacity,
                        "the intent table is full for this epoch",
                    ));
                }
                IntentLookup::Conflict => {
                    return Err(rejection(
                        ErrorCode::IntentConflict,
                        "Idempotency-Key was used for a different command",
                    ));
                }
                IntentLookup::Known(intent) => {
                    let answer = known_turn(&state, intent)?;
                    return Ok(result(position(&state), scopes, answer));
                }
                IntentLookup::Unknown => {}
            }
            let Some(revision) = tag.history_revision.clone() else {
                return Err(rejection(
                    ErrorCode::PreconditionRequired,
                    "a turn needs If-Match with the history revision",
                ));
            };
            let incarnation = held_owner(
                &state,
                &session_id,
                &OwnerTag {
                    history_revision: None,
                    ..tag.clone()
                },
            )?;
            if state
                .sessions
                .state(&session_id)
                .and_then(|session| session.history_revision.as_deref())
                != Some(revision.as_str())
            {
                return Err(rejection(
                    ErrorCode::HistoryMismatch,
                    "If-Match does not name the current history revision",
                ));
            }
            let Some(events) = state.observer_events.clone() else {
                return Err(unavailable());
            };
            let (response, received) = oneshot::channel();
            events
                .send(BridgeInput::TurnRequest {
                    session_id: session_id.clone(),
                    history_revision: revision,
                    client_intent_id: key.clone(),
                    prompt,
                    response,
                })
                .map_err(|_| unavailable())?;
            let attempt = state.sessions.commands.admit(
                &key,
                fingerprint,
                Subject::Turn {
                    session_id: session_id.clone(),
                    incarnation,
                    operation_id: None,
                },
            );
            (received, attempt)
        };
        // The bridge settles the intent when it admits or refuses the turn;
        // this caller may disconnect at any point from here on.
        let (response, attempt) = response;
        let response = response
            .await
            .unwrap_or_else(|_| Err("bridge stopped before admitting the turn".to_string()));
        let mut state = lock.lock().await;
        match response {
            Ok(value) => {
                let operation_id = value["operationId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                // Only this caller's own admission answer reaches here; the
                // bridge normally settled the intent already.
                state.sessions.commands.turn_admitted(&key, &operation_id);
                let disposition = if value["disposition"] == "duplicate" {
                    Disposition::Duplicate
                } else {
                    Disposition::Accepted
                };
                Ok(result(
                    committed(&mut state),
                    scopes,
                    TurnAccepted {
                        operation_id,
                        disposition,
                        result_expired: None,
                        status: None,
                    },
                ))
            }
            Err(message) => {
                // The turn never reached the handler (refused at ingress) or
                // the handler already withdrew it.
                state.sessions.commands.withdraw_attempt(&key, attempt);
                Err(turn_rejection(message))
            }
        }
    }

    /// `POST /api/v2/sessions/{id}/turns/{operationId}/cancel`: a control
    /// action.
    pub(crate) async fn cancel_turn(
        &self,
        session_id: String,
        tag: OwnerTag,
        operation_id: String,
    ) -> Outcome<EmptyBody> {
        let lock = self.live().ok_or_else(unavailable)?;
        let response = {
            let state = lock.lock().await;
            held_owner(&state, &session_id, &tag)?;
            dispatch(
                &state,
                business(json!({
                    "type": "session/cancel",
                    "sessionId": session_id,
                    "expectedOperationId": operation_id,
                })),
            )
            .ok_or_else(unavailable)?
        };
        let response = response.await.map_err(|_| unavailable())?;
        response.map_err(|error| {
            rejection(
                ErrorCode::Conflict,
                error
                    .data
                    .as_ref()
                    .and_then(Value::as_str)
                    .map_or(error.message.clone(), str::to_string),
            )
        })?;
        let state = lock.lock().await;
        Ok(result(
            position(&state),
            vec![session_scope(&session_id)],
            EmptyBody {},
        ))
    }

    /// `POST /api/v2/interactions/{interactionId}`. The record follows
    /// delivery: it succeeds only once the Agent has the response.
    pub(crate) async fn respond(
        &self,
        key: String,
        interaction_id: String,
        tag: OwnerTag,
        body: InteractionResponseBody,
    ) -> Outcome<RequestAccepted> {
        let lock = self.live().ok_or_else(unavailable)?;
        let mut state = lock.lock().await;
        let body_value = serde_json::to_value(&body).unwrap_or_default();
        let fingerprint = Fingerprint::of(
            "interaction_response",
            &[("interactionId", interaction_id.as_str())],
            &tag.text(),
            &body_value,
        );
        let mut scopes = vec![GLOBAL.to_string()];
        let owner = interaction_owner(&state, &interaction_id);
        if let Some(Some(owner)) = &owner {
            scopes.push(session_scope(&owner.session_id));
        }
        if let Some(duplicate) = known_request(&state, &key, fingerprint)? {
            return Ok(result(position(&state), scopes, duplicate));
        }
        if tag.epoch != state.sessions.epoch() {
            return Err(rejection(
                ErrorCode::OwnerMismatch,
                "If-Match names another epoch",
            ));
        }
        let Some(owner) = owner else {
            return Err(rejection(
                ErrorCode::Conflict,
                "the interaction is no longer pending",
            ));
        };
        if tag.incarnation != owner.as_ref().map(|owner| owner.incarnation) {
            return Err(rejection(
                ErrorCode::OwnerMismatch,
                "If-Match does not name the interaction's owner",
            ));
        }
        if running_record(&state, |request| {
            request.interaction_id.as_deref() == Some(interaction_id.as_str())
        }) {
            return Err(rejection(
                ErrorCode::Conflict,
                "the interaction is already being answered",
            ));
        }
        let (command, method, params) = match &body {
            InteractionResponseBody::Permission { outcome } => {
                let Some((_, pending)) = state.sessions.permission(&interaction_id) else {
                    return Err(rejection(
                        ErrorCode::InvalidRequest,
                        "the interaction is not a permission request",
                    ));
                };
                let option_id = outcome.get("optionId").and_then(Value::as_str);
                match outcome.get("outcome").and_then(Value::as_str) {
                    Some("selected")
                        if option_id.is_some_and(|id| pending.option_ids.contains(id)) => {}
                    Some("cancelled") => {}
                    _ => {
                        return Err(rejection(
                            ErrorCode::InvalidRequest,
                            "the permission outcome is not one the Agent offered",
                        ));
                    }
                }
                (
                    json!({
                        "type": "permission/respond", "permissionId": interaction_id,
                        "outcome": outcome,
                    }),
                    "session/request_permission",
                    BTreeMap::from([(
                        "optionId".to_string(),
                        option_id.map_or(json!("cancelled"), |id| json!(id)),
                    )]),
                )
            }
            InteractionResponseBody::Elicitation { response } => {
                let Some((_, pending)) = pending_elicitation(&state, &interaction_id) else {
                    return Err(rejection(
                        ErrorCode::InvalidRequest,
                        "the interaction is not an elicitation",
                    ));
                };
                validate_elicitation_response_value(&pending.request, response)
                    .map_err(|error| rejection(ErrorCode::InvalidRequest, error))?;
                (
                    json!({
                        "type": "elicitation/respond", "elicitationId": interaction_id,
                        "response": response,
                    }),
                    "elicitation/create",
                    BTreeMap::from([(
                        "action".to_string(),
                        response.get("action").cloned().unwrap_or(Value::Null),
                    )]),
                )
            }
        };
        let request_id = state.sessions.commands.allocate_request_id();
        let mut command = command;
        command["requestId"] = json!(request_id);
        let Some(response) = dispatch(&state, business(command)) else {
            return Err(unavailable());
        };
        let mut request = record(
            &request_id,
            RequestOperation::InteractionResponse,
            method,
            &key,
            owner.as_ref().map(|owner| RequestTarget {
                session_id: owner.session_id.clone(),
                incarnation: Some(owner.incarnation),
            }),
            params,
        );
        request.interaction_id = Some(interaction_id);
        state.sessions.commands.open(request);
        state
            .sessions
            .commands
            .admit(&key, fingerprint, Subject::Request(request_id.clone()));
        let at = committed(&mut state);
        finish_on_response(
            Arc::downgrade(&lock),
            request_id.clone(),
            response,
            |response| {
                response.map(|_| RequestResult::Done).map_err(|error| {
                    let mut ended = ProtocolError::new(
                        ErrorCode::InteractionEnded,
                        "the interaction ended before the response was delivered",
                    );
                    ended.data = Some(json!({
                        "reason": error
                            .data
                            .as_ref()
                            .and_then(Value::as_str)
                            .map_or(error.message.clone(), str::to_string),
                    }));
                    ended
                })
            },
        );
        Ok(result(at, scopes, accepted(request_id)))
    }

    /// `DELETE /api/v2/requests/{requestId}`: a control action.
    pub(crate) async fn acknowledge_request(&self, request_id: String) -> Outcome<EmptyBody> {
        let lock = self.live().ok_or_else(unavailable)?;
        let mut state = lock.lock().await;
        match state.sessions.commands.acknowledge(&request_id) {
            Ok(()) => Ok(result(
                committed(&mut state),
                vec![GLOBAL.to_string()],
                EmptyBody {},
            )),
            Err(AcknowledgeError::NotFound) => Err(rejection(
                ErrorCode::NotFound,
                format!("request {request_id} is not public"),
            )),
            Err(AcknowledgeError::Running) => Err(rejection(
                ErrorCode::Conflict,
                "a running request cannot be acknowledged",
            )),
        }
    }

    /// `DELETE /api/v2/sessions/{id}/url-flows/{interactionId}`: a control
    /// action on a finished URL flow.
    pub(crate) async fn acknowledge_url_flow(
        &self,
        session_id: String,
        tag: OwnerTag,
        interaction_id: String,
    ) -> Outcome<EmptyBody> {
        let lock = self.live().ok_or_else(unavailable)?;
        let mut state = lock.lock().await;
        let incarnation = held_owner(&state, &session_id, &tag)?;
        let epoch = state.sessions.epoch().to_string();
        match state
            .sessions
            .acknowledge_url_flow(&epoch, &session_id, incarnation, &interaction_id)
        {
            Ok(()) => Ok(result(
                committed(&mut state),
                vec![session_scope(&session_id)],
                EmptyBody {},
            )),
            Err(UrlFlowAcknowledgement::Waiting) => Err(rejection(
                ErrorCode::Conflict,
                "the URL flow is still waiting",
            )),
            Err(UrlFlowAcknowledgement::Unknown) => Err(rejection(
                ErrorCode::NotFound,
                format!("URL flow {interaction_id} is not public"),
            )),
        }
    }

    /// `POST /api/v2/agent/authenticate`, `…/logout`, `…/auth-terminals` and
    /// `POST /api/v2/sessions` (protocol §5.4).
    pub(crate) async fn global_command(
        &self,
        key: String,
        command: GlobalCommand,
    ) -> Outcome<RequestAccepted> {
        let lock = self.live().ok_or_else(unavailable)?;
        let mut state = lock.lock().await;
        let fingerprint = Fingerprint::of(command.name(), &[], "", &command.body());
        let scopes = vec![GLOBAL.to_string()];
        if let Some(duplicate) = known_request(&state, &key, fingerprint)? {
            renew_returned_owner(&mut state, &key);
            return Ok(result(committed(&mut state), scopes, duplicate));
        }
        admit_global(&state, &command)?;
        let request_id = state.sessions.commands.allocate_request_id();
        let Some(response) = dispatch(&state, business(command.command(&request_id))) else {
            return Err(unavailable());
        };
        let mut request = record(
            &request_id,
            command.operation(),
            command.method(),
            &key,
            None,
            command.params(),
        );
        request.method_id = command.method_id();
        if let GlobalCommand::AuthTerminal { .. } = command {
            request.terminal = Some(Default::default());
        }
        state.sessions.commands.open(request);
        state
            .sessions
            .commands
            .admit(&key, fingerprint, Subject::Request(request_id.clone()));
        let at = committed(&mut state);
        let id = request_id.clone();
        on_response(Arc::downgrade(&lock), response, move |state, response| {
            let outcome = match command {
                GlobalCommand::Create { .. } => response
                    .map(|response| returned_session(&response))
                    .map_err(|error| record_error(&error)),
                // A started terminal finishes with its exit.
                GlobalCommand::AuthTerminal { .. } => match response {
                    Ok(_) => return,
                    Err(error) => Err(record_error(&error)),
                },
                _ => done(response),
            };
            state.sessions.commands.finish(&id, outcome, Instant::now());
            end_request_children(state, &id);
        });
        Ok(result(at, scopes, accepted(request_id)))
    }

    /// `POST /api/v2/agent/auth-terminals/{requestId}/input`, `…/resize` and
    /// `…/cancel`: control actions on a running auth terminal record.
    pub(crate) async fn auth_terminal_control(
        &self,
        request_id: String,
        control: TerminalControl,
    ) -> Outcome<EmptyBody> {
        let lock = self.live().ok_or_else(unavailable)?;
        let response = {
            let state = lock.lock().await;
            match state.sessions.commands.request(&request_id) {
                Some(request) if request.operation == RequestOperation::AuthTerminal => {
                    if request.status != RequestStatus::Running {
                        return Err(rejection(
                            ErrorCode::Conflict,
                            "the auth terminal already ended",
                        ));
                    }
                }
                _ => {
                    return Err(rejection(
                        ErrorCode::NotFound,
                        format!("auth terminal {request_id} is not public"),
                    ));
                }
            }
            let command = match control {
                TerminalControl::Input { data } => json!({
                    "type": "auth/terminal_input", "requestId": request_id, "data": data,
                }),
                TerminalControl::Resize { cols, rows } => json!({
                    "type": "auth/terminal_resize", "requestId": request_id,
                    "cols": cols, "rows": rows,
                }),
                TerminalControl::Cancel => {
                    json!({ "type": "auth/terminal_cancel", "requestId": request_id })
                }
            };
            dispatch(&state, business(command)).ok_or_else(unavailable)?
        };
        response
            .await
            .map_err(|_| unavailable())?
            .map_err(|error| handler_rejection(&error))?;
        let state = lock.lock().await;
        Ok(result(
            position(&state),
            vec![GLOBAL.to_string()],
            EmptyBody {},
        ))
    }

    /// `DELETE /api/v2/sessions/{id}` with `If-Match: "<epoch>:0"`: deletes a
    /// session the bridge does not hold, without loading it (protocol §5.3).
    pub(crate) async fn cold_delete(
        &self,
        key: String,
        session_id: String,
        tag: OwnerTag,
    ) -> Outcome<RequestAccepted> {
        let lock = self.live().ok_or_else(unavailable)?;
        let mut state = lock.lock().await;
        let fingerprint = Fingerprint::of(
            "delete",
            &[("sessionId", session_id.as_str())],
            &tag.text(),
            &json!({}),
        );
        let scopes = vec![GLOBAL.to_string()];
        if let Some(duplicate) = known_request(&state, &key, fingerprint)? {
            return Ok(result(committed(&mut state), scopes, duplicate));
        }
        if tag.epoch != state.sessions.epoch() {
            return Err(rejection(
                ErrorCode::OwnerMismatch,
                "If-Match names another epoch",
            ));
        }
        if state.replication.held_incarnation(&session_id).is_some()
            || state
                .sessions
                .state(&session_id)
                .is_some_and(|session| session.incarnation != 0)
        {
            return Err(rejection(
                ErrorCode::OwnerMismatch,
                format!("the bridge holds session {session_id}"),
            ));
        }
        agent_method_supported(&state, "session/delete")?;
        if state.catalog_deletions.contains_key(&session_id)
            || running_record(&state, |request| targets(request, &session_id))
        {
            return Err(rejection(
                ErrorCode::Conflict,
                "another operation on this session is in progress",
            ));
        }
        let request_id = state.sessions.commands.allocate_request_id();
        let Some(response) = dispatch(
            &state,
            business(json!({
                "type": "session/delete", "requestId": request_id, "sessionId": session_id,
                "expectedOwnerIncarnation": 0,
            })),
        ) else {
            return Err(unavailable());
        };
        state.sessions.commands.open(record(
            &request_id,
            RequestOperation::Delete,
            "session/delete",
            &key,
            Some(RequestTarget {
                session_id,
                incarnation: None,
            }),
            BTreeMap::new(),
        ));
        state
            .sessions
            .commands
            .admit(&key, fingerprint, Subject::Request(request_id.clone()));
        let at = committed(&mut state);
        finish_on_response(Arc::downgrade(&lock), request_id.clone(), response, done);
        Ok(result(at, scopes, accepted(request_id)))
    }

    /// `GET /api/v2/catalog`: one page of the Agent's `session/list`. The read
    /// leaves nothing behind but the record of its ACP call while that call
    /// runs (protocol §6).
    pub(crate) async fn catalog(
        &self,
        cursor: Option<String>,
        revision: Option<String>,
    ) -> Result<Value, ProtocolError> {
        let lock = self.live().ok_or_else(unavailable)?;
        let (response, request_id) = {
            let mut state = lock.lock().await;
            let Some(capabilities) = state.agent_capabilities.as_ref() else {
                return Err(unavailable());
            };
            if capabilities.session_capabilities.list.is_none() {
                return Err(rejection(
                    ErrorCode::Unsupported,
                    "Agent did not advertise support for session/list",
                ));
            }
            let current = state.catalog_revision.to_string();
            if revision
                .as_ref()
                .is_some_and(|revision| revision != &current)
            {
                return Err(catalog_changed());
            }
            let request_id = state.sessions.commands.allocate_request_id();
            let command = json!({
                "type": "session/list", "requestId": request_id, "cursor": cursor,
                "expectedCatalogRevision": format!("{}:{current}", state.sessions.epoch()),
            });
            let response = dispatch(&state, business(command)).ok_or_else(unavailable)?;
            let mut request = record(
                &request_id,
                RequestOperation::AgentRequest,
                "session/list",
                "",
                None,
                BTreeMap::new(),
            );
            request.client_intent_id = None;
            state.sessions.commands.open(request);
            (response, request_id)
        };
        // The record ends with its call, whether or not this caller waits.
        let (answer, answered) = oneshot::channel();
        on_response(Arc::downgrade(&lock), response, move |state, response| {
            let commands = &mut state.sessions.commands;
            commands.finish(
                &request_id,
                response
                    .as_ref()
                    .map(|_| RequestResult::Done)
                    .map_err(record_error),
                Instant::now(),
            );
            let _ = commands.acknowledge(&request_id);
            end_request_children(state, &request_id);
            let _ = answer.send(response);
        });
        let response = answered.await.map_err(|_| unavailable())?;
        let page = response.map_err(|error| {
            if error.data.as_ref().and_then(|data| data.get("kind"))
                == Some(&json!("session_catalog_changed"))
            {
                catalog_changed()
            } else if error.code.is_some_and(|code| {
                code != i32::from(agent_client_protocol::ErrorCode::InvalidParams)
            }) {
                record_error(&error)
            } else {
                handler_rejection(&error)
            }
        })?;
        let revision = page
            .get("catalogRevision")
            .and_then(Value::as_str)
            .and_then(|revision| revision.rsplit(':').next())
            .unwrap_or_default()
            .to_string();
        Ok(json!({
            "revision": revision,
            "sessions": page.get("sessions").cloned().unwrap_or_else(|| json!([])),
            "nextCursor": page.get("nextCursor").cloned().unwrap_or(Value::Null),
        }))
    }

    /// `DELETE /api/v2/url-flows/{interactionId}`: a control action on a
    /// finished global URL flow.
    pub(crate) async fn acknowledge_global_url_flow(
        &self,
        interaction_id: String,
    ) -> Outcome<EmptyBody> {
        let lock = self.live().ok_or_else(unavailable)?;
        let mut state = lock.lock().await;
        let epoch = state.sessions.epoch().to_string();
        match state
            .sessions
            .acknowledge_request_url_flow(&epoch, &interaction_id)
        {
            Ok(()) => Ok(result(
                committed(&mut state),
                vec![GLOBAL.to_string()],
                EmptyBody {},
            )),
            Err(UrlFlowAcknowledgement::Waiting) => Err(rejection(
                ErrorCode::Conflict,
                "the URL flow is still waiting",
            )),
            Err(UrlFlowAcknowledgement::Unknown) => Err(rejection(
                ErrorCode::NotFound,
                format!("URL flow {interaction_id} is not public"),
            )),
        }
    }

    /// The v1 view of exactly the tagged owner, for the immutable and
    /// workspace reads. Never loads anything.
    pub(crate) async fn owner_view(
        &self,
        session_id: &str,
        tag: Option<&OwnerTag>,
    ) -> Result<Value, ProtocolError> {
        let lock = self.live().ok_or_else(unavailable)?;
        let mut state = lock.lock().await;
        let incarnation = match tag {
            Some(tag) => held_owner(&state, session_id, tag)?,
            None => state
                .replication
                .held_incarnation(session_id)
                .ok_or_else(|| {
                    rejection(
                        ErrorCode::NotFound,
                        format!("session {session_id} is not held"),
                    )
                })?,
        };
        session_view_value(&mut state, session_id, incarnation)
            .map_err(|error| admission_error(&error))
    }

    /// The terminals a process page of `revision` shows: frozen at the first
    /// read of that revision, so an immutable page never follows later output.
    pub(crate) async fn process_terminals(
        &self,
        session_id: &str,
        revision: &str,
        current: Value,
    ) -> Result<Value, ProtocolError> {
        let lock = self.live().ok_or_else(unavailable)?;
        let mut state = lock.lock().await;
        let state = &mut *state;
        let Some(incarnation) = state.replication.held_incarnation(session_id) else {
            return Err(rejection(
                ErrorCode::Gone,
                "the history revision is no longer retained",
            ));
        };
        // An overtaken read of an older revision is gone; it must never
        // replace the freeze a newer revision already published.
        if state
            .sessions
            .state(session_id)
            .and_then(|session| session.history_revision.as_deref())
            != Some(revision)
        {
            return Err(rejection(
                ErrorCode::Gone,
                "the history revision is no longer retained",
            ));
        }
        let frozen = state
            .process_terminals
            .entry(session_id.to_string())
            .or_insert_with(|| (incarnation, revision.to_string(), Default::default()));
        if frozen.0 != incarnation || frozen.1 != revision {
            *frozen = (incarnation, revision.to_string(), Default::default());
        }
        if let Value::Object(current) = current {
            for (terminal_id, terminal) in current {
                frozen.2.entry(terminal_id).or_insert(terminal);
            }
        }
        Ok(Value::Object(frozen.2.clone()))
    }

    /// A v1 business read through the bridge input (workspace files).
    pub(crate) async fn read(&self, command: Value) -> Result<Value, ProtocolError> {
        let lock = self.live().ok_or_else(unavailable)?;
        let response = {
            let state = lock.lock().await;
            dispatch(&state, business(command)).ok_or_else(unavailable)?
        };
        response
            .await
            .map_err(|_| unavailable())?
            .map_err(|error| handler_rejection(&error))
    }
}

fn catalog_changed() -> ProtocolError {
    rejection(
        ErrorCode::CatalogChanged,
        "the catalog changed; read it again from the first page",
    )
}

fn accepted(request_id: String) -> RequestAccepted {
    RequestAccepted {
        request_id,
        disposition: Disposition::Accepted,
        result_expired: None,
        status: None,
    }
}

/// `None`: no such pending interaction. `Some(None)`: a request-scoped one.
fn interaction_owner(
    state: &BridgeState,
    interaction_id: &str,
) -> Option<Option<SessionResourceOwner>> {
    if let Some((owner, _)) = state.sessions.permission(interaction_id) {
        return Some(Some(owner.clone()));
    }
    pending_elicitation(state, interaction_id).map(|(owner, _)| owner.cloned())
}

/// The answer for a resent turn key (recovery order: `turn`, then
/// `outcomes`; protocol §5.2).
fn known_turn(
    state: &BridgeState,
    intent: &crate::protocol::commands::Intent,
) -> Result<TurnAccepted, ProtocolError> {
    let Subject::Turn {
        session_id,
        incarnation,
        operation_id,
    } = &intent.subject
    else {
        return Err(rejection(
            ErrorCode::IntentConflict,
            "Idempotency-Key was used for another command",
        ));
    };
    let Some(operation_id) = operation_id else {
        return Err(rejection(
            ErrorCode::Conflict,
            "a turn with this Idempotency-Key is being admitted",
        ));
    };
    let public = state
        .sessions
        .state(session_id)
        .filter(|session| session.incarnation == *incarnation)
        .is_some_and(|session| {
            session
                .active_turn
                .as_ref()
                .is_some_and(|turn| &turn.operation_id == operation_id)
                || session
                    .turn_outcomes
                    .iter()
                    .any(|outcome| &outcome.operation_id == operation_id)
        });
    Ok(TurnAccepted {
        operation_id: operation_id.clone(),
        disposition: Disposition::Duplicate,
        result_expired: (!public).then_some(true),
        status: (!public).then(|| match intent.status {
            Some(FinalStatus::Turn(status)) => status,
            _ => TurnOutcomeStatus::Failed,
        }),
    })
}

/// A turn the bridge refused at admission.
fn turn_rejection(message: String) -> ProtocolError {
    let code = if message.contains("history revision is stale") {
        ErrorCode::HistoryMismatch
    } else if message.contains("client intent ID was reused") {
        ErrorCode::IntentConflict
    } else if message.contains("unknown or inactive session") {
        ErrorCode::NotFound
    } else if message.contains("bridge stopped") {
        ErrorCode::Unavailable
    } else {
        ErrorCode::Conflict
    };
    rejection(code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_tags_parse_every_documented_form() {
        assert_eq!(
            OwnerTag::parse("\"e\""),
            Some(OwnerTag {
                epoch: "e".into(),
                incarnation: None,
                history_revision: None,
            })
        );
        assert_eq!(
            OwnerTag::parse(" \"e:0\" "),
            Some(OwnerTag {
                epoch: "e".into(),
                incarnation: Some(0),
                history_revision: None,
            })
        );
        let tag = OwnerTag::parse("\"e:3:rev:with:colons\"").unwrap();
        assert_eq!(tag.incarnation, Some(3));
        assert_eq!(tag.history_revision.as_deref(), Some("rev:with:colons"));
        assert_eq!(tag.text(), "e:3:rev:with:colons");
        for invalid in ["e:1", "W/\"e:1\"", "\"\"", "\":1\"", "\"e:x\"", "\"e:1:\""] {
            assert_eq!(OwnerTag::parse(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn intent_keys_are_bounded_visible_ascii() {
        assert!(valid_intent_key("intent-1"));
        assert!(!valid_intent_key(""));
        assert!(!valid_intent_key("has space"));
        assert!(!valid_intent_key(&"k".repeat(INTENT_KEY_LIMIT + 1)));
    }
}
