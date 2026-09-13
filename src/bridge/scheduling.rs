//! The bridge's shared ingress and short execution tickets. `run_connection`
//! owns routing and drives both receivers and `pump`; this module starts no task.
//! In particular, a response is not handed to its RPC task until the coordinator
//! has inspected it and claimed any newly returned session ID.

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_client_protocol::schema::v1::RequestId;
use agent_client_protocol::{
    Agent, ConnectionTo, Dispatch, Error, Handled, JsonRpcRequest, SentRequest,
};
use serde::Serialize;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::inbound_requests::{CapturedInboundRequest, InboundRequestLease};
use super::{
    BridgeInput, EventSink, MAX_AGENT_RELAY_BYTES, MAX_BRIDGE_MESSAGE_BYTES, MAX_TRACKED_SESSIONS,
};
use crate::completion_handoff::{
    CompletionHandoff, CompletionRegistration, CompletionTurn, HandoffDisposition,
};
use crate::ordered_ingress::{
    IngressLimits, OrderedCompletion, OrderedIngress, RequestBudget, RequestClass, RequestOwner,
    RequestReservation,
};
use crate::session_dispatch::{
    Budget, ClassLimits, Delivery, DeliveryGuard, DispatchLimits, EventOrigin, SessionDispatch,
    SessionHandle, TrafficClass,
};
use crate::session_resources::{SessionResourceOwner, UrlRegistration};
use crate::terminal::TerminalSnapshot;

const ORDINARY_ITEMS: usize = 256;
const REQUIRED_ITEMS: usize = 128;
const ORDINARY_BYTES: usize = 16 * 1024 * 1024;
const REQUIRED_BYTES: usize = 32 * 1024 * 1024;
const LONG_REQUESTS: usize = 32;
const CONTROL_REQUESTS: usize = 32;
const COMPLETION_ITEMS: usize = LONG_REQUESTS + CONTROL_REQUESTS;
const COORDINATOR_ID: &str = "attyd:connection-coordinator";

pub(super) enum BridgeIngress {
    Browser(BridgeInput),
    Acp(Dispatch),
    RpcCompleted(OrderedCompletion),
    Terminal(TerminalSnapshot),
    Continuation {
        owner: RequestOwner,
        reply: oneshot::Sender<Result<ExecutionTurn, Error>>,
    },
    CreationFinished {
        owner: RequestOwner,
        target_id: String,
        incarnation: Option<u64>,
    },
    InboundRequestCancelled(CapturedInboundRequest),
    InboundRequestFinished {
        request_id: RequestId,
        allocation: String,
    },
}

/// Captured by the coordinator at the original ingress boundary. Handlers must
/// use this route instead of resolving an interaction against a later incarnation.
#[derive(Clone)]
pub(super) struct CapturedRoute {
    pub owner: Option<SessionResourceOwner>,
    pub dispatch_owner: RequestOwner,
    pub url_registration: Option<UrlRegistration>,
}

/// The payload's ingress budget travels into the local execution turn, so draining
/// the shared channel cannot turn its byte limit into an unbounded staging buffer.
pub(super) struct IngressItem {
    pub request_lease: Option<InboundRequestLease>,
    pub event: BridgeIngress,
    pub route: Option<CapturedRoute>,
    bytes: usize,
    class: TrafficClass,
    origin: EventOrigin,
    budget: Option<IngressBudget>,
}

impl IngressItem {
    fn completion(completion: OrderedCompletion) -> Self {
        Self {
            bytes: completion.response_bytes.max(1),
            event: BridgeIngress::RpcCompleted(completion),
            route: None,
            request_lease: None,
            class: TrafficClass::Reserved,
            origin: EventOrigin::RequiredInbound,
            // OrderedIngress already reserved this response's slot and bytes.
            budget: None,
        }
    }
}

struct IngressBudget {
    _items: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
}

struct IngressPool {
    items: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
}

impl IngressPool {
    fn new(items: usize, bytes: usize) -> Self {
        Self {
            items: Arc::new(Semaphore::new(items)),
            bytes: Arc::new(Semaphore::new(bytes)),
        }
    }

    fn reserve(&self, bytes: usize) -> Result<IngressBudget, Error> {
        let items = self
            .items
            .clone()
            .try_acquire_owned()
            .map_err(|_| capacity_error("ingress item budget exhausted"))?;
        let bytes = self
            .bytes
            .clone()
            .try_acquire_many_owned(bytes as u32)
            .map_err(|_| capacity_error("ingress byte budget exhausted"))?;
        Ok(IngressBudget {
            _items: items,
            _bytes: bytes,
        })
    }
}

#[derive(Clone)]
struct FaultReporter {
    sink: EventSink,
    cancellation: CancellationToken,
    reported: Arc<AtomicBool>,
}

impl FaultReporter {
    fn fail(&self, error: &Error) {
        if !self.reported.swap(true, Ordering::AcqRel) {
            self.sink
                .acp_error(error.clone(), None, Some("bridge/ingress"));
        }
        self.cancellation.cancel();
    }
}

struct SenderInner {
    epoch: String,
    events: mpsc::Sender<IngressItem>,
    ordinary: IngressPool,
    required: IngressPool,
    ordered: OrderedIngress<IngressItem>,
    handoff: CompletionHandoff,
    wake: Arc<Notify>,
    // Main cancellation begins grace. Only closing the dispatcher retires
    // accepted result transitions and cleanup continuations.
    closed: CancellationToken,
    fault: FaultReporter,
}

#[derive(Clone)]
pub(super) struct IngressSender {
    inner: Arc<SenderInner>,
}

/// One global lane is independent of every session queue and has its own budgets.
/// Its internal handle is never used as a canonical ACP session handle.
pub(super) struct Scheduling {
    pub ingress_rx: mpsc::Receiver<IngressItem>,
    pub wake: Arc<Notify>,
    sessions: SessionDispatch<IngressItem>,
    global: SessionDispatch<IngressItem>,
    global_handle: SessionHandle,
    sender: IngressSender,
    prefer_global: bool,
}

pub(super) struct ScheduledInput {
    pub request_lease: Option<InboundRequestLease>,
    pub event: BridgeIngress,
    pub turn: ExecutionTurn,
    pub route: Option<CapturedRoute>,
}

pub(super) struct RouteRejected {
    pub item: IngressItem,
    pub error: Error,
}

#[derive(Clone)]
enum ExecutionScope {
    Session(SessionHandle),
    Global(String),
}

impl ExecutionScope {
    fn matches(&self, owner: &RequestOwner) -> bool {
        match self {
            Self::Session(handle) => {
                owner.epoch == handle.epoch()
                    && owner.session_id.as_deref() == Some(handle.session_id())
                    && owner.incarnation == Some(handle.incarnation())
            }
            Self::Global(epoch) => {
                owner.epoch == *epoch && owner.session_id.is_none() && owner.incarnation.is_none()
            }
        }
    }
}

enum ExecutionLease {
    Local {
        guard: DeliveryGuard,
        _ingress: Option<IngressBudget>,
    },
    Completion(CompletionTurn),
}

