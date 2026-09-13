use std::sync::Arc;

use tokio_util::sync::CancellationToken;

/// Shared lifetime of one session observer. Cancellation is immediate even when
/// its Unobserve command is waiting for bridge input capacity.
#[derive(Clone, Debug, Default)]
pub(crate) struct ObservationLease {
    cancellation: Arc<CancellationToken>,
    finished: Arc<CancellationToken>,
}

impl ObservationLease {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub(crate) async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }

    pub(crate) fn finish(&self) {
        self.finished.cancel();
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.finished.is_cancelled()
    }

    pub(crate) async fn finished(&self) {
        self.finished.cancelled().await;
    }

    pub(crate) fn same_identity(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.cancellation, &other.cancellation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_wakes_the_owner_without_waiting_for_unobserve_delivery() {
        let lease = ObservationLease::new();
        let owner = lease.clone();
        let unrelated = ObservationLease::new();
        assert!(lease.same_identity(&owner));
        assert!(!lease.same_identity(&unrelated));
        lease.cancel();
        owner.cancelled().await;
        assert!(owner.is_cancelled());
        assert!(!unrelated.is_cancelled());
    }
}
