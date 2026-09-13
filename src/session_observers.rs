use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;

use crate::auto_close::AutoClosePermit;
use crate::runtime_state::SessionLifecycle;
use crate::session_observation::ObservationLease;

/// Observation belongs to the session allocation. Delivery only owns cancellable
/// leases; it never decides whether a session is eligible to close.
#[derive(Default)]
pub(crate) struct SessionObservers {
    observers: HashMap<u64, ObservationLease>,
    next_absence: u64,
    absence: Option<AbsenceTimer>,
}

#[derive(Clone)]
pub(crate) struct AbsenceTimer {
    pub(crate) id: u64,
    started: Instant,
    timeout: Duration,
    pub(crate) permit: AutoClosePermit,
}

impl AbsenceTimer {
    /// Avoid adding an arbitrarily large configured timeout to an Instant.
    pub(crate) async fn wait(&self) -> bool {
        loop {
            if !self.permit.pending() {
                return false;
            }
            let remaining = self.timeout.saturating_sub(self.started.elapsed());
            if remaining.is_zero() {
                return true;
            }
            tokio::select! {
                _ = self.permit.cancelled.cancelled() => return false,
                _ = tokio::time::sleep(remaining.min(Duration::from_secs(86_400))) => {}
            }
        }
    }
}

impl SessionObservers {
    pub(crate) fn observe(&mut self, id: u64, lease: ObservationLease) -> bool {
        if lease.is_cancelled() || lease.is_finished() {
            return false;
        }
        if let Some(existing) = self.observers.get(&id) {
            return existing.same_identity(&lease);
        }
        self.observers.insert(id, lease);
        self.cancel_absence();
        true
    }

    pub(crate) fn unobserve(&mut self, id: u64, lease: &ObservationLease) -> bool {
        if !self
            .observers
            .get(&id)
            .is_some_and(|existing| existing.same_identity(lease))
        {
            return false;
        }
        if let Some(lease) = self.observers.remove(&id) {
            lease.cancel();
        }
        true
    }

    /// Called after an owner transition. Output, turn completion and temporary
    /// control/loading work preserve an existing absence interval.
    pub(crate) fn refresh(
        &mut self,
        lifecycle: Option<&SessionLifecycle>,
        timeout_seconds: i64,
        now: Instant,
    ) -> Option<AbsenceTimer> {
        self.observers.retain(|_, lease| !lease.is_cancelled());
        if timeout_seconds < 0
            || lifecycle.is_none_or(|phase| {
                matches!(phase, SessionLifecycle::Closed | SessionLifecycle::Deleting)
            })
            || !self.observers.is_empty()
        {
            self.cancel_absence();
            return None;
        }
        if self.absence.is_some() || lifecycle != Some(&SessionLifecycle::Active) {
            return None;
        }
        self.next_absence = self.next_absence.wrapping_add(1).max(1);
        let timer = AbsenceTimer {
            id: self.next_absence,
            started: now,
            timeout: Duration::from_secs(timeout_seconds as u64),
            permit: AutoClosePermit::default(),
        };
        self.absence = Some(timer.clone());
        Some(timer)
    }

    pub(crate) fn matches_absence(&self, id: u64, permit: &AutoClosePermit) -> bool {
        self.absence
            .as_ref()
            .is_some_and(|absence| absence.id == id && absence.permit.same_identity(permit))
            && self.observers.values().all(ObservationLease::is_cancelled)
    }

    pub(crate) fn shutdown(&mut self) {
        self.cancel_absence();
        for (_, lease) in self.observers.drain() {
            lease.cancel();
        }
    }

    pub(crate) fn stop_absence(&mut self) {
        self.cancel_absence();
    }

    /// The caller publishes an end marker before finishing these leases. Taking
    /// them prevents resource retirement from turning an ordered end into abort.
    pub(crate) fn drain_for_finish(&mut self) -> Vec<(u64, ObservationLease)> {
        self.cancel_absence();
        self.observers.drain().collect()
    }

    fn cancel_absence(&mut self) {
        if let Some(absence) = self.absence.take() {
            absence.permit.cancel();
        }
    }
}

impl Drop for SessionObservers {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arm(owner: &mut SessionObservers, timeout: i64) -> Option<AbsenceTimer> {
        owner.refresh(Some(&SessionLifecycle::Active), timeout, Instant::now())
    }

