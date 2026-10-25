//! Operation slots (protocol §4.4, docs/state-sync.md §2.1).
//!
//! Every scope keeps one `OpSlot`: the operation its `live.op` shows. A slot
//! keeps its final status, result or error until the next operation replaces
//! it. Pending interactions and a prompt's process are not stored here; the
//! projection reads them from the bridge state they live in.
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};

use super::canon::{Attachments, normalize};
use super::documents::{OperationKind, OperationStatus, ProtocolError};

/// A 128-bit random operation ID in unpadded base64url (protocol §6.5).
pub(crate) fn operation_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the system random source is available");
    URL_SAFE_NO_PAD.encode(bytes)
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OpSlot {
    pub id: String,
    pub kind: OperationKind,
    pub status: OperationStatus,
    pub input: Value,
    pub cancel_requested: bool,
    pub result: Option<Value>,
    pub error: Option<ProtocolError>,
    /// The blocks a normalized prompt input references.
    pub attachments: Attachments,
    /// The scope the operation was written to: its issuing scope (protocol §4.4).
    pub scope: Option<String>,
    /// The operation this one replaced in its issuing scope: the `expect` it
    /// was admitted with, which lets a client resolve a lost answer (§6.6).
    pub after: Option<String>,
}

impl OpSlot {
    pub(crate) fn running(kind: OperationKind, input: Value) -> Self {
        Self {
            id: operation_id(),
            kind,
            status: OperationStatus::Running,
            input,
            cancel_requested: false,
            result: None,
            error: None,
            attachments: Attachments::default(),
            scope: None,
            after: None,
        }
    }

    /// A scope's first operation that is already over: `none` or `open`.
    pub(crate) fn completed(kind: OperationKind, input: Value) -> Self {
        Self {
            status: OperationStatus::Completed,
            result: Some(super::wire::value(&super::wire::Empty {})),
            ..Self::running(kind, input)
        }
    }

    /// A running prompt; its input is the normalized prompt (protocol §6.2).
    pub(crate) fn prompt(prompt: &[Value]) -> Self {
        let mut attachments = Attachments::default();
        let blocks = normalize(prompt, &mut attachments);
        Self {
            attachments,
            ..Self::running(OperationKind::Prompt, json!({ "prompt": blocks }))
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.status == OperationStatus::Running
    }

    pub(crate) fn is_running_kind(&self, kind: OperationKind) -> bool {
        self.is_running() && self.kind == kind
    }

    pub(crate) fn complete(&mut self, result: Value) {
        self.status = OperationStatus::Completed;
        self.result = Some(result);
    }

    pub(crate) fn fail(&mut self, error: ProtocolError) {
        self.status = OperationStatus::Failed;
        self.error = Some(error);
    }
}
