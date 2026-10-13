//! The Hub (plan §2.3): delivery of the log to stream subscribers. It owns
//! no bridge state; the commit step hands it each new suffix, and a
//! subscription's cut is taken under the same bridge lock, so the cut is
//! atomic with every transition.
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::log::{Change, Log, LogEntry};
use super::{EndReason, Entry, GLOBAL, GlobalState, Position, ScopeState};

/// Queued bytes past which a lagging subscriber's queue collapses into one
/// `reset` or `snapshot` per scope (protocol §4, "Lag").
pub(crate) const DEFAULT_SUBSCRIBER_BUDGET: usize = 1024 * 1024;

/// One subscribed scope, as the stream request names it (protocol §4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScopeRequest {
    Global,
    Session {
        session_id: String,
        epoch: String,
        incarnation: u64,
    },
}

impl ScopeRequest {
    /// Parses `global` or `session/<id>@<epoch>:<incarnation>`.
    pub(crate) fn parse(scope: &str) -> Option<Self> {
        if scope == GLOBAL {
            return Some(Self::Global);
        }
        let (session_id, owner) = scope.strip_prefix("session/")?.rsplit_once('@')?;
        let (epoch, incarnation) = owner.rsplit_once(':')?;
        let incarnation = incarnation
            .parse()
            .ok()
            .filter(|incarnation| *incarnation > 0)?;
        (!session_id.is_empty() && !epoch.is_empty()).then(|| Self::Session {
            session_id: session_id.to_string(),
            epoch: epoch.to_string(),
            incarnation,
        })
    }

    /// The scope its entries carry.
    pub(crate) fn scope(&self) -> String {
        match self {
            Self::Global => GLOBAL.to_string(),
            Self::Session { session_id, .. } => super::session_scope(session_id),
        }
    }
}

/// Parses an SSE `Last-Event-ID` of the form `<epoch>:<seq>`.
pub(crate) fn parse_event_id(id: &str) -> Option<Position> {
    let (epoch, seq) = id.rsplit_once(':')?;
    Some(Position {
        epoch: epoch.to_string(),
        seq: seq.parse().ok()?,
    })
}

/// What the Hub needs from the bridge besides the log: which owners are held,
/// why retired ones ended, and the current `global` document.
pub(crate) trait Documents {
    fn held_incarnation(&self, session_id: &str) -> Option<u64>;
    fn retired(&self, session_id: &str, incarnation: u64) -> Option<EndReason>;
    fn global(&self) -> GlobalState;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `snapshot`, `reset` or `ops`: replaceable by a later resync.
    Data,
    /// `end`: always delivered.
    End,
    Position,
}

#[derive(Debug)]
struct Queued {
    scope: Option<String>,
    seq: u64,
    kind: Kind,
    data: String,
}

/// One SSE message: its JSON `data`, and its `id`, empty when a later
/// message shares its position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Message {
    pub id: String,
    pub data: String,
}

#[derive(Debug, Default)]
struct QueueState {
    items: VecDeque<Queued>,
    bytes: usize,
    finished: bool,
}

/// A subscriber's queue, shared between the Hub and the stream's consumer.
#[derive(Debug)]
pub(crate) struct StreamQueue {
    epoch: String,
    state: std::sync::Mutex<QueueState>,
    ready: tokio::sync::Notify,
    closed: AtomicBool,
}

