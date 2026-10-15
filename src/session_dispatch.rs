//! FIFO queues for short session transitions at the ordered ingress boundary.
//!
//! Taking a delivery makes its session busy until the guard is dropped. Other
//! sessions remain eligible for round-robin draining. A guard covers local state
//! reduction, not an Agent request, user interaction, or external cleanup.
//! Queued items and bytes are accounted, but bursts never reject accepted content.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DispatchError {
    Closed,
    RegistryPoisoned,
    EpochMismatch,
    ForeignHandle,
    SessionRemoved,
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
    registry: Arc<()>,
}

impl SessionHandle {
    pub(crate) fn epoch(&self) -> &str {
        &self.epoch
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl PartialEq for SessionHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.registry, &other.registry)
            && self.epoch == other.epoch
            && self.session_id == other.session_id
    }
}

impl Eq for SessionHandle {}

#[derive(Debug)]
pub(crate) struct Rejected<E> {
    pub error: DispatchError,
    pub event: E,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QueueUsage {
    // Includes queued deliveries and deliveries currently being reduced.
    pub items: usize,
    pub bytes: usize,
}

#[derive(Debug, Default)]
struct QueueAccounting {
    items: Arc<AtomicUsize>,
    bytes: Arc<AtomicUsize>,
}

impl QueueAccounting {
    fn record(&self, bytes: usize) -> QueueLease {
        self.items.fetch_add(1, Ordering::AcqRel);
        self.bytes.fetch_add(bytes, Ordering::AcqRel);
        QueueLease {
            items: self.items.clone(),
            bytes: self.bytes.clone(),
            size: bytes,
        }
    }

    fn usage(&self) -> QueueUsage {
        QueueUsage {
            items: self.items.load(Ordering::Acquire),
            bytes: self.bytes.load(Ordering::Acquire),
        }
    }
}

#[derive(Default)]
struct ClassPools {
    ordinary: QueueAccounting,
    reserved: QueueAccounting,
}

impl ClassPools {
    fn get(&self, class: TrafficClass) -> &QueueAccounting {
        match class {
            TrafficClass::Ordinary => &self.ordinary,
            TrafficClass::Reserved => &self.reserved,
        }
    }
}

#[derive(Debug)]
struct QueueLease {
    items: Arc<AtomicUsize>,
    bytes: Arc<AtomicUsize>,
    size: usize,
}

impl Drop for QueueLease {
    fn drop(&mut self) {
        self.items.fetch_sub(1, Ordering::AcqRel);
        self.bytes.fetch_sub(self.size, Ordering::AcqRel);
    }
}

#[derive(Debug)]
pub(crate) struct DeliveryGuard {
    // Dropping a removed queue's guard never changes its reopened queue's flag.
    busy: Option<Arc<AtomicBool>>,
    session: Option<QueueLease>,
    global: Option<QueueLease>,
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
    event: E,
    guard: DeliveryGuard,
}

impl<E> Delivery<E> {
    #[cfg(test)]
    pub(crate) fn event(&self) -> &E {
        &self.event
    }

    #[cfg(test)]
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
    accounting: ClassPools,
    queue: VecDeque<Delivery<E>>,
    scheduled: bool,
    busy: Arc<AtomicBool>,
}

struct Registry<E> {
    closed: bool,
    sessions: HashMap<Arc<str>, SessionQueue<E>>,
    ready: VecDeque<SessionHandle>,
}

struct Inner<E> {
    epoch: Arc<str>,
    identity: Arc<()>,
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
    pub(crate) fn new(epoch: impl Into<String>) -> Result<Self, DispatchError> {
        Ok(Self {
            inner: Arc::new(Inner {
                epoch: Arc::from(epoch.into()),
                identity: Arc::new(()),
                global: ClassPools::default(),
                registry: Mutex::new(Registry {
                    closed: false,
                    sessions: HashMap::new(),
                    ready: VecDeque::new(),
                }),
            }),
        })
    }

