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
use tokio::sync::mpsc;

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
    /// Completion provenance, asserted by tests; production routes uniformly.
    #[cfg_attr(not(test), allow(dead_code))]
    pub source: CompletionSource,
    pub response_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionDisposition {
    Delivered,
    // Includes an unknown request ID. No unbounded settled-ID journal is retained.
    NotPending,
}

#[derive(Debug)]
pub(crate) enum IngressErrorKind {
    CompletionQueueClosed,
    ForeignReservation,
    DuplicateRequestId,
    RegistryPoisoned,
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
    /// Failure context asserted by tests; production only reports the kind.
    #[cfg_attr(not(test), allow(dead_code))]
    pub owner: Option<RequestOwner>,
    #[cfg_attr(not(test), allow(dead_code))]
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
            Self::CompletionQueueClosed => write!(formatter, "completion queue is closed"),
            Self::ForeignReservation => {
                write!(formatter, "reservation belongs to another connection")
            }
            Self::DuplicateRequestId => write!(formatter, "SDK request ID is already registered"),
            Self::RegistryPoisoned => write!(formatter, "request registry is poisoned"),
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

pub(crate) struct RequestReservation<Event> {
    identity: Arc<()>,
    owner: RequestOwner,
    method: String,
    delivery: mpsc::UnboundedSender<Event>,
}

struct Inner<Event> {
    identity: Arc<()>,
    pending: Mutex<HashMap<RequestId, RequestReservation<Event>>>,
    events: mpsc::UnboundedSender<Event>,
    to_event: fn(OrderedCompletion) -> Event,
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
        events: mpsc::UnboundedSender<Event>,
        to_event: fn(OrderedCompletion) -> Event,
    ) -> Result<Self, IngressError> {
        Ok(Self {
            inner: Arc::new(Inner {
                identity: Arc::new(()),
                pending: Mutex::new(HashMap::new()),
                events,
                to_event,
            }),
        })
    }

    /// Capture the request owner before dispatch. Completion uses the shared FIFO.
    pub(crate) fn try_reserve(
        &self,
        _class: RequestClass,
        owner: RequestOwner,
    ) -> Result<RequestReservation<Event>, IngressError> {
        if self.inner.events.is_closed() {
            return Err(IngressError::new(IngressErrorKind::CompletionQueueClosed));
        }
        let delivery = self.inner.events.clone();
        Ok(RequestReservation {
            identity: self.inner.identity.clone(),
            owner,
            method: String::new(),
            delivery,
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
    /// owned task; this module does not spawn a task per request.
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
            delivery,
            ..
        }) = pending
        else {
            return Ok(CompletionDisposition::NotPending);
        };
        let fail = |kind| IngressError::for_request(kind, owner.clone(), request_id.clone());
        let bytes = response_bytes(result)
            .map_err(|error| fail(IngressErrorKind::Serialization(error.to_string())))?;
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
        });
        delivery
            .send(event)
            .map_err(|_| fail(IngressErrorKind::CompletionQueueClosed))?;
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

    fn setup() -> (
        OrderedIngress<OrderedCompletion>,
        mpsc::UnboundedReceiver<OrderedCompletion>,
    ) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            OrderedIngress::new(sender, |event| event).unwrap(),
            receiver,
        )
    }

    fn register(
        ingress: &OrderedIngress<OrderedCompletion>,
        id: &str,
        class: RequestClass,
    ) -> RequestId {
        let reservation = ingress.try_reserve(class, owner(id)).unwrap();
        let id = RequestId::from(id.to_string());
        ingress
            .register_with(reservation, || (id.clone(), ()))
            .unwrap();
        id
    }

    #[test]
    fn response_and_local_failure_share_one_completion_owner() {
        for response_first in [true, false] {
            let (ingress, mut receiver) = setup();
            let id = register(&ingress, "request", RequestClass::LongRunning);
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
        }
    }

    #[test]
    fn response_cannot_pass_between_send_and_owner_registration() {
        let (ingress, mut receiver) = setup();
        let reservation = ingress
            .try_reserve(RequestClass::LongRunning, owner("fast"))
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
    fn large_response_is_delivered_and_closed_delivery_reports_the_affected_owner() {
        let (ingress, mut receiver) = setup();
        let id = register(&ingress, "large", RequestClass::LongRunning);
        let payload = json!("x".repeat(8_000_001));
        ingress
            .complete(&id, &Ok(payload.clone()), CompletionSource::Response)
            .unwrap();
        let completion = receiver.try_recv().unwrap();
        assert_eq!(completion.result, Ok(payload));
        assert_eq!(completion.request_id, id);
        let id = register(&ingress, "closed", RequestClass::Control);
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
    }

    #[test]
    fn reservation_from_another_connection_is_rejected_without_sending() {
        let (first, _first_receiver) = setup();
        let (second, _second_receiver) = setup();
        let reservation = first
            .try_reserve(RequestClass::LongRunning, owner("foreign"))
            .unwrap();
        let error = second
            .register_with(reservation, || -> (RequestId, ()) {
                panic!("foreign reservation must not send")
            })
            .unwrap_err();
        assert!(matches!(error.kind, IngressErrorKind::ForeignReservation));
    }

    enum TestEvent {
        Completion(OrderedCompletion),
        Notification(Value),
    }

    #[test]
    fn completion_delivery_survives_an_unread_notification_backlog() {
        let (events, mut receiver) = mpsc::unbounded_channel();
        let ingress = OrderedIngress::new(events.clone(), TestEvent::Completion).unwrap();
        for index in 0..4_096 {
            events.send(TestEvent::Notification(json!(index))).unwrap();
        }
        let reservation = ingress
            .try_reserve(RequestClass::Control, owner("complete"))
            .unwrap();
        let id = RequestId::from("complete".to_string());
        ingress
            .register_with(reservation, || (id.clone(), ()))
            .unwrap();
        ingress
            .complete(&id, &Ok(json!({"ok":true})), CompletionSource::Response)
            .unwrap();
        for index in 0..4_096 {
            let TestEvent::Notification(value) = receiver.try_recv().unwrap() else {
                panic!("completion overtook content")
            };
            assert_eq!(value, index);
        }
        assert!(matches!(
            receiver.try_recv().unwrap(),
            TestEvent::Completion(_)
        ));
        drop(receiver);
        assert!(matches!(
            ingress.try_reserve(RequestClass::Control, owner("closed")),
            Err(IngressError {
                kind: IngressErrorKind::CompletionQueueClosed,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn incoming_eof_publishes_one_local_failure_for_the_registered_owner() {
        let (events, mut receiver) = mpsc::unbounded_channel();
        let ingress = OrderedIngress::new(events, |event| event).unwrap();
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
                    .try_reserve(RequestClass::LongRunning, owner("lost"))
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
        let (events, mut received) = mpsc::unbounded_channel();
        let ingress = OrderedIngress::new(events.clone(), TestEvent::Completion).unwrap();
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
                                .send(TestEvent::Notification(notification.params))
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
                    .try_reserve(RequestClass::Control, creation_owner)
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
        let (events, receiver) = mpsc::unbounded_channel();
        let ingress = OrderedIngress::new(events, |event| event).unwrap();
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
                    .try_reserve(RequestClass::Control, owner("query"))
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