impl StreamQueue {
    fn new(epoch: &str) -> Self {
        Self {
            epoch: epoch.to_string(),
            state: std::sync::Mutex::default(),
            ready: tokio::sync::Notify::new(),
            closed: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The next message, or `None` once the epoch ended and the queue drained.
    pub(crate) async fn next(&self) -> Option<Message> {
        loop {
            let ready = self.ready.notified();
            if let Some(message) = self.try_next() {
                return Some(message);
            }
            if self.lock().finished {
                return None;
            }
            ready.await;
        }
    }

    pub(crate) fn try_next(&self) -> Option<Message> {
        let mut state = self.lock();
        let item = state.items.pop_front()?;
        state.bytes -= item.data.len();
        // Entries that share a position carry the SSE id only on the last of
        // them, so `Last-Event-ID` passes a position once all of it arrived.
        // A commit's batch enters the queue atomically, so the rest of a
        // position is queued whenever its first entry is.
        let id = if state.items.front().is_some_and(|next| next.seq == item.seq) {
            String::new()
        } else {
            format!("{}:{}", self.epoch, item.seq)
        };
        Some(Message {
            id,
            data: item.data,
        })
    }

    /// The consumer went away; the Hub drops the subscriber on its next pass.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    pub(crate) fn queued_bytes(&self) -> usize {
        self.lock().bytes
    }
}

#[derive(Debug)]
struct ScopeCursor {
    request: ScopeRequest,
    /// The `seq` of the last entry delivered for this scope on this stream;
    /// `None` after a resume until the first one, which carries its log `prev`.
    last: Option<u64>,
    ended: bool,
}

#[derive(Debug)]
struct Subscriber {
    queue: Arc<StreamQueue>,
    scopes: BTreeMap<String, ScopeCursor>,
    /// The `seq` of the last message queued.
    sent: u64,
}

#[derive(Debug)]
pub(crate) struct Hub {
    epoch: String,
    /// The log position delivered to every subscriber.
    cursor: u64,
    budget: usize,
    next_id: u64,
    subscribers: BTreeMap<u64, Subscriber>,
    finished: bool,
}

impl Hub {
    pub(crate) fn new(epoch: &str) -> Self {
        Self::with_budget(epoch, DEFAULT_SUBSCRIBER_BUDGET)
    }

    pub(crate) fn with_budget(epoch: &str, budget: usize) -> Self {
        Self {
            epoch: epoch.to_string(),
            cursor: 0,
            budget,
            next_id: 0,
            subscribers: BTreeMap::new(),
            finished: false,
        }
    }

    pub(crate) fn epoch(&self) -> &str {
        &self.epoch
    }

    #[cfg(test)]
    pub(crate) fn set_budget(&mut self, budget: usize) {
        self.budget = budget;
    }

    pub(crate) fn subscribers(&self) -> usize {
        self.subscribers.len()
    }

    fn at(&self, seq: u64) -> Position {
        Position {
            epoch: self.epoch.clone(),
            seq,
        }
    }

    /// Registers a stream and queues its cut: `global` as an inline snapshot,
    /// each held session owner as a `reset`, every other owner as `end`. A
    /// `Last-Event-ID` in this epoch whose suffix the log still holds resumes
    /// from it instead. The caller holds the bridge lock, so the cut and the
    /// log position agree. After the epoch finished, the cut is the stream's
    /// last batch.
    pub(crate) fn subscribe(
        &mut self,
        log: &Log,
        documents: &impl Documents,
        scopes: Vec<ScopeRequest>,
        last_event: Option<&Position>,
    ) -> Arc<StreamQueue> {
        debug_assert_eq!(self.cursor, log.seq(), "every commit delivers its suffix");
        let now = log.seq();
        let queue = Arc::new(StreamQueue::new(&self.epoch));
        let mut subscriber = Subscriber {
            queue: queue.clone(),
            scopes: scopes
                .into_iter()
                .map(|request| {
                    (
                        request.scope(),
                        ScopeCursor {
                            request,
                            last: None,
                            ended: false,
                        },
                    )
                })
                .collect(),
            sent: 0,
        };
        let resume = last_event
            .filter(|position| position.epoch == self.epoch && position.seq <= now)
            .and_then(|position| log.after(position.seq).map(Iterator::collect::<Vec<_>>));
        let mut batch = Vec::new();
        let mut global = None;
        for (scope, cursor) in &mut subscriber.scopes {
            let suffix = resume.as_ref().map(|entries| {
                entries
                    .iter()
                    .copied()
                    .filter(|entry| &entry.scope == scope)
                    .collect::<Vec<_>>()
            });
            let resumable = |suffix: &[&LogEntry]| {
                suffix
                    .iter()
                    .all(|entry| entry.prev != 0 && matches!(entry.change, Change::Ops(_)))
            };
            // A resumed scope continues its log suffix, unless the suffix
            // ends or creates its document; it starts over then.
            let resumed =
                suffix.filter(|suffix| self.held(documents, &cursor.request) && resumable(suffix));
            match resumed {
                Some(suffix) => {
                    for entry in suffix {
                        batch.push(self.entry(cursor, entry));
                    }
                }
                None => batch.push(self.resync(cursor, documents, now, &mut global)),
            }
        }
        batch.sort_by_key(|item| item.seq);
        {
            let mut state = queue.lock();
            for item in batch {
                enqueue(&mut state, &mut subscriber.sent, item);
            }
            self.heartbeat(&mut state, &mut subscriber.sent, now);
            state.finished = self.finished;
        }
        queue.ready.notify_one();
        if !self.finished {
            self.next_id += 1;
            self.subscribers.insert(self.next_id, subscriber);
        }
        queue
    }

