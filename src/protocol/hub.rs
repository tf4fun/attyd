//! The Hub (docs/state-sync.md §2.4): delivery of committed events to streams.
//!
//! The Hub owns delivery only. A subscription is registered under the bridge
//! lock and aligned from the published documents (the cut); the events of
//! later commits enter its queue in the critical section that produced them
//! (S3). Each subscription keeps, per session scope, a baseline and a queued
//! tail; a queue past its byte budget is coalesced per scope from the
//! baseline. No log is retained.
//!
//! The published documents and the Hub outlive one Agent connection: they are
//! bridge memory, shared by the generations of the bridge actor, and written
//! by the commit step only (I1).
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use super::canon::{ChainHash, LogEntry};
use super::documents::{Event, ProtocolError, Scope};
use super::publish::Publisher;

/// A queue past this many bytes is coalesced (§2.4).
pub(crate) const QUEUE_BUDGET: usize = 1 << 20;

/// An idle stream gets an SSE comment at least this often (protocol §5.4).
pub(crate) const HEARTBEAT: Duration = Duration::from_secs(15);

/// A log position `(n, h_n)` (protocol S3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Head {
    pub n: usize,
    pub hash: String,
}

impl Head {
    /// `(0, h_0)`.
    pub(crate) fn zero() -> Self {
        Self {
            n: 0,
            hash: ChainHash::ZERO.wire(),
        }
    }

    /// The head of a log holding `entries`.
    pub(crate) fn of(entries: &[Arc<LogEntry>]) -> Self {
        entries.last().map_or_else(Self::zero, |entry| Self {
            n: entries.len(),
            hash: entry.hash.clone(),
        })
    }
}

/// One scope a stream follows (protocol §5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Subscribed {
    pub scope: Scope,
    /// The head of the log the client holds; `(0, h_0)` without `have`.
    pub have: Head,
    /// The working directory to load an unheld session with.
    pub cwd: Option<String>,
}

/// The scopes of `GET /api/v2/stream?scope=…&have=…&cwd=…` (protocol §5.1).
pub(crate) fn parse_stream_query(query: &str) -> Result<Vec<Subscribed>, ProtocolError> {
    let _ = query;
    todo!("parse a stream query")
}

pub(crate) type SubscriptionId = u64;

/// The open subscriptions and their queues.
#[derive(Default)]
pub(crate) struct Hub {}

impl Hub {
    /// Registers a subscription to `scopes` and queues the alignment of each
    /// scope `publisher` publishes; a scope not published yet is aligned when
    /// it is. Called under the bridge lock.
    pub(crate) fn subscribe(
        &mut self,
        publisher: &Publisher,
        scopes: &[Subscribed],
    ) -> Subscription {
        let _ = (publisher, scopes);
        todo!("subscribe")
    }

    /// Queues one commit's events, which `publisher` has just published, for
    /// every open subscription.
    pub(crate) fn dispatch(&mut self, publisher: &Publisher, events: &[Event]) {
        let _ = (publisher, events);
        todo!("dispatch a commit")
    }

    /// The open subscriptions that follow `session/<session_id>`: the
    /// session's subscribers (S6).
    pub(crate) fn subscribers(&self, session_id: &str) -> usize {
        let _ = session_id;
        todo!("count subscribers")
    }
}

/// One stream's side of a subscription. Dropping it closes the subscription.
pub(crate) struct Subscription {}

impl Subscription {
    pub(crate) fn id(&self) -> SubscriptionId {
        todo!("the subscription's ID")
    }

    /// Takes the next queued event and hands it to the stream's ordered
    /// output: the only step that advances a baseline.
    pub(crate) fn try_next(&self) -> Option<Event> {
        todo!("dequeue")
    }

    /// Waits for the next event; `None` once the Hub closed the stream.
    pub(crate) async fn next(&self) -> Option<Event> {
        todo!("wait for the next event")
    }

    /// The baseline of a session scope: the log position last handed to the
    /// output, or the client's `have`.
    pub(crate) fn baseline(&self, scope: &str) -> Option<Head> {
        let _ = scope;
        todo!("the baseline")
    }

    /// The queued tail of a session scope: the target of its last queued
    /// event, or the baseline when nothing of it is queued.
    pub(crate) fn tail(&self, scope: &str) -> Option<Head> {
        let _ = scope;
        todo!("the queued tail")
    }

    /// The bytes of the queued events.
    pub(crate) fn queued_bytes(&self) -> usize {
        todo!("the queued bytes")
    }

    /// How many times the queue was coalesced.
    pub(crate) fn coalesced(&self) -> usize {
        todo!("the coalesce count")
    }
}

/// The published documents and the Hub: bridge memory shared by the actor's
/// generations. The commit step is their only writer.
#[derive(Default)]
pub(crate) struct Published {
    pub publisher: Publisher,
    pub hub: Hub,
}

#[derive(Clone, Default)]
pub(crate) struct Publication(Arc<Mutex<Published>>);

impl Publication {
    pub(crate) fn lock(&self) -> MutexGuard<'_, Published> {
        self.0.lock().expect("publication poisoned")
    }
}
