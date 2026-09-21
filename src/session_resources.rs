use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    CreateElicitationResponse, ElicitationAction, RequestPermissionOutcome,
    RequestPermissionResponse,
};
use serde_json::Value;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::bridge::SessionViewWaiter;
use crate::semantic::SessionUpdateSemanticState;
use crate::session_observers::SessionObservers;

/// Non-cloneable handles and ingest buffers for one session incarnation.
/// Business admission, lifecycle and turn execution remain in SessionState.
#[derive(Default)]
pub(crate) struct SessionResources {
    pub(crate) validation: Option<SessionUpdateSemanticState>,
    pub(crate) replay_validation_backup: Option<ReplayValidationBackup>,
    pub(crate) attachment: AttachmentDelivery,
    pub(crate) materialization: Option<MaterializationResources>,
    pub(crate) history_sync: Option<HistorySyncResources>,
    pub(crate) permissions: HashMap<String, PermissionResponder>,
    pub(crate) elicitations: HashMap<String, ElicitationResponder>,
    pub(crate) observers: SessionObservers,
}

#[derive(Default)]
pub(crate) struct AttachmentDelivery {
    pub(crate) subscriber: Option<u64>,
    pub(crate) control_candidate: SyncControlCandidate,
}

#[derive(Default)]
pub(crate) struct SyncControlCandidate {
    pub(crate) updates: Vec<Value>,
    pub(crate) bytes: usize,
}

/// An external continuation can retain this token without cloning the ingest state.
#[derive(Clone)]
pub(crate) struct SessionUpdateOwner {
    pub(crate) incarnation: Option<u64>,
    pub(crate) allocation: Arc<()>,
}

pub(crate) struct ReplayValidationBackup {
    pub(crate) owner: SessionUpdateOwner,
    pub(crate) previous: Option<SessionUpdateSemanticState>,
}

pub(crate) struct MaterializationResources {
    pub(crate) attempt_id: String,
    pub(crate) waiters: Vec<SessionViewWaiter>,
    pub(crate) cancellation: CancellationToken,
}

/// One optional history workflow owns this slot for its whole lifetime:
/// every load attempt, every retry wait, and the final cached fallback.
/// The slot is work evidence; the token is a termination request.
pub(crate) struct HistorySyncResources {
    pub(crate) flow_id: String,
    pub(crate) cancellation: CancellationToken,
}

/// A lightweight continuation identity for one history workflow. It carries
/// no history payload: attempts still allocate their own replay candidates.
#[derive(Clone)]
pub(crate) struct HistorySyncOwner {
    pub(crate) session: SessionResourceOwner,
    pub(crate) flow_id: String,
    pub(crate) cancellation: CancellationToken,
}

pub(crate) struct PermissionResponder {
    pub(crate) tool_call_id: String,
    pub(crate) option_ids: HashSet<String>,
    pub(crate) sender: oneshot::Sender<RequestPermissionResponse>,
}

pub(crate) struct ElicitationResponder {
    pub(crate) url_elicitation_id: Option<String>,
    pub(crate) request: Value,
    pub(crate) sender: oneshot::Sender<CreateElicitationResponse>,
}

impl SessionResources {
    /// Cancel externally awaited work explicitly before removing its owner.
    /// Sending a terminal result also distinguishes retirement from an accidentally
    /// dropped sender. Repeated cancellation is harmless because handles are moved.
    pub(crate) fn cancel(&mut self, reason: &str) {
        for (_, permission) in self.permissions.drain() {
            let _ = permission.sender.send(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ));
        }
        for (_, elicitation) in self.elicitations.drain() {
            let _ = elicitation
                .sender
                .send(CreateElicitationResponse::new(ElicitationAction::Cancel));
        }
        if let Some(materialization) = self.materialization.take() {
            materialization.cancellation.cancel();
            for waiter in materialization.waiters {
                waiter.cancel(reason);
            }
        }
        if let Some(history_sync) = self.history_sync.take() {
            history_sync.cancellation.cancel();
        }
        self.observers.shutdown();
    }
}

impl Drop for SessionResources {
    fn drop(&mut self) {
        // Covers connection teardown as well as explicit Registry retirement.
        self.cancel("session resource owner was dropped");
    }
}

/// A lightweight global route to one incarnation's real resource owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionResourceOwner {
    pub(crate) epoch: String,
    pub(crate) session_id: String,
    pub(crate) incarnation: u64,
}

/// One accepted URL registration, including request-scoped flows without a session owner.
/// The token is the originating bridge interaction ID, not the reusable Agent URL ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UrlRegistration {
    pub(crate) owner: Option<SessionResourceOwner>,
    pub(crate) registration_id: String,
}

impl SessionResourceOwner {
    pub(crate) fn new(
        epoch: impl Into<String>,
        session_id: impl Into<String>,
        incarnation: u64,
    ) -> Self {
        Self {
            epoch: epoch.into(),
            session_id: session_id.into(),
            incarnation,
        }
    }
}
