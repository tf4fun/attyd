//! Operation slots (protocol §4.4, docs/state-sync.md §2.1).
//!
//! Every scope keeps one `OpSlot`: the operation its `live.op` shows. A slot
//! keeps its final status, result or error until the next operation replaces
//! it. Pending interactions and a prompt's process are not stored here; the
//! projection reads them from the bridge state they live in.
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};

use super::canon::{Attachments, normalize};
use super::documents::{OperationKind, OperationStatus, ProtocolError};

/// How an operation ended, as a held response takes it (protocol §6.3).
pub(crate) type OpEnd = Result<Value, ProtocolError>;

/// The held response of a `create`, `fork` or `list` (protocol §6.3): it takes
/// the operation's end once, when its slot ends, and nothing is kept after.
/// It is not part of what the slot shows.
#[derive(Clone, Default)]
pub(crate) struct Completion(Option<Arc<Mutex<Option<tokio::sync::oneshot::Sender<OpEnd>>>>>);

impl Completion {
    pub(crate) fn new(sender: tokio::sync::oneshot::Sender<OpEnd>) -> Self {
        Self(Some(Arc::new(Mutex::new(Some(sender)))))
    }

    fn deliver(&self, end: OpEnd) {
        if let Some(sender) = self
            .0
            .as_ref()
            .and_then(|sender| sender.lock().expect("completion poisoned").take())
        {
            let _ = sender.send(end);
        }
    }
}

impl std::fmt::Debug for Completion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Completion")
    }
}

impl PartialEq for Completion {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

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
    /// The held response waiting for this operation's end, if any.
    pub completion: Completion,
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
            completion: Completion::default(),
        }
    }

    /// A scope's first operation that is already over: `none` or `open`.
    pub(crate) fn completed(kind: OperationKind, input: Value) -> Self {
        Self {
            status: OperationStatus::Completed,
            result: Some(json!({})),
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
        self.completion.deliver(Ok(result.clone()));
        self.result = Some(result);
    }

    pub(crate) fn fail(&mut self, error: ProtocolError) {
        self.status = OperationStatus::Failed;
        self.completion.deliver(Err(error.clone()));
        self.error = Some(error);
    }
}
