//! Protocol v2 wire types ([protocol](../../docs/protocol.md)).
//!
//! These DTOs are the only definition of the v2 documents, log entries,
//! commands and errors. `shared/protocol.ts` is generated from them by
//! `typescript::generated_typescript_is_current`; embedded ACP values stay
//! `serde_json::Value` in Rust and name their `@agentclientprotocol/sdk` type in
//! TypeScript.
//!
//! Nothing is routed yet: the commit step (migration step 2) and the v2 routes
//! (step 3) are the first consumers.
#![allow(dead_code)]

mod controls;
mod project;
#[cfg(test)]
mod typescript;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) use project::{project_global, project_session};

/// A log position (protocol §2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct Position {
    pub epoch: String,
    pub seq: u64,
}

// ---------------------------------------------------------------------------
// Global document (protocol §3.1)

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct GlobalState {
    pub agent: AgentState,
    pub catalog: CatalogState,
    pub sessions: BTreeMap<String, SessionSummary>,
    pub requests: BTreeMap<String, Request>,
    pub interactions: BTreeMap<String, Interaction>,
    pub url_flows: BTreeMap<String, UrlFlow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentState {
    pub phase: AgentPhase,
    pub error: Option<ProtocolError>,
    pub launch: AgentLaunch,
    #[cfg_attr(test, ts(type = "InitializeResponse | null"))]
    pub initialize: Option<Value>,
    pub authenticated_method_id: Option<String>,
    pub predecessor: Option<EpochPredecessor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentPhase {
    Starting,
    Initializing,
    Ready,
    Stopped,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentLaunch {
    pub transport: AgentTransport,
    pub command: Vec<String>,
    pub cwd: String,
    pub read_only: bool,
    pub additional_directories: Vec<String>,
    pub mcp_servers: Vec<McpServerSummary>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentTransport {
    Stdio,
    Http,
    Ws,
}

/// Launch metadata only: commands and environment stay host-private.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct McpServerSummary {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: McpServerKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum McpServerKind {
    Stdio,
    Http,
    Sse,
    Acp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct EpochPredecessor {
    pub epoch: String,
    pub reason: EpochEndReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum EpochEndReason {
    AgentExit,
    AgentError,
    AuthRestart,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct CatalogState {
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionSummary {
    pub session_id: String,
    pub incarnation: u64,
    pub cwd: String,
    pub title: Option<String>,
    pub lifecycle: SessionLifecycle,
    pub phase: SessionPhase,
    pub work: SessionWork,
    pub pending: PendingCounts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionLifecycle {
    Attaching,
    Active,
    Closing,
    ClosingForDelete,
    Deleting,
    Closed,
    Uncertain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionWork {
    Idle,
    Running,
    AwaitingInput,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingCounts {
    pub permissions: usize,
    pub elicitations: usize,
    pub url_flows: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct Request {
    pub request_id: String,
    pub operation: RequestOperation,
    /// The ACP method, or `attyd/auth_terminal`.
    pub method: String,
    pub client_intent_id: Option<String>,
    pub target: Option<RequestTarget>,
    pub interaction_id: Option<String>,
    pub method_id: Option<String>,
    /// Non-secret parameters that tell two submissions apart (protocol §3.1).
    #[cfg_attr(test, ts(type = "Record<string, unknown>"))]
    pub params: BTreeMap<String, Value>,
    pub status: RequestStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub result: Option<RequestResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub error: Option<ProtocolError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub terminal: Option<AuthTerminalState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum RequestOperation {
    Open,
    Create,
    Fork,
    SetMode,
    SetConfig,
    Close,
    Delete,
    Authenticate,
    Logout,
    AuthTerminal,
    InteractionResponse,
    /// An ACP request the bridge starts itself, such as `session/list`.
    AgentRequest,
}

/// `incarnation` is `null` for a session the bridge does not hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct RequestTarget {
    pub session_id: String,
    pub incarnation: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum RequestStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum RequestResult {
    #[serde(rename_all = "camelCase")]
    Session {
        session_id: String,
        incarnation: u64,
    },
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuthTerminalState {
    pub output: String,
    pub truncated: bool,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub message: Option<String>,
}

// ---------------------------------------------------------------------------
// Session document (protocol §3.2)

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionState {
    pub session_id: String,
    pub incarnation: u64,
    pub cwd: String,
    pub controls: SessionControls,
    pub history: SessionHistory,
    pub turn: Option<ActiveTurn>,
    pub outcomes: BTreeMap<String, TurnOutcome>,
    pub operation: Option<SessionOperation>,
    pub interactions: BTreeMap<String, Interaction>,
    pub url_flows: BTreeMap<String, UrlFlow>,
    pub terminals: BTreeMap<String, TerminalState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionPhase {
    Cold,
    Loading,
    Ready,
    Running,
    Reconciling,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionHistory {
    pub revision: Option<String>,
    pub phase: SessionPhase,
    pub error: Option<String>,
    pub notice: Option<String>,
    /// Completed history with process bodies collapsed (protocol §3.2).
    #[cfg_attr(test, ts(type = "SessionUpdate[]"))]
    pub timeline: Vec<Value>,
    pub collapsed_turns: Vec<CollapsedTurn>,
}

/// A completed turn in `timeline`. Its process body is the immutable page
/// `(history.revision, turnId)` (protocol §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollapsedTurn {
    pub turn_id: String,
    pub operation_id: Option<String>,
    pub before_update: usize,
    pub after_update: usize,
    pub process_count: usize,
    pub visible_ranges: Vec<TimelineRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct TimelineRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct ActiveTurn {
    pub operation_id: String,
    pub client_intent_id: String,
    #[cfg_attr(test, ts(type = "ContentBlock[]"))]
    pub prompt: Vec<Value>,
    /// Folded overlay; entries may change in place.
    #[cfg_attr(test, ts(type = "SessionUpdate[]"))]
    pub updates: Vec<Value>,
    pub cancel_requested: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct TurnOutcome {
    pub operation_id: String,
    pub client_intent_id: String,
    pub status: TurnOutcomeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional, type = "PromptResponse"))]
    pub response: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub error: Option<ProtocolError>,
    /// Failed turns keep the exact prompt for a retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional, type = "ContentBlock[]"))]
    pub prompt: Option<Vec<Value>>,
    /// The `timeline` index this outcome follows.
    pub after_update: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum TurnOutcomeStatus {
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionOperation {
    pub operation_id: String,
    pub kind: OperationKind,
    pub stage: String,
    /// The request record, when a command started the operation.
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationKind {
    Load,
    Resume,
    Fork,
    Close,
    Delete,
    SetMode,
    SetConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Interaction {
    #[serde(rename_all = "camelCase")]
    Permission {
        interaction_id: String,
        #[cfg_attr(test, ts(type = "RequestPermissionRequest"))]
        request: Value,
        operation_id: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Elicitation {
        interaction_id: String,
        #[cfg_attr(test, ts(type = "CreateElicitationRequest"))]
        request: Value,
        operation_id: Option<String>,
        /// The parent request; global scope only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(test, ts(optional))]
        request_id: Option<String>,
    },
}

/// A URL flow, keyed by the `interactionId` of the accepted elicitation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct UrlFlow {
    pub interaction_id: String,
    /// Agent-chosen; a later flow may reuse it.
    pub elicitation_id: String,
    /// The parent request; global scope only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub request_id: Option<String>,
    #[cfg_attr(test, ts(type = "CreateElicitationRequest"))]
    pub request: Value,
    pub status: UrlFlowStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum UrlFlowStatus {
    Waiting,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct TerminalState {
    pub terminal_id: String,
    /// Bounded tail; moves forward with `trim` plus `append`.
    pub output: String,
    pub truncated: bool,
    pub exit_status: Option<TerminalExitStatus>,
    pub released: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct TerminalExitStatus {
    pub exit_code: Option<u32>,
    pub signal: Option<String>,
}

// ---------------------------------------------------------------------------
// Controls (protocol §3.3); the reduction lives in `controls`.

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionControls {
    pub modes: Option<SessionModes>,
    /// `null`: never provided; `[]`: provided empty. Preferred over `modes`
    /// whenever non-null.
    #[cfg_attr(test, ts(type = "SessionConfigOption[] | null"))]
    pub config_options: Option<Vec<Value>>,
    #[cfg_attr(test, ts(type = "AvailableCommand[]"))]
    pub commands: Vec<Value>,
    pub info: SessionInfo,
    pub usage: Option<SessionUsage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionModes {
    pub current_mode_id: String,
    #[cfg_attr(test, ts(type = "SessionMode[]"))]
    pub available_modes: Vec<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionInfo {
    pub title: Option<String>,
    pub updated_at: Option<String>,
    #[cfg_attr(test, ts(type = "unknown | null"))]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionUsage {
    pub used: u64,
    pub size: u64,
    #[cfg_attr(test, ts(type = "Cost | null"))]
    pub cost: Option<Value>,
}

// ---------------------------------------------------------------------------
// Replication stream (protocol §4)

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Entry {
    Snapshot {
        scope: String,
        at: Position,
        state: ScopeState,
    },
    Reset {
        scope: String,
        at: Position,
    },
    Ops {
        scope: String,
        at: Position,
        /// The `seq` of the previous entry delivered for this scope.
        prev: u64,
        ops: Vec<Op>,
    },
    End {
        scope: String,
        at: Position,
        reason: EndReason,
    },
    Position {
        at: Position,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(untagged)]
pub(crate) enum ScopeState {
    Global(Box<GlobalState>),
    Session(Box<SessionState>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum EndReason {
    Closed,
    Deleted,
    Unobserved,
    Replaced,
    Gone,
}

/// A JSON Pointer operation on a scope document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Op {
    /// Create or replace; an array index modifies in place.
    Set {
        path: String,
        #[cfg_attr(test, ts(type = "unknown"))]
        value: Value,
    },
    /// A map key or array index.
    Remove { path: String },
    /// Array or string tail.
    Append {
        path: String,
        #[serde(flatten)]
        tail: AppendTail,
    },
    /// Drops the first `count` array items or UTF-16 units.
    Trim { path: String, count: usize },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(untagged)]
pub(crate) enum AppendTail {
    Values {
        #[cfg_attr(test, ts(type = "unknown[]"))]
        values: Vec<Value>,
    },
    Text {
        text: String,
    },
}

// ---------------------------------------------------------------------------
// Commands (protocol §5)

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandResult<T> {
    /// The effect is committed at or before this position.
    pub at: Position,
    pub scopes: Vec<String>,
    pub result: T,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum Disposition {
    Accepted,
    Duplicate,
}

/// The result of an accepted operation command (protocol §5.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct RequestAccepted {
    pub request_id: String,
    pub disposition: Disposition,
    /// A duplicate whose record is no longer public; `status` is final.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub result_expired: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub status: Option<RequestStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct TurnAccepted {
    pub operation_id: String,
    pub disposition: Disposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub result_expired: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub status: Option<TurnOutcomeStatus>,
}

/// The body of control actions and of commands without parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub(crate) struct EmptyBody {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AuthenticateBody {
    pub method_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AuthTerminalStartBody {
    pub method_id: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AuthTerminalInputBody {
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TerminalSizeBody {
    pub cols: u16,
    pub rows: u16,
}

/// `POST /sessions` and `POST /sessions/{id}/open`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SessionCwdBody {
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TurnBody {
    #[cfg_attr(test, ts(type = "ContentBlock[]"))]
    pub prompt: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SetModeBody {
    pub mode_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SetConfigBody {
    #[cfg_attr(test, ts(type = "unknown"))]
    pub value: Value,
}

/// `POST /interactions/{interactionId}`: the shape follows the interaction kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(untagged)]
pub(crate) enum InteractionResponseBody {
    Permission {
        #[cfg_attr(test, ts(type = "RequestPermissionOutcome"))]
        outcome: Value,
    },
    Elicitation {
        #[cfg_attr(test, ts(type = "CreateElicitationResponse"))]
        response: Value,
    },
}

// ---------------------------------------------------------------------------
// Reads (protocol §6) and errors (protocol §7)

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct Snapshot<T> {
    pub at: Position,
    pub state: T,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
    /// `agent_error`: the ACP error `{ code, message, data }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional, type = "unknown"))]
    pub data: Option<Value>,
}

impl ProtocolError {
    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorCode {
    InvalidRequest,
    NotFound,
    OwnerMismatch,
    HistoryMismatch,
    PreconditionRequired,
    Conflict,
    IntentConflict,
    CatalogChanged,
    Unsupported,
    Gone,
    AgentError,
    Unavailable,
    IntentCapacity,
    /// Record only: the interaction ended before delivery.
    InteractionEnded,
}

impl ErrorCode {
    /// The HTTP status of a synchronous rejection. `interaction_ended` only
    /// appears inside request records.
    pub(crate) fn status(self) -> u16 {
        match self {
            Self::InvalidRequest => 400,
            Self::NotFound => 404,
            Self::OwnerMismatch | Self::Conflict | Self::IntentConflict | Self::CatalogChanged => {
                409
            }
            Self::Gone => 410,
            Self::HistoryMismatch => 412,
            Self::PreconditionRequired => 428,
            Self::Unsupported => 501,
            Self::AgentError => 502,
            Self::Unavailable | Self::IntentCapacity => 503,
            Self::InteractionEnded => 500,
        }
    }
}

/// The body of every non-2xx response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct ErrorBody {
    pub error: ProtocolError,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn ops_and_entries_use_the_documented_wire_shape() {
        let entry = Entry::Ops {
            scope: "session/s".into(),
            at: Position {
                epoch: "e".into(),
                seq: 7,
            },
            prev: 5,
            ops: vec![
                Op::Set {
                    path: "/turn".into(),
                    value: Value::Null,
                },
                Op::Remove {
                    path: "/interactions/i".into(),
                },
                Op::Append {
                    path: "/history/timeline".into(),
                    tail: AppendTail::Values {
                        values: vec![json!({ "sessionUpdate": "plan" })],
                    },
                },
                Op::Append {
                    path: "/terminals/t/output".into(),
                    tail: AppendTail::Text { text: "ok".into() },
                },
                Op::Trim {
                    path: "/terminals/t/output".into(),
                    count: 2,
                },
            ],
        };
        let wire = json!({
            "type": "ops", "scope": "session/s", "at": { "epoch": "e", "seq": 7 }, "prev": 5,
            "ops": [
                { "op": "set", "path": "/turn", "value": null },
                { "op": "remove", "path": "/interactions/i" },
                { "op": "append", "path": "/history/timeline", "values": [{ "sessionUpdate": "plan" }] },
                { "op": "append", "path": "/terminals/t/output", "text": "ok" },
                { "op": "trim", "path": "/terminals/t/output", "count": 2 },
            ],
        });
        assert_eq!(serde_json::to_value(&entry).unwrap(), wire);
        assert_eq!(serde_json::from_value::<Entry>(wire).unwrap(), entry);
        assert_eq!(
            serde_json::to_value(Entry::End {
                scope: "session/s".into(),
                at: Position {
                    epoch: "e".into(),
                    seq: 8
                },
                reason: EndReason::Replaced,
            })
            .unwrap(),
            json!({ "type": "end", "scope": "session/s", "at": { "epoch": "e", "seq": 8 }, "reason": "replaced" }),
        );
    }

    #[test]
    fn records_omit_absent_optional_fields_and_errors_map_to_http_status() {
        let request = Request {
            request_id: "r".into(),
            operation: RequestOperation::SetConfig,
            method: "session/set_config_option".into(),
            client_intent_id: Some("k".into()),
            target: Some(RequestTarget {
                session_id: "s".into(),
                incarnation: Some(2),
            }),
            interaction_id: None,
            method_id: None,
            params: BTreeMap::from([("configId".into(), json!("model"))]),
            status: RequestStatus::Failed,
            result: None,
            error: Some(ProtocolError {
                code: ErrorCode::AgentError,
                message: "refused".into(),
                data: Some(json!({ "code": -32603, "message": "refused" })),
            }),
            terminal: None,
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["operation"], "set_config");
        assert_eq!(value["error"]["code"], "agent_error");
        assert!(value.get("result").is_none() && value.get("terminal").is_none());
        assert_eq!(serde_json::from_value::<Request>(value).unwrap(), request);
        assert_eq!(
            serde_json::to_value(RequestResult::Session {
                session_id: "s".into(),
                incarnation: 3
            })
            .unwrap(),
            json!({ "kind": "session", "sessionId": "s", "incarnation": 3 }),
        );
        assert_eq!(
            serde_json::to_value(RequestAccepted {
                request_id: "r".into(),
                disposition: Disposition::Accepted,
                result_expired: None,
                status: None,
            })
            .unwrap(),
            json!({ "requestId": "r", "disposition": "accepted" }),
        );
        for (code, status) in [
            (ErrorCode::InvalidRequest, 400),
            (ErrorCode::NotFound, 404),
            (ErrorCode::OwnerMismatch, 409),
            (ErrorCode::HistoryMismatch, 412),
            (ErrorCode::PreconditionRequired, 428),
            (ErrorCode::Conflict, 409),
            (ErrorCode::IntentConflict, 409),
            (ErrorCode::CatalogChanged, 409),
            (ErrorCode::Unsupported, 501),
            (ErrorCode::Gone, 410),
            (ErrorCode::AgentError, 502),
            (ErrorCode::Unavailable, 503),
            (ErrorCode::IntentCapacity, 503),
        ] {
            assert_eq!(code.status(), status, "{code:?}");
        }
    }

    #[test]
    fn command_bodies_reject_unknown_fields_and_pick_the_interaction_kind() {
        assert!(
            serde_json::from_value::<SetModeBody>(json!({ "modeId": "plan", "x": 1 })).is_err()
        );
        assert!(serde_json::from_value::<EmptyBody>(json!({ "unexpected": true })).is_err());
        assert_eq!(
            serde_json::from_value::<InteractionResponseBody>(
                json!({ "outcome": { "outcome": "cancelled" } })
            )
            .unwrap(),
            InteractionResponseBody::Permission {
                outcome: json!({ "outcome": "cancelled" })
            },
        );
        assert!(matches!(
            serde_json::from_value::<InteractionResponseBody>(
                json!({ "response": { "action": "decline" } })
            )
            .unwrap(),
            InteractionResponseBody::Elicitation { .. }
        ));
    }
}
