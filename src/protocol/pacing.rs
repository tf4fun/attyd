//! When a scope is due a checkpoint (docs/protocol.md §5.5).
//!
//! A checkpoint states the digest of a scope's document at its current rev.
//! While a scope keeps changing, a checkpoint every change would cost more than
//! it protects: one is due after a pause in the changes, and at the latest a
//! fixed time after the first change no checkpoint has covered. The pacer
//! knows only instants it is given.
use std::collections::BTreeMap;
use std::time::Duration;

use tokio::time::Instant;

/// How long a scope must be unchanged for its checkpoint to be due.
pub(crate) const PAUSE: Duration = Duration::from_millis(500);
/// How long a scope may change before its checkpoint is due regardless.
pub(crate) const LIMIT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
struct Unchecked {
    /// The first change since the last checkpoint.
    first: Instant,
    last: Instant,
}

impl Unchecked {
    fn deadline(&self) -> Instant {
        (self.last + PAUSE).min(self.first + LIMIT)
    }
}

#[derive(Clone, Default)]
pub(crate) struct Pacer {
    unchecked: BTreeMap<String, Unchecked>,
}

impl Pacer {
    /// `scope` changed at `now` and the change is not covered by a checkpoint.
    pub(crate) fn changed(&mut self, scope: &str, now: Instant) {
        self.unchecked
            .entry(scope.to_string())
            .and_modify(|unchecked| unchecked.last = now)
            .or_insert(Unchecked {
                first: now,
                last: now,
            });
    }

    /// `scope` has nothing a checkpoint would add: a snapshot carried its
    /// digest, or the scope is over.
    pub(crate) fn settled(&mut self, scope: &str) {
        self.unchecked.remove(scope);
    }

    /// The scopes due a checkpoint at `now`, which are handed out once.
    pub(crate) fn due(&mut self, now: Instant) -> Vec<String> {
        let due: Vec<String> = self
            .unchecked
            .iter()
            .filter(|(_, unchecked)| unchecked.deadline() <= now)
            .map(|(scope, _)| scope.clone())
            .collect();
        for scope in &due {
            self.unchecked.remove(scope);
        }
        due
    }

    /// When the next scope is due, if any is waiting.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.unchecked.values().map(Unchecked::deadline).min()
    }
}

#[cfg(test)]
mod tests;
