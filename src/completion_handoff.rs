//! Temporary handoff from an ordered session delivery to an existing RPC task.
//!
//! Register before `OrderedIngress::send_registered`, then bind its returned SDK
//! request ID synchronously, before waiting for the Agent. Only an `RpcCompleted`
//! delivery dequeued from that session's FIFO may supply the delivery guard. The
//! RPC task waits without a guard and uses the received turn for its short, local
//! result transition. A turn must never span Agent/user I/O or resource cleanup.
//!
//! Owner equality alone cannot identify a registration: its private allocation
//! fences withdrawals, and binding the request ID fences late responses even when
//! an owner tuple is reused. At most one completion can wait for a binding. An
//! additional early completion is explicitly rejected, never silently replaced.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, Weak};

use agent_client_protocol::schema::v1::RequestId;
use tokio::sync::{Notify, oneshot};

use crate::ordered_ingress::{OrderedCompletion, RequestOwner};
use crate::session_dispatch::DeliveryGuard;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandoffError {
    InvalidLimit,
    Closed,
    RegistrationLimit,
    DuplicateOwner,
    RegistryPoisoned,
    Withdrawn,
    AlreadyBound,
    Unbound,
    EarlyCompletionAlreadyParked,
    CompletionUnavailable,
}

impl fmt::Display for HandoffError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "completion handoff: {self:?}")
    }
}