    /// Registering an existing queue returns its handle and keeps its queued
    /// events and busy delivery. Registering after removal creates a new queue.
    pub(crate) fn register(
        &self,
        session_id: impl Into<String>,
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
        if let Some(existing) = registry.sessions.get(&session_id) {
            return Ok(existing.handle.clone());
        }
        let handle = SessionHandle {
            epoch: self.inner.epoch.clone(),
            session_id: session_id.clone(),
            registry: self.inner.identity.clone(),
        };
        registry.sessions.insert(
            session_id,
            SessionQueue {
                handle: handle.clone(),
                accounting: ClassPools::default(),
                queue: VecDeque::new(),
                scheduled: false,
                busy: Arc::new(AtomicBool::new(false)),
            },
        );
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
        if !registry.sessions.contains_key(handle.session_id()) {
            return Err(DispatchError::SessionRemoved);
        }
        Ok(())
    }

    /// `bytes` is measured by the ingress serializer for accounting only.
    /// Enqueueing never rejects an event because earlier work is still pending.
    pub(crate) fn try_route(
        &self,
        handle: &SessionHandle,
        class: TrafficClass,
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
            let queue = registry
                .sessions
                .get_mut(handle.session_id())
                .expect("validated queue");
            let session = queue.accounting.get(class).record(bytes);
            let global = self.inner.global.get(class).record(bytes);
            Ok((
                registry,
                DeliveryGuard {
                    busy: None,
                    session: Some(session),
                    global: Some(global),
                },
            ))
        })();
        match result {
            Ok((mut registry, guard)) => {
                let queue = registry
                    .sessions
                    .get_mut(handle.session_id())
                    .expect("validated queue remains locked");
                queue.queue.push_back(Delivery {
                    handle: handle.clone(),
                    event,
                    guard,
                });
                if !queue.scheduled {
                    queue.scheduled = true;
                    registry.ready.push_back(handle.clone());
                }
                Ok(())
            }
            Err(error) => Err(Rejected { error, event }),
        }
    }

    /// Takes one event from the next non-busy session. FIFO is shared by both
    /// traffic classes. Busy sessions remain scheduled, so the caller can drain
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

    pub(crate) fn global_usage(&self, class: TrafficClass) -> QueueUsage {
        self.inner.global.get(class).usage()
    }

    pub(crate) fn session_usage(
        &self,
        handle: &SessionHandle,
        class: TrafficClass,
    ) -> Result<QueueUsage, DispatchError> {
        let registry = self
            .inner
            .registry
            .lock()
            .map_err(|_| DispatchError::RegistryPoisoned)?;
        self.validate_handle(&registry, handle)?;
        Ok(registry.sessions[handle.session_id()]
            .accounting
            .get(class)
            .usage())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicUsize;

    fn dispatch() -> SessionDispatch<&'static str> {
        SessionDispatch::new("epoch").unwrap()
    }

    fn ordinary(
        dispatch: &SessionDispatch<&'static str>,
        handle: &SessionHandle,
        event: &'static str,
    ) {
        dispatch
            .try_route(handle, TrafficClass::Ordinary, 1, event)
            .unwrap();
    }

    fn required(
        dispatch: &SessionDispatch<&'static str>,
        handle: &SessionHandle,
        event: &'static str,
    ) {
        dispatch
            .try_route(handle, TrafficClass::Reserved, 1, event)
            .unwrap();
    }

    #[test]
    fn empty_epoch_is_rejected_before_allocating_queues() {
        let dispatch = dispatch();
        let session = dispatch.register("a").unwrap();
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
            QueueUsage { items: 0, bytes: 0 }
        );
        assert_eq!(
            dispatch.global_usage(TrafficClass::Reserved),
            QueueUsage { items: 0, bytes: 0 }
        );
    }

    #[test]
    fn zero_byte_events_and_extracted_payloads_retain_accounting_until_guard_drop() {
        let dispatch = dispatch();
        let session = dispatch.register("a").unwrap();
        dispatch
            .try_route(&session, TrafficClass::Reserved, 0, "complete")
            .unwrap();
        let mut delivery = dispatch.try_next().unwrap().unwrap();
        *delivery.event_mut() = "reduced";
        let (event, guard) = delivery.into_parts();
        assert_eq!(event, "reduced");
        assert_eq!(
            dispatch.global_usage(TrafficClass::Reserved),
            QueueUsage { items: 1, bytes: 1 }
        );
        assert_eq!(
            dispatch
                .session_usage(&session, TrafficClass::Reserved)
                .unwrap(),
            QueueUsage { items: 1, bytes: 1 }
        );
        dispatch
            .try_route(&session, TrafficClass::Reserved, 0, "next")
            .unwrap();
        assert!(dispatch.try_next().unwrap().is_none());
        assert_eq!(
            dispatch.global_usage(TrafficClass::Reserved),
            QueueUsage { items: 2, bytes: 2 }
        );
        drop(guard);
        let next = dispatch.try_next().unwrap().unwrap();
        assert_eq!(*next.event(), "next");
        drop(next);
        assert_eq!(
            dispatch.global_usage(TrafficClass::Reserved),
            QueueUsage { items: 0, bytes: 0 }
        );
    }

    #[test]
    fn large_backlogs_preserve_fifo_and_other_sessions_can_progress() {
        let dispatch = SessionDispatch::new("epoch").unwrap();
        let a = dispatch.register("a").unwrap();
        let b = dispatch.register("b").unwrap();
        for index in 0..4_096 {
            dispatch
                .try_route(&a, TrafficClass::Ordinary, 64 * 1024, index)
                .unwrap();
        }
        dispatch
            .try_route(&a, TrafficClass::Reserved, 1, 4_096)
            .unwrap();
        dispatch
            .try_route(&b, TrafficClass::Ordinary, 1, 99)
            .unwrap();
        let first = dispatch.try_next().unwrap().unwrap();
        assert_eq!(*first.event(), 0);
        let other = dispatch.try_next().unwrap().unwrap();
        assert_eq!(other.handle, b);
        assert_eq!(*other.event(), 99);
        assert!(dispatch.try_next().unwrap().is_none());
        drop((first, other));
        for index in 1..=4_096 {
            let next = dispatch.try_next().unwrap().unwrap();
            assert_eq!(next.handle, a);
            assert_eq!(*next.event(), index);
        }
        assert!(dispatch.try_next().unwrap().is_none());
        assert_eq!(
            dispatch.global_usage(TrafficClass::Ordinary),
            QueueUsage { items: 0, bytes: 0 }
        );
        assert_eq!(
            dispatch.global_usage(TrafficClass::Reserved),
            QueueUsage { items: 0, bytes: 0 }
        );
    }

    #[test]
    fn round_robin_skips_busy_sessions_without_advancing_their_fifo() {
        let dispatch = dispatch();
        let a = dispatch.register("a").unwrap();
        let b = dispatch.register("b").unwrap();
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
    fn replacement_releases_queued_accounting_but_old_delivery_cannot_unblock_reopened_session() {
        let dispatch = dispatch();
        let old = dispatch.register("a").unwrap();
        ordinary(&dispatch, &old, "old-running");
        ordinary(&dispatch, &old, "old-queued");
        let old_delivery = dispatch.try_next().unwrap().unwrap();
        dispatch.remove(&old).unwrap();
        assert_eq!(dispatch.remove(&old), Err(DispatchError::SessionRemoved));
        let new = dispatch.register("a").unwrap();
        assert_eq!(
            dispatch.global_usage(TrafficClass::Ordinary),
            QueueUsage { items: 1, bytes: 1 }
        );
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
        drop(new_delivery);
        assert_eq!(*dispatch.try_next().unwrap().unwrap().event(), "new-queued");
        assert_eq!(dispatch.global_usage(TrafficClass::Ordinary).items, 0);
    }

    #[test]
    fn registering_an_existing_queue_keeps_its_events_and_busy_delivery() {
        let dispatch = dispatch();
        let session = dispatch.register("a").unwrap();
        ordinary(&dispatch, &session, "running");
        required(&dispatch, &session, "queued-response");
        let running = dispatch.try_next().unwrap().unwrap();
        assert_eq!(dispatch.register("a").unwrap(), session);
        assert_eq!(session.epoch(), "epoch");
        assert_eq!(session.session_id(), "a");
        assert_eq!(dispatch.global_usage(TrafficClass::Ordinary).items, 1);
        assert_eq!(dispatch.global_usage(TrafficClass::Reserved).items, 1);
        assert!(
            dispatch.try_next().unwrap().is_none(),
            "registering again must not release the running delivery"
        );
        drop(running);
        assert_eq!(
            *dispatch.try_next().unwrap().unwrap().event(),
            "queued-response"
        );
        assert!(dispatch.try_next().unwrap().is_none());
    }

    #[test]
    fn registry_identity_is_checked_without_limiting_session_count() {
        let dispatch = dispatch();
        let another = SessionDispatch::<()>::new("epoch").unwrap();
        let foreign = another.register("a").unwrap();
        let next_epoch = SessionDispatch::<()>::new("other-epoch").unwrap();
        let epoch_handle = next_epoch.register("a").unwrap();
        let own = dispatch.register("a").unwrap();
        assert_eq!(
            dispatch
                .try_route(&foreign, TrafficClass::Ordinary, 1, "foreign")
                .unwrap_err()
                .error,
            DispatchError::ForeignHandle
        );
        assert_eq!(
            dispatch
                .try_route(&epoch_handle, TrafficClass::Ordinary, 1, "foreign")
                .unwrap_err()
                .error,
            DispatchError::EpochMismatch
        );
        for id in ["b", "c", "d"] {
            dispatch.register(id).unwrap();
        }
        for index in 0..1_024 {
            dispatch.register(format!("extra-{index}")).unwrap();
        }
        assert_eq!(dispatch.register("a").unwrap(), own);
    }

    #[test]
    fn close_releases_queued_events_and_preserves_outstanding_delivery_accounting() {
        let dispatch = dispatch();
        let a = dispatch.register("a").unwrap();
        let b = dispatch.register("b").unwrap();
        ordinary(&dispatch, &a, "in-flight");
        ordinary(&dispatch, &a, "queued");
        required(&dispatch, &b, "required-queued");
        let delivery = dispatch.try_next().unwrap().unwrap();
        dispatch.close().unwrap();
        dispatch.close().unwrap();
        assert_eq!(
            dispatch.global_usage(TrafficClass::Ordinary),
            QueueUsage { items: 1, bytes: 1 }
        );
        assert_eq!(
            dispatch.global_usage(TrafficClass::Reserved),
            QueueUsage { items: 0, bytes: 0 }
        );
        assert!(matches!(dispatch.try_next(), Err(DispatchError::Closed)));
        assert_eq!(dispatch.register("new"), Err(DispatchError::Closed));
        let rejected = dispatch
            .try_route(&a, TrafficClass::Reserved, 1, "late-response")
            .unwrap_err();
        assert_eq!(rejected.error, DispatchError::Closed);
        assert_eq!(rejected.event, "late-response");
        drop(delivery);
        assert_eq!(
            dispatch.global_usage(TrafficClass::Ordinary),
            QueueUsage { items: 0, bytes: 0 }
        );
    }

    #[test]
    fn simultaneous_workers_cannot_take_two_deliveries_for_one_session() {
        let dispatch = dispatch();
        let a = dispatch.register("a").unwrap();
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
        let dispatch = SessionDispatch::new("epoch").unwrap();
        let session = dispatch.register("a").unwrap();
        let dropped = Arc::new(AtomicUsize::new(0));
        let reentrant = dispatch.clone();
        let observed = dropped.clone();
        let event = OnDrop(Some(Box::new(move || {
            reentrant.register("b").unwrap();
            observed.fetch_add(1, Ordering::Relaxed);
        })));
        assert!(
            dispatch
                .try_route(&session, TrafficClass::Ordinary, 1, event)
                .is_ok()
        );
        dispatch.remove(&session).unwrap();
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }
}
