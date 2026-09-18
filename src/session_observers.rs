use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;

use crate::auto_close::AutoClosePermit;
use crate::runtime_state::SessionLifecycle;
use crate::session_observation::ObservationLease;
use crate::session_state::SessionState;

/// Attention state machine input: what an unobserved session is doing.
/// Only `Idle` arms the closing countdown; every other state is live work.
///
/// The full machine, driven by Agent/observer events through `refresh`:
///
/// ```text
///   OBSERVED ── ≥1 observation lease; never retires
///      │ last observer leaves (classified by work)
///      ▼
///   WORKING ── active_turn / operation / load_attempt / live terminal
///      │         in-flight: real work, no countdown
///      │ interaction request arrives (permission, elicitation, url flow)
///      ▼
///   AWAITING ── interaction pending: the Agent is parked on a user decision
///      │         Retirement would cancel it Agent-side, so no countdown.
///      │ interaction resolves → WORKING (turn continues) or CLOSING
///      │ turn ends with nothing pending
///      ▼
///   CLOSING ── nothing in-flight: countdown armed (idle since T,
///      │       deadline T + interval). Expiry retires; arriving work or an
///      │       observer cancels it.
///      ▼
///   RETIRED ── permit claimed → session/close or local retirement
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionWork {
    /// Nothing in-flight: the unobserved countdown may arm.
    Idle,
    /// A turn, exclusive operation, materialization attempt, or live managed
    /// terminal is in-flight: real work that must not be cut off.
    Running,
    /// The Agent is parked on a user decision (permission, elicitation, or URL
    /// flow). Cancelling it changes Agent state, so it must block retirement.
    AwaitingInteraction,
}

impl SessionWork {
    /// Derive the work state from the session owner.
    pub(crate) fn of(session: &SessionState) -> Self {
        if session.live.as_ref().is_some_and(|live| {
            !live.permissions.is_empty()
                || !live.elicitations.is_empty()
                || !live.url_flows.is_empty()
        }) {
            return Self::AwaitingInteraction;
        }
        if session.active_turn.is_some()
            || session.operation.is_some()
            || session.load_attempt.is_some()
            || session
                .live
                .as_ref()
                .is_some_and(|live| live.terminals.values().any(terminal_is_running))
        {
            return Self::Running;
        }
        Self::Idle
    }
}

/// A retained terminal counts as running work only while its process is alive:
/// released entries are removed upstream, and an exited terminal has its
/// `exitStatus` recorded even before release.
fn terminal_is_running(terminal: &serde_json::Value) -> bool {
    !terminal
        .get("released")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        && terminal
            .get("exitStatus")
            .is_none_or(serde_json::Value::is_null)
}

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

    /// Called after an owner transition. The absence interval measures
    /// *continuously idle* time: an observer or any non-Idle work state clears
    /// the deadline, and settling back into idle starts a fresh interval.
    pub(crate) fn refresh(
        &mut self,
        lifecycle: Option<&SessionLifecycle>,
        work: SessionWork,
        timeout_seconds: i64,
        now: Instant,
    ) -> Option<AbsenceTimer> {
        self.observers.retain(|_, lease| !lease.is_cancelled());
        if timeout_seconds < 0
            || lifecycle.is_none_or(|phase| {
                matches!(phase, SessionLifecycle::Closed | SessionLifecycle::Deleting)
            })
            || !self.observers.is_empty()
            || work != SessionWork::Idle
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
        owner.refresh(
            Some(&SessionLifecycle::Active),
            SessionWork::Idle,
            timeout,
            Instant::now(),
        )
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
    async fn non_idle_work_cancels_absence_and_idle_rearms_a_fresh_interval() {
        for work in [SessionWork::Running, SessionWork::AwaitingInteraction] {
            let mut owner = SessionObservers::default();
            let timer = arm(&mut owner, 30).unwrap();
            tokio::time::advance(Duration::from_secs(20)).await;
            // In-flight work or a pending interaction cancels the deadline.
            assert!(
                owner
                    .refresh(Some(&SessionLifecycle::Active), work, 30, Instant::now())
                    .is_none()
            );
            tokio::time::advance(Duration::from_secs(10)).await;
            assert!(!timer.wait().await);
            assert!(!timer.permit.pending());
            // Settling back to idle starts a fresh interval, not a residual one.
            let next = arm(&mut owner, 30).unwrap();
            tokio::time::advance(Duration::from_secs(29)).await;
            let waiting = next.wait();
            tokio::pin!(waiting);
            assert!(futures::poll!(&mut waiting).is_pending());
            tokio::time::advance(Duration::from_secs(1)).await;
            assert!(waiting.await);
            assert!(!owner.matches_absence(timer.id, &timer.permit));
            assert!(owner.matches_absence(next.id, &next.permit));
        }
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
        new_owner.refresh(
            Some(&SessionLifecycle::Closed),
            SessionWork::Idle,
            0,
            Instant::now(),
        );
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
