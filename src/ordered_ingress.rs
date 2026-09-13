//! Ordered response registration at the shared ACP dispatch boundary.
//!
//! Notifications must be admitted to the same event queue by the caller. This module
//! neither schedules sessions nor waits for event consumers. A response is recorded
//! before its SDK waiter is released; local failures use the same registration owner
//! to avoid publishing a second completion after a normally received response.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::RequestId;
use agent_client_protocol::{
    Agent, ConnectionTo, Dispatch, Error, Handled, JsonRpcRequest, SentRequest,
};
use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RequestOwner {
    pub epoch: String,
    pub session_id: Option<String>,
    pub incarnation: Option<u64>,
    pub operation_id: String,
    pub attempt_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestClass {
    LongRunning,
    Control,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RequestBudget {
    pub requests: usize,
    pub completion_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct IngressLimits {
    pub max_response_bytes: usize,
    pub long_running: RequestBudget,
    pub control: RequestBudget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionSource {
    Response,
    LocalFailure,
}

#[derive(Debug)]
pub(crate) struct OrderedCompletion {
    pub owner: RequestOwner,
    pub method: String,
    pub request_id: RequestId,
    pub result: Result<Value, Error>,
    pub source: CompletionSource,
    pub response_bytes: usize,
    // Retain both budgets through delivery, so queued long-running completions
    // cannot repeatedly release and reacquire admission ahead of control traffic.
    _budget: CompletionBudget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionDisposition {
    Delivered,
    // Includes an unknown request ID. No unbounded settled-ID journal is retained.
    NotPending,
}

#[derive(Debug)]
pub(crate) enum IngressErrorKind {
    InvalidLimits(&'static str),
    InvalidResponseLimit {
        requested: usize,
        maximum: usize,
    },
    RequestSlotsExhausted(RequestClass),
    CompletionBytesExhausted(RequestClass),
    CompletionQueueFull,
    CompletionQueueClosed,
    ForeignReservation,
    DuplicateRequestId,
    RegistryPoisoned,
    ResponseTooLarge {
        bytes: usize,
        limit: usize,
    },
    Serialization(String),
    RouteFailed(Error),
    DeliveryAndRouteFailed {
        delivery: Box<IngressErrorKind>,
        route: Error,
    },
}

#[derive(Debug)]
pub(crate) struct IngressError {
    pub kind: IngressErrorKind,
    pub owner: Option<RequestOwner>,
    pub request_id: Option<RequestId>,
}

impl IngressError {
    fn new(kind: IngressErrorKind) -> Self {
        Self {
            kind,
            owner: None,
            request_id: None,
        }
    }

    fn for_request(kind: IngressErrorKind, owner: RequestOwner, request_id: RequestId) -> Self {
        Self {
            kind,
            owner: Some(owner),
            request_id: Some(request_id),
        }
    }
}

impl fmt::Display for IngressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ordered ACP ingress: {}", self.kind)
    }
}

impl std::error::Error for IngressError {}

impl fmt::Display for IngressErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits(message) => write!(formatter, "invalid limits: {message}"),
            Self::InvalidResponseLimit { requested, maximum } => {
                write!(
                    formatter,
                    "response reservation {requested} must be within 1..={maximum} bytes"
                )
            }
            Self::RequestSlotsExhausted(class) => {
                write!(formatter, "{class:?} request slots exhausted")
            }
            Self::CompletionBytesExhausted(class) => {
                write!(formatter, "{class:?} completion bytes exhausted")
            }
            Self::CompletionQueueFull => write!(formatter, "completion queue is full"),
            Self::CompletionQueueClosed => write!(formatter, "completion queue is closed"),
            Self::ForeignReservation => {
                write!(formatter, "reservation belongs to another connection")
            }
            Self::DuplicateRequestId => write!(formatter, "SDK request ID is already registered"),
            Self::RegistryPoisoned => write!(formatter, "request registry is poisoned"),
            Self::ResponseTooLarge { bytes, limit } => write!(
                formatter,
                "response has {bytes} bytes; reservation allows {limit}"
            ),
            Self::Serialization(error) => {
                write!(formatter, "response serialization failed: {error}")
            }
            Self::RouteFailed(error) => write!(formatter, "SDK response routing failed: {error}"),
            Self::DeliveryAndRouteFailed { delivery, route } => {
                write!(
                    formatter,
                    "{delivery}; SDK response routing also failed: {route}"
                )
            }
        }
    }
}

struct ClassBudget {
    requests: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
}

impl ClassBudget {
    fn new(budget: RequestBudget) -> Self {
        Self {
            requests: Arc::new(Semaphore::new(budget.requests)),
            bytes: Arc::new(Semaphore::new(budget.completion_bytes)),
        }
    }
}

#[derive(Debug)]
struct CompletionBudget {
    _request: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
}

pub(crate) struct RequestReservation<Event> {
    identity: Arc<()>,
    owner: RequestOwner,
    method: String,
    response_limit: usize,
    delivery: mpsc::OwnedPermit<Event>,
    budget: CompletionBudget,
}

struct Inner<Event> {
    identity: Arc<()>,
    pending: Mutex<HashMap<RequestId, RequestReservation<Event>>>,
    events: mpsc::Sender<Event>,
    to_event: fn(OrderedCompletion) -> Event,
    max_response_bytes: usize,
    long_running: ClassBudget,
    control: ClassBudget,
}

pub(crate) struct OrderedIngress<Event> {
    inner: Arc<Inner<Event>>,
}

impl<Event> Clone for OrderedIngress<Event> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

pub(crate) struct RegisteredRequest<Response> {
    pub request_id: RequestId,
    pub sent: SentRequest<Response>,
}

impl<Event: Send + 'static> OrderedIngress<Event> {
    pub(crate) fn new(
        limits: IngressLimits,
        events: mpsc::Sender<Event>,
        to_event: fn(OrderedCompletion) -> Event,
    ) -> Result<Self, IngressError> {
        let request_slots = limits
            .long_running
            .requests
            .checked_add(limits.control.requests);
        if request_slots.is_none_or(|slots| slots == 0 || slots > events.max_capacity()) {
            return Err(IngressError::new(IngressErrorKind::InvalidLimits(
                "event queue must hold the configured request slots for both classes",
            )));
        }
        if limits.max_response_bytes == 0 || limits.max_response_bytes > u32::MAX as usize {
            return Err(IngressError::new(IngressErrorKind::InvalidLimits(
                "response limit must be positive and fit the byte-permit counter",
            )));
        }
        for budget in [limits.long_running, limits.control] {
            if budget.requests > Semaphore::MAX_PERMITS
                || budget.completion_bytes > Semaphore::MAX_PERMITS
            {
                return Err(IngressError::new(IngressErrorKind::InvalidLimits(
                    "request or byte budget exceeds the semaphore capacity",
                )));
            }
        }
        Ok(Self {
            inner: Arc::new(Inner {
                identity: Arc::new(()),
                pending: Mutex::new(HashMap::new()),
                events,
                to_event,
                max_response_bytes: limits.max_response_bytes,
                long_running: ClassBudget::new(limits.long_running),
                control: ClassBudget::new(limits.control),
            }),
        })
    }

    /// Reserve every resource before dispatch. Dropping an unused reservation releases
    /// its request slot, byte budget, and queue slot together. Bytes are a reservation
    /// for one serialized response, not a cumulative history limit.
    pub(crate) fn try_reserve(
        &self,
        class: RequestClass,
        owner: RequestOwner,
        response_byte_limit: usize,
    ) -> Result<RequestReservation<Event>, IngressError> {
        if response_byte_limit == 0 || response_byte_limit > self.inner.max_response_bytes {
            return Err(IngressError::new(IngressErrorKind::InvalidResponseLimit {
                requested: response_byte_limit,
                maximum: self.inner.max_response_bytes,
            }));
        }
        let class_budget = match class {
            RequestClass::LongRunning => &self.inner.long_running,
            RequestClass::Control => &self.inner.control,
        };
        let request = class_budget
            .requests
            .clone()
            .try_acquire_owned()
            .map_err(|_| IngressError::new(IngressErrorKind::RequestSlotsExhausted(class)))?;
        let bytes = class_budget
            .bytes
            .clone()
            .try_acquire_many_owned(response_byte_limit as u32)
            .map_err(|_| IngressError::new(IngressErrorKind::CompletionBytesExhausted(class)))?;
        let delivery = self
            .inner
            .events
            .clone()
            .try_reserve_owned()
            .map_err(|error| {
                IngressError::new(match error {
                    mpsc::error::TrySendError::Full(_) => IngressErrorKind::CompletionQueueFull,
                    mpsc::error::TrySendError::Closed(_) => IngressErrorKind::CompletionQueueClosed,
                })
            })?;
        Ok(RequestReservation {
            identity: self.inner.identity.clone(),
            owner,
            method: String::new(),
            response_limit: response_byte_limit,
            delivery,
            budget: CompletionBudget {
                _request: request,
                _bytes: bytes,
            },
        })
    }

    pub(crate) fn send_registered<Req: JsonRpcRequest>(
        &self,
        connection: &ConnectionTo<Agent>,
        request: Req,
        mut reservation: RequestReservation<Event>,
    ) -> Result<RegisteredRequest<Req::Response>, IngressError> {
        reservation.method = request.method().to_string();
        self.register_with(reservation, || {
            let sent = connection.send_request(request);
            let request_id = sent.id().clone();
            (request_id.clone(), RegisteredRequest { request_id, sent })
        })
    }

    fn register_with<T>(
        &self,
        reservation: RequestReservation<Event>,
        send: impl FnOnce() -> (RequestId, T),
    ) -> Result<T, IngressError> {
        if !Arc::ptr_eq(&reservation.identity, &self.inner.identity) {
            return Err(IngressError::new(IngressErrorKind::ForeignReservation));
        }
        let mut pending = self
            .inner
            .pending
            .lock()
            .map_err(|_| IngressError::new(IngressErrorKind::RegistryPoisoned))?;
        // send_request only registers/enqueues SDK work. Hold this short synchronous
        // gate across send and ID assignment: a fast response cannot observe a gap.
        // No network write, actor completion, or external future is awaited here.
        let (request_id, result) = send();
        if pending.contains_key(&request_id) {
            return Err(IngressError::for_request(
                IngressErrorKind::DuplicateRequestId,
                reservation.owner,
                request_id,
            ));
        }
        pending.insert(request_id, reservation);
        Ok(result)
    }

    /// Call from on_receive_dispatch. Unregistered responses and other dispatches are
    /// declined. A failed event delivery still routes the RPC response before returning
    /// its explicit error, so the original SDK waiter cannot remain parked indefinitely.
    pub(crate) fn intercept(&self, dispatch: Dispatch) -> Result<Handled<Dispatch>, IngressError> {
        let Dispatch::Response(result, router) = dispatch else {
            return Ok(Handled::No {
                message: dispatch,
                retry: false,
            });
        };
        let request_id = router.id().clone();
        let completion = self.complete(&request_id, &result, CompletionSource::Response);
        if matches!(completion, Ok(CompletionDisposition::NotPending)) {
            return Ok(Handled::No {
                message: Dispatch::Response(result, router),
                retry: false,
            });
        }
        let route = router.route_with_result(result);
        match (completion, route) {
            (Ok(_), Ok(())) => Ok(Handled::Yes),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(IngressError {
                kind: IngressErrorKind::RouteFailed(error),
                owner: None,
                request_id: Some(request_id),
            }),
            (Err(mut error), Err(route)) => {
                error.kind = IngressErrorKind::DeliveryAndRouteFailed {
                    delivery: Box::new(error.kind),
                    route,
                };
                Err(error)
            }
        }
    }

    /// Only for a request that ended without a received response (e.g. EOF or local
    /// dispatch failure). The caller keeps the SDK response consumer in its existing
    /// bounded task; this module does not spawn a task per request.
    pub(crate) fn complete_without_response(
        &self,
        request_id: &RequestId,
        error: Error,
    ) -> Result<CompletionDisposition, IngressError> {
        self.complete(request_id, &Err(error), CompletionSource::LocalFailure)
    }

    fn complete(
        &self,
        request_id: &RequestId,
        result: &Result<Value, Error>,
        source: CompletionSource,
    ) -> Result<CompletionDisposition, IngressError> {
        let pending = self
            .inner
            .pending
            .lock()
            .map_err(|_| IngressError::new(IngressErrorKind::RegistryPoisoned))?
            .remove(request_id);
        let Some(RequestReservation {
            owner,
            method,
            response_limit,
            delivery,
            budget,
            ..
        }) = pending
        else {
            return Ok(CompletionDisposition::NotPending);
        };
        let fail = |kind| IngressError::for_request(kind, owner.clone(), request_id.clone());
        let bytes = response_bytes(result)
            .map_err(|error| fail(IngressErrorKind::Serialization(error.to_string())))?;
        if bytes > response_limit {
            return Err(fail(IngressErrorKind::ResponseTooLarge {
                bytes,
                limit: response_limit,
            }));
        }
        if self.inner.events.is_closed() {
            return Err(fail(IngressErrorKind::CompletionQueueClosed));
        }
        let event = (self.inner.to_event)(OrderedCompletion {
            owner: owner.clone(),
            method,
            request_id: request_id.clone(),
            result: result.clone(),
            source,
            response_bytes: bytes,
            _budget: budget,
        });
        let sender = delivery.send(event);
        // OwnedPermit::send is infallible even if the receiver has closed. Report a
        // closure observed around publication; success means enqueued, not consumed.
        if sender.is_closed() {
            return Err(fail(IngressErrorKind::CompletionQueueClosed));
        }
        Ok(CompletionDisposition::Delivered)
    }
}

