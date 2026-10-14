//! Operation slots, occupation and the turn log in bridge state
//! (docs/state-sync.md §2.1).
//!
//! A held session keeps its `OpSlot` and reduced controls in its live state and
//! its `TurnLog` in its history state; `global` and `catalog` keep their slots
//! here. The registry's transitions maintain them; the commit step publishes
//! them.
use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use super::canon::TurnLog;
use super::documents::{End, OperationKind, Scope};
use super::op::OpSlot;
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

impl SessionRegistry {
    /// Installs a running operation in `scope` and occupies the other scopes it
    /// acts on (a delete occupies its session). Admission (protocol §6.4) is
    /// checked by the caller; this only refuses a scope that is not free.
    pub(crate) fn install_operation(
        &mut self,
        scope: &Scope,
        kind: OperationKind,
        input: Value,
    ) -> Result<String, InstallError> {
        let _ = (scope, kind, input);
        todo!("install an operation")
    }

    /// Installs a running prompt with its normalized input.
    pub(crate) fn install_prompt(
        &mut self,
        session_id: &str,
        prompt: &[Value],
    ) -> Result<String, InstallError> {
        let _ = (session_id, prompt);
        todo!("install a prompt")
    }

    /// The slot a held scope shows: the occupying operation's, if any.
    pub(crate) fn shown_slot(&self, scope: &Scope) -> Option<&OpSlot> {
        let _ = scope;
        todo!("the shown slot")
    }
}