impl std::error::Error for HandoffError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandoffDisposition {
    ParkedUntilBound,
    Delivered,
    Unregistered,
    StaleRequestId,
    ReceiverGone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BindDisposition {
    AwaitingCompletion {
        // Discarding an old response does not terminate the new registration.
        discarded_early_request_id: Option<RequestId>,
    },
    Delivered,
    ReceiverGone,
    Discarded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiscardDisposition {
    Discarded,
    ParkedUntilBound,
    Unregistered,
    StaleRequestId,
}

/// Owns both ingress completion budgets and the session's current FIFO delivery.
/// Dropping it releases all budgets and the busy flag before waking the pump.
#[must_use = "reduce the local result transition or drop the turn to release the session"]
#[derive(Debug)]
pub(crate) struct CompletionTurn {
    completion: Option<OrderedCompletion>,
    guard: Option<DeliveryGuard>,
    wake: Arc<Notify>,
}

impl CompletionTurn {
    fn new(completion: OrderedCompletion, guard: DeliveryGuard, wake: Arc<Notify>) -> Self {
        Self {
            completion: Some(completion),
            guard: Some(guard),
            wake,
        }
    }

    pub(crate) fn completion(&self) -> &OrderedCompletion {
        self.completion.as_ref().expect("live completion turn")
    }

    /// Run a synchronous local result transition, retaining the guard through
    /// success, early error, and panic. The completion itself cannot be moved out
    /// of this API; callers may take its result while keeping its budget here.
    /// Return external cleanup work for execution only after this method returns.
    pub(crate) fn reduce<R>(mut self, reduce: impl FnOnce(&mut OrderedCompletion) -> R) -> R {
        let result = reduce(self.completion.as_mut().expect("live completion turn"));
        drop(self);
        result
    }
}

impl Drop for CompletionTurn {
    fn drop(&mut self) {
        drop(self.completion.take());
        drop(self.guard.take());
        self.wake.notify_one();
    }
}

struct Entry {
    allocation: Arc<()>,
    request_id: Option<RequestId>,
    sender: oneshot::Sender<CompletionTurn>,
    early: Option<CompletionTurn>,
    discarded_early: Option<RequestId>,
}

struct State {
    closed: bool,
    entries: HashMap<RequestOwner, Entry>,
}

struct Inner {
    limit: usize,
    wake: Arc<Notify>,
    state: Mutex<State>,
}

#[derive(Clone)]
pub(crate) struct CompletionHandoff {
    inner: Arc<Inner>,
}

impl CompletionHandoff {
    pub(crate) fn new(limit: usize, wake: Arc<Notify>) -> Result<Self, HandoffError> {
        if limit == 0 {
            return Err(HandoffError::InvalidLimit);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                limit,
                wake,
                state: Mutex::new(State {
                    closed: false,
                    entries: HashMap::new(),
                }),
            }),
        })
    }

    /// Reserve context before sending the RPC. This registration holds no
    /// delivery guard while the RPC is pending and cannot wait for capacity.
    pub(crate) fn register(
        &self,
        owner: RequestOwner,
    ) -> Result<CompletionRegistration, HandoffError> {
        let (sender, receiver) = oneshot::channel();
        let allocation = Arc::new(());
        {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| HandoffError::RegistryPoisoned)?;
            if state.closed {
                return Err(HandoffError::Closed);
            }
            if state.entries.contains_key(&owner) {
                return Err(HandoffError::DuplicateOwner);
            }
            if state.entries.len() == self.inner.limit {
                return Err(HandoffError::RegistrationLimit);
            }
            state.entries.insert(
                owner.clone(),
                Entry {
                    allocation: allocation.clone(),
                    request_id: None,
                    sender,
                    early: None,
                    discarded_early: None,
                },
            );
        }
        Ok(CompletionRegistration {
            inner: Arc::downgrade(&self.inner),
            owner,
            allocation,
            bound: false,
            receiver,
        })
    }

    /// Called only for a completion taken from the owning session FIFO. Every
    /// rejected or failed handoff releases the delivery and wakes the dispatcher.
    pub(crate) fn handoff(
        &self,
        completion: OrderedCompletion,
        guard: DeliveryGuard,
    ) -> Result<HandoffDisposition, HandoffError> {
        let turn = CompletionTurn::new(completion, guard, self.inner.wake.clone());
        let entry = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| HandoffError::RegistryPoisoned)?;
            if state.closed {
                return Err(HandoffError::Closed);
            }
            let completion = turn.completion();
            let Some(entry) = state.entries.get_mut(&completion.owner) else {
                return Ok(HandoffDisposition::Unregistered);
            };
            match &entry.request_id {
                None => {
                    if entry.early.is_some() || entry.discarded_early.is_some() {
                        return Err(HandoffError::EarlyCompletionAlreadyParked);
                    }
                    entry.early = Some(turn);
                    return Ok(HandoffDisposition::ParkedUntilBound);
                }
                Some(request_id) if request_id != &completion.request_id => {
                    return Ok(HandoffDisposition::StaleRequestId);
                }
                Some(_) => state
                    .entries
                    .remove(&completion.owner)
                    .expect("matched entry"),
            }
        };
        // Sending is synchronous and outside the registry mutex. On cancellation
        // the returned turn drops here, including its guard and wake notification.
        Ok(match entry.sender.send(turn) {
            Ok(()) => HandoffDisposition::Delivered,
            Err(_turn) => HandoffDisposition::ReceiverGone,
        })
    }

    /// Withdraw only the registration bound to this exact response. Before bind,
    /// retain one request ID as a tombstone; a later bind to another ID discards
    /// the tombstone and keeps the new registration alive. Payloads and senders
    /// are always dropped after releasing the registry mutex.
    pub(crate) fn discard(
        &self,
        owner: &RequestOwner,
        request_id: &RequestId,
    ) -> Result<DiscardDisposition, HandoffError> {
        let (disposition, removed, early) = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| HandoffError::RegistryPoisoned)?;
            let Some(entry) = state.entries.get_mut(owner) else {
                return Ok(DiscardDisposition::Unregistered);
            };
            match &entry.request_id {
                Some(bound) if bound != request_id => {
                    return Ok(DiscardDisposition::StaleRequestId);
                }
                Some(_) => (
                    DiscardDisposition::Discarded,
                    state.entries.remove(owner),
                    None,
                ),
                None => {
                    if entry
                        .discarded_early
                        .as_ref()
                        .is_some_and(|id| id != request_id)
                        || entry
                            .early
                            .as_ref()
                            .is_some_and(|turn| &turn.completion().request_id != request_id)
                    {
                        return Err(HandoffError::EarlyCompletionAlreadyParked);
                    }
                    entry.discarded_early = Some(request_id.clone());
                    (
                        DiscardDisposition::ParkedUntilBound,
                        None,
                        entry.early.take(),
                    )
                }
            }
        };
        drop(early);
        drop(removed);
        Ok(disposition)
    }

    /// Retire registrations belonging to exactly this canonical incarnation.
    /// Already transferred turns and any replacement incarnation are untouched.
    pub(crate) fn discard_session(
        &self,
        epoch: &str,
        session_id: &str,
        incarnation: u64,
    ) -> Result<usize, HandoffError> {
        let removed = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| HandoffError::RegistryPoisoned)?;
            let owners = state
                .entries
                .keys()
                .filter(|owner| {
                    owner.epoch == epoch
                        && owner.session_id.as_deref() == Some(session_id)
                        && owner.incarnation == Some(incarnation)
                })
                .cloned()
                .collect::<Vec<_>>();
            owners
                .into_iter()
                .filter_map(|owner| state.entries.remove(&owner))
                .collect::<Vec<_>>()
        };
        let count = removed.len();
        for mut entry in removed {
            drop(entry.early.take());
            drop(entry);
        }
        Ok(count)
    }

    /// Cancel pending registrations. Already handed-off turns remain owned by
    /// their tasks and release their own guards when their local transition ends.
    pub(crate) fn close(&self) {
        let entries = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.closed = true;
            std::mem::take(&mut state.entries)
        };
        drop(entries);
    }
}

#[must_use = "keep the registration alive until its ordered completion is received"]
pub(crate) struct CompletionRegistration {
    // A pending task must not keep its own sender alive after every dispatcher
    // handle exits. Dropping the last handoff then closes all pending receivers.
    inner: Weak<Inner>,
    owner: RequestOwner,
    allocation: Arc<()>,
    bound: bool,
    receiver: oneshot::Receiver<CompletionTurn>,
}