/// Hold only through a short local admission or result transition. Agent, user,
/// filesystem and terminal waits must run after dropping this ticket.
#[must_use = "keep the execution turn through its local state transition"]
pub(super) struct ExecutionTurn {
    scope: ExecutionScope,
    lease: Option<ExecutionLease>,
    wake: Arc<Notify>,
}

impl ExecutionTurn {
    pub(super) fn handle(&self) -> Option<&SessionHandle> {
        match &self.scope {
            ExecutionScope::Session(handle) => Some(handle),
            ExecutionScope::Global(_) => None,
        }
    }

    pub(super) fn matches_owner(&self, owner: &RequestOwner) -> bool {
        self.scope.matches(owner)
    }

    /// Completion content is inspectable while the execution ticket remains held.
    pub(super) fn completion(&self) -> Option<&OrderedCompletion> {
        match &self.lease {
            Some(ExecutionLease::Completion(turn)) => Some(turn.completion()),
            _ => None,
        }
    }
}

impl Drop for ExecutionTurn {
    fn drop(&mut self) {
        match self.lease.take() {
            Some(ExecutionLease::Local { guard, _ingress }) => {
                drop(_ingress);
                drop(guard);
                self.wake.notify_one();
            }
            Some(ExecutionLease::Completion(turn)) => drop(turn),
            None => {}
        }
    }
}

impl Scheduling {
    pub(super) fn new(
        epoch: String,
        sink: EventSink,
        cancellation: CancellationToken,
    ) -> Result<(Self, IngressSender), Error> {
        let (events, ingress_rx) =
            mpsc::channel(ORDINARY_ITEMS + REQUIRED_ITEMS + COMPLETION_ITEMS);
        let wake = Arc::new(Notify::new());
        let handoff =
            CompletionHandoff::new(COMPLETION_ITEMS, wake.clone()).map_err(scheduling_error)?;
        let ordered = OrderedIngress::new(
            IngressLimits {
                max_response_bytes: MAX_AGENT_RELAY_BYTES,
                long_running: RequestBudget {
                    requests: LONG_REQUESTS,
                    completion_bytes: LONG_REQUESTS * MAX_AGENT_RELAY_BYTES,
                },
                control: RequestBudget {
                    requests: CONTROL_REQUESTS,
                    completion_bytes: CONTROL_REQUESTS * MAX_AGENT_RELAY_BYTES,
                },
            },
            events.clone(),
            IngressItem::completion,
        )
        .map_err(scheduling_error)?;
        let sessions = SessionDispatch::new(
            epoch.clone(),
            DispatchLimits {
                max_sessions: MAX_TRACKED_SESSIONS,
                max_event_bytes: MAX_BRIDGE_MESSAGE_BYTES,
                per_session: ClassLimits {
                    ordinary: Budget {
                        items: 64,
                        bytes: 8 * 1024 * 1024,
                    },
                    reserved: Budget {
                        items: 64,
                        bytes: 16 * 1024 * 1024,
                    },
                },
                global: ClassLimits {
                    ordinary: Budget {
                        items: ORDINARY_ITEMS,
                        bytes: ORDINARY_BYTES,
                    },
                    reserved: Budget {
                        items: REQUIRED_ITEMS + COMPLETION_ITEMS,
                        bytes: REQUIRED_BYTES + COMPLETION_ITEMS * MAX_AGENT_RELAY_BYTES,
                    },
                },
            },
        )
        .map_err(scheduling_error)?;
        let global_limits = ClassLimits {
            ordinary: Budget {
                items: 64,
                bytes: ORDINARY_BYTES,
            },
            reserved: Budget {
                items: REQUIRED_ITEMS + COMPLETION_ITEMS,
                bytes: REQUIRED_BYTES + COMPLETION_ITEMS * MAX_AGENT_RELAY_BYTES,
            },
        };
        let global = SessionDispatch::new(
            epoch.clone(),
            DispatchLimits {
                max_sessions: 1,
                max_event_bytes: MAX_BRIDGE_MESSAGE_BYTES,
                per_session: global_limits,
                global: global_limits,
            },
        )
        .map_err(scheduling_error)?;
        let global_handle = global
            .register(COORDINATOR_ID, 1)
            .map_err(scheduling_error)?;
        let sender = IngressSender {
            inner: Arc::new(SenderInner {
                epoch,
                events,
                ordinary: IngressPool::new(ORDINARY_ITEMS, ORDINARY_BYTES),
                required: IngressPool::new(REQUIRED_ITEMS, REQUIRED_BYTES),
                ordered,
                handoff,
                wake: wake.clone(),
                closed: CancellationToken::new(),
                fault: FaultReporter {
                    sink,
                    cancellation,
                    reported: Arc::new(AtomicBool::new(false)),
                },
            }),
        };
        Ok((
            Self {
                ingress_rx,
                wake,
                sessions,
                global,
                global_handle,
                sender: sender.clone(),
                prefer_global: true,
            },
            sender,
        ))
    }

    /// Call only after the coordinator has verified this canonical incarnation.
    pub(super) fn register_session(
        &self,
        id: &str,
        incarnation: u64,
    ) -> Result<SessionHandle, Error> {
        if incarnation == 0 {
            return Err(capacity_error(
                "a cold placeholder cannot own an execution queue",
            ));
        }
        self.sessions
            .register(id, incarnation)
            .map_err(scheduling_error)
    }

    pub(super) fn remove_session(&self, handle: &SessionHandle) -> Result<(), Error> {
        // Drop queued completion allocations before waking their RPC waiters.
        self.sessions.remove(handle).map_err(scheduling_error)?;
        self.sender
            .inner
            .handoff
            .discard_session(handle.epoch(), handle.session_id(), handle.incarnation())
            .map_err(scheduling_error)?;
        Ok(())
    }

