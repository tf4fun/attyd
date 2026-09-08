use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
use tokio_util::sync::CancellationToken;

/// An absence interval can be cancelled until the bridge commits to sending close.
#[derive(Clone, Default)]
pub(crate) struct AutoClosePermit {
    state: Arc<AtomicU8>,
    pub cancelled: CancellationToken,
}

impl AutoClosePermit {
    pub fn pending(&self) -> bool {
        self.state.load(Ordering::Acquire) == 0
    }
    pub fn claim(&self) -> bool {
        self.state
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    pub fn cancel(&self) {
        let _ = self
            .state
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire);
        self.cancelled.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn returning_observer_invalidates_queued_close_but_not_a_dispatched_close() {
        let permit = AutoClosePermit::default();
        let queued = permit.clone();
        permit.cancel();
        assert!(!queued.claim());
        let next_absence = AutoClosePermit::default();
        assert!(next_absence.claim());
        next_absence.cancel();
        assert!(!next_absence.claim());
        assert_eq!(next_absence.state.load(Ordering::Acquire), 1);
    }
}