impl CompletionRegistration {
    /// Bind immediately after `send_registered` returns, without intervening I/O.
    /// An old pre-bind response is released while this registration keeps waiting
    /// for the exact SDK request ID supplied here.
    pub(crate) fn bind(&mut self, request_id: RequestId) -> Result<BindDisposition, HandoffError> {
        if self.bound {
            return Err(HandoffError::AlreadyBound);
        }
        let inner = self.inner.upgrade().ok_or(HandoffError::Closed)?;
        let (early, delivery, discarded, discarded_id) = {
            let mut state = inner
                .state
                .lock()
                .map_err(|_| HandoffError::RegistryPoisoned)?;
            if state.closed {
                return Err(HandoffError::Closed);
            }
            let entry = state
                .entries
                .get_mut(&self.owner)
                .filter(|entry| Arc::ptr_eq(&entry.allocation, &self.allocation))
                .ok_or(HandoffError::Withdrawn)?;
            entry.request_id = Some(request_id.clone());
            self.bound = true;
            let early = entry.early.take();
            let discarded_id = entry.discarded_early.take();
            let discarded = discarded_id.as_ref() == Some(&request_id);
            let delivery = if discarded
                || early
                    .as_ref()
                    .is_some_and(|turn| turn.completion().request_id == request_id)
            {
                state.entries.remove(&self.owner)
            } else {
                None
            };
            (early, delivery, discarded, discarded_id)
        };
        if discarded {
            drop(early);
            drop(delivery);
            return Ok(BindDisposition::Discarded);
        }
        Ok(match (early, delivery) {
            (Some(turn), Some(entry)) => match entry.sender.send(turn) {
                Ok(()) => BindDisposition::Delivered,
                Err(_turn) => BindDisposition::ReceiverGone,
            },
            (early, None) => BindDisposition::AwaitingCompletion {
                discarded_early_request_id: early
                    .as_ref()
                    .map(|turn| turn.completion().request_id.clone())
                    .or(discarded_id),
                // The old turn drops outside the lock before this method returns.
            },
            (None, Some(_)) => unreachable!("delivery requires an early completion"),
        })
    }

    /// Wait without holding a delivery guard. Dropping this future withdraws only
    /// this allocation and also drops a turn already buffered in its receiver.
    pub(crate) async fn wait(mut self) -> Result<CompletionTurn, HandoffError> {
        if !self.bound {
            return Err(HandoffError::Unbound);
        }
        (&mut self.receiver)
            .await
            .map_err(|_| HandoffError::CompletionUnavailable)
    }

    pub(crate) fn withdraw(self) {
        drop(self);
    }
}

