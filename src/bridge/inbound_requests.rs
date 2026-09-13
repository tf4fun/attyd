//! Bounded routing for Agent request cancellation at the wire ingress cut.
//!
//! The coordinator owns the map. It retains values, never leases: only the
//! queued request and its responder keep an allocation alive. A captured cancel
//! therefore cannot prolong an old request or retarget a reused JSON-RPC ID.

use std::collections::HashMap;
use std::sync::Arc;

use agent_client_protocol::schema::v1::RequestId;
use agent_client_protocol::{Error, JsonRpcMessage};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::scheduling::{CapturedRoute, IngressSender};
use super::{CreateElicitationRequest, RequestPermissionRequest};

const MAX_INBOUND_REQUESTS: usize = 512;
const MAX_INBOUND_REQUEST_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InboundRequestKind {
    Permission,
    Elicitation,
    Other,
}

#[derive(Clone)]
pub(super) struct CapturedInboundRequest {
    pub(super) request_id: RequestId,
    pub(super) allocation: String,
    pub(super) kind: InboundRequestKind,
    pub(super) route: CapturedRoute,
}

pub(super) struct InboundRequests {
    limit: usize,
    requests: HashMap<RequestId, CapturedInboundRequest>,
    count: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
}

impl Default for InboundRequests {
    fn default() -> Self {
        Self::new(MAX_INBOUND_REQUESTS)
    }
}

impl InboundRequests {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            limit,
            requests: HashMap::new(),
            count: Arc::new(Semaphore::new(limit)),
            bytes: Arc::new(Semaphore::new(MAX_INBOUND_REQUEST_BYTES)),
        }
    }

    pub(super) fn register(
        &mut self,
        request_id: RequestId,
        method: &str,
        route: &CapturedRoute,
        ingress: IngressSender,
        payload_bytes: usize,
    ) -> Result<InboundRequestLease, Error> {
        if self.requests.contains_key(&request_id) {
            return Err(Error::invalid_request().data("Agent request ID is already outstanding"));
        }
        if self.requests.len() >= self.limit {
            return Err(Error::invalid_request().data("Agent inbound request limit reached"));
        }
        let allocation = route
            .dispatch_owner
            .attempt_id
            .as_ref()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| Error::internal_error().data("Agent request has no allocation"))?
            .clone();
        let kind = if RequestPermissionRequest::matches_method(method) {
            InboundRequestKind::Permission
        } else if CreateElicitationRequest::matches_method(method) {
            InboundRequestKind::Elicitation
        } else {
            InboundRequestKind::Other
        };
        // Cover the retained external-work payload independently of ingress
        // delivery slots, which must remain available for cancellation/finish.
        let payload_bytes = payload_bytes
            .checked_add(256)
            .and_then(|bytes| u32::try_from(bytes).ok())
            .filter(|bytes| *bytes as usize <= MAX_INBOUND_REQUEST_BYTES)
            .ok_or_else(|| Error::invalid_request().data("Agent inbound payload limit reached"))?;
        let count =
            self.count.clone().try_acquire_owned().map_err(|_| {
                Error::invalid_request().data("Agent inbound request limit reached")
            })?;
        let bytes = self
            .bytes
            .clone()
            .try_acquire_many_owned(payload_bytes)
            .map_err(|_| Error::invalid_request().data("Agent inbound payload limit reached"))?;
        self.requests.insert(
            request_id.clone(),
            CapturedInboundRequest {
                request_id: request_id.clone(),
                allocation: allocation.clone(),
                kind,
                route: route.clone(),
            },
        );
        Ok(InboundRequestLease(Arc::new(RequestLifetime {
            request_id,
            allocation,
            ingress,
            count: Some(count),
            bytes: Some(bytes),
        })))
    }

    pub(super) fn capture_cancel(&self, request_id: &RequestId) -> Option<CapturedInboundRequest> {
        self.requests.get(request_id).cloned()
    }

    /// Bind a creation-staged request once its returned session becomes live.
    /// Existing live owners, epochs, operations and allocations cannot change.
    pub(super) fn rebind(
        &mut self,
        request_id: &RequestId,
        allocation: &str,
        route: CapturedRoute,
    ) -> bool {
        let Some(captured) = self.requests.get_mut(request_id) else {
            return false;
        };
        let before = &captured.route;
        if captured.allocation != allocation
            || route.dispatch_owner.attempt_id.as_deref() != Some(allocation)
            || before.dispatch_owner.epoch != route.dispatch_owner.epoch
            || before.dispatch_owner.session_id != route.dispatch_owner.session_id
            || before.dispatch_owner.operation_id != route.dispatch_owner.operation_id
            || before.url_registration != route.url_registration
        {
            return false;
        }
        if before.owner.is_some() || before.dispatch_owner.incarnation.is_some() {
            return before.owner == route.owner && before.dispatch_owner == route.dispatch_owner;
        }
        let Some(owner) = &route.owner else {
            return false;
        };
        if owner.epoch != route.dispatch_owner.epoch
            || Some(owner.session_id.as_str()) != route.dispatch_owner.session_id.as_deref()
            || Some(owner.incarnation) != route.dispatch_owner.incarnation
        {
            return false;
        }
        captured.route = route;
        true
    }

    pub(super) fn finish(&mut self, request_id: &RequestId, allocation: &str) -> bool {
        if self
            .requests
            .get(request_id)
            .is_some_and(|request| request.allocation == allocation)
        {
            self.requests.remove(request_id);
            true
        } else {
            false
        }
    }
}