    pub(super) fn session_is_idle(&self, handle: &SessionHandle) -> Result<bool, Error> {
        for class in [TrafficClass::Ordinary, TrafficClass::Reserved] {
            if self
                .sessions
                .session_usage(handle, class)
                .map_err(scheduling_error)?
                .items
                != 0
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Includes queued and currently reducing local deliveries, but excludes
    /// response reservations for RPCs still waiting on the Agent. The coordinator
    /// separately owns any ingress items parked behind a provisional target claim.
    pub(super) fn has_local_work(&self) -> bool {
        !self.ingress_rx.is_empty()
            || [TrafficClass::Ordinary, TrafficClass::Reserved]
                .into_iter()
                .any(|class| {
                    self.sessions.global_usage(class).items != 0
                        || self.global.global_usage(class).items != 0
                })
    }

    pub(super) fn route_session(
        &self,
        handle: &SessionHandle,
        item: IngressItem,
    ) -> Result<(), RouteRejected> {
        let owner = match &item.event {
            BridgeIngress::RpcCompleted(completion) => Some(&completion.owner),
            BridgeIngress::Continuation { owner, .. } => Some(owner),
            _ => None,
        };
        if owner.is_some_and(|owner| !ExecutionScope::Session(handle.clone()).matches(owner)) {
            return Err(self.reject(
                item,
                capacity_error("event owner does not match the canonical session route"),
            ));
        }
        self.sessions
            .try_route(handle, item.class, item.origin, item.bytes, item)
            .map_err(|rejected| self.reject(rejected.event, scheduling_error(rejected.error)))
    }

    pub(super) fn route_global(&self, item: IngressItem) -> Result<(), RouteRejected> {
        self.global
            .try_route(
                &self.global_handle,
                item.class,
                item.origin,
                item.bytes,
                item,
            )
            .map_err(|rejected| self.reject(rejected.event, scheduling_error(rejected.error)))
    }

    fn reject(&self, item: IngressItem, error: Error) -> RouteRejected {
        if item.origin == EventOrigin::RequiredInbound {
            self.sender.inner.fault.fail(&error);
        }
        // Admission may already have installed a fresh attachment. Return the
        // input so the coordinator can roll it back before answering the caller.
        RouteRejected { item, error }
    }

    /// Dequeues at most one short transition. It deliberately does not auto-handoff
    /// responses: New/Fork target claiming belongs to the coordinator first.
    pub(super) fn pump(&mut self) -> Result<Option<ScheduledInput>, Error> {
        let global_first = self.prefer_global;
        self.prefer_global = !self.prefer_global;
        let delivery = if global_first {
            match self.global.try_next().map_err(scheduling_error)? {
                Some(delivery) => Some((delivery, true)),
                None => self
                    .sessions
                    .try_next()
                    .map_err(scheduling_error)?
                    .map(|delivery| (delivery, false)),
            }
        } else {
            match self.sessions.try_next().map_err(scheduling_error)? {
                Some(delivery) => Some((delivery, false)),
                None => self
                    .global
                    .try_next()
                    .map_err(scheduling_error)?
                    .map(|delivery| (delivery, true)),
            }
        };
        Ok(delivery.map(|(delivery, global)| self.execution(delivery, global)))
    }

    fn execution(&self, delivery: Delivery<IngressItem>, global: bool) -> ScheduledInput {
        let scope = if global {
            ExecutionScope::Global(self.sender.inner.epoch.clone())
        } else {
            ExecutionScope::Session(delivery.handle.clone())
        };
        let (item, guard) = delivery.into_parts();
        ScheduledInput {
            event: item.event,
            route: item.route,
            request_lease: item.request_lease,
            turn: ExecutionTurn {
                scope,
                lease: Some(ExecutionLease::Local {
                    guard,
                    _ingress: item.budget,
                }),
                wake: self.wake.clone(),
            },
        }
    }

    pub(super) fn handoff_completion(
        &self,
        completion: OrderedCompletion,
        mut turn: ExecutionTurn,
    ) -> Result<HandoffDisposition, Error> {
        if !turn.matches_owner(&completion.owner) {
            let error = capacity_error("completion ticket does not belong to its RPC owner");
            self.sender.inner.fault.fail(&error);
            return Err(error);
        }
        let Some(ExecutionLease::Local { guard, _ingress }) = turn.lease.take() else {
            let error = capacity_error("completion was handed off more than once");
            self.sender.inner.fault.fail(&error);
            return Err(error);
        };
        drop(_ingress);
        self.sender
            .inner
            .handoff
            .handoff(completion, guard)
            .map_err(|error| {
                let error = scheduling_error(error);
                self.sender.inner.fault.fail(&error);
                error
            })
    }

    pub(super) fn close(&mut self) {
        self.sender.inner.fault.cancellation.cancel();
        self.sender.inner.closed.cancel();
        self.ingress_rx.close();
        while let Ok(item) = self.ingress_rx.try_recv() {
            reject_event(item.event, &Error::request_cancelled());
        }
        self.sender.inner.handoff.close();
        if let Err(error) = self.sessions.close() {
            self.sender.inner.fault.fail(&scheduling_error(error));
        }
        if let Err(error) = self.global.close() {
            self.sender.inner.fault.fail(&scheduling_error(error));
        }
    }
}

impl Drop for Scheduling {
    fn drop(&mut self) {
        self.close();
    }
}

impl IngressSender {
    pub(super) fn try_browser(&self, input: BridgeInput, class: TrafficClass) -> Result<(), Error> {
        let origin = match &input {
            BridgeInput::RuntimeSnapshotRequest
            | BridgeInput::UnobserveSession { .. }
            | BridgeInput::RetireUnobservedSession { .. } => EventOrigin::RequiredInbound,
            _ => EventOrigin::Command,
        };
        self.submit(BridgeIngress::Browser(input), class, origin)
    }

    pub(super) fn try_terminal(&self, snapshot: TerminalSnapshot) -> Result<(), Error> {
        self.submit(
            BridgeIngress::Terminal(snapshot),
            TrafficClass::Ordinary,
            EventOrigin::RequiredInbound,
        )
    }

    /// Complete the coordinator's provisional target claim after the RPC task has
    /// committed (or abandoned) its local creation transition. This marker joins
    /// the same FIFO as the notifications waiting behind that claim.
    pub(super) fn finish_creation(
        &self,
        owner: RequestOwner,
        target_id: String,
        incarnation: Option<u64>,
    ) -> Result<(), Error> {
        self.submit(
            BridgeIngress::CreationFinished {
                owner,
                target_id,
                incarnation,
            },
            TrafficClass::Reserved,
            EventOrigin::RequiredInbound,
        )
    }

    pub(super) fn inbound_request_finished(
        &self,
        request_id: RequestId,
        allocation: String,
    ) -> Result<(), Error> {
        self.submit(
            BridgeIngress::InboundRequestFinished {
                request_id,
                allocation,
            },
            TrafficClass::Reserved,
            EventOrigin::RequiredInbound,
        )
    }

    /// A canonically retired owner must wake its RPC task with an explicit error,
    /// while leaving unrelated registrations and the connection alive.
    pub(super) fn discard_completion(&self, completion: OrderedCompletion) -> Result<(), Error> {
        let owner = completion.owner.clone();
        let request_id = completion.request_id.clone();
        // Canceling the handoff sender may wake a task immediately. Release the
        // completion allocation first so that wake observes reclaimed capacity.
        drop(completion);
        self.inner
            .handoff
            .discard(&owner, &request_id)
            .map_err(scheduling_error)?;
        Ok(())
    }

    /// Install before typed SDK callbacks. Registered responses enqueue their
    /// completion before routing the SDK waiter; all other dispatches use that
    /// exact same mpsc FIFO. Unregistered global responses remain with the SDK
    /// (initialize and the MCP manager); session-mutating RPCs must be registered.
    pub(super) fn receive_dispatch(&self, dispatch: Dispatch) -> Result<Handled<Dispatch>, Error> {
        let handled = self.inner.ordered.intercept(dispatch).map_err(|error| {
            let error = scheduling_error(error);
            self.inner.fault.fail(&error);
            error
        })?;
        match handled {
            Handled::Yes => Ok(Handled::Yes),
            Handled::No {
                message: message @ Dispatch::Response(..),
                retry,
            } => Ok(Handled::No { message, retry }),
            Handled::No { message, .. } => {
                let class = match &message {
                    Dispatch::Notification(message) if message.method == "session/update" => {
                        TrafficClass::Ordinary
                    }
                    _ => TrafficClass::Reserved,
                };
                self.submit(
                    BridgeIngress::Acp(message),
                    class,
                    EventOrigin::RequiredInbound,
                )?;
                Ok(Handled::Yes)
            }
        }
    }

    fn submit(
        &self,
        event: BridgeIngress,
        class: TrafficClass,
        origin: EventOrigin,
    ) -> Result<(), Error> {
        let admitted = (|| {
            if self.inner.closed.is_cancelled()
                || (origin == EventOrigin::Command && self.inner.fault.cancellation.is_cancelled())
            {
                return Err(Error::request_cancelled());
            }
            let bytes = measure_event(&event)?.max(1);
            let maximum = if origin == EventOrigin::Command {
                MAX_BRIDGE_MESSAGE_BYTES
            } else {
                MAX_AGENT_RELAY_BYTES
            };
            if bytes > maximum {
                return Err(capacity_error("ingress event exceeds its byte limit"));
            }
            let pool = match class {
                TrafficClass::Ordinary => &self.inner.ordinary,
                TrafficClass::Reserved => &self.inner.required,
            };
            let budget = pool.reserve(bytes)?;
            let permit = self
                .inner
                .events
                .clone()
                .try_reserve_owned()
                .map_err(|error| scheduling_error(error.to_string()))?;
            Ok((bytes, budget, permit))
        })();
        match admitted {
            Ok((bytes, budget, permit)) => {
                let sender = permit.send(IngressItem {
                    event,
                    route: None,
                    request_lease: None,
                    bytes,
                    class,
                    origin,
                    budget: Some(budget),
                });
                if sender.is_closed() {
                    let error =
                        Error::request_cancelled().data("bridge ingress closed during delivery");
                    if origin == EventOrigin::RequiredInbound {
                        self.inner.fault.fail(&error);
                    }
                    Err(error)
                } else {
                    Ok(())
                }
            }
            Err(error) => {
                reject_event(event, &error);
                if origin == EventOrigin::RequiredInbound {
                    self.inner.fault.fail(&error);
                }
                Err(error)
            }
        }
    }

    /// The caller passes a handle already checked against canonical state. No
    /// owner tuple can cause this method to register or resurrect a session.
    pub(super) fn prepare_rpc(
        &self,
        class: RequestClass,
        owner: RequestOwner,
        handle: Option<SessionHandle>,
    ) -> Result<PreparedRpc, Error> {
        let scope = handle.map_or_else(
            || ExecutionScope::Global(self.inner.epoch.clone()),
            ExecutionScope::Session,
        );
        if !scope.matches(&owner) {
            return Err(capacity_error("RPC owner and execution route disagree"));
        }
        if self.inner.fault.cancellation.is_cancelled() {
            return Err(Error::request_cancelled());
        }
        let reservation = self
            .inner
            .ordered
            .try_reserve(class, owner.clone(), MAX_AGENT_RELAY_BYTES)
            .map_err(scheduling_error)?;
        let registration = self
            .inner
            .handoff
            .register(owner)
            .map_err(scheduling_error)?;
        Ok(PreparedRpc {
            sender: self.clone(),
            reservation,
            registration,
            scope,
        })
    }

    /// Drop the previous execution turn before awaiting a continuation. This
    /// inserts new work in FIFO order and never holds a ticket while waiting.
    pub(super) async fn continue_owner(&self, owner: RequestOwner) -> Result<ExecutionTurn, Error> {
        let (reply, response) = oneshot::channel();
        self.submit(
            BridgeIngress::Continuation { owner, reply },
            TrafficClass::Reserved,
            EventOrigin::RequiredInbound,
        )?;
        tokio::select! {
            result = response => result.map_err(|_| Error::request_cancelled())?,
            _ = self.inner.closed.cancelled() => Err(Error::request_cancelled()),
        }
    }
}

pub(super) struct PreparedRpc {
    sender: IngressSender,
    reservation: RequestReservation<IngressItem>,
    registration: CompletionRegistration,
    scope: ExecutionScope,
}

impl PreparedRpc {
    /// Keep the caller's admission ticket through this call. On failure it can
    /// roll back under the original ticket; on success drop it before `wait`.
    pub(super) fn send<Req: JsonRpcRequest>(
        mut self,
        connection: &ConnectionTo<Agent>,
        request: Req,
    ) -> Result<PendingRpc<Req::Response>, Error> {
        let registered = self
            .sender
            .inner
            .ordered
            .send_registered(connection, request, self.reservation)
            .map_err(scheduling_error)?;
        if let Err(error) = self.registration.bind(registered.request_id.clone()) {
            let error = scheduling_error(error);
            self.sender.inner.fault.fail(&error);
            return Err(error);
        }
        Ok(PendingRpc {
            sender: self.sender,
            registration: self.registration,
            scope: self.scope,
            request_id: registered.request_id,
            sent: registered.sent,
        })
    }
}

pub(super) struct PendingRpc<Response> {
    sender: IngressSender,
    registration: CompletionRegistration,
    scope: ExecutionScope,
    pub request_id: agent_client_protocol::schema::v1::RequestId,
    sent: SentRequest<Response>,
}

impl<Response> PendingRpc<Response> {
    pub(super) async fn wait(self) -> Result<(Result<Response, Error>, ExecutionTurn), Error> {
        let result = tokio::select! {
            result = self.sent.block_task() => result,
            _ = self.sender.inner.closed.cancelled() => return Err(Error::request_cancelled()),
        };
        if let Err(error) = &result {
            self.sender
                .inner
                .ordered
                .complete_without_response(&self.request_id, error.clone())
                .map_err(|error| {
                    let error = scheduling_error(error);
                    self.sender.inner.fault.fail(&error);
                    error
                })?;
        }
        let turn = tokio::select! {
            turn = self.registration.wait() => turn.map_err(scheduling_error)?,
            _ = self.sender.inner.closed.cancelled() => return Err(Error::request_cancelled()),
        };
        // The coordinator can reject an invalid New/Fork target at the response
        // cut after the SDK routed its original result. Preserve that rejection;
        // successful values still use the SDK's typed deserialization result.
        let result = turn
            .completion()
            .result
            .as_ref()
            .err()
            .cloned()
            .map_or(result, Err);
        Ok((
            result,
            ExecutionTurn {
                scope: self.scope,
                lease: Some(ExecutionLease::Completion(turn)),
                wake: self.sender.inner.wake.clone(),
            },
        ))
    }
}

fn reject_event(event: BridgeIngress, error: &Error) {
    match event {
        BridgeIngress::Browser(input) => input.reject(error.clone()),
        BridgeIngress::Acp(Dispatch::Request(_, responder)) => {
            let _ = responder.respond_with_result(Err(error.clone()));
        }
        BridgeIngress::Acp(Dispatch::Response(_, router)) => {
            let _ = router.route_with_result(Err(error.clone()));
        }
        BridgeIngress::Continuation { reply, .. } => {
            let _ = reply.send(Err(error.clone()));
        }
        BridgeIngress::Acp(Dispatch::Notification(_))
        | BridgeIngress::RpcCompleted(_)
        | BridgeIngress::Terminal(_)
        | BridgeIngress::InboundRequestCancelled(_)
        | BridgeIngress::InboundRequestFinished { .. }
        | BridgeIngress::CreationFinished { .. } => {}
    }
}

fn capacity_error(message: &str) -> Error {
    Error::invalid_request().data(message)
}
fn scheduling_error(error: impl std::fmt::Display) -> Error {
    Error::internal_error().data(error.to_string())
}

fn measure_event(event: &BridgeIngress) -> Result<usize, Error> {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0 = self.0.saturating_add(data.len());
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    fn bytes(value: &impl Serialize) -> Result<usize, Error> {
        let mut counter = Counter(0);
        serde_json::to_writer(&mut counter, value).map_err(scheduling_error)?;
        Ok(counter.0.max(1))
    }
    match event {
        BridgeIngress::Browser(input) => Ok(input.payload_bytes()),
        BridgeIngress::Acp(Dispatch::Request(message, _) | Dispatch::Notification(message)) => {
            bytes(&(&message.method, &message.params))
        }
        BridgeIngress::Acp(Dispatch::Response(result, _)) => match result {
            Ok(value) => bytes(value),
            Err(error) => bytes(error),
        },
        BridgeIngress::RpcCompleted(completion) => Ok(completion.response_bytes.max(1)),
        BridgeIngress::Terminal(snapshot) => bytes(&(snapshot.incarnation, &snapshot.value)),
        BridgeIngress::InboundRequestCancelled(capture) => bytes(&(
            &capture.request_id,
            &capture.allocation,
            &capture.route.dispatch_owner.epoch,
            &capture.route.dispatch_owner.session_id,
        )),
        BridgeIngress::InboundRequestFinished {
            request_id,
            allocation,
        } => bytes(&(request_id, allocation)),
        BridgeIngress::Continuation { owner, .. } => bytes(&(
            &owner.epoch,
            &owner.session_id,
            owner.incarnation,
            &owner.operation_id,
            &owner.attempt_id,
        )),
        BridgeIngress::CreationFinished {
            owner,
            target_id,
            incarnation,
        } => bytes(&(
            &owner.epoch,
            &owner.session_id,
            owner.incarnation,
            &owner.operation_id,
            &owner.attempt_id,
            target_id,
            incarnation,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::{Client, Lines, UntypedMessage};
    use futures::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use std::time::Duration;

    fn setup() -> (
        Scheduling,
        IngressSender,
        CancellationToken,
        mpsc::UnboundedReceiver<String>,
    ) {
        let (tx, errors) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let (scheduling, sender) = Scheduling::new(
            "epoch".into(),
            EventSink { tx: tx.into() },
            cancellation.clone(),
        )
        .unwrap();
        (scheduling, sender, cancellation, errors)
    }

    fn owner(id: Option<&str>, operation: &str) -> RequestOwner {
        RequestOwner {
            epoch: "epoch".into(),
            session_id: id.map(str::to_string),
            incarnation: id.map(|_| 1),
            operation_id: operation.into(),
            attempt_id: None,
        }
    }

    fn browser_input() -> BridgeInput {
        let (response, _receiver) = oneshot::channel();
        BridgeInput::TurnRequest {
            session_id: "a".into(),
            history_revision: "base".into(),
            client_intent_id: "intent".into(),
            prompt: Vec::new(),
            response,
        }
    }

    fn browser(sender: &IngressSender) {
        sender
            .try_browser(browser_input(), TrafficClass::Ordinary)
            .unwrap();
    }

    #[test]
    fn rpc_class_reservations_leave_control_capacity_and_release_unused_contexts() {
        let (scheduling, sender, cancellation, _errors) = setup();
        let handle = scheduling.register_session("a", 1).unwrap();
        assert!(scheduling.register_session("placeholder", 0).is_err());
        let mut pending = Vec::new();
        for index in 0..LONG_REQUESTS {
            pending.push(
                sender
                    .prepare_rpc(
                        RequestClass::LongRunning,
                        owner(Some("a"), &format!("prompt-{index}")),
                        Some(handle.clone()),
                    )
                    .unwrap(),
            );
        }
        assert!(
            sender
                .prepare_rpc(
                    RequestClass::LongRunning,
                    owner(Some("a"), "excess"),
                    Some(handle.clone())
                )
                .is_err()
        );
        let control = sender
            .prepare_rpc(RequestClass::Control, owner(None, "list"), None)
            .unwrap();
        assert!(!cancellation.is_cancelled());
        drop(control);
        drop(pending.pop());
        assert!(
            sender
                .prepare_rpc(
                    RequestClass::LongRunning,
                    owner(Some("a"), "replacement"),
                    Some(handle)
                )
                .is_ok()
        );
        assert!(
            sender
                .prepare_rpc(RequestClass::Control, owner(None, "list"), None)
                .is_ok()
        );
    }

    #[test]
    fn local_drain_counts_delivery_guards_but_not_remote_response_reservations() {
        let (mut scheduling, sender, _cancellation, _errors) = setup();
        let handle = scheduling.register_session("a", 1).unwrap();
        let _waiting = sender
            .prepare_rpc(
                RequestClass::LongRunning,
                owner(Some("a"), "pending"),
                Some(handle.clone()),
            )
            .unwrap();
        assert!(!scheduling.has_local_work());
        browser(&sender);
        assert!(scheduling.has_local_work());
        let item = scheduling.ingress_rx.try_recv().unwrap();
        scheduling
            .route_session(&handle, item)
            .unwrap_or_else(|_| panic!("route failed"));
        let session_turn = scheduling.pump().unwrap().unwrap();
        assert!(scheduling.has_local_work());
        sender
            .try_browser(BridgeInput::RuntimeSnapshotRequest, TrafficClass::Reserved)
            .unwrap();
        let item = scheduling.ingress_rx.try_recv().unwrap();
        scheduling
            .route_global(item)
            .unwrap_or_else(|_| panic!("route failed"));
        let global_turn = scheduling.pump().unwrap().unwrap();
        drop(session_turn);
        assert!(scheduling.has_local_work());
        drop(global_turn);
        assert!(!scheduling.has_local_work());
    }

    #[test]
    fn ordinary_ingress_capacity_preserves_reserved_commands_and_faults_required_overflow() {
        let (_scheduling, sender, cancellation, mut errors) = setup();
        for _ in 0..ORDINARY_ITEMS {
            browser(&sender);
        }
        assert!(
            sender
                .try_browser(browser_input(), TrafficClass::Ordinary)
                .is_err()
        );
        assert!(
            !cancellation.is_cancelled(),
            "a rejected command must not stop the connection"
        );
        sender
            .try_browser(browser_input(), TrafficClass::Reserved)
            .unwrap();
        let update =
            UntypedMessage::new("session/update", json!({ "sessionId": "a", "update": {} }))
                .unwrap();
        assert!(
            sender
                .receive_dispatch(Dispatch::Notification(update))
                .is_err()
        );
        assert!(cancellation.is_cancelled());
        assert!(
            errors.try_recv().is_ok(),
            "required ingress overflow is reported visibly"
        );
        assert!(
            errors.try_recv().is_err(),
            "fault publication is emitted once"
        );
    }

    #[test]
    fn mandatory_browser_bookkeeping_overflow_is_a_connection_fault() {
        let (_scheduling, sender, cancellation, mut errors) = setup();
        for _ in 0..REQUIRED_ITEMS {
            sender
                .try_browser(browser_input(), TrafficClass::Reserved)
                .unwrap();
        }
        assert!(
            sender
                .try_browser(BridgeInput::RuntimeSnapshotRequest, TrafficClass::Reserved)
                .is_err()
        );
        assert!(cancellation.is_cancelled());
        assert!(errors.try_recv().is_ok());
    }

    #[tokio::test]
    async fn busy_session_does_not_hide_global_or_other_session_work_and_keeps_ingress_budget() {
        let (mut scheduling, sender, _cancellation, _errors) = setup();
        let a = scheduling.register_session("a", 1).unwrap();
        let b = scheduling.register_session("b", 1).unwrap();
        for handle in [&a, &a, &b] {
            browser(&sender);
            let item = scheduling.ingress_rx.try_recv().unwrap();
            scheduling
                .route_session(handle, item)
                .unwrap_or_else(|_| panic!("route failed"));
        }
        let a1 = scheduling.pump().unwrap().unwrap();
        assert_eq!(a1.turn.handle(), Some(&a));
        assert_eq!(
            sender.inner.ordinary.items.available_permits(),
            ORDINARY_ITEMS - 3
        );
        browser(&sender);
        let item = scheduling.ingress_rx.try_recv().unwrap();
        scheduling
            .route_global(item)
            .unwrap_or_else(|_| panic!("global route failed"));
        let next = scheduling.pump().unwrap().unwrap();
        let last = scheduling.pump().unwrap().unwrap();
        assert!(next.turn.handle().is_none() || last.turn.handle().is_none());
        assert!(next.turn.handle() == Some(&b) || last.turn.handle() == Some(&b));
        drop(next);
        drop(last);
        assert!(
            scheduling.pump().unwrap().is_none(),
            "a2 cannot pass a1's local ticket"
        );
        drop(a1);
        assert!(futures::poll!(Box::pin(scheduling.wake.notified())).is_ready());
        let a2 = scheduling.pump().unwrap().unwrap();
        assert_eq!(a2.turn.handle(), Some(&a));
        drop(a2);
        assert_eq!(
            sender.inner.ordinary.items.available_permits(),
            ORDINARY_ITEMS
        );
    }

    #[test]
    fn stale_routes_return_the_original_command_for_coordinator_rollback() {
        let (mut scheduling, sender, cancellation, _errors) = setup();
        let old = scheduling.register_session("a", 1).unwrap();
        scheduling.remove_session(&old).unwrap();
        let new = scheduling.register_session("a", 2).unwrap();
        browser(&sender);
        let item = scheduling.ingress_rx.try_recv().unwrap();
        let rejected = match scheduling.route_session(&old, item) {
            Err(rejected) => rejected,
            Ok(()) => panic!("stale route accepted"),
        };
        assert!(matches!(rejected.item.event, BridgeIngress::Browser(_)));
        assert!(!cancellation.is_cancelled());
        assert!(scheduling.pump().unwrap().is_none());
        drop(rejected);
        browser(&sender);
        let item = scheduling.ingress_rx.try_recv().unwrap();
        scheduling
            .route_session(&new, item)
            .unwrap_or_else(|_| panic!("new route rejected"));
        assert_eq!(
            scheduling.pump().unwrap().unwrap().turn.handle(),
            Some(&new)
        );
    }

    #[tokio::test]
    async fn continuation_rejoins_the_fifo_and_waits_without_an_execution_ticket() {
        let (mut scheduling, sender, _cancellation, _errors) = setup();
        let handle = scheduling.register_session("a", 1).unwrap();
        browser(&sender);
        let item = scheduling.ingress_rx.try_recv().unwrap();
        scheduling
            .route_session(&handle, item)
            .unwrap_or_else(|_| panic!("route failed"));
        let previous = scheduling.pump().unwrap().unwrap();
        let mut continuation =
            Box::pin(sender.continue_owner(owner(Some("a"), "resource-finished")));
        assert!(futures::poll!(&mut continuation).is_pending());
        let item = scheduling.ingress_rx.try_recv().unwrap();
        scheduling
            .route_session(&handle, item)
            .unwrap_or_else(|_| panic!("continuation route failed"));
        assert!(scheduling.pump().unwrap().is_none());
        drop(previous);
        let ScheduledInput { event, turn, .. } = scheduling.pump().unwrap().unwrap();
        let BridgeIngress::Continuation { owner, reply } = event else {
            panic!("expected continuation")
        };
        assert!(turn.matches_owner(&owner));
        assert!(reply.send(Ok(turn)).is_ok());
        let turn = continuation.await.unwrap();
        assert_eq!(turn.handle(), Some(&handle));
        assert_eq!(
            sender.inner.required.items.available_permits(),
            REQUIRED_ITEMS - 1
        );
        drop(turn);
        assert_eq!(
            sender.inner.required.items.available_permits(),
            REQUIRED_ITEMS
        );
    }

    #[tokio::test]
    async fn graceful_cancellation_keeps_cleanup_ingress_open_until_dispatcher_close() {
        let (mut scheduling, sender, cancellation, _errors) = setup();
        cancellation.cancel();
        assert!(
            sender
                .try_browser(browser_input(), TrafficClass::Reserved)
                .is_err()
        );
        assert!(
            sender
                .prepare_rpc(RequestClass::Control, owner(None, "new-rpc"), None)
                .is_err()
        );
        let mut cleanup = Box::pin(sender.continue_owner(owner(None, "cleanup")));
        assert!(futures::poll!(&mut cleanup).is_pending());
        let item = scheduling.ingress_rx.try_recv().unwrap();
        scheduling
            .route_global(item)
            .unwrap_or_else(|_| panic!("cleanup route failed"));
        let ScheduledInput { event, turn, .. } = scheduling.pump().unwrap().unwrap();
        let BridgeIngress::Continuation { reply, .. } = event else {
            panic!("expected cleanup")
        };
        assert!(reply.send(Ok(turn)).is_ok());
        drop(cleanup.await.unwrap());
        sender
            .finish_creation(owner(None, "new"), "created".into(), None)
            .unwrap();
        let mut abandoned = Box::pin(sender.continue_owner(owner(None, "last-cleanup")));
        assert!(futures::poll!(&mut abandoned).is_pending());
        scheduling.close();
        assert!(abandoned.await.is_err());
        assert!(
            sender
                .finish_creation(owner(None, "new"), "created".into(), None)
                .is_err()
        );
    }

    #[tokio::test]
    async fn real_sdk_updates_and_response_share_fifo_and_result_ticket_blocks_only_its_session() {
        let (mut scheduling, sender, _cancellation, _errors) = setup();
        let a = scheduling.register_session("a", 1).unwrap();
        let b = scheduling.register_session("b", 1).unwrap();
        let (outgoing, mut peer_requests) = futures::channel::mpsc::channel::<String>(4);
        let (mut peer_responses, incoming) =
            futures::channel::mpsc::channel::<io::Result<String>>(4);
        let transport = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
        let (done, finished) = oneshot::channel();
        let connection = Client
            .builder()
            .on_receive_dispatch(
                {
                    let sender = sender.clone();
                    async move |dispatch: Dispatch, _connection| sender.receive_dispatch(dispatch)
                },
                agent_client_protocol::on_receive_dispatch!(),
            )
            .connect_with(transport, async move |connection| {
                let pending = sender
                    .prepare_rpc(
                        RequestClass::LongRunning,
                        owner(Some("a"), "prompt"),
                        Some(a.clone()),
                    )
                    .unwrap()
                    .send(
                        &connection,
                        UntypedMessage::new("test/prompt", json!({ "sessionId": "a" })).unwrap(),
                    )
                    .unwrap();
                let mut waiting = Box::pin(pending.wait());
                for _ in 0..3 {
                    let item = tokio::select! {
                        item = scheduling.ingress_rx.recv() => item.unwrap(),
                        _ = &mut waiting => panic!("RPC escaped before ordered completion handoff"),
                    };
                    scheduling
                        .route_session(&a, item)
                        .unwrap_or_else(|_| panic!("route failed"));
                }
                let ScheduledInput { event, turn, .. } = scheduling.pump().unwrap().unwrap();
                let BridgeIngress::Acp(Dispatch::Notification(update)) = event else {
                    panic!("first update lost its position")
                };
                assert_eq!(update.params["order"], "before");
                drop(turn);
                let ScheduledInput { event, turn, .. } = scheduling.pump().unwrap().unwrap();
                let BridgeIngress::RpcCompleted(completion) = event else {
                    panic!("response lost its position")
                };
                assert_eq!(completion.result.as_ref().unwrap()["answer"], 42);
                assert_eq!(completion.method, "test/prompt");
                assert!(
                    futures::poll!(&mut waiting).is_pending(),
                    "pump must leave target claiming to the coordinator"
                );
                assert_eq!(
                    scheduling.handoff_completion(completion, turn).unwrap(),
                    HandoffDisposition::Delivered
                );
                let (result, result_turn) = waiting.await.unwrap();
                assert_eq!(result.unwrap()["answer"], 42);
                assert_eq!(result_turn.handle(), Some(&a));
                assert!(
                    scheduling.pump().unwrap().is_none(),
                    "post-response update passed result reduction"
                );
                browser(&sender);
                let item = scheduling.ingress_rx.try_recv().unwrap();
                scheduling
                    .route_session(&b, item)
                    .unwrap_or_else(|_| panic!("b route failed"));
                let other = scheduling.pump().unwrap().unwrap();
                assert_eq!(other.turn.handle(), Some(&b));
                drop(other);
                drop(result_turn);
                let ScheduledInput { event, turn, .. } = scheduling.pump().unwrap().unwrap();
                let BridgeIngress::Acp(Dispatch::Notification(update)) = event else {
                    panic!("last update lost its position")
                };
                assert_eq!(update.params["order"], "after");
                drop(turn);
                done.send(()).unwrap();
                Ok(())
            });
        let peer = async move {
            let request: Value =
                serde_json::from_str(&peer_requests.next().await.unwrap()).unwrap();
            peer_responses.send(Ok(json!([
                {"jsonrpc":"2.0","method":"test/update","params":{"sessionId":"a","order":"before"}},
                {"jsonrpc":"2.0","id":request["id"],"result":{"answer":42}},
                {"jsonrpc":"2.0","method":"test/update","params":{"sessionId":"a","order":"after"}}
            ]).to_string())).await.unwrap();
            finished.await.unwrap();
        };
        let (result, ()) = tokio::time::timeout(
            Duration::from_secs(3),
            futures::future::join(connection, peer),
        )
        .await
        .expect("ordered bridge RPC hung");
        result.unwrap();
    }

    #[tokio::test]
    async fn rpc_eof_enters_the_shared_queue_and_reacquires_its_local_result_ticket() {
        let (mut scheduling, sender, cancellation, _errors) = setup();
        let handle = scheduling.register_session("a", 1).unwrap();
        let (outgoing, mut peer_requests) = futures::channel::mpsc::channel::<String>(4);
        let (peer_responses, incoming) = futures::channel::mpsc::channel::<io::Result<String>>(4);
        let transport = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
        let connection = Client
            .builder()
            .on_receive_dispatch(
                {
                    let sender = sender.clone();
                    async move |dispatch: Dispatch, _connection| sender.receive_dispatch(dispatch)
                },
                agent_client_protocol::on_receive_dispatch!(),
            )
            .connect_with(transport, async move |connection| {
                let pending = sender
                    .prepare_rpc(
                        RequestClass::LongRunning,
                        owner(Some("a"), "lost"),
                        Some(handle.clone()),
                    )
                    .unwrap()
                    .send(
                        &connection,
                        UntypedMessage::new("test/lost", json!({})).unwrap(),
                    )
                    .unwrap();
                cancellation.cancel();
                let mut waiting = Box::pin(pending.wait());
                let item = tokio::select! {
                    item = scheduling.ingress_rx.recv() => item.unwrap(),
                    _ = &mut waiting => panic!("EOF escaped the ordered queue"),
                };
                scheduling
                    .route_session(&handle, item)
                    .unwrap_or_else(|_| panic!("EOF route failed"));
                let ScheduledInput { event, turn, .. } = scheduling.pump().unwrap().unwrap();
                let BridgeIngress::RpcCompleted(completion) = event else {
                    panic!("EOF was not a completion")
                };
                assert_eq!(
                    completion.source,
                    crate::ordered_ingress::CompletionSource::LocalFailure
                );
                assert_eq!(completion.method, "test/lost");
                assert!(agent_client_protocol::is_incoming_transport_closed(
                    completion.result.as_ref().unwrap_err()
                ));
                scheduling.handoff_completion(completion, turn).unwrap();
                let (result, turn) = waiting.await.unwrap();
                assert!(agent_client_protocol::is_incoming_transport_closed(
                    result.as_ref().unwrap_err()
                ));
                assert_eq!(turn.handle(), Some(&handle));
                drop(turn);
                assert!(scheduling.ingress_rx.try_recv().is_err());
                Ok(())
            });
        let peer = async move {
            assert!(peer_requests.next().await.is_some());
            drop(peer_responses);
        };
        let (result, ()) = tokio::time::timeout(
            Duration::from_secs(3),
            futures::future::join(connection, peer),
        )
        .await
        .expect("EOF handoff hung");
        result.unwrap();
    }

    #[tokio::test]
    async fn unregistered_global_response_keeps_the_sdk_initialization_path_live() {
        let (mut scheduling, sender, _cancellation, _errors) = setup();
        let (outgoing, mut peer_requests) = futures::channel::mpsc::channel::<String>(4);
        let (mut peer_responses, incoming) =
            futures::channel::mpsc::channel::<io::Result<String>>(4);
        let transport = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
        let (done, finished) = oneshot::channel();
        let connection = Client
            .builder()
            .on_receive_dispatch(
                async move |dispatch: Dispatch, _connection| sender.receive_dispatch(dispatch),
                agent_client_protocol::on_receive_dispatch!(),
            )
            .connect_with(transport, async move |connection| {
                let response = connection
                    .send_request(UntypedMessage::new("test/global", json!({})).unwrap())
                    .block_task()
                    .await?;
                assert_eq!(response, json!({"initialized":true}));
                assert!(
                    scheduling.ingress_rx.try_recv().is_err(),
                    "unregistered globals do not depend on the bridge pump"
                );
                done.send(()).unwrap();
                Ok(())
            });
        let peer = async move {
            let request: Value =
                serde_json::from_str(&peer_requests.next().await.unwrap()).unwrap();
            peer_responses
                .send(Ok(
                    json!({"jsonrpc":"2.0","id":request["id"],"result":{"initialized":true}})
                        .to_string(),
                ))
                .await
                .unwrap();
            finished.await.unwrap();
        };
        let (result, ()) = tokio::time::timeout(
            Duration::from_secs(3),
            futures::future::join(connection, peer),
        )
        .await
        .expect("unregistered global response hung");
        result.unwrap();
    }

    #[tokio::test]
    async fn coordinator_rejection_or_retirement_overrides_an_already_routed_sdk_success() {
        for mode in 0..3 {
            let (mut scheduling, sender, cancellation, _errors) = setup();
            let handle = scheduling.register_session("a", 1).unwrap();
            let (outgoing, mut peer_requests) = futures::channel::mpsc::channel::<String>(4);
            let (mut peer_responses, incoming) =
                futures::channel::mpsc::channel::<io::Result<String>>(4);
            let transport = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
            let (done, finished) = oneshot::channel();
            let connection = Client
                .builder()
                .on_receive_dispatch(
                    {
                        let sender = sender.clone();
                        async move |dispatch: Dispatch, _connection| {
                            sender.receive_dispatch(dispatch)
                        }
                    },
                    agent_client_protocol::on_receive_dispatch!(),
                )
                .connect_with(transport, async move |connection| {
                    let pending = sender
                        .prepare_rpc(
                            RequestClass::Control,
                            owner(Some("a"), "fork"),
                            Some(handle.clone()),
                        )
                        .unwrap()
                        .send(
                            &connection,
                            UntypedMessage::new("session/fork", json!({"sessionId":"a"})).unwrap(),
                        )
                        .unwrap();
                    let mut waiting = Box::pin(pending.wait());
                    let mut item = tokio::select! {
                        item = scheduling.ingress_rx.recv() => item.unwrap(),
                        _ = &mut waiting => panic!("RPC escaped before coordinator validation"),
                    };
                    if mode == 1 {
                        let BridgeIngress::RpcCompleted(completion) = item.event else {
                            panic!("expected response")
                        };
                        sender.discard_completion(completion).unwrap();
                        assert!(
                            waiting.await.is_err(),
                            "retired completion left a waiter alive"
                        );
                    } else if mode == 2 {
                        scheduling
                            .route_session(&handle, item)
                            .unwrap_or_else(|_| panic!("route failed"));
                        scheduling.remove_session(&handle).unwrap();
                        assert!(
                            waiting.await.is_err(),
                            "queue retirement left a waiter alive"
                        );
                    } else {
                        let BridgeIngress::RpcCompleted(completion) = &mut item.event else {
                            panic!("expected response")
                        };
                        assert_eq!(completion.method, "session/fork");
                        assert_eq!(
                            completion.result.as_ref().unwrap()["sessionId"],
                            "duplicate"
                        );
                        completion.result =
                            Err(Error::invalid_request().data("target already claimed"));
                        scheduling
                            .route_session(&handle, item)
                            .unwrap_or_else(|_| panic!("route failed"));
                        let ScheduledInput { event, turn, .. } =
                            scheduling.pump().unwrap().unwrap();
                        let BridgeIngress::RpcCompleted(completion) = event else {
                            panic!("expected response")
                        };
                        scheduling.handoff_completion(completion, turn).unwrap();
                        let (result, turn) = waiting.await.unwrap();
                        assert!(
                            result
                                .unwrap_err()
                                .to_string()
                                .contains("target already claimed")
                        );
                        drop(turn);
                    }
                    assert!(!cancellation.is_cancelled());
                    assert!(
                        sender
                            .prepare_rpc(
                                RequestClass::Control,
                                owner(Some("a"), "fork"),
                                Some(handle)
                            )
                            .is_ok()
                    );
                    done.send(()).unwrap();
                    Ok(())
                });
            let peer = async move {
                let request: Value =
                    serde_json::from_str(&peer_requests.next().await.unwrap()).unwrap();
                peer_responses.send(Ok(json!({"jsonrpc":"2.0","id":request["id"],"result":{"sessionId":"duplicate"}}).to_string())).await.unwrap();
                finished.await.unwrap();
            };
            let (result, ()) = tokio::time::timeout(
                Duration::from_secs(3),
                futures::future::join(connection, peer),
            )
            .await
            .expect("coordinator rejection hung");
            result.unwrap();
        }
    }

    #[test]
    fn creation_marker_and_captured_route_survive_the_shared_global_fifo() {
        let (mut scheduling, sender, _cancellation, _errors) = setup();
        sender
            .finish_creation(owner(None, "new"), "created".into(), Some(7))
            .unwrap();
        let mut item = scheduling.ingress_rx.try_recv().unwrap();
        assert!(item.route.is_none());
        let captured = SessionResourceOwner {
            epoch: "epoch".into(),
            session_id: "created".into(),
            incarnation: 7,
        };
        item.route = Some(CapturedRoute {
            owner: Some(captured.clone()),
            dispatch_owner: owner(None, "new"),
            url_registration: Some(UrlRegistration {
                owner: Some(captured),
                registration_id: "registration-1".into(),
            }),
        });
        scheduling
            .route_global(item)
            .unwrap_or_else(|_| panic!("creation marker route failed"));
        let ScheduledInput {
            event, turn, route, ..
        } = scheduling.pump().unwrap().unwrap();
        let BridgeIngress::CreationFinished {
            target_id,
            incarnation,
            ..
        } = event
        else {
            panic!("marker changed")
        };
        assert_eq!(target_id, "created");
        assert_eq!(incarnation, Some(7));
        let route = route.unwrap();
        assert_eq!(route.owner.unwrap().incarnation, 7);
        assert_eq!(route.dispatch_owner.operation_id, "new");
        assert_eq!(
            route.url_registration.unwrap().registration_id,
            "registration-1"
        );
        assert_eq!(
            sender.inner.required.items.available_permits(),
            REQUIRED_ITEMS - 1
        );
        drop(turn);
        assert_eq!(
            sender.inner.required.items.available_permits(),
            REQUIRED_ITEMS
        );
    }
}
