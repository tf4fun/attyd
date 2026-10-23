//! Documents, events and errors (protocol §2, §4, §5.2, §9).
//!
//! Embedded ACP values stay `serde_json::Value` and name their SDK type in
//! TypeScript. Operation requests and results are typed with the operations
//! endpoint (migration step 6).
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::canon::LogEntry;
use super::{Op, SessionControls};

// ---------------------------------------------------------------------------
// Scopes (protocol §2)

/// A document's name: `global`, `catalog` or `session/<sessionId>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Scope {
    Global,
    Catalog,
    Session(String),
}

impl Scope {
    pub(crate) fn parse(scope: &str) -> Option<Self> {
        match scope {
            "global" => Some(Self::Global),
            "catalog" => Some(Self::Catalog),
            _ => scope
                .strip_prefix("session/")
                .filter(|id| !id.is_empty())
                .map(|id| Self::Session(id.to_string())),
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Global => formatter.write_str("global"),
            Self::Catalog => formatter.write_str("catalog"),
            Self::Session(id) => write!(formatter, "session/{id}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Errors (protocol §9)

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
    /// `agent_error`: the ACP error `{ code, message, data }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional, type = "unknown"))]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorCode {
    InvalidRequest,
    NotFound,
    Stale,
    NotAllowed,
    CatalogChanged,
    Unsupported,
    Unavailable,
    AgentError,
}

impl ProtocolError {
    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// An Agent's JSON-RPC error `{ code, message, data }`.
    pub(crate) fn agent(error: &Value) -> Self {
        Self {
            code: ErrorCode::AgentError,
            message: error["message"]
                .as_str()
                .unwrap_or("Agent error")
                .to_string(),
            data: Some(error.clone()),
        }
    }
}

// ---------------------------------------------------------------------------
// Operations (protocol §4.4)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationKind {
    Prompt,
    SetMode,
    SetConfig,
    Fork,
    Create,
    Delete,
    Authenticate,
    Logout,
    TerminalLogin,
    List,
    Load,
    Open,
    None,
    AgentRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct Operation {
    /// Random, never reused (protocol §6.5).
    pub id: String,
    pub kind: OperationKind,
    pub status: OperationStatus,
    /// The write's `args` (a prompt as normalized `Block[]`), or the load's cwd.
    #[cfg_attr(test, ts(type = "unknown"))]
    pub input: Value,
    pub cancel_requested: bool,
    /// Pending, by interaction ID. They are a member of `live` beside the operation on the wire
    /// (protocol §4.4), so the generated type leaves them out.
    #[cfg_attr(test, ts(skip))]
    pub interactions: BTreeMap<String, Interaction>,
    /// Revision 2 showed a running prompt's updates here; entries carry them now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(skip))]
    pub process: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional, type = "unknown"))]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub error: Option<ProtocolError>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Interaction {
    Permission {
        #[serde(rename = "interactionId")]
        interaction_id: String,
        #[cfg_attr(test, ts(type = "RequestPermissionRequest"))]
        request: Value,
    },
    Elicitation {
        #[serde(rename = "interactionId")]
        interaction_id: String,
        #[cfg_attr(test, ts(type = "CreateElicitationRequest"))]
        request: Value,
    },
}

// ---------------------------------------------------------------------------
// Resources (protocol §4.6)

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct UrlFlow {
    /// The accepted elicitation; identifies the flow.
    pub interaction_id: String,
    /// Agent-chosen; may be reused after the flow ends.
    pub elicitation_id: String,
    #[cfg_attr(test, ts(type = "CreateElicitationRequest"))]
    pub request: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct Terminal {
    /// Session: the Agent's ID; global: the operation ID.
    pub terminal_id: String,
    /// A bounded tail (`trim`, protocol §5.3).
    pub output: String,
    pub truncated: bool,
    pub exit_status: Option<ExitStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExitStatus {
    pub exit_code: Option<i64>,
    pub signal: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoginTerminal {
    pub terminal_id: String,
    pub output: String,
    pub truncated: bool,
    pub exit_status: Option<ExitStatus>,
    /// The authentication method it runs.
    pub method_id: String,
}

// ---------------------------------------------------------------------------
// Live documents (protocol §4.1–§4.3)

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionLive {
    pub op: Operation,
    /// By interaction ID.
    pub url_flows: BTreeMap<String, UrlFlow>,
    pub session_id: String,
    pub cwd: String,
    pub controls: SessionControls,
    /// Agent terminals not yet released.
    pub terminals: BTreeMap<String, Terminal>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct GlobalLive {
    pub op: Operation,
    pub url_flows: BTreeMap<String, UrlFlow>,
    pub agent: AgentInfo,
    /// Sessions the bridge holds.
    pub sessions: BTreeMap<String, SessionSummary>,
    /// Terminal logins, by operation ID.
    pub terminals: BTreeMap<String, LoginTerminal>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct CatalogLive {
    pub op: Operation,
    pub url_flows: BTreeMap<String, UrlFlow>,
    /// Changes when the Agent's list may have changed.
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentInfo {
    pub phase: AgentPhase,
    pub error: Option<ProtocolError>,
    pub launch: AgentLaunch,
    #[cfg_attr(test, ts(type = "InitializeResponse | null"))]
    pub initialize: Option<Value>,
    pub authenticated_method_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentPhase {
    Starting,
    Initializing,
    Ready,
    Stopped,
    Error,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentTransport {
    #[default]
    Stdio,
    Http,
    Ws,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct McpServerSummary {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: McpServerKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum McpServerKind {
    Stdio,
    Http,
    Sse,
    Acp,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionSummary {
    pub session_id: String,
    pub cwd: String,
    /// `controls.info.title`.
    pub title: Option<String>,
    pub op: OperationSummary,
    pub pending: PendingCounts,
    /// The `create` or `fork` that installed the session, while it is held.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub origin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct OperationSummary {
    pub kind: OperationKind,
    pub status: OperationStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingCounts {
    pub interactions: usize,
    pub url_flows: usize,
}

// ---------------------------------------------------------------------------
// The end of a scope (protocol §5.2)

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct End {
    pub reason: EndReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub error: Option<ProtocolError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub(crate) enum EndReason {
    Deleted,
    /// A `close` action succeeded (protocol §7).
    Closed,
    LoadFailed,
}