fn response_bytes(result: &Result<Value, Error>) -> Result<usize, serde_json::Error> {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    match result {
        Ok(value) => serde_json::to_writer(&mut counter, value)?,
        Err(error) => serde_json::to_writer(&mut counter, error)?,
    }
    Ok(counter.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::{Client, Lines, UntypedMessage, is_incoming_transport_closed};
    use futures::{SinkExt, StreamExt};
    use serde_json::json;
    use std::sync::mpsc as thread_channel;
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(3);

    fn owner(operation_id: &str) -> RequestOwner {
        RequestOwner {
            epoch: "epoch".into(),
            session_id: Some("session".into()),
            incarnation: Some(7),
            operation_id: operation_id.into(),
            attempt_id: None,
        }
    }

    fn limits(long_running: usize, control: usize) -> IngressLimits {
        IngressLimits {
            max_response_bytes: 1024,
            long_running: RequestBudget {
                requests: long_running,
                completion_bytes: long_running * 1024,
            },
            control: RequestBudget {
                requests: control,
                completion_bytes: control * 1024,
            },
        }
    }

    fn setup() -> (
        OrderedIngress<OrderedCompletion>,
        mpsc::Receiver<OrderedCompletion>,
    ) {
        let (sender, receiver) = mpsc::channel(4);
        (
            OrderedIngress::new(limits(2, 1), sender, |event| event).unwrap(),
            receiver,
        )
    }

    fn register(
        ingress: &OrderedIngress<OrderedCompletion>,
        id: &str,
        class: RequestClass,
        response_limit: usize,
    ) -> RequestId {
        let reservation = ingress
            .try_reserve(class, owner(id), response_limit)
            .unwrap();
        let id = RequestId::from(id.to_string());
        ingress
            .register_with(reservation, || (id.clone(), ()))
            .unwrap();
        id
    }

    #[test]
    fn unsent_reservation_releases_request_bytes_and_delivery_slots() {
        let (ingress, _receiver) = setup();
        let available = ingress.inner.events.capacity();
        let reservation = ingress
            .try_reserve(RequestClass::LongRunning, owner("unused"), 1024)
            .unwrap();
        assert_eq!(ingress.inner.events.capacity(), available - 1);
        assert_eq!(ingress.inner.long_running.requests.available_permits(), 1);
        assert_eq!(ingress.inner.long_running.bytes.available_permits(), 1024);
        drop(reservation);
        assert_eq!(ingress.inner.events.capacity(), available);
        assert_eq!(ingress.inner.long_running.requests.available_permits(), 2);
        assert_eq!(ingress.inner.long_running.bytes.available_permits(), 2048);
    }

    #[test]
    fn long_running_requests_and_queued_completions_leave_control_budget_available() {
        let (sender, mut receiver) = mpsc::channel(2);
        let ingress = OrderedIngress::new(limits(1, 1), sender, |event| event).unwrap();
        let id = register(&ingress, "long", RequestClass::LongRunning, 1024);
        assert!(matches!(
            ingress.complete(&id, &Ok(json!({ "ok": true })), CompletionSource::Response),
            Ok(CompletionDisposition::Delivered),
        ));
        assert!(matches!(
            ingress.try_reserve(RequestClass::LongRunning, owner("next-long"), 1024),
            Err(IngressError {
                kind: IngressErrorKind::RequestSlotsExhausted(RequestClass::LongRunning),
                ..
            }),
        ));
        let control = ingress
            .try_reserve(RequestClass::Control, owner("list"), 1024)
            .unwrap();
        assert_eq!(ingress.inner.events.capacity(), 0);
        drop(receiver.try_recv().unwrap());
        let next = ingress
            .try_reserve(RequestClass::LongRunning, owner("next-long"), 1024)
            .unwrap();
        drop((control, next));
    }

    #[test]
    fn byte_admission_failure_rolls_back_the_request_slot() {
        let (sender, _receiver) = mpsc::channel(3);
        let mut configured = limits(2, 1);
        configured.long_running.completion_bytes = 1024;
        let ingress = OrderedIngress::new(configured, sender, |event| event).unwrap();
        let first = ingress
            .try_reserve(RequestClass::LongRunning, owner("first"), 768)
            .unwrap();
        assert!(matches!(
            ingress.try_reserve(RequestClass::LongRunning, owner("second"), 512),
            Err(IngressError {
                kind: IngressErrorKind::CompletionBytesExhausted(RequestClass::LongRunning),
                ..
            }),
        ));
        assert_eq!(ingress.inner.long_running.requests.available_permits(), 1);
        assert_eq!(ingress.inner.long_running.bytes.available_permits(), 256);
        let control = ingress
            .try_reserve(RequestClass::Control, owner("cancel"), 1024)
            .unwrap();
        drop(first);
        assert!(
            ingress
                .try_reserve(RequestClass::LongRunning, owner("second"), 1024)
                .is_ok()
        );
        drop(control);
    }

    #[test]
    fn response_and_local_failure_share_one_completion_owner() {
        for response_first in [true, false] {
            let (ingress, mut receiver) = setup();
            let id = register(&ingress, "request", RequestClass::LongRunning, 1024);
            let response =
                || ingress.complete(&id, &Ok(json!({ "ok": true })), CompletionSource::Response);
            let local =
                || ingress.complete_without_response(&id, Error::internal_error().data("EOF"));
            let (first, second) = if response_first {
                (response(), local())
            } else {
                (local(), response())
            };
            assert!(matches!(first, Ok(CompletionDisposition::Delivered)));
            assert!(matches!(second, Ok(CompletionDisposition::NotPending)));
            let completion = receiver.try_recv().unwrap();
            assert_eq!(completion.owner, owner("request"));
            assert_eq!(completion.request_id, id);
            assert_eq!(
                completion.response_bytes,
                response_bytes(&completion.result).unwrap()
            );
            assert_eq!(
                completion.source,
                if response_first {
                    CompletionSource::Response
                } else {
                    CompletionSource::LocalFailure
                }
            );
            assert!(receiver.try_recv().is_err());
            drop(completion);
            assert_eq!(ingress.inner.long_running.requests.available_permits(), 2);
        }
    }

    #[test]
    fn response_cannot_pass_between_send_and_owner_registration() {
        let (ingress, mut receiver) = setup();
        let reservation = ingress
            .try_reserve(RequestClass::LongRunning, owner("fast"), 1024)
            .unwrap();
        let (sending, sent) = thread_channel::sync_channel(1);
        let (release, released) = thread_channel::sync_channel(1);
        let (attempted, attempt) = thread_channel::sync_channel(1);
        let sender = ingress.clone();
        let sending_thread = std::thread::spawn(move || {
            sender
                .register_with(reservation, || {
                    sending.send(()).unwrap();
                    released.recv_timeout(TIMEOUT).unwrap();
                    (RequestId::from("fast".to_string()), ())
                })
                .unwrap();
        });
        sent.recv_timeout(TIMEOUT).unwrap();
        let responding = ingress.clone();
        let response_thread = std::thread::spawn(move || {
            // The test forces a response to be ready before the send function returns.
            assert!(matches!(
                responding.inner.pending.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
            attempted.send(()).unwrap();
            responding.complete(
                &RequestId::from("fast".to_string()),
                &Ok(json!("response")),
                CompletionSource::Response,
            )
        });
        attempt.recv_timeout(TIMEOUT).unwrap();
        release.send(()).unwrap();
        sending_thread.join().unwrap();
        assert!(matches!(
            response_thread.join().unwrap(),
            Ok(CompletionDisposition::Delivered)
        ));
        assert_eq!(receiver.try_recv().unwrap().owner.operation_id, "fast");
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn response_limits_and_closed_delivery_report_the_affected_owner() {
        let (ingress, mut receiver) = setup();
        let id = register(&ingress, "large", RequestClass::LongRunning, 16);
        let error = ingress
            .complete(&id, &Ok(json!("x".repeat(64))), CompletionSource::Response)
            .unwrap_err();
        assert!(matches!(
            error.kind,
            IngressErrorKind::ResponseTooLarge { limit: 16, .. }
        ));
        assert_eq!(error.owner, Some(owner("large")));
        assert_eq!(error.request_id, Some(id));
        assert!(receiver.try_recv().is_err());
        assert_eq!(ingress.inner.long_running.requests.available_permits(), 2);
        let id = register(&ingress, "closed", RequestClass::Control, 1024);
        drop(receiver);
        let error = ingress
            .complete_without_response(&id, Error::internal_error().data("EOF"))
            .unwrap_err();
        assert!(matches!(
            error.kind,
            IngressErrorKind::CompletionQueueClosed
        ));
        assert_eq!(error.owner, Some(owner("closed")));
        assert_eq!(error.request_id, Some(id));
        assert_eq!(ingress.inner.control.requests.available_permits(), 1);
    }

    #[test]
    fn reservation_from_another_connection_is_rejected_without_sending() {
        let (first, _first_receiver) = setup();
        let (second, _second_receiver) = setup();
        let reservation = first
            .try_reserve(RequestClass::LongRunning, owner("foreign"), 1024)
            .unwrap();
        let error = second
            .register_with(reservation, || -> (RequestId, ()) {
                panic!("foreign reservation must not send")
            })
            .unwrap_err();
        assert!(matches!(error.kind, IngressErrorKind::ForeignReservation));
        assert_eq!(first.inner.long_running.requests.available_permits(), 2);
        assert_eq!(first.inner.events.capacity(), 4);
    }

    enum TestEvent {
        Completion(OrderedCompletion),
        Notification(Value),
    }

    #[test]
    fn queue_admission_reports_full_or_closed_and_releases_partial_reservations() {
        let (events, mut receiver) = mpsc::channel(3);
        let ingress =
            OrderedIngress::new(limits(2, 1), events.clone(), TestEvent::Completion).unwrap();
        for _ in 0..3 {
            assert!(
                events
                    .try_send(TestEvent::Notification(Value::Null))
                    .is_ok()
            );
        }
        assert!(matches!(
            ingress.try_reserve(RequestClass::Control, owner("full"), 1024),
            Err(IngressError {
                kind: IngressErrorKind::CompletionQueueFull,
                ..
            }),
        ));
        assert_eq!(ingress.inner.control.requests.available_permits(), 1);
        assert_eq!(ingress.inner.control.bytes.available_permits(), 1024);
        drop(receiver.try_recv().unwrap());
        assert!(
            ingress
                .try_reserve(RequestClass::Control, owner("available"), 1024)
                .is_ok()
        );
        drop(receiver);
        assert!(matches!(
            ingress.try_reserve(RequestClass::Control, owner("closed"), 1024),
            Err(IngressError {
                kind: IngressErrorKind::CompletionQueueClosed,
                ..
            }),
        ));
        assert_eq!(ingress.inner.control.requests.available_permits(), 1);
        assert_eq!(ingress.inner.control.bytes.available_permits(), 1024);
    }

    #[tokio::test]
    async fn incoming_eof_publishes_one_local_failure_for_the_registered_owner() {
        let (events, mut receiver) = mpsc::channel(2);
        let ingress = OrderedIngress::new(limits(1, 1), events, |event| event).unwrap();
        let (outgoing, mut peer_requests) = futures::channel::mpsc::channel::<String>(4);
        let (peer_responses, incoming) = futures::channel::mpsc::channel::<io::Result<String>>(4);
        let transport = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
        let connection = Client
            .builder()
            .on_receive_dispatch(
                {
                    let ingress = ingress.clone();
                    async move |dispatch: Dispatch, _connection| {
                        ingress
                            .intercept(dispatch)
                            .map_err(|error| Error::internal_error().data(error.to_string()))
                    }
                },
                agent_client_protocol::on_receive_dispatch!(),
            )
            .connect_with(transport, async move |connection| {
                let reservation = ingress
                    .try_reserve(RequestClass::LongRunning, owner("lost"), 1024)
                    .unwrap();
                let request = ingress
                    .send_registered(
                        &connection,
                        UntypedMessage::new("test/never-respond", json!({})).unwrap(),
                        reservation,
                    )
                    .unwrap();
                let error = request.sent.block_task().await.unwrap_err();
                assert!(is_incoming_transport_closed(&error));
                assert!(matches!(
                    ingress.complete_without_response(&request.request_id, error.clone()),
                    Ok(CompletionDisposition::Delivered)
                ));
                assert!(matches!(
                    ingress.complete_without_response(&request.request_id, error),
                    Ok(CompletionDisposition::NotPending)
                ));
                let completion = receiver.recv().await.unwrap();
                assert_eq!(completion.source, CompletionSource::LocalFailure);
                assert_eq!(completion.request_id, request.request_id);
                assert_eq!(completion.owner, owner("lost"));
                assert!(is_incoming_transport_closed(
                    &completion.result.unwrap_err()
                ));
                assert!(receiver.try_recv().is_err());
                Ok(())
            });
        let peer = async move {
            assert!(peer_requests.next().await.is_some());
            drop(peer_responses);
        };
        let (result, ()) = tokio::time::timeout(TIMEOUT, futures::future::join(connection, peer))
            .await
            .expect("EOF did not settle the pending request");
        result.unwrap();
    }

    #[tokio::test]
    async fn shared_queue_preserves_updates_on_both_sides_of_creation_response() {
        let (events, mut received) = mpsc::channel(6);
        let ingress =
            OrderedIngress::new(limits(2, 1), events.clone(), TestEvent::Completion).unwrap();
        let (outgoing, mut peer_requests) = futures::channel::mpsc::channel::<String>(4);
        let (mut peer_responses, incoming) =
            futures::channel::mpsc::channel::<io::Result<String>>(4);
        let transport = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
        let (done, finished) = tokio::sync::oneshot::channel();
        let connection = Client
            .builder()
            .on_receive_dispatch(
                {
                    let ingress = ingress.clone();
                    async move |dispatch: Dispatch, _connection| {
                        if let Dispatch::Notification(notification) = dispatch {
                            events
                                .try_send(TestEvent::Notification(notification.params))
                                .map_err(|error| Error::internal_error().data(error.to_string()))?;
                            Ok(Handled::Yes)
                        } else {
                            ingress
                                .intercept(dispatch)
                                .map_err(|error| Error::internal_error().data(error.to_string()))
                        }
                    }
                },
                agent_client_protocol::on_receive_dispatch!(),
            )
            .connect_with(transport, async move |connection| {
                let mut creation_owner = owner("new-session");
                creation_owner.session_id = None;
                creation_owner.incarnation = None;
                let reservation = ingress
                    .try_reserve(RequestClass::Control, creation_owner, 1024)
                    .unwrap();
                let request = ingress
                    .send_registered(
                        &connection,
                        UntypedMessage::new("session/new", json!({})).unwrap(),
                        reservation,
                    )
                    .unwrap();
                let result = request.sent.block_task().await?;
                assert_eq!(result["sessionId"], "created");
                let first = received.recv().await.unwrap();
                let second = received.recv().await.unwrap();
                let third = received.recv().await.unwrap();
                let TestEvent::Notification(early) = first else {
                    panic!("early update must stay first")
                };
                let TestEvent::Completion(completion) = second else {
                    panic!("response must precede later updates")
                };
                let TestEvent::Notification(live) = third else {
                    panic!("live update must stay last")
                };
                assert_eq!(early["sessionId"], "created");
                assert_eq!(early["order"], "before");
                assert_eq!(completion.owner.operation_id, "new-session");
                assert_eq!(completion.owner.session_id, None);
                assert_eq!(completion.result.unwrap()["sessionId"], "created");
                assert_eq!(live["order"], "after");
                done.send(()).unwrap();
                Ok(())
            });
        let peer = async move {
            let request: Value =
                serde_json::from_str(&peer_requests.next().await.unwrap()).unwrap();
            peer_responses.send(Ok(json!([
                { "jsonrpc": "2.0", "method": "test/update", "params": { "sessionId": "created", "order": "before" } },
                { "jsonrpc": "2.0", "id": request["id"], "result": { "sessionId": "created" } },
                { "jsonrpc": "2.0", "method": "test/update", "params": { "sessionId": "created", "order": "after" } }
            ]).to_string())).await.unwrap();
            finished.await.unwrap();
        };
        let (result, ()) = tokio::time::timeout(TIMEOUT, futures::future::join(connection, peer))
            .await
            .expect("ordered response flow hung");
        result.unwrap();
    }

    #[tokio::test]
    async fn closed_completion_receiver_does_not_leave_the_rpc_waiter_parked() {
        let (events, receiver) = mpsc::channel(2);
        let ingress = OrderedIngress::new(limits(1, 1), events, |event| event).unwrap();
        let delivery_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (outgoing, mut peer_requests) = futures::channel::mpsc::channel::<String>(4);
        let (mut peer_responses, incoming) =
            futures::channel::mpsc::channel::<io::Result<String>>(4);
        let transport = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
        let (done, finished) = tokio::sync::oneshot::channel();
        let connection = Client
            .builder()
            .on_receive_dispatch(
                {
                    let ingress = ingress.clone();
                    let delivery_failed = delivery_failed.clone();
                    async move |dispatch: Dispatch, _connection| {
                        match ingress.intercept(dispatch) {
                            Ok(handled) => Ok(handled),
                            Err(error) => {
                                assert!(matches!(
                                    error.kind,
                                    IngressErrorKind::CompletionQueueClosed
                                ));
                                delivery_failed.store(true, std::sync::atomic::Ordering::Release);
                                // The integration owns the shutdown policy; the original RPC
                                // result must already have been routed before this error arrives.
                                Ok(Handled::Yes)
                            }
                        }
                    }
                },
                agent_client_protocol::on_receive_dispatch!(),
            )
            .connect_with(transport, async move |connection| {
                let reservation = ingress
                    .try_reserve(RequestClass::Control, owner("query"), 1024)
                    .unwrap();
                let request = ingress
                    .send_registered(
                        &connection,
                        UntypedMessage::new("test/query", json!({})).unwrap(),
                        reservation,
                    )
                    .unwrap();
                drop(receiver);
                let result = request.sent.block_task().await?;
                assert_eq!(result, json!({ "ok": true }));
                done.send(()).unwrap();
                Ok(())
            });
        let peer = async move {
            let request: Value =
                serde_json::from_str(&peer_requests.next().await.unwrap()).unwrap();
            peer_responses
                .send(Ok(
                    json!({ "jsonrpc": "2.0", "id": request["id"], "result": { "ok": true } })
                        .to_string(),
                ))
                .await
                .unwrap();
            finished.await.unwrap();
        };
        let (result, ()) = tokio::time::timeout(TIMEOUT, futures::future::join(connection, peer))
            .await
            .expect("SDK response waiter remained parked");
        result.unwrap();
        assert!(delivery_failed.load(std::sync::atomic::Ordering::Acquire));
    }
}