    #[tokio::test(start_paused = true)]
    async fn return_cancels_queued_close_and_last_departure_rearms_full_interval() {
        let mut owner = SessionObservers::default();
        let old = arm(&mut owner, 0).unwrap();
        assert!(old.wait().await);
        let first = ObservationLease::default();
        let second = ObservationLease::default();
        assert!(owner.observe(1, first.clone()));
        assert!(owner.observe(2, second.clone()));
        assert!(!old.permit.claim());
        owner.unobserve(1, &first);
        assert!(arm(&mut owner, 30).is_none());
        owner.unobserve(2, &second);
        let next = arm(&mut owner, 30).unwrap();
        assert!(!owner.matches_absence(old.id, &old.permit));
        let waiting = next.wait();
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        tokio::time::advance(Duration::from_secs(29)).await;
        assert!(futures::poll!(&mut waiting).is_pending());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(waiting.await);
        assert!(owner.matches_absence(next.id, &next.permit));
    }

    #[tokio::test(start_paused = true)]
    async fn output_and_busy_transitions_do_not_reset_absence() {
        let mut owner = SessionObservers::default();
        let timer = arm(&mut owner, 30).unwrap();
        tokio::time::advance(Duration::from_secs(20)).await;
        assert!(arm(&mut owner, 30).is_none());
        assert!(
            owner
                .refresh(Some(&SessionLifecycle::Closing), 30, Instant::now())
                .is_none()
        );
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(timer.wait().await);
        assert!(arm(&mut owner, 30).is_none());
        assert!(owner.matches_absence(timer.id, &timer.permit));
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_handshake_and_old_unobserve_do_not_leak_or_remove_other_lease() {
        let mut owner = SessionObservers::default();
        let cancelled = ObservationLease::default();
        cancelled.cancel();
        assert!(!owner.observe(1, cancelled));
        let lease = ObservationLease::default();
        assert!(owner.observe(1, lease.clone()));
        assert!(!owner.unobserve(1, &ObservationLease::default()));
        assert!(arm(&mut owner, 30).is_none());
        lease.cancel();
        let timer = arm(&mut owner, 30).unwrap();
        tokio::time::advance(Duration::from_secs(29)).await;
        let waiting = timer.wait();
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(waiting.await);
    }

    #[tokio::test(start_paused = true)]
    async fn negative_timeouts_disable_and_huge_timeouts_do_not_overflow() {
        for timeout in [i64::MIN, -2, -1] {
            assert!(arm(&mut SessionObservers::default(), timeout).is_none());
        }
        let mut owner = SessionObservers::default();
        let timer = arm(&mut owner, i64::MAX).unwrap();
        let waiting = timer.wait();
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        tokio::time::advance(Duration::from_secs(1_000_000)).await;
        assert!(futures::poll!(&mut waiting).is_pending());
        owner.shutdown();
        assert!(!waiting.await);
    }

    #[tokio::test(start_paused = true)]
    async fn retirement_and_replacement_cancel_old_timer_without_affecting_new_owner() {
        let mut old_owner = SessionObservers::default();
        let old = arm(&mut old_owner, 0).unwrap();
        let mut new_owner = SessionObservers::default();
        let new = arm(&mut new_owner, 0).unwrap();
        assert!(!new_owner.matches_absence(old.id, &old.permit));
        drop(old_owner);
        assert!(!old.wait().await);
        assert!(new.wait().await);
        new_owner.refresh(Some(&SessionLifecycle::Closed), 0, Instant::now());
        assert!(!new.permit.claim());
    }

    #[tokio::test(start_paused = true)]
    async fn stopping_timers_preserves_delivery_until_ordered_finish() {
        let mut owner = SessionObservers::default();
        let old_timer = arm(&mut owner, 30).unwrap();
        owner.stop_absence();
        assert!(!old_timer.permit.claim());
        let lease = ObservationLease::new();
        owner.observe(1, lease.clone());
        owner.stop_absence();
        assert!(!lease.is_cancelled());
        let drained = owner.drain_for_finish();
        assert_eq!(drained.len(), 1);
        assert!(drained[0].1.same_identity(&lease));
        drained[0].1.finish();
        drop(owner);
        assert!(lease.is_finished());
        assert!(
            !lease.is_cancelled(),
            "normal retirement must not abort queued events"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn observation_cannot_revoke_an_already_dispatched_close() {
        let mut owner = SessionObservers::default();
        let timer = arm(&mut owner, 0).unwrap();
        assert!(timer.permit.claim());
        assert!(owner.observe(1, ObservationLease::default()));
        assert!(!timer.permit.pending());
        assert!(!timer.permit.claim());
    }
}