/// Clones are only for moving a request's lifetime through queued/task contexts.
/// Captured routing entries must never retain this lease.
#[derive(Clone)]
pub(super) struct InboundRequestLease(Arc<RequestLifetime>);

impl InboundRequestLease {
    pub(super) fn allocation(&self) -> &str {
        &self.0.allocation
    }
}

struct RequestLifetime {
    request_id: RequestId,
    allocation: String,
    ingress: IngressSender,
    count: Option<OwnedSemaphorePermit>,
    bytes: Option<OwnedSemaphorePermit>,
}

impl Drop for RequestLifetime {
    fn drop(&mut self) {
        drop(self.count.take());
        drop(self.bytes.take());
        // Synchronous enqueue, no detached cleanup task. Required-ingress
        // exhaustion already fails the connection through IngressSender.
        let _ = self
            .ingress
            .inbound_request_finished(self.request_id.clone(), self.allocation.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::super::EventSink;
    use super::super::scheduling::{BridgeIngress, Scheduling};
    use super::*;
    use crate::ordered_ingress::RequestOwner;
    use crate::session_resources::SessionResourceOwner;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn scheduling() -> (Scheduling, IngressSender) {
        let (tx, _events) = mpsc::unbounded_channel();
        Scheduling::new(
            "epoch".to_owned(),
            EventSink { tx: tx.into() },
            CancellationToken::new(),
        )
        .unwrap()
    }

    fn route(allocation: &str, incarnation: Option<u64>) -> CapturedRoute {
        CapturedRoute {
            owner: incarnation.map(|incarnation| SessionResourceOwner {
                epoch: "epoch".to_owned(),
                session_id: "session".to_owned(),
                incarnation,
            }),
            dispatch_owner: RequestOwner {
                epoch: "epoch".to_owned(),
                session_id: Some("session".to_owned()),
                incarnation,
                operation_id: "agent-request-1".to_owned(),
                attempt_id: Some(allocation.to_owned()),
            },
            url_registration: None,
        }
    }

    #[tokio::test]
    async fn cancellation_capture_does_not_keep_lease_alive_and_finish_is_allocation_exact() {
        let (mut scheduling, ingress) = scheduling();
        let mut requests = InboundRequests::new(2);
        let id = RequestId::from(1);
        let lease = requests
            .register(
                id.clone(),
                "session/request_permission",
                &route("old", Some(1)),
                ingress.clone(),
                0,
            )
            .unwrap();
        let duplicate = lease.clone();
        let cancelled = requests.capture_cancel(&id).unwrap();
        assert_eq!(cancelled.kind, InboundRequestKind::Permission);
        assert!(
            requests
                .capture_cancel(&RequestId::from("missing".to_owned()))
                .is_none()
        );
        drop(lease);
        assert!(scheduling.ingress_rx.try_recv().is_err());
        drop(duplicate);
        let BridgeIngress::InboundRequestFinished {
            request_id,
            allocation,
        } = scheduling.ingress_rx.try_recv().unwrap().event
        else {
            panic!("expected exact lifetime cleanup");
        };
        assert_eq!(request_id, id);
        assert_eq!(allocation, "old");
        assert!(scheduling.ingress_rx.try_recv().is_err());
        assert!(requests.finish(&request_id, &allocation));
        let _next = requests
            .register(
                id.clone(),
                "elicitation/create",
                &route("new", Some(2)),
                ingress,
                0,
            )
            .unwrap();
        assert!(!requests.finish(&cancelled.request_id, &cancelled.allocation));
        assert_eq!(cancelled.route.owner.unwrap().incarnation, 1);
        let current = requests.capture_cancel(&id).unwrap();
        assert_eq!(current.allocation, "new");
        assert_eq!(current.kind, InboundRequestKind::Elicitation);
        assert_eq!(current.route.owner.unwrap().incarnation, 2);
    }

    #[tokio::test]
    async fn routing_bounds_distinguish_numeric_and_string_ids() {
        let (_scheduling, ingress) = scheduling();
        let mut requests = InboundRequests::new(2);
        let number = RequestId::from(1);
        let string = RequestId::from("1".to_owned());
        let _first = requests
            .register(
                number.clone(),
                "fs/read_text_file",
                &route("number", Some(1)),
                ingress.clone(),
                0,
            )
            .unwrap();
        assert!(
            requests
                .register(
                    number.clone(),
                    "test",
                    &route("duplicate", Some(1)),
                    ingress.clone(),
                    0
                )
                .is_err()
        );
        let _second = requests
            .register(
                string.clone(),
                "fs/write_text_file",
                &route("string", Some(1)),
                ingress.clone(),
                0,
            )
            .unwrap();
        assert_eq!(
            requests.capture_cancel(&number).unwrap().allocation,
            "number"
        );
        assert_eq!(
            requests.capture_cancel(&string).unwrap().allocation,
            "string"
        );
        assert_eq!(
            requests.capture_cancel(&string).unwrap().kind,
            InboundRequestKind::Other
        );
        assert!(
            requests
                .register(
                    RequestId::from(2),
                    "test",
                    &route("overflow", Some(1)),
                    ingress,
                    0
                )
                .is_err()
        );
        assert_eq!(requests.requests.len(), 2);
    }

    #[tokio::test]
    async fn external_payload_budget_is_released_before_finish_delivery() {
        let (mut scheduling, ingress) = scheduling();
        let mut requests = InboundRequests::new(4);
        let first = requests
            .register(
                RequestId::from(1),
                "fs/write_text_file",
                &route("large", Some(1)),
                ingress.clone(),
                MAX_INBOUND_REQUEST_BYTES - 256,
            )
            .unwrap();
        assert_eq!(requests.bytes.available_permits(), 0);
        assert!(
            requests
                .register(
                    RequestId::from(2),
                    "test",
                    &route("blocked", Some(1)),
                    ingress.clone(),
                    0
                )
                .is_err()
        );
        assert_eq!(requests.requests.len(), 1);
        drop(first);
        // The coordinator has not consumed Finished yet. The lease, rather than
        // its routing row, owns the retained-work budget.
        let _next = requests
            .register(
                RequestId::from(2),
                "fs/write_text_file",
                &route("next", Some(1)),
                ingress.clone(),
                MAX_INBOUND_REQUEST_BYTES - 256,
            )
            .unwrap();
        assert!(matches!(
            scheduling.ingress_rx.try_recv().unwrap().event,
            BridgeIngress::InboundRequestFinished { .. }
        ));
        assert!(
            requests
                .register(
                    RequestId::from(3),
                    "test",
                    &route("oversized", Some(1)),
                    ingress,
                    usize::MAX
                )
                .is_err()
        );
        assert_eq!(requests.requests.len(), 2);
    }

    #[tokio::test]
    async fn creation_binding_is_once_and_cannot_recapture_a_replacement_owner() {
        let (_scheduling, ingress) = scheduling();
        let mut requests = InboundRequests::new(1);
        let id = RequestId::from(1);
        let _lease = requests
            .register(
                id.clone(),
                "elicitation/create",
                &route("allocation", None),
                ingress,
                0,
            )
            .unwrap();
        let early_cancel = requests.capture_cancel(&id).unwrap();
        assert!(early_cancel.route.owner.is_none());
        assert!(!requests.rebind(&id, "wrong", route("allocation", Some(1))));
        assert!(requests.rebind(&id, "allocation", route("allocation", Some(1))));
        assert!(requests.rebind(&id, "allocation", route("allocation", Some(1))));
        assert!(!requests.rebind(&id, "allocation", route("allocation", Some(2))));
        assert!(!requests.rebind(&id, "allocation", route("allocation", None)));
        let rebound = requests.capture_cancel(&id).unwrap();
        assert_eq!(rebound.allocation, early_cancel.allocation);
        assert_eq!(rebound.route.owner.unwrap().incarnation, 1);
    }
}