    /// Delivers every entry the log gained since the last pass. Runs at the
    /// end of each commit step. `released` names the scopes whose process
    /// content this commit released (S6): their undelivered entries leave
    /// every queue, and the scope starts over unless it ends.
    pub(crate) fn deliver(&mut self, log: &Log, documents: &impl Documents, released: &[String]) {
        let now = log.seq();
        if now == self.cursor && self.subscribers.is_empty() {
            return;
        }
        self.subscribers
            .retain(|_, subscriber| !subscriber.queue.closed.load(Ordering::Acquire));
        let entries = log.after(self.cursor).map(Iterator::collect::<Vec<_>>);
        self.cursor = now;
        let mut subscribers = std::mem::take(&mut self.subscribers);
        let mut global = None;
        for subscriber in subscribers.values_mut() {
            let Subscriber {
                queue,
                scopes,
                sent,
            } = subscriber;
            let mut state = queue.lock();
            // Scopes that start over at `now` instead of taking this batch.
            let mut restart = std::collections::BTreeSet::new();
            for scope in released {
                if !scopes.get(scope).is_some_and(|cursor| !cursor.ended) {
                    continue;
                }
                let before = state.items.len();
                state.items.retain(|item| {
                    item.kind != Kind::Data || item.scope.as_deref() != Some(scope.as_str())
                });
                let ends = entries.as_ref().is_some_and(|entries| {
                    entries.iter().any(|entry| {
                        &entry.scope == scope && matches!(entry.change, Change::End(_))
                    })
                });
                if state.items.len() != before && !ends {
                    restart.insert(scope.clone());
                }
            }
            match &entries {
                Some(entries) => {
                    for entry in entries {
                        if restart.contains(&entry.scope) {
                            continue;
                        }
                        let Some(cursor) = scopes.get_mut(&entry.scope) else {
                            continue;
                        };
                        if cursor.ended {
                            continue;
                        }
                        let item = self.entry(cursor, entry);
                        enqueue(&mut state, sent, item);
                    }
                }
                // The suffix was compacted in the commit that appended it:
                // every scope starts over at the current position.
                None => restart.extend(scopes.keys().cloned()),
            }
            if state.bytes > self.budget {
                // A lagging subscriber: its queued data collapses into one
                // resync per scope. `end` entries stay.
                let mut kept = VecDeque::new();
                for item in std::mem::take(&mut state.items) {
                    match item.kind {
                        Kind::End => kept.push_back(item),
                        Kind::Data => {
                            restart.insert(item.scope.clone().expect("data has a scope"));
                        }
                        Kind::Position => {}
                    }
                }
                state.items = kept;
            }
            for (scope, cursor) in scopes.iter_mut() {
                if restart.contains(scope) && !cursor.ended {
                    let item = self.resync(cursor, documents, now, &mut global);
                    enqueue(&mut state, sent, item);
                }
            }
            state.bytes = state.items.iter().map(|item| item.data.len()).sum();
            self.heartbeat(&mut state, sent, now);
            drop(state);
            queue.ready.notify_one();
        }
        self.subscribers = subscribers;
    }

