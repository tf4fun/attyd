//! Bounded queues for short session transitions at the ordered ingress boundary.
//!
//! The caller puts commands, ACP updates, and ordered RPC completions through this
//! one dispatcher. Reserved capacity never changes a session's FIFO order. Taking
//! a delivery makes that allocation busy until its guard is dropped; other sessions
//! remain eligible for round-robin draining. A guard covers local state reduction,
//! not an Agent request, a user interaction, or external resource cleanup.
//!
//! No task or waiting sender is created here. Capacity errors preserve the rejected
//! event and distinguish commands from required ingress so the connection owner can
//! apply its rejection or connection-failure policy. Shared global budgets remain
//! finite: this component does not promise unlimited isolation between sessions.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrafficClass {
    Ordinary,
    Reserved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventOrigin {
    Command,
    // Includes accepted local completions that cannot simply be dropped.
    RequiredInbound,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Budget {
    pub items: usize,
    pub bytes: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ClassLimits {
    pub ordinary: Budget,
    pub reserved: Budget,
}

impl ClassLimits {
    fn get(self, class: TrafficClass) -> Budget {
        match class {
            TrafficClass::Ordinary => self.ordinary,
            TrafficClass::Reserved => self.reserved,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DispatchLimits {
    pub max_sessions: usize,
    pub max_event_bytes: usize,
    pub per_session: ClassLimits,
    pub global: ClassLimits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BudgetScope {
    Session,
    Global,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BudgetDimension {
    Items,
    Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DispatchError {
    InvalidLimits(&'static str),
    Closed,
    RegistryPoisoned,
    EpochMismatch,
    ForeignHandle,
    SessionRemoved,
    StaleHandle,
    StaleIncarnation {
        current: u64,
        requested: u64,
    },
    SessionLimit,
    AllocationExhausted,
    EventTooLarge {
        bytes: usize,
        limit: usize,
    },
    BudgetExhausted {
        scope: BudgetScope,
        class: TrafficClass,
        dimension: BudgetDimension,
    },
}

impl fmt::Display for DispatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "session dispatch: {self:?}")
    }
}

impl std::error::Error for DispatchError {}

#[derive(Debug, Clone)]
pub(crate) struct SessionHandle {
    epoch: Arc<str>,
    session_id: Arc<str>,
    incarnation: u64,
    allocation: u64,
    registry: Arc<()>,
}

impl SessionHandle {
    pub(crate) fn epoch(&self) -> &str {
        &self.epoch
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn incarnation(&self) -> u64 {
        self.incarnation
    }
}

impl PartialEq for SessionHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.registry, &other.registry)
            && self.epoch == other.epoch
            && self.session_id == other.session_id
            && self.incarnation == other.incarnation
            && self.allocation == other.allocation
    }
}

impl Eq for SessionHandle {}

#[derive(Debug)]
pub(crate) struct Rejected<E> {
    pub handle: SessionHandle,
    pub class: TrafficClass,
    pub origin: EventOrigin,
    pub error: DispatchError,
    pub event: E,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BudgetUsage {
    // Includes queued deliveries and deliveries currently being reduced.
    pub items: usize,
    pub bytes: usize,
}

struct BudgetPool {
    limits: Budget,
    items: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
}

impl BudgetPool {
    fn new(limits: Budget) -> Self {
        Self {
            limits,
            items: Arc::new(Semaphore::new(limits.items)),
            bytes: Arc::new(Semaphore::new(limits.bytes)),
        }
    }

    fn acquire(
        &self,
        bytes: usize,
        scope: BudgetScope,
        class: TrafficClass,
    ) -> Result<BudgetLease, DispatchError> {
        let items =
            self.items
                .clone()
                .try_acquire_owned()
                .map_err(|_| DispatchError::BudgetExhausted {
                    scope,
                    class,
                    dimension: BudgetDimension::Items,
                })?;
        let bytes = self
            .bytes
            .clone()
            .try_acquire_many_owned(bytes as u32)
            .map_err(|_| DispatchError::BudgetExhausted {
                scope,
                class,
                dimension: BudgetDimension::Bytes,
            })?;
        Ok(BudgetLease {
            _items: items,
            _bytes: bytes,
        })
    }

    fn usage(&self) -> BudgetUsage {
        BudgetUsage {
            items: self.limits.items - self.items.available_permits(),
            bytes: self.limits.bytes - self.bytes.available_permits(),
        }
    }
}

struct ClassPools {
    ordinary: BudgetPool,
    reserved: BudgetPool,
}

impl ClassPools {
    fn new(limits: ClassLimits) -> Self {
        Self {
            ordinary: BudgetPool::new(limits.ordinary),
            reserved: BudgetPool::new(limits.reserved),
        }
    }

    fn get(&self, class: TrafficClass) -> &BudgetPool {
        match class {
            TrafficClass::Ordinary => &self.ordinary,
            TrafficClass::Reserved => &self.reserved,
        }
    }
}

#[derive(Debug)]
struct BudgetLease {
    _items: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
}

#[derive(Debug)]
pub(crate) struct DeliveryGuard {
    // Dropping an old allocation's guard never changes its replacement's flag.
    busy: Option<Arc<AtomicBool>>,
    session: Option<BudgetLease>,
    global: Option<BudgetLease>,
}

impl Drop for DeliveryGuard {
    fn drop(&mut self) {
        drop(self.session.take());
        drop(self.global.take());
        if let Some(busy) = &self.busy {
            busy.store(false, Ordering::Release);
        }
    }
}

#[derive(Debug)]
pub(crate) struct Delivery<E> {
    pub handle: SessionHandle,
    pub class: TrafficClass,
    pub origin: EventOrigin,
    pub bytes: usize,
    event: E,
    guard: DeliveryGuard,
}

impl<E> Delivery<E> {
    pub(crate) fn event(&self) -> &E {
        &self.event
    }

    pub(crate) fn event_mut(&mut self) -> &mut E {
        &mut self.event
    }

    /// Keep the guard until the event's local transition has finished. Transferring
    /// an external effect out of that transition must not make its I/O own this guard.
    pub(crate) fn into_parts(self) -> (E, DeliveryGuard) {
        (self.event, self.guard)
    }
}

struct SessionQueue<E> {
    handle: SessionHandle,
    budgets: ClassPools,
    queue: VecDeque<Delivery<E>>,
    scheduled: bool,
    busy: Arc<AtomicBool>,
}

struct Registry<E> {
    closed: bool,
    next_allocation: u64,
    sessions: HashMap<Arc<str>, SessionQueue<E>>,
    ready: VecDeque<SessionHandle>,
}

struct Inner<E> {
    epoch: Arc<str>,
    identity: Arc<()>,
    limits: DispatchLimits,
    global: ClassPools,
    registry: Mutex<Registry<E>>,
}

pub(crate) struct SessionDispatch<E> {
    inner: Arc<Inner<E>>,
}

impl<E> Clone for SessionDispatch<E> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<E> SessionDispatch<E> {
    pub(crate) fn new(
        epoch: impl Into<String>,
        limits: DispatchLimits,
    ) -> Result<Self, DispatchError> {
        if limits.max_sessions == 0
            || limits.max_event_bytes == 0
            || limits.max_event_bytes > u32::MAX as usize
        {
            return Err(DispatchError::InvalidLimits(
                "session and event limits must be positive; event bytes must fit u32",
            ));
        }
        for class in [TrafficClass::Ordinary, TrafficClass::Reserved] {
            for budget in [limits.per_session.get(class), limits.global.get(class)] {
                if budget.items == 0
                    || budget.items > Semaphore::MAX_PERMITS
                    || budget.bytes == 0
                    || budget.bytes > u32::MAX as usize
                    || budget.bytes > Semaphore::MAX_PERMITS
                {
                    return Err(DispatchError::InvalidLimits(
                        "item and byte budgets must fit their positive semaphore bounds",
                    ));
                }
            }
        }
        Ok(Self {
            inner: Arc::new(Inner {
                epoch: Arc::from(epoch.into()),
                identity: Arc::new(()),
                limits,
                global: ClassPools::new(limits.global),
                registry: Mutex::new(Registry {
                    closed: false,
                    next_allocation: 0,
                    sessions: HashMap::new(),
                    ready: VecDeque::new(),
                }),
            }),
        })
    }

    /// Same-incarnation registration is idempotent. A newer incarnation replaces
    /// the queue; removing and registering the same incarnation also creates a new
    /// allocation so its old handles cannot regain access.
    pub(crate) fn register(
        &self,
        session_id: impl Into<String>,
        incarnation: u64,
    ) -> Result<SessionHandle, DispatchError> {
        let session_id: Arc<str> = Arc::from(session_id.into());
        let mut registry = self
            .inner
            .registry
            .lock()
            .map_err(|_| DispatchError::RegistryPoisoned)?;
        if registry.closed {
            return Err(DispatchError::Closed);
        }
        if let Some(previous) = registry.sessions.get(&session_id) {
            if previous.handle.incarnation == incarnation {
                return Ok(previous.handle.clone());
            }
            if previous.handle.incarnation > incarnation {
                return Err(DispatchError::StaleIncarnation {
                    current: previous.handle.incarnation,
                    requested: incarnation,
                });
            }
        } else if registry.sessions.len() >= self.inner.limits.max_sessions {
            return Err(DispatchError::SessionLimit);
        }
        let allocation = registry
            .next_allocation
            .checked_add(1)
            .ok_or(DispatchError::AllocationExhausted)?;
        registry.next_allocation = allocation;
        let handle = SessionHandle {
            epoch: self.inner.epoch.clone(),
            session_id: session_id.clone(),
            incarnation,
            allocation,
            registry: self.inner.identity.clone(),
        };
        let previous = registry.sessions.insert(
            session_id.clone(),
            SessionQueue {
                handle: handle.clone(),
                budgets: ClassPools::new(self.inner.limits.per_session),
                queue: VecDeque::new(),
                scheduled: false,
                busy: Arc::new(AtomicBool::new(false)),
            },
        );
        registry
            .ready
            .retain(|ready| ready.session_id != session_id);
        drop(registry);
        // Event destructors are caller code and must run outside the registry lock.
        drop(previous);
        Ok(handle)
    }

    fn validate_handle(
        &self,
        registry: &Registry<E>,
        handle: &SessionHandle,
    ) -> Result<(), DispatchError> {
        if registry.closed {
            return Err(DispatchError::Closed);
        }
        if handle.epoch != self.inner.epoch {
            return Err(DispatchError::EpochMismatch);
        }
        if !Arc::ptr_eq(&handle.registry, &self.inner.identity) {
            return Err(DispatchError::ForeignHandle);
        }
        let queue = registry
            .sessions
            .get(handle.session_id())
            .ok_or(DispatchError::SessionRemoved)?;
        if queue.handle != *handle {
            return Err(DispatchError::StaleHandle);
        }
        Ok(())
    }

    /// `bytes` is measured by the trusted ingress serializer. Zero-byte events
    /// still consume one byte and one item; the wrapper's fixed overhead is bounded
    /// by the item limits. This function never waits for queue capacity.
    pub(crate) fn try_route(
        &self,
        handle: &SessionHandle,
        class: TrafficClass,
        origin: EventOrigin,
        bytes: usize,
        event: E,
    ) -> Result<(), Rejected<E>> {
        let result = (|| {
            let mut registry = self
                .inner
                .registry
                .lock()
                .map_err(|_| DispatchError::RegistryPoisoned)?;
            self.validate_handle(&registry, handle)?;
            let bytes = bytes.max(1);
            if bytes > self.inner.limits.max_event_bytes {
                return Err(DispatchError::EventTooLarge {
                    bytes,
                    limit: self.inner.limits.max_event_bytes,
                });
            }
            let queue = registry
                .sessions
                .get_mut(handle.session_id())
                .expect("validated queue");
            let session = queue
                .budgets
                .get(class)
                .acquire(bytes, BudgetScope::Session, class)?;
            let global = self
                .inner
                .global
                .get(class)
                .acquire(bytes, BudgetScope::Global, class)?;
            Ok((
                registry,
                bytes,
                DeliveryGuard {
                    busy: None,
                    session: Some(session),
                    global: Some(global),
                },
            ))
        })();
        match result {
            Ok((mut registry, bytes, guard)) => {
                let queue = registry
                    .sessions
                    .get_mut(handle.session_id())
                    .expect("validated queue remains locked");
                queue.queue.push_back(Delivery {
                    handle: handle.clone(),
                    class,
                    origin,
                    bytes,
                    event,
                    guard,
                });
                if !queue.scheduled {
                    queue.scheduled = true;
                    registry.ready.push_back(handle.clone());
                }
                Ok(())
            }
            Err(error) => Err(Rejected {
                handle: handle.clone(),
                class,
                origin,
                error,
                event,
            }),
        }
    }

    /// Takes one event from the next non-busy session. FIFO is shared by both
    /// traffic classes. Busy allocations remain scheduled, so the caller can drain
    /// again after completing any delivery without sending a new event.
    pub(crate) fn try_next(&self) -> Result<Option<Delivery<E>>, DispatchError> {
        let mut registry = self
            .inner
            .registry
            .lock()
            .map_err(|_| DispatchError::RegistryPoisoned)?;
        if registry.closed {
            return Err(DispatchError::Closed);
        }
        for _ in 0..registry.ready.len() {
            let handle = registry.ready.pop_front().expect("ready length checked");
            let queue = registry
                .sessions
                .get_mut(handle.session_id())
                .expect("scheduled queue is registered");
            if queue.busy.load(Ordering::Acquire) {
                registry.ready.push_back(handle);
                continue;
            }
            let mut delivery = queue
                .queue
                .pop_front()
                .expect("scheduled queue is nonempty");
            queue.busy.store(true, Ordering::Release);
            delivery.guard.busy = Some(queue.busy.clone());
            queue.scheduled = !queue.queue.is_empty();
            if queue.scheduled {
                registry.ready.push_back(handle);
            }
            return Ok(Some(delivery));
        }
        Ok(None)
    }

    pub(crate) fn remove(&self, handle: &SessionHandle) -> Result<(), DispatchError> {
        let mut registry = self
            .inner
            .registry
            .lock()
            .map_err(|_| DispatchError::RegistryPoisoned)?;
        self.validate_handle(&registry, handle)?;
        let removed = registry.sessions.remove(handle.session_id());
        registry.ready.retain(|ready| ready != handle);
        drop(registry);
        drop(removed);
        Ok(())
    }

    pub(crate) fn close(&self) -> Result<(), DispatchError> {
        let mut registry = self
            .inner
            .registry
            .lock()
            .map_err(|_| DispatchError::RegistryPoisoned)?;
        registry.closed = true;
        registry.ready.clear();
        let removed = std::mem::take(&mut registry.sessions);
        drop(registry);
        drop(removed);
        Ok(())
    }

    pub(crate) fn global_usage(&self, class: TrafficClass) -> BudgetUsage {
        self.inner.global.get(class).usage()
    }

    pub(crate) fn session_usage(
        &self,
        handle: &SessionHandle,
        class: TrafficClass,
    ) -> Result<BudgetUsage, DispatchError> {
        let registry = self
            .inner
            .registry
            .lock()
            .map_err(|_| DispatchError::RegistryPoisoned)?;
        self.validate_handle(&registry, handle)?;
        Ok(registry.sessions[handle.session_id()]
            .budgets
            .get(class)
            .usage())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicUsize;

    fn limits() -> DispatchLimits {
        DispatchLimits {
            max_sessions: 4,
            max_event_bytes: 16,
            per_session: ClassLimits {
                ordinary: Budget {
                    items: 2,
                    bytes: 16,
                },
                reserved: Budget {
                    items: 1,
                    bytes: 16,
                },
            },
            global: ClassLimits {
                ordinary: Budget {
                    items: 4,
                    bytes: 32,
                },
                reserved: Budget {
                    items: 2,
                    bytes: 32,
                },
            },
        }
    }

    fn dispatch() -> SessionDispatch<&'static str> {
        SessionDispatch::new("epoch", limits()).unwrap()
    }

    fn ordinary(
        dispatch: &SessionDispatch<&'static str>,
        handle: &SessionHandle,
        event: &'static str,
    ) {
        dispatch
            .try_route(
                handle,
                TrafficClass::Ordinary,
                EventOrigin::Command,
                1,
                event,
            )
            .unwrap();
    }

    fn required(
        dispatch: &SessionDispatch<&'static str>,
        handle: &SessionHandle,
        event: &'static str,
    ) {
        dispatch
            .try_route(
                handle,
                TrafficClass::Reserved,
                EventOrigin::RequiredInbound,
                1,
                event,
            )
            .unwrap();
    }

    #[test]
    fn invalid_limits_are_rejected_before_allocating_queues() {
        for invalid in [
            DispatchLimits {
                max_sessions: 0,
                ..limits()
            },
            DispatchLimits {
                max_event_bytes: 0,
                ..limits()
            },
            DispatchLimits {
                global: ClassLimits {
                    ordinary: Budget { items: 0, bytes: 1 },
                    ..limits().global
                },
                ..limits()
            },
        ] {
            assert!(matches!(
                SessionDispatch::<()>::new("epoch", invalid),
                Err(DispatchError::InvalidLimits(_))
            ));
        }
    }

    #[test]
    fn reserved_response_stays_between_the_updates_in_one_fifo() {
        let dispatch = dispatch();
        let session = dispatch.register("a", 1).unwrap();
        ordinary(&dispatch, &session, "update-before");
        required(&dispatch, &session, "response");
        ordinary(&dispatch, &session, "update-after");
        for expected in ["update-before", "response", "update-after"] {
            let delivery = dispatch.try_next().unwrap().unwrap();
            assert_eq!(*delivery.event(), expected);
            assert_eq!(delivery.handle, session);
            assert!(
                dispatch.try_next().unwrap().is_none(),
                "one session may have only one delivery in flight"
            );
            drop(delivery);
        }
        assert!(dispatch.try_next().unwrap().is_none());
        assert_eq!(
            dispatch.global_usage(TrafficClass::Ordinary),
            BudgetUsage { items: 0, bytes: 0 }
        );
        assert_eq!(
            dispatch.global_usage(TrafficClass::Reserved),
            BudgetUsage { items: 0, bytes: 0 }
        );
    }

    #[test]
    fn zero_byte_events_and_extracted_payloads_retain_budgets_until_guard_drop() {
        let dispatch = dispatch();
        let session = dispatch.register("a", 1).unwrap();
        dispatch
            .try_route(
                &session,
                TrafficClass::Reserved,
                EventOrigin::RequiredInbound,
                0,
                "complete",
            )
            .unwrap();
        let mut delivery = dispatch.try_next().unwrap().unwrap();
        assert_eq!(delivery.bytes, 1);
        assert_eq!(delivery.class, TrafficClass::Reserved);
        assert_eq!(delivery.origin, EventOrigin::RequiredInbound);
        *delivery.event_mut() = "reduced";
        let (event, guard) = delivery.into_parts();
        assert_eq!(event, "reduced");
        assert_eq!(
            dispatch.global_usage(TrafficClass::Reserved),
            BudgetUsage { items: 1, bytes: 1 }
        );
        assert_eq!(
            dispatch
                .session_usage(&session, TrafficClass::Reserved)
                .unwrap(),
            BudgetUsage { items: 1, bytes: 1 }
        );
        let rejected = dispatch
            .try_route(
                &session,
                TrafficClass::Reserved,
                EventOrigin::RequiredInbound,
                0,
                "next",
            )
            .unwrap_err();
        assert_eq!(rejected.origin, EventOrigin::RequiredInbound);
        drop(guard);
        required(&dispatch, &session, "next");
    }

    #[test]
    fn one_full_ordinary_queue_leaves_other_sessions_and_reserved_capacity_available() {
        let dispatch = dispatch();
        let a = dispatch.register("a", 1).unwrap();
        let b = dispatch.register("b", 2).unwrap();
        ordinary(&dispatch, &a, "a1");
        ordinary(&dispatch, &a, "a2");
        let rejected = dispatch
            .try_route(&a, TrafficClass::Ordinary, EventOrigin::Command, 1, "a3")
            .unwrap_err();
        assert_eq!(rejected.handle, a);
        assert_eq!(rejected.class, TrafficClass::Ordinary);
        assert_eq!(rejected.origin, EventOrigin::Command);
        assert_eq!(rejected.event, "a3");
        assert_eq!(
            rejected.error,
            DispatchError::BudgetExhausted {
                scope: BudgetScope::Session,
                class: TrafficClass::Ordinary,
                dimension: BudgetDimension::Items
            }
        );
        ordinary(&dispatch, &b, "b1");
        ordinary(&dispatch, &b, "b2");
        required(&dispatch, &a, "a-control");
        required(&dispatch, &b, "b-complete");
        assert_eq!(dispatch.global_usage(TrafficClass::Ordinary).items, 4);
        assert_eq!(dispatch.global_usage(TrafficClass::Reserved).items, 2);
        let a1 = dispatch.try_next().unwrap().unwrap();
        let b1 = dispatch.try_next().unwrap().unwrap();
        assert_eq!(*a1.event(), "a1");
        assert_eq!(*b1.event(), "b1");
    }

    #[test]
    fn required_global_overflow_is_explicit_and_rolls_back_local_reservations() {
        let dispatch = dispatch();
        let a = dispatch.register("a", 1).unwrap();
        let b = dispatch.register("b", 1).unwrap();
        let c = dispatch.register("c", 1).unwrap();
        ordinary(&dispatch, &a, "a1");
        ordinary(&dispatch, &a, "a2");
        ordinary(&dispatch, &b, "b1");
        ordinary(&dispatch, &b, "b2");
        let rejected = dispatch
            .try_route(
                &c,
                TrafficClass::Ordinary,
                EventOrigin::RequiredInbound,
                2,
                "canonical-update",
            )
            .unwrap_err();
        assert_eq!(rejected.origin, EventOrigin::RequiredInbound);
        assert_eq!(rejected.event, "canonical-update");
        assert_eq!(
            rejected.error,
            DispatchError::BudgetExhausted {
                scope: BudgetScope::Global,
                class: TrafficClass::Ordinary,
                dimension: BudgetDimension::Items
            }
        );
        assert_eq!(
            dispatch.session_usage(&c, TrafficClass::Ordinary).unwrap(),
            BudgetUsage { items: 0, bytes: 0 }
        );
        assert_eq!(
            dispatch.global_usage(TrafficClass::Ordinary),
            BudgetUsage { items: 4, bytes: 4 }
        );
        required(&dispatch, &c, "required-response");
    }

    #[test]
    fn byte_limits_and_single_event_limits_release_every_partial_permit() {
        let mut limits = limits();
        limits.per_session.ordinary.bytes = 3;
        limits.global.ordinary.bytes = 3;
        let dispatch = SessionDispatch::new("epoch", limits).unwrap();
        let a = dispatch.register("a", 1).unwrap();
        let b = dispatch.register("b", 1).unwrap();
        dispatch
            .try_route(&a, TrafficClass::Ordinary, EventOrigin::Command, 2, "a")
            .unwrap();
        for (handle, scope) in [(&a, BudgetScope::Session), (&b, BudgetScope::Global)] {
            let rejected = dispatch
                .try_route(
                    handle,
                    TrafficClass::Ordinary,
                    EventOrigin::Command,
                    2,
                    "overflow",
                )
                .unwrap_err();
            assert_eq!(
                rejected.error,
                DispatchError::BudgetExhausted {
                    scope,
                    class: TrafficClass::Ordinary,
                    dimension: BudgetDimension::Bytes
                }
            );
        }
        assert_eq!(
            dispatch.session_usage(&b, TrafficClass::Ordinary).unwrap(),
            BudgetUsage { items: 0, bytes: 0 }
        );
        assert_eq!(
            dispatch.global_usage(TrafficClass::Ordinary),
            BudgetUsage { items: 1, bytes: 2 }
        );
        let rejected = dispatch
            .try_route(
                &b,
                TrafficClass::Reserved,
                EventOrigin::RequiredInbound,
                17,
                "oversized",
            )
            .unwrap_err();
        assert_eq!(
            rejected.error,
            DispatchError::EventTooLarge {
                bytes: 17,
                limit: 16
            }
        );
        assert_eq!(
            dispatch.global_usage(TrafficClass::Reserved),
            BudgetUsage { items: 0, bytes: 0 }
        );
    }

    #[test]
    fn round_robin_skips_busy_sessions_without_advancing_their_fifo() {
        let dispatch = dispatch();
        let a = dispatch.register("a", 1).unwrap();
        let b = dispatch.register("b", 1).unwrap();
        ordinary(&dispatch, &a, "a1");
        ordinary(&dispatch, &a, "a2");
        ordinary(&dispatch, &b, "b1");
        ordinary(&dispatch, &b, "b2");
        let a1 = dispatch.try_next().unwrap().unwrap();
        let b1 = dispatch.try_next().unwrap().unwrap();
        assert_eq!(*a1.event(), "a1");
        assert_eq!(*b1.event(), "b1");
        assert!(dispatch.try_next().unwrap().is_none());
        drop(b1);
        let b2 = dispatch.try_next().unwrap().unwrap();
        assert_eq!(*b2.event(), "b2");
        assert!(dispatch.try_next().unwrap().is_none());
        drop(a1);
        assert_eq!(*dispatch.try_next().unwrap().unwrap().event(), "a2");
    }

    #[test]
    fn replacement_releases_queued_budget_but_old_delivery_cannot_unblock_reopened_session() {
        for remove_first in [false, true] {
            let dispatch = dispatch();
            let old = dispatch.register("a", 1).unwrap();
            ordinary(&dispatch, &old, "old-running");
            ordinary(&dispatch, &old, "old-queued");
            let old_delivery = dispatch.try_next().unwrap().unwrap();
            let new = if remove_first {
                dispatch.remove(&old).unwrap();
                assert_eq!(dispatch.remove(&old), Err(DispatchError::SessionRemoved));
                dispatch.register("a", 1).unwrap()
            } else {
                dispatch.register("a", 2).unwrap()
            };
            assert_eq!(
                dispatch.global_usage(TrafficClass::Ordinary),
                BudgetUsage { items: 1, bytes: 1 }
            );
            assert_ne!(old, new);
            ordinary(&dispatch, &new, "new-running");
            ordinary(&dispatch, &new, "new-queued");
            let new_delivery = dispatch.try_next().unwrap().unwrap();
            assert_eq!(*new_delivery.event(), "new-running");
            drop(old_delivery);
            assert!(
                dispatch.try_next().unwrap().is_none(),
                "old guard must not release replacement's busy flag"
            );
            assert_eq!(
                dispatch
                    .session_usage(&new, TrafficClass::Ordinary)
                    .unwrap()
                    .items,
                2
            );
            assert_eq!(dispatch.remove(&old), Err(DispatchError::StaleHandle));
            let rejected = dispatch
                .try_route(
                    &old,
                    TrafficClass::Reserved,
                    EventOrigin::RequiredInbound,
                    1,
                    "late",
                )
                .unwrap_err();
            assert_eq!(rejected.error, DispatchError::StaleHandle);
            drop(new_delivery);
            assert_eq!(*dispatch.try_next().unwrap().unwrap().event(), "new-queued");
            assert_eq!(dispatch.global_usage(TrafficClass::Ordinary).items, 0);
        }
    }

    #[test]
    fn replacement_discards_old_queued_events_and_refuses_older_incarnations() {
        let dispatch = dispatch();
        let old = dispatch.register("a", 7).unwrap();
        ordinary(&dispatch, &old, "old-queued");
        required(&dispatch, &old, "old-response");
        let new = dispatch.register("a", 8).unwrap();
        assert_eq!(dispatch.global_usage(TrafficClass::Ordinary).items, 0);
        assert_eq!(dispatch.global_usage(TrafficClass::Reserved).items, 0);
        assert_eq!(
            dispatch.register("a", 7),
            Err(DispatchError::StaleIncarnation {
                current: 8,
                requested: 7
            })
        );
        assert_eq!(dispatch.register("a", 8).unwrap(), new);
        assert_eq!(new.epoch(), "epoch");
        assert_eq!(new.session_id(), "a");
        assert_eq!(new.incarnation(), 8);
        ordinary(&dispatch, &new, "new");
        assert_eq!(*dispatch.try_next().unwrap().unwrap().event(), "new");
        assert!(dispatch.try_next().unwrap().is_none());
    }

    #[test]
    fn registry_identity_and_session_count_are_bounded_independently_of_queue_items() {
        let dispatch = dispatch();
        let another = SessionDispatch::<()>::new("epoch", limits()).unwrap();
        let foreign = another.register("a", 1).unwrap();
        let next_epoch = SessionDispatch::<()>::new("other-epoch", limits()).unwrap();
        let epoch_handle = next_epoch.register("a", 1).unwrap();
        let own = dispatch.register("a", 1).unwrap();
        assert_eq!(
            dispatch
                .try_route(
                    &foreign,
                    TrafficClass::Ordinary,
                    EventOrigin::Command,
                    1,
                    "foreign"
                )
                .unwrap_err()
                .error,
            DispatchError::ForeignHandle
        );
        assert_eq!(
            dispatch
                .try_route(
                    &epoch_handle,
                    TrafficClass::Ordinary,
                    EventOrigin::Command,
                    1,
                    "foreign"
                )
                .unwrap_err()
                .error,
            DispatchError::EpochMismatch
        );
        for id in ["b", "c", "d"] {
            dispatch.register(id, 1).unwrap();
        }
        assert_eq!(dispatch.register("e", 1), Err(DispatchError::SessionLimit));
        dispatch.register("a", 2).unwrap();
        assert_eq!(dispatch.remove(&own), Err(DispatchError::StaleHandle));
    }

    #[test]
    fn close_releases_queued_events_and_preserves_outstanding_delivery_accounting() {
        let dispatch = dispatch();
        let a = dispatch.register("a", 1).unwrap();
        let b = dispatch.register("b", 1).unwrap();
        ordinary(&dispatch, &a, "in-flight");
        ordinary(&dispatch, &a, "queued");
        required(&dispatch, &b, "required-queued");
        let delivery = dispatch.try_next().unwrap().unwrap();
        dispatch.close().unwrap();
        dispatch.close().unwrap();
        assert_eq!(
            dispatch.global_usage(TrafficClass::Ordinary),
            BudgetUsage { items: 1, bytes: 1 }
        );
        assert_eq!(
            dispatch.global_usage(TrafficClass::Reserved),
            BudgetUsage { items: 0, bytes: 0 }
        );
        assert!(matches!(dispatch.try_next(), Err(DispatchError::Closed)));
        assert_eq!(dispatch.register("new", 1), Err(DispatchError::Closed));
        let rejected = dispatch
            .try_route(
                &a,
                TrafficClass::Reserved,
                EventOrigin::RequiredInbound,
                1,
                "late-response",
            )
            .unwrap_err();
        assert_eq!(rejected.error, DispatchError::Closed);
        assert_eq!(rejected.event, "late-response");
        drop(delivery);
        assert_eq!(
            dispatch.global_usage(TrafficClass::Ordinary),
            BudgetUsage { items: 0, bytes: 0 }
        );
    }

    #[test]
    fn simultaneous_workers_cannot_take_two_deliveries_for_one_session() {
        let dispatch = dispatch();
        let a = dispatch.register("a", 1).unwrap();
        ordinary(&dispatch, &a, "first");
        ordinary(&dispatch, &a, "second");
        let attempted = Barrier::new(2);
        let taken = std::thread::scope(|scope| {
            let worker = || {
                let delivery = dispatch.try_next().unwrap();
                attempted.wait();
                delivery.is_some()
            };
            let first = scope.spawn(worker);
            let second = scope.spawn(worker);
            usize::from(first.join().unwrap()) + usize::from(second.join().unwrap())
        });
        assert_eq!(taken, 1);
        assert_eq!(*dispatch.try_next().unwrap().unwrap().event(), "second");
    }

    #[test]
    fn removing_events_runs_their_destructors_outside_the_registry_lock() {
        struct OnDrop(Option<Box<dyn FnOnce() + Send>>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                self.0.take().unwrap()();
            }
        }
        let dispatch = SessionDispatch::new("epoch", limits()).unwrap();
        let session = dispatch.register("a", 1).unwrap();
        let dropped = Arc::new(AtomicUsize::new(0));
        let reentrant = dispatch.clone();
        let observed = dropped.clone();
        let event = OnDrop(Some(Box::new(move || {
            reentrant.register("b", 2).unwrap();
            observed.fetch_add(1, Ordering::Relaxed);
        })));
        assert!(
            dispatch
                .try_route(
                    &session,
                    TrafficClass::Ordinary,
                    EventOrigin::Command,
                    1,
                    event
                )
                .is_ok()
        );
        dispatch.remove(&session).unwrap();
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }
}
