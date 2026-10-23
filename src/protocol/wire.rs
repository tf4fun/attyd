//! The request and response bodies of the protocol's HTTP writes and reads
//! (protocol §6–§9). They are the public wire shapes; how the bridge admits
//! and applies a write is internal. The server reads an operation write
//! field by field, so admission keeps its refusal order (§6.4); the request
//! shapes that it does not deserialize exist for the generated client types.
#![cfg_attr(not(test), allow(dead_code))]

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::canon::PromptBlock;
use super::documents::ProtocolError;

// ---------------------------------------------------------------------------
// Operations (protocol §6)

/// `POST /api/v2/scopes/{scope}/ops` (§6.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct OpRequest {
    /// The `op.id` the write expects the scope to show.
    pub expect: String,
    #[serde(flatten)]
    pub write: OpArgs,
}

/// A client-written kind and its `args` (§6.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "kind", content = "args", rename_all = "snake_case")]
pub(crate) enum OpArgs {
    Prompt(PromptArgs),
    Fork(ForkArgs),
    Create(CreateArgs),
    Delete(DeleteArgs),
    Authenticate(MethodArgs),
    Logout(Empty),
    TerminalLogin(TerminalLoginArgs),
    List(ListArgs),
    Cancel(Empty),
}

/// An empty body, `args` or result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct Empty {}

/// `dto` as the JSON an operation slot keeps as its result.
pub(crate) fn value<T: Serialize>(dto: &T) -> Value {
    serde_json::to_value(dto).expect("a protocol DTO serializes")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct PromptArgs {
    pub prompt: Vec<PromptBlock>,
}

/// Default `cwd`: the source session's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct ForkArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct CreateArgs {
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeleteArgs {
    pub session_id: String,
}

/// `authenticate`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct MethodArgs {
    pub method_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct TerminalLoginArgs {
    pub method_id: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct ListArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub cursor: Option<String>,
    /// Refused with `catalog_changed` unless it is `catalog.live.revision`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub revision: Option<String>,
}

/// The response to one pending interaction, by its kind (the body of its `answer` action).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(untagged)]
pub(crate) enum Answer {
    Permission {
        #[cfg_attr(test, ts(type = "RequestPermissionOutcome"))]
        outcome: Value,
    },
    Elicitation {
        #[cfg_attr(test, ts(type = "CreateElicitationResponse"))]
        response: Value,
    },
}

/// `202`: the new operation's ID, or the running one's for `cancel` and
/// `answer` (§6.3).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct OpAccepted {
    pub op: String,
}

/// The result of `create` and `fork`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionCreated {
    pub session_id: String,
}

/// The result of `list`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct CatalogPage {
    pub revision: String,
    #[cfg_attr(
        test,
        ts(type = "Array<import(\"@agentclientprotocol/sdk\").SessionInfo>")
    )]
    pub sessions: Vec<Value>,
    pub next_cursor: Option<String>,
    /// The Agent's `_meta` of this page.
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional, type = "unknown"))]
    pub meta: Option<Value>,
}

/// The result of `prompt`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct PromptResult {
    #[cfg_attr(test, ts(type = "PromptResponse[\"stopReason\"]"))]
    pub stop_reason: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional, type = "PromptResponse[\"usage\"]"))]
    pub usage: Option<Value>,
}

// ---------------------------------------------------------------------------
// Resource actions (protocol §7); `dismiss` takes `Empty`.

/// `input` of a login terminal: written to its stdin each time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub(crate) struct TerminalInput {
    pub data: String,
}

/// `resize` of a login terminal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub(crate) struct TerminalResize {
    pub cols: u16,
    pub rows: u16,
}

// ---------------------------------------------------------------------------
// Reads (protocol §8)


/// `GET /api/v2/sessions/{id}/files?query=`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct ContextMatches {
    pub matches: Vec<ContextMatch>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContextMatch {
    /// Absolute.
    pub path: String,
    pub name: String,
    pub relative_path: String,
    pub root_name: String,
    pub size: u64,
}

/// `GET /api/v2/sessions/{id}/files/content?path=`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct ContextRead {
    pub attachment: ContextAttachment,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct ContextAttachment {
    pub name: String,
    pub size: u64,
    #[cfg_attr(test, ts(type = "Extract<ContentBlock, { type: \"resource\" }>"))]
    pub block: Value,
}

// ---------------------------------------------------------------------------
// Errors (protocol §9)

/// The body of every non-2xx response.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct ErrorBody {
    pub error: ProtocolError,
}

/// The body of a request refused before any route: a disallowed Host or
/// Origin (`403`) and an unknown `/api` path (`404`). It is not the protocol
/// error body.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct HostError {
    pub error: String,
}

// ---------------------------------------------------------------------------
// Host

/// `GET /api/health`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct Health {
    pub ok: bool,
    pub protocol: String,
    pub backend: String,
}