    /// The epoch ended: every stream drains its queue and closes, and a
    /// later subscription gets only its cut.
    pub(crate) fn finish(&mut self) {
        self.finished = true;
        for subscriber in std::mem::take(&mut self.subscribers).into_values() {
            subscriber.queue.lock().finished = true;
            subscriber.queue.ready.notify_one();
        }
    }

    fn held(&self, documents: &impl Documents, request: &ScopeRequest) -> bool {
        match request {
            ScopeRequest::Global => true,
            ScopeRequest::Session {
                session_id,
                epoch,
                incarnation,
            } => {
                *epoch == self.epoch && documents.held_incarnation(session_id) == Some(*incarnation)
            }
        }
    }

    fn entry(&self, cursor: &mut ScopeCursor, entry: &LogEntry) -> Queued {
        let prev = cursor.last.unwrap_or(entry.prev);
        cursor.last = Some(entry.seq);
        let kind = match entry.change {
            Change::Ops(_) => Kind::Data,
            Change::End(_) => {
                cursor.ended = true;
                Kind::End
            }
        };
        queued(
            Some(entry.scope.clone()),
            entry.seq,
            kind,
            &entry.stream_entry(&self.epoch, prev),
        )
    }

    /// A scope's start over at `now`: a `global` snapshot, a `reset` for a
    /// held owner, or `end` for any other owner.
    fn resync(
        &self,
        cursor: &mut ScopeCursor,
        documents: &impl Documents,
        now: u64,
        global: &mut Option<GlobalState>,
    ) -> Queued {
        let scope = cursor.request.scope();
        cursor.last = Some(now);
        let entry = match &cursor.request {
            ScopeRequest::Global => Entry::Snapshot {
                scope: scope.clone(),
                at: self.at(now),
                state: ScopeState::Global(Box::new(
                    global.get_or_insert_with(|| documents.global()).clone(),
                )),
            },
            request if self.held(documents, request) => Entry::Reset {
                scope: scope.clone(),
                at: self.at(now),
            },
            ScopeRequest::Session {
                session_id,
                epoch,
                incarnation,
            } => {
                cursor.ended = true;
                let retired = documents.retired(session_id, *incarnation);
                let reason = match retired {
                    _ if *epoch != self.epoch => EndReason::Replaced,
                    Some(
                        reason @ (EndReason::Closed | EndReason::Deleted | EndReason::Unobserved),
                    ) => reason,
                    _ if documents.held_incarnation(session_id).is_some() => EndReason::Replaced,
                    retired => retired.unwrap_or(EndReason::Gone),
                };
                Entry::End {
                    scope: scope.clone(),
                    at: self.at(now),
                    reason,
                }
            }
        };
        let kind = if cursor.ended { Kind::End } else { Kind::Data };
        queued(Some(scope), now, kind, &entry)
    }

    /// A coalesced `position` when the log moved past the last message.
    fn heartbeat(&self, state: &mut QueueState, sent: &mut u64, now: u64) {
        if *sent >= now {
            return;
        }
        if state
            .items
            .back()
            .is_some_and(|last| last.kind == Kind::Position)
        {
            let last = state.items.pop_back().expect("checked above");
            state.bytes -= last.data.len();
        }
        let item = queued(
            None,
            now,
            Kind::Position,
            &Entry::Position { at: self.at(now) },
        );
        enqueue(state, sent, item);
    }
}

fn enqueue(state: &mut QueueState, sent: &mut u64, item: Queued) {
    *sent = (*sent).max(item.seq);
    state.bytes += item.data.len();
    state.items.push_back(item);
}

fn queued(scope: Option<String>, seq: u64, kind: Kind, entry: &Entry) -> Queued {
    Queued {
        scope,
        seq,
        kind,
        data: serde_json::to_string(entry).expect("entries serialize"),
    }
}