impl Drop for CompletionRegistration {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let entry = {
            let mut state = inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state
                .entries
                .get(&self.owner)
                .is_some_and(|entry| Arc::ptr_eq(&entry.allocation, &self.allocation))
            {
                state.entries.remove(&self.owner)
            } else {
                None
            }
        };
        // Also drops the receiver after this method, outside the registry mutex.
        drop(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Context;
    use std::time::Duration;

    use agent_client_protocol::{Client, Dispatch, Error, Lines, UntypedMessage};
    use futures::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio::sync::mpsc;

    use crate::ordered_ingress::{CompletionSource, OrderedIngress};
    use crate::ordered_ingress::{IngressLimits, RequestBudget, RequestClass};
    use crate::session_dispatch::{
        Budget, ClassLimits, DispatchLimits, EventOrigin, SessionDispatch, TrafficClass,
    };

    const TIMEOUT: Duration = Duration::from_secs(3);

    fn owner() -> RequestOwner {
        RequestOwner {
            epoch: "epoch".to_owned(),
            session_id: Some("a".to_owned()),
            incarnation: Some(1),
            operation_id: "operation".to_owned(),
            attempt_id: Some("attempt".to_owned()),
        }
    }

    struct Fixture {
        completion: OrderedCompletion,
        ingress: OrderedIngress<OrderedCompletion>,
        _receiver: mpsc::Receiver<OrderedCompletion>,
    }

    // Use the real ingress budget and in-memory ACP response route. No private
    // completion constructor, socket, sleeps, or test-only production hooks.
    async fn response(owner: RequestOwner) -> Fixture {
        let budget = RequestBudget {
            requests: 1,
            completion_bytes: 1024,
        };
        let (events, mut receiver) = mpsc::channel(2);
        let ingress = OrderedIngress::new(
            IngressLimits {
                max_response_bytes: 1024,
                long_running: budget,
                control: budget,
            },
            events,
            |event| event,
        )
        .unwrap();
        let (outgoing, mut peer_requests) = futures::channel::mpsc::channel::<String>(4);
        let (mut peer_responses, incoming) =
            futures::channel::mpsc::channel::<io::Result<String>>(4);
        let transport = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
        let (done, finished) = oneshot::channel();
        let (fixture, produced) = oneshot::channel();
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
                    .try_reserve(RequestClass::LongRunning, owner, 1024)
                    .unwrap();
                let request = ingress
                    .send_registered(
                        &connection,
                        UntypedMessage::new("test/result", json!({})).unwrap(),
                        reservation,
                    )
                    .unwrap();
                assert_eq!(request.sent.block_task().await?, json!({"answer": 42}));
                let completion = receiver.recv().await.unwrap();
                assert_eq!(completion.source, CompletionSource::Response);
                assert_eq!(completion.request_id, request.request_id);
                assert!(
                    fixture
                        .send(Fixture {
                            completion,
                            ingress,
                            _receiver: receiver,
                        })
                        .is_ok()
                );
                done.send(()).unwrap();
                Ok(())
            });
        let peer = async move {
            let request: Value =
                serde_json::from_str(&peer_requests.next().await.unwrap()).unwrap();
            peer_responses
                .send(Ok(json!({
                    "jsonrpc": "2.0", "id": request["id"], "result": {"answer": 42}
                })
                .to_string()))
                .await
                .unwrap();
            finished.await.unwrap();
        };
        let (result, ()) = tokio::time::timeout(TIMEOUT, futures::future::join(connection, peer))
            .await
            .expect("in-memory response fixture hung");
        result.unwrap();
        produced.await.unwrap()
    }

    // A2 is already queued before the completion guard is transferred. The test
    // pump receives no fresh ingress; only the turn's drop may wake it to take A2.
    fn queued_delivery() -> (SessionDispatch<&'static str>, DeliveryGuard) {
        let classes = ClassLimits {
            ordinary: Budget {
                items: 4,
                bytes: 64,
            },
            reserved: Budget {
                items: 4,
                bytes: 64,
            },
        };
        let dispatch = SessionDispatch::new(
            "epoch",
            DispatchLimits {
                max_sessions: 2,
                max_event_bytes: 16,
                per_session: classes,
                global: classes,
            },
        )
        .unwrap();
        let session = dispatch.register("a", 1).unwrap();
        for event in ["completion", "A2"] {
            dispatch
                .try_route(
                    &session,
                    TrafficClass::Reserved,
                    EventOrigin::RequiredInbound,
                    1,
                    event,
                )
                .unwrap();
        }
        let (event, guard) = dispatch.try_next().unwrap().unwrap().into_parts();
        assert_eq!(event, "completion");
        assert!(dispatch.try_next().unwrap().is_none());
        (dispatch, guard)
    }

    async fn assert_woken(wake: &Notify, dispatch: &SessionDispatch<&'static str>) {
        // The permit must already exist when the pump starts waiting; notify_waiters
        // would lose this wake. Poll directly instead of relying on elapsed time.
        assert!(futures::poll!(Box::pin(wake.notified())).is_ready());
        let delivery = dispatch.try_next().unwrap().expect("A2 remained busy");
        assert_eq!(*delivery.event(), "A2");
        drop(delivery);
        assert_eq!(dispatch.global_usage(TrafficClass::Reserved).items, 0);
    }

    #[tokio::test]
    async fn discard_is_exact_and_terminates_only_the_matching_bound_waiter() {
        let handoff = CompletionHandoff::new(1, Arc::new(Notify::new())).unwrap();
        let mut registration = handoff.register(owner()).unwrap();
        let current = RequestId::from("current".to_owned());
        let old = RequestId::from("old".to_owned());
        registration.bind(current.clone()).unwrap();
        assert_eq!(
            handoff.discard(&owner(), &old),
            Ok(DiscardDisposition::StaleRequestId)
        );
        let mut waiting = Box::pin(registration.wait());
        assert!(futures::poll!(&mut waiting).is_pending());
        assert_eq!(
            handoff.discard(&owner(), &current),
            Ok(DiscardDisposition::Discarded)
        );
        assert_eq!(
            waiting.await.unwrap_err(),
            HandoffError::CompletionUnavailable
        );
        assert!(handoff.register(owner()).is_ok());
    }

    #[tokio::test]
    async fn discard_before_bind_checks_the_later_request_id() {
        let handoff = CompletionHandoff::new(1, Arc::new(Notify::new())).unwrap();
        let old = RequestId::from("old".to_owned());
        let current = RequestId::from("current".to_owned());
        let mut matching = handoff.register(owner()).unwrap();
        assert_eq!(
            handoff.discard(&owner(), &old),
            Ok(DiscardDisposition::ParkedUntilBound)
        );
        assert_eq!(matching.bind(old.clone()), Ok(BindDisposition::Discarded));
        assert_eq!(
            matching.wait().await.unwrap_err(),
            HandoffError::CompletionUnavailable
        );

        let mut unrelated = handoff.register(owner()).unwrap();
        assert_eq!(
            handoff.discard(&owner(), &old),
            Ok(DiscardDisposition::ParkedUntilBound)
        );
        assert_eq!(
            unrelated.bind(current.clone()),
            Ok(BindDisposition::AwaitingCompletion {
                discarded_early_request_id: Some(old),
            })
        );
        let mut waiting = Box::pin(unrelated.wait());
        assert!(futures::poll!(&mut waiting).is_pending());
        assert_eq!(
            handoff.discard(&owner(), &current),
            Ok(DiscardDisposition::Discarded)
        );
        assert_eq!(
            waiting.await.unwrap_err(),
            HandoffError::CompletionUnavailable
        );
    }

    #[tokio::test]
    async fn retiring_one_incarnation_preserves_other_sessions_and_epochs() {
        let handoff = CompletionHandoff::new(4, Arc::new(Notify::new())).unwrap();
        let mut registrations = Vec::new();
        for index in 0..4 {
            let mut identity = owner();
            match index {
                1 => identity.incarnation = Some(2),
                2 => identity.session_id = Some("b".into()),
                3 => identity.epoch = "other-epoch".into(),
                _ => {}
            }
            let mut registration = handoff.register(identity).unwrap();
            registration
                .bind(RequestId::from(format!("request-{index}")))
                .unwrap();
            registrations.push(registration);
        }
        assert_eq!(handoff.discard_session("epoch", "a", 1), Ok(1));
        assert_eq!(
            registrations.remove(0).wait().await.unwrap_err(),
            HandoffError::CompletionUnavailable
        );
        for registration in registrations {
            assert!(futures::poll!(Box::pin(registration.wait())).is_pending());
        }
        assert!(handoff.register(owner()).is_ok());
    }

    #[tokio::test]
    async fn discard_releases_a_matching_early_delivery_before_the_next_turn() {
        let wake = Arc::new(Notify::new());
        let handoff = CompletionHandoff::new(1, wake.clone()).unwrap();
        let mut registration = handoff.register(owner()).unwrap();
        let fixture = response(owner()).await;
        let request_id = fixture.completion.request_id.clone();
        let (dispatch, guard) = queued_delivery();
        handoff.handoff(fixture.completion, guard).unwrap();
        assert_eq!(
            handoff.discard(&owner(), &request_id),
            Ok(DiscardDisposition::ParkedUntilBound)
        );
        assert_woken(&wake, &dispatch).await;
        assert!(
            fixture
                .ingress
                .try_reserve(RequestClass::LongRunning, owner(), 1024)
                .is_ok()
        );
        assert_eq!(
            registration.bind(request_id),
            Ok(BindDisposition::Discarded)
        );
        assert_eq!(
            registration.wait().await.unwrap_err(),
            HandoffError::CompletionUnavailable
        );
    }

    #[tokio::test]
    async fn early_response_waits_for_binding_and_local_reduction_before_a2() {
        let wake = Arc::new(Notify::new());
        let handoff = CompletionHandoff::new(1, wake.clone()).unwrap();
        let mut registration = handoff.register(owner()).unwrap();
        // The response can arrive as soon as send_registered runs: context is
        // already present, but its returned request ID has not yet been bound.
        let Fixture {
            completion,
            ingress,
            _receiver,
        } = response(owner()).await;
        let request_id = completion.request_id.clone();
        let (dispatch, guard) = queued_delivery();
        assert_eq!(
            handoff.handoff(completion, guard),
            Ok(HandoffDisposition::ParkedUntilBound)
        );
        assert!(dispatch.try_next().unwrap().is_none());
        assert!(futures::poll!(Box::pin(wake.notified())).is_pending());
        assert_eq!(
            registration.bind(request_id),
            Ok(BindDisposition::Delivered)
        );
        let turn = registration.wait().await.unwrap();
        assert!(
            ingress
                .try_reserve(RequestClass::LongRunning, owner(), 1024)
                .is_err()
        );
        let answer = turn.reduce(|completion| {
            assert!(dispatch.try_next().unwrap().is_none());
            completion.result.as_ref().unwrap()["answer"]
                .as_u64()
                .unwrap()
        });
        assert_eq!(answer, 42);
        assert_woken(&wake, &dispatch).await;
        assert!(
            ingress
                .try_reserve(RequestClass::LongRunning, owner(), 1024)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn withdrawn_task_late_response_cannot_satisfy_reused_owner() {
        let wake = Arc::new(Notify::new());
        let handoff = CompletionHandoff::new(1, wake.clone()).unwrap();
        handoff.register(owner()).unwrap().withdraw();
        let mut replacement = handoff.register(owner()).unwrap();
        let mut old = response(owner()).await;
        // Different SDK IDs on the same connection are represented explicitly;
        // each independent in-memory fixture otherwise starts its SDK IDs at 0.
        old.completion.request_id = RequestId::from("old".to_owned());
        let fresh = response(owner()).await;
        let fresh_id = fresh.completion.request_id.clone();
        let (old_dispatch, old_guard) = queued_delivery();
        assert_eq!(
            handoff.handoff(old.completion, old_guard),
            Ok(HandoffDisposition::ParkedUntilBound)
        );
        assert_eq!(
            replacement.bind(fresh_id),
            Ok(BindDisposition::AwaitingCompletion {
                discarded_early_request_id: Some(RequestId::from("old".to_owned())),
            })
        );
        assert_woken(&wake, &old_dispatch).await;
        let mut waiting = Box::pin(replacement.wait());
        assert!(futures::poll!(&mut waiting).is_pending());
        let (dispatch, guard) = queued_delivery();
        assert_eq!(
            handoff.handoff(fresh.completion, guard),
            Ok(HandoffDisposition::Delivered)
        );
        waiting.await.unwrap().reduce(|completion| {
            assert_eq!(completion.result.as_ref().unwrap()["answer"], 42);
        });
        assert_woken(&wake, &dispatch).await;
    }

    #[tokio::test]
    async fn delivered_old_registration_drop_does_not_withdraw_new_allocation() {
        let wake = Arc::new(Notify::new());
        let handoff = CompletionHandoff::new(1, wake.clone()).unwrap();
        let mut old = handoff.register(owner()).unwrap();
        let fixture = response(owner()).await;
        old.bind(fixture.completion.request_id.clone()).unwrap();
        let (dispatch, guard) = queued_delivery();
        assert_eq!(
            handoff.handoff(fixture.completion, guard),
            Ok(HandoffDisposition::Delivered)
        );
        let mut new = handoff.register(owner()).unwrap();
        drop(old); // Drops its buffered turn as well as its old allocation token.
        assert_woken(&wake, &dispatch).await;
        assert_eq!(
            new.bind(RequestId::from("new".to_owned())),
            Ok(BindDisposition::AwaitingCompletion {
                discarded_early_request_id: None,
            })
        );
    }

    #[tokio::test]
    async fn every_owner_component_and_bound_request_id_are_fenced() {
        let wake = Arc::new(Notify::new());
        let handoff = CompletionHandoff::new(1, wake.clone()).unwrap();
        let mut registration = handoff.register(owner()).unwrap();
        registration
            .bind(RequestId::from("expected".to_owned()))
            .unwrap();
        for mismatch in 0..6 {
            let mut foreign = owner();
            match mismatch {
                0 => foreign.epoch = "old-epoch".to_owned(),
                1 => foreign.session_id = Some("other-session".to_owned()),
                2 => foreign.incarnation = Some(2),
                3 => foreign.operation_id = "other-operation".to_owned(),
                4 => foreign.attempt_id = Some("old-attempt".to_owned()),
                _ => {}
            }
            let fixture = response(foreign).await;
            let (dispatch, guard) = queued_delivery();
            assert_eq!(
                handoff.handoff(fixture.completion, guard),
                Ok(if mismatch == 5 {
                    HandoffDisposition::StaleRequestId
                } else {
                    HandoffDisposition::Unregistered
                })
            );
            assert_woken(&wake, &dispatch).await;
        }
        // None/None is a distinct, supported owner for creation/global RPC paths.
        drop(registration);
        let mut global = owner();
        global.session_id = None;
        global.incarnation = None;
        let _global_registration = handoff.register(global).unwrap();
    }

    #[tokio::test]
    async fn failed_synchronous_send_releases_turn_on_both_handoff_paths() {
        for early in [false, true] {
            let wake = Arc::new(Notify::new());
            let handoff = CompletionHandoff::new(1, wake.clone()).unwrap();
            let mut registration = handoff.register(owner()).unwrap();
            let fixture = response(owner()).await;
            let request_id = fixture.completion.request_id.clone();
            let (dispatch, guard) = queued_delivery();
            if early {
                handoff.handoff(fixture.completion, guard).unwrap();
                registration.receiver.close();
                assert_eq!(
                    registration.bind(request_id),
                    Ok(BindDisposition::ReceiverGone)
                );
            } else {
                registration.bind(request_id).unwrap();
                // Force the exact send/cancellation race without scheduling sleeps.
                registration.receiver.close();
                assert_eq!(
                    handoff.handoff(fixture.completion, guard),
                    Ok(HandoffDisposition::ReceiverGone)
                );
            }
            assert_woken(&wake, &dispatch).await;
            assert!(matches!(
                registration.wait().await,
                Err(HandoffError::CompletionUnavailable)
            ));
            assert!(handoff.register(owner()).is_ok());
        }
    }

    #[tokio::test]
    async fn cancelled_wait_future_releases_buffered_turn_and_registration() {
        for delivered in [false, true] {
            let wake = Arc::new(Notify::new());
            let handoff = CompletionHandoff::new(1, wake.clone()).unwrap();
            let mut registration = handoff.register(owner()).unwrap();
            let fixture = response(owner()).await;
            registration
                .bind(fixture.completion.request_id.clone())
                .unwrap();
            let mut waiting = Box::pin(registration.wait());
            assert!(futures::poll!(&mut waiting).is_pending());
            let (dispatch, guard) = queued_delivery();
            if delivered {
                assert_eq!(
                    handoff.handoff(fixture.completion, guard),
                    Ok(HandoffDisposition::Delivered)
                );
                drop(waiting);
            } else {
                drop(waiting);
                assert_eq!(
                    handoff.handoff(fixture.completion, guard),
                    Ok(HandoffDisposition::Unregistered)
                );
            }
            assert_woken(&wake, &dispatch).await;
            assert!(handoff.register(owner()).is_ok());
        }
    }

    #[tokio::test]
    async fn local_error_and_panic_release_both_budgets_before_waking_pump() {
        for panics in [false, true] {
            let wake = Arc::new(Notify::new());
            let handoff = CompletionHandoff::new(1, wake.clone()).unwrap();
            let mut registration = handoff.register(owner()).unwrap();
            let fixture = response(owner()).await;
            registration
                .bind(fixture.completion.request_id.clone())
                .unwrap();
            let (dispatch, guard) = queued_delivery();
            handoff.handoff(fixture.completion, guard).unwrap();
            let turn = registration.wait().await.unwrap();
            if panics {
                assert!(
                    catch_unwind(AssertUnwindSafe(|| turn.reduce(|_| {
                        assert!(dispatch.try_next().unwrap().is_none());
                        panic!("local result reducer failed");
                    })))
                    .is_err()
                );
            } else {
                let result: Result<(), &str> = turn.reduce(|_| Err("local result rejected"));
                assert_eq!(result, Err("local result rejected"));
            }
            assert_woken(&wake, &dispatch).await;
            assert!(
                fixture
                    .ingress
                    .try_reserve(RequestClass::LongRunning, owner(), 1024)
                    .is_ok()
            );
        }
    }

    #[tokio::test]
    async fn close_and_withdraw_release_parked_turns_and_close_rejects_delivery() {
        for close in [false, true] {
            let wake = Arc::new(Notify::new());
            let handoff = CompletionHandoff::new(1, wake.clone()).unwrap();
            let registration = handoff.register(owner()).unwrap();
            let fixture = response(owner()).await;
            let (dispatch, guard) = queued_delivery();
            handoff.handoff(fixture.completion, guard).unwrap();
            if close {
                handoff.close();
                assert!(matches!(
                    handoff.register(owner()),
                    Err(HandoffError::Closed)
                ));
            } else {
                registration.withdraw();
                assert!(handoff.register(owner()).is_ok());
            }
            assert_woken(&wake, &dispatch).await;
        }
        let wake = Arc::new(Notify::new());
        let handoff = CompletionHandoff::new(1, wake.clone()).unwrap();
        let mut registration = handoff.register(owner()).unwrap();
        registration
            .bind(RequestId::from("pending".to_owned()))
            .unwrap();
        handoff.close();
        assert!(matches!(
            registration.wait().await,
            Err(HandoffError::CompletionUnavailable)
        ));
        let fixture = response(owner()).await;
        let (dispatch, guard) = queued_delivery();
        assert_eq!(
            handoff.handoff(fixture.completion, guard),
            Err(HandoffError::Closed)
        );
        assert_woken(&wake, &dispatch).await;
    }

    #[tokio::test]
    async fn additional_early_completion_is_explicitly_rejected_without_overwrite() {
        let wake = Arc::new(Notify::new());
        let handoff = CompletionHandoff::new(1, wake.clone()).unwrap();
        let mut registration = handoff.register(owner()).unwrap();
        let first = response(owner()).await;
        let request_id = first.completion.request_id.clone();
        let (first_dispatch, first_guard) = queued_delivery();
        handoff.handoff(first.completion, first_guard).unwrap();
        let extra = response(owner()).await;
        let (extra_dispatch, extra_guard) = queued_delivery();
        assert_eq!(
            handoff.handoff(extra.completion, extra_guard),
            Err(HandoffError::EarlyCompletionAlreadyParked)
        );
        assert_woken(&wake, &extra_dispatch).await;
        assert!(first_dispatch.try_next().unwrap().is_none());
        assert_eq!(
            registration.bind(request_id),
            Ok(BindDisposition::Delivered)
        );
        drop(registration.wait().await.unwrap());
        assert_woken(&wake, &first_dispatch).await;
    }

    #[tokio::test]
    async fn registration_limits_and_unbound_wait_do_not_leave_contexts() {
        assert!(matches!(
            CompletionHandoff::new(0, Arc::new(Notify::new())),
            Err(HandoffError::InvalidLimit)
        ));
        let handoff = CompletionHandoff::new(1, Arc::new(Notify::new())).unwrap();
        let registration = handoff.register(owner()).unwrap();
        assert!(matches!(
            handoff.register(owner()),
            Err(HandoffError::DuplicateOwner)
        ));
        let mut other = owner();
        other.operation_id = "other".to_owned();
        assert!(matches!(
            handoff.register(other.clone()),
            Err(HandoffError::RegistrationLimit)
        ));
        assert!(matches!(
            registration.wait().await,
            Err(HandoffError::Unbound)
        ));
        assert!(handoff.register(other).is_ok());
    }

    #[tokio::test]
    async fn poisoned_registry_still_releases_rejected_and_parked_guards() {
        let wake = Arc::new(Notify::new());
        let handoff = CompletionHandoff::new(1, wake.clone()).unwrap();
        let registration = handoff.register(owner()).unwrap();
        let parked = response(owner()).await;
        let (parked_dispatch, parked_guard) = queued_delivery();
        handoff.handoff(parked.completion, parked_guard).unwrap();
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _lock = handoff.inner.state.lock().unwrap();
                panic!("poisoned context registry");
            }))
            .is_err()
        );
        let incoming = response(owner()).await;
        let (dispatch, guard) = queued_delivery();
        assert_eq!(
            handoff.handoff(incoming.completion, guard),
            Err(HandoffError::RegistryPoisoned)
        );
        assert_woken(&wake, &dispatch).await;
        drop(registration); // RAII cleanup deliberately recovers a poisoned mutex.
        assert_woken(&wake, &parked_dispatch).await;
        handoff.close();
    }

    #[tokio::test]
    async fn closing_parked_turn_wakes_pump_only_after_unlock_and_budgets_release() {
        struct PumpWake {
            handoff: CompletionHandoff,
            dispatch: SessionDispatch<&'static str>,
            ingress: OrderedIngress<OrderedCompletion>,
            unlocked: AtomicBool,
            ingress_released: AtomicBool,
            a2_ready: AtomicBool,
        }

        impl futures::task::ArcWake for PumpWake {
            fn wake_by_ref(pump: &Arc<Self>) {
                // Runs synchronously inside notify_one. Checking only after drop
                // returned would miss notify-before-release races with the pump.
                pump.unlocked.store(
                    pump.handoff.inner.state.try_lock().is_ok(),
                    Ordering::SeqCst,
                );
                pump.ingress_released.store(
                    pump.ingress
                        .try_reserve(RequestClass::LongRunning, owner(), 1024)
                        .is_ok(),
                    Ordering::SeqCst,
                );
                let ready = pump
                    .dispatch
                    .try_next()
                    .unwrap()
                    .is_some_and(|delivery| *delivery.event() == "A2");
                pump.a2_ready.store(ready, Ordering::SeqCst);
            }
        }

        let wake = Arc::new(Notify::new());
        let handoff = CompletionHandoff::new(1, wake.clone()).unwrap();
        let _registration = handoff.register(owner()).unwrap();
        let fixture = response(owner()).await;
        let (dispatch, guard) = queued_delivery();
        handoff.handoff(fixture.completion, guard).unwrap();
        let pump = Arc::new(PumpWake {
            handoff: handoff.clone(),
            dispatch,
            ingress: fixture.ingress,
            unlocked: AtomicBool::new(false),
            ingress_released: AtomicBool::new(false),
            a2_ready: AtomicBool::new(false),
        });
        let waker = futures::task::waker_ref(&pump);
        let mut context = Context::from_waker(&waker);
        let mut notified = Box::pin(wake.notified());
        assert!(notified.as_mut().poll(&mut context).is_pending());
        handoff.close();
        assert!(pump.unlocked.load(Ordering::SeqCst));
        assert!(pump.ingress_released.load(Ordering::SeqCst));
        assert!(pump.a2_ready.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn last_dispatcher_handle_drop_closes_waiters_and_releases_early_turns() {
        let wake = Arc::new(Notify::new());
        let handoff = CompletionHandoff::new(2, wake.clone()).unwrap();
        let clone = handoff.clone();
        let mut bound_owner = owner();
        bound_owner.operation_id = "waiting".to_owned();
        let mut bound = handoff.register(bound_owner).unwrap();
        bound.bind(RequestId::from("pending".to_owned())).unwrap();
        let mut waiting = Box::pin(bound.wait());
        assert!(futures::poll!(&mut waiting).is_pending());
        let mut early = handoff.register(owner()).unwrap();
        let fixture = response(owner()).await;
        let request_id = fixture.completion.request_id.clone();
        let (dispatch, guard) = queued_delivery();
        handoff.handoff(fixture.completion, guard).unwrap();
        drop(handoff);
        assert!(futures::poll!(&mut waiting).is_pending());
        assert!(dispatch.try_next().unwrap().is_none());
        drop(clone);
        assert!(matches!(
            waiting.await,
            Err(HandoffError::CompletionUnavailable)
        ));
        assert_eq!(early.bind(request_id), Err(HandoffError::Closed));
        assert_woken(&wake, &dispatch).await;
    }
}
