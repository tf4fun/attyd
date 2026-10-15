//! The request and response bodies of the protocol's HTTP writes and reads
//! (protocol §6–§9). They are the public wire shapes; how the bridge admits
//! and applies a write is internal.
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Resource actions (protocol §7)

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
