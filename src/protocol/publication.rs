//! What the bridge publishes, shared by the actor's generations (docs/protocol.md §5.4).
use std::sync::{Arc, Mutex, MutexGuard};

use super::feed::FeedHandle;
use super::registry::WaitingSession;

/// The feed and the sessions an ended connection left for the next one: bridge memory shared by
/// the generations. The commit step is the feed's only writer.
pub(crate) struct Published {
    pub feed: FeedHandle,
    /// The held sessions an ended connection left for the next one to load
    /// again, because a subscription follows them (S6, §5.4).
    pub waiting: Vec<WaitingSession>,
}

impl Default for Published {
    fn default() -> Self {
        Self {
            // Names the epoch `global` and `catalog` are published under: new
            // for every bridge process, so that a client of an earlier one
            // asks for the whole document.
            feed: FeedHandle::new(super::op::operation_id()),
            waiting: Vec::new(),
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct Publication(Arc<Mutex<Published>>);

impl Publication {
    pub(crate) fn lock(&self) -> MutexGuard<'_, Published> {
        self.0.lock().expect("publication poisoned")
    }
}
