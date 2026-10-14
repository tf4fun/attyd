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
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{Notify, watch};

use super::canon::{ChainHash, LogEntry};
use super::documents::{End, ErrorCode, Event, LiveChange, LogChange, ProtocolError, Scope};
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
    let invalid = |message: String| ProtocolError::new(ErrorCode::InvalidRequest, message);
    let mut scopes: Vec<Subscribed> = Vec::new();
    let mut haves = Vec::new();
    let mut cwds = Vec::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "scope" => {
                let scope = Scope::parse(&value)
                    .ok_or_else(|| invalid(format!("not a scope: {value:?}")))?;
                if scopes.iter().any(|followed| followed.scope == scope) {
                    return Err(invalid(format!("scope named twice: {value}")));
                }
                scopes.push(Subscribed {
                    scope,
                    have: Head::zero(),
                    cwd: None,
                });
            }
            "have" => haves.push(value.into_owned()),
            "cwd" => cwds.push(value.into_owned()),
            _ => return Err(invalid(format!("unknown stream parameter: {key}"))),
        }
    }
    if scopes.is_empty() {
        return Err(invalid("a stream follows at least one scope".into()));
    }
    let mut had = vec![false; scopes.len()];
    for have in haves {
        // `<scope>:<n>:<h_n>`; a session ID may itself hold a colon.
        let mut parts = have.rsplitn(3, ':');
        let (Some(hash), Some(n), Some(scope)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(invalid(format!("have is not <scope>:<n>:<hash>: {have:?}")));
        };
        let n: usize = n
            .parse()
            .map_err(|_| invalid(format!("have names no length: {have:?}")))?;
        if hash.is_empty() {
            return Err(invalid(format!("have names no hash: {have:?}")));
        }
        let index = scopes
            .iter()
            .position(|followed| {
                matches!(followed.scope, Scope::Session(_)) && followed.scope.to_string() == scope
            })
            .ok_or_else(|| invalid(format!("have of a session scope not followed: {have:?}")))?;
        if std::mem::replace(&mut had[index], true) {
            return Err(invalid(format!("have named twice for {scope}")));
        }
        scopes[index].have = Head {
            n,
            hash: hash.to_string(),
        };
    }
    for cwd in cwds {
        // `<scope>:<path>`: the longest followed session scope it starts with.
        let target = scopes
            .iter_mut()
            .filter(|followed| matches!(followed.scope, Scope::Session(_)))
            .map(|followed| (followed.scope.to_string(), followed))
            .filter(|(scope, _)| {
                cwd.strip_prefix(scope.as_str())
                    .is_some_and(|rest| rest.starts_with(':'))
            })
            .max_by_key(|(scope, _)| scope.len());
        let Some((scope, followed)) = target else {
            return Err(invalid(format!(
                "cwd of a session scope not followed: {cwd:?}"
            )));
        };
        let path = &cwd[scope.len() + 1..];
        if path.is_empty() {
            return Err(invalid(format!("cwd names no directory: {cwd:?}")));
        }
        if followed.cwd.replace(path.to_string()).is_some() {
            return Err(invalid(format!("cwd named twice for {scope}")));
        }
    }
    Ok(scopes)
}

pub(crate) type SubscriptionId = u64;

/// The open subscriptions and their queues.
pub(crate) struct Hub {
    subscriptions: Vec<Arc<Shared>>,
    next_id: SubscriptionId,
    /// The head of each published session's log after the last dispatch;
    /// `None` while it is unknown. A queued tail equal to it takes a commit's
    /// log change as it is.
    heads: BTreeMap<String, Option<Head>>,
    /// Counts closed subscriptions: the need of a session may have changed.
    closes: Arc<watch::Sender<u64>>,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            subscriptions: Vec::new(),
            next_id: 1,
            heads: BTreeMap::new(),
            closes: Arc::new(watch::channel(0).0),
        }
    }
}

impl Hub {
    /// Registers a subscription to `scopes` and queues the alignment of each
    /// scope `publisher` publishes; a scope not published yet is aligned when
    /// it is. Called under the bridge lock.
    pub(crate) fn subscribe(
        &mut self,
        publisher: &Publisher,
        scopes: &[Subscribed],
    ) -> Subscription {
        self.prune();
        let id = self.next_id;
        self.next_id += 1;
        let mut queue = Queue::default();
        for subscribed in scopes {
            queue.followed.insert(
                subscribed.scope.to_string(),
                Followed {
                    session: matches!(subscribed.scope, Scope::Session(_)),
                    baseline: subscribed.have.clone(),
                    tail: subscribed.have.clone(),
                    aligned: false,
                    ended: false,
                },
            );
        }
        for subscribed in scopes {
            queue.align(publisher, &subscribed.scope.to_string());
        }
        let shared = Arc::new(Shared {
            id,
            queue: Mutex::new(queue),
            ready: Notify::new(),
            closes: self.closes.clone(),
            #[cfg(test)]
            after_empty: Mutex::new(None),
        });
        shared.ready.notify_one();
        self.subscriptions.push(shared.clone());
        Subscription { shared }
    }

    /// Queues one commit's events, which `publisher` has just published, for
    /// every open subscription.
    pub(crate) fn dispatch(&mut self, publisher: &Publisher, events: &[Event]) {
        self.prune();
        let logs: BTreeMap<String, Option<&[Arc<LogEntry>]>> = publisher
            .session_logs()
            .map(|(session_id, log)| (Scope::Session(session_id.to_string()).to_string(), log))
            .collect();
        let mut shared: Vec<Option<(Arc<Event>, usize)>> = vec![None; events.len()];
        let mut as_is = |index: usize| {
            shared[index]
                .get_or_insert_with(|| {
                    let event = events[index].clone();
                    let bytes = size(&event);
                    (Arc::new(event), bytes)
                })
                .clone()
        };
        for subscription in &self.subscriptions {
            let mut queue = subscription.queue();
            let before = queue.items.len();
            for (index, event) in events.iter().enumerate() {
                let scope = event.scope();
                let Some(followed) = queue.followed.get(scope) else {
                    continue;
                };
                if followed.ended {
                    continue;
                }
                if !followed.aligned {
                    if !matches!(event, Event::End { .. }) {
                        queue.align(publisher, scope);
                        continue;
                    }
                } else if let Event::Log { log: _, live, .. } = event
                    && followed.session
                    && let Some(Some(entries)) = logs.get(scope)
                {
                    let target = Head::of(entries);
                    if self.heads.get(scope) == Some(&Some(followed.tail.clone())) {
                        let (event, bytes) = as_is(index);
                        queue.push(scope, event, bytes, Some(target));
                    } else {
                        // The tail is not the log last published: the change
                        // is rebuilt from it.
                        let event = Event::Log {
                            scope: scope.to_string(),
                            log: log_from(&followed.tail, entries),
                            live: live.clone(),
                        };
                        let bytes = size(&event);
                        queue.push(scope, Arc::new(event), bytes, Some(target));
                    }
                    continue;
                }
                if matches!(event, Event::End { .. }) {
                    queue.followed.get_mut(scope).expect("followed").ended = true;
                }
                let (event, bytes) = as_is(index);
                queue.push(scope, event, bytes, None);
            }
            // A session released without an end is aligned again if it is
            // published again.
            for (scope, followed) in &mut queue.followed {
                if followed.session && !followed.ended && !logs.contains_key(scope) {
                    followed.aligned = false;
                }
            }
            if queue.bytes > QUEUE_BUDGET {
                queue.coalesce(publisher);
            }
            if queue.items.len() != before || queue.coalesced_now {
                queue.coalesced_now = false;
                drop(queue);
                subscription.ready.notify_one();
            }
        }
        self.heads = logs
            .into_iter()
            .map(|(scope, log)| (scope, log.map(Head::of)))
            .collect();
    }

    /// Ends `scope` on one subscription only, as when the subscription cannot
    /// start its load (protocol §5.1).
    pub(crate) fn end(&mut self, id: SubscriptionId, scope: &str, end: End) {
        let Some(subscription) = self.subscriptions.iter().find(|shared| shared.id == id) else {
            return;
        };
        let mut queue = subscription.queue();
        let Some(followed) = queue.followed.get_mut(scope) else {
            return;
        };
        if followed.ended {
            return;
        }
        followed.ended = true;
        let event = Event::End {
            scope: scope.to_string(),
            end,
        };
        let bytes = size(&event);
        queue.push(scope, Arc::new(event), bytes, None);
        drop(queue);
        subscription.ready.notify_one();
    }

    /// The open subscriptions that follow `session/<session_id>`: the
    /// session's subscribers (S6).
    pub(crate) fn subscribers(&self, session_id: &str) -> usize {
        let scope = Scope::Session(session_id.to_string()).to_string();
        self.subscriptions
            .iter()
            .filter(|shared| {
                let queue = shared.queue();
                !queue.closed
                    && queue
                        .followed
                        .get(&scope)
                        .is_some_and(|followed| !followed.ended)
            })
            .count()
    }

    /// The sessions some open subscription follows, with the subscriptions
    /// that follow each and have not seen it published yet.
    pub(crate) fn followed_sessions(&self) -> BTreeMap<String, Vec<SubscriptionId>> {
        let mut sessions: BTreeMap<String, Vec<SubscriptionId>> = BTreeMap::new();
        for shared in &self.subscriptions {
            let queue = shared.queue();
            if queue.closed {
                continue;
            }
            for (scope, followed) in &queue.followed {
                if let Some(Scope::Session(session_id)) = Scope::parse(scope)
                    && !followed.ended
                {
                    let waiting = sessions.entry(session_id).or_default();
                    if !followed.aligned {
                        waiting.push(shared.id);
                    }
                }
            }
        }
        sessions
    }

    /// Changes whenever a subscription closes.
    pub(crate) fn closes(&self) -> watch::Receiver<u64> {
        self.closes.subscribe()
    }

    /// Closes every stream once it drained its queue, as on shutdown.
    pub(crate) fn close(&mut self) {
        for shared in std::mem::take(&mut self.subscriptions) {
            shared.queue().finished = true;
            shared.ready.notify_one();
        }
    }

    fn prune(&mut self) {
        self.subscriptions.retain(|shared| !shared.queue().closed);
    }
}

/// The log change that brings a client holding `base` to `entries`: the
/// suffix when `base` is a prefix, else the whole log.
fn log_from(base: &Head, entries: &[Arc<LogEntry>]) -> LogChange {
    if base.n <= entries.len() && Head::of(&entries[..base.n]) == *base {
        LogChange {
            from: base.n,
            prev: base.hash.clone(),
            entries: entries[base.n..].to_vec(),
        }
    } else {
        LogChange {
            from: 0,
            prev: ChainHash::ZERO.wire(),
            entries: entries.to_vec(),
        }
    }
}

/// The serialized length of an event, without serializing it into memory.
fn size(event: &Event) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, event).expect("events serialize");
    count.0
}

struct Shared {
    id: SubscriptionId,
    queue: Mutex<Queue>,
    ready: Notify,
    closes: Arc<watch::Sender<u64>>,
    // A one-shot pause owned by this subscription only, never a global hook.
    #[cfg(test)]
    after_empty: Mutex<Option<close_race_tests::Pause>>,
}

impl Shared {
    fn queue(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().expect("subscription poisoned")
    }
}

#[derive(Default)]
struct Queue {
    followed: BTreeMap<String, Followed>,
    items: VecDeque<Item>,
    bytes: usize,
    coalesced: usize,
    /// The queue was coalesced by this dispatch.
    coalesced_now: bool,
    /// The stream side dropped the subscription.
    closed: bool,
    /// The Hub closed the stream.
    finished: bool,
}

struct Followed {
    session: bool,
    /// The log position last handed to the output.
    baseline: Head,
    /// The target of the last queued log change, or the baseline.
    tail: Head,
    /// The scope's alignment was queued since it was last published.
    aligned: bool,
    ended: bool,
}

struct Item {
    scope: String,
    event: Arc<Event>,
    bytes: usize,
    /// The log position the event brings its scope to.
    target: Option<Head>,
}

impl Queue {
    fn push(&mut self, scope: &str, event: Arc<Event>, bytes: usize, target: Option<Head>) {
        if let Some(target) = &target
            && let Some(followed) = self.followed.get_mut(scope)
        {
            followed.tail = target.clone();
        }
        self.bytes += bytes;
        self.items.push_back(Item {
            scope: scope.to_string(),
            event,
            bytes,
            target,
        });
    }

    /// Queues the alignment of `scope` from its tail, if it is published: a
    /// full `live` replace, with the log from the tail.
    fn align(&mut self, publisher: &Publisher, scope: &str) {
        let Some(followed) = self.followed.get_mut(scope) else {
            return;
        };
        let Some(document) = publisher.document(scope) else {
            return;
        };
        followed.aligned = true;
        let live = LiveChange::Replace {
            replace: document.live,
        };
        let (event, target) = match (followed.session, document.log) {
            (true, Some(entries)) => (
                Event::Log {
                    scope: scope.to_string(),
                    log: log_from(&followed.tail, &entries),
                    live: Some(live),
                },
                Some(Head::of(&entries)),
            ),
            (true, None) => (
                Event::Live {
                    scope: scope.to_string(),
                    live,
                },
                None,
            ),
            (false, _) => (
                Event::Log {
                    scope: scope.to_string(),
                    log: LogChange {
                        from: 0,
                        prev: ChainHash::ZERO.wire(),
                        entries: Vec::new(),
                    },
                    live: Some(live),
                },
                None,
            ),
        };
        let bytes = size(&event);
        self.push(scope, Arc::new(event), bytes, target);
    }

    /// Replaces the queue, per scope, with the alignment of the current cut
    /// from the baseline; a queued end is kept (§2.4, I4).
    fn coalesce(&mut self, publisher: &Publisher) {
        let items = std::mem::take(&mut self.items);
        self.bytes = 0;
        self.coalesced += 1;
        self.coalesced_now = true;
        let mut scopes: Vec<String> = Vec::new();
        for item in items {
            if matches!(*item.event, Event::End { .. }) {
                self.bytes += item.bytes;
                self.items.push_back(item);
            } else if !scopes.contains(&item.scope) {
                scopes.push(item.scope);
            }
        }
        for scope in scopes {
            let Some(followed) = self.followed.get_mut(&scope) else {
                continue;
            };
            if followed.ended {
                continue;
            }
            followed.tail = followed.baseline.clone();
            followed.aligned = false;
            self.align(publisher, &scope);
        }
    }
}

/// One stream's side of a subscription. Dropping it closes the subscription.
pub(crate) struct Subscription {
    shared: Arc<Shared>,
}

impl Subscription {
    pub(crate) fn id(&self) -> SubscriptionId {
        self.shared.id
    }

    /// Takes the next queued event and hands it to the stream's ordered
    /// output: the only step that advances a baseline.
    pub(crate) fn try_next(&self) -> Option<Event> {
        let mut queue = self.shared.queue();
        let item = queue.items.pop_front()?;
        queue.bytes -= item.bytes;
        if let Some(target) = item.target
            && let Some(followed) = queue.followed.get_mut(&item.scope)
        {
            followed.baseline = target;
        }
        drop(queue);
        Some(Arc::unwrap_or_clone(item.event))
    }

    /// Waits for the next event; `None` once the Hub closed the stream.
    pub(crate) async fn next(&self) -> Option<Event> {
        loop {
            let ready = self.shared.ready.notified();
            if let Some(event) = self.try_next() {
                return Some(event);
            }
            #[cfg(test)]
            {
                // No queue lock is held while the test commits and closes.
                let pause = self.shared.after_empty.lock().unwrap().take();
                if let Some((reached, resume)) = pause {
                    let _ = reached.send(());
                    resume
                        .await
                        .expect("the close-race test releases its pause");
                }
            }
            {
                // One critical section decides: a stream ends only once it is
                // closed with nothing left to deliver.
                let queue = self.shared.queue();
                if !queue.items.is_empty() {
                    continue;
                }
                if queue.finished {
                    return None;
                }
            }
            ready.await;
        }
    }

    /// The baseline of a session scope: the log position last handed to the
    /// output, or the client's `have`.
    pub(crate) fn baseline(&self, scope: &str) -> Option<Head> {
        let queue = self.shared.queue();
        let followed = queue
            .followed
            .get(scope)
            .filter(|followed| followed.session)?;
        Some(followed.baseline.clone())
    }

    /// The queued tail of a session scope: the target of its last queued
    /// event, or the baseline when nothing of it is queued.
    pub(crate) fn tail(&self, scope: &str) -> Option<Head> {
        let queue = self.shared.queue();
        let followed = queue
            .followed
            .get(scope)
            .filter(|followed| followed.session)?;
        Some(followed.tail.clone())
    }

    /// The bytes of the queued events.
    pub(crate) fn queued_bytes(&self) -> usize {
        self.shared.queue().bytes
    }

    /// How many times the queue was coalesced.
    pub(crate) fn coalesced(&self) -> usize {
        self.shared.queue().coalesced
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        {
            let mut queue = self.shared.queue();
            queue.closed = true;
            queue.items.clear();
            queue.bytes = 0;
        }
        self.shared
            .closes
            .send_modify(|closes| *closes = closes.wrapping_add(1));
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

#[cfg(test)]
mod close_race_tests {
    use super::*;
    use crate::protocol::documents::{AgentInfo, AgentLaunch, AgentPhase};
    use crate::session_registry::SessionRegistry;
    use tokio::sync::oneshot;

    pub(super) type Pause = (oneshot::Sender<()>, oneshot::Receiver<()>);

    #[tokio::test]
    async fn review_next_delivers_the_last_commit_when_close_races_its_empty_check() {
        let mut registry = SessionRegistry::new("close-race");
        let mut publisher = Publisher::default();
        let mut agent = AgentInfo {
            phase: AgentPhase::Ready,
            error: None,
            launch: AgentLaunch::default(),
            initialize: None,
            authenticated_method_id: None,
        };
        publisher.commit(&mut registry, &agent, 0);
        let mut hub = Hub::default();
        let subscription = hub.subscribe(
            &publisher,
            &[Subscribed {
                scope: Scope::Global,
                have: Head::zero(),
                cwd: None,
            }],
        );
        subscription.next().await.expect("the initial alignment");
        assert_eq!(subscription.queued_bytes(), 0);

        let (reached_tx, reached) = oneshot::channel();
        let (resume, resume_rx) = oneshot::channel();
        *subscription.shared.after_empty.lock().unwrap() = Some((reached_tx, resume_rx));
        let consumer = tokio::spawn(async move {
            let last = subscription
                .next()
                .await
                .expect("the last commit must be delivered before EOF");
            assert!(
                subscription.next().await.is_none(),
                "EOF follows the final event"
            );
            last
        });
        tokio::time::timeout(Duration::from_secs(5), reached)
            .await
            .expect("next reaches its empty check")
            .unwrap();

        // next has seen an empty queue, but has not checked finished. Queue
        // a real published terminal change and close before it continues.
        agent.phase = AgentPhase::Stopped;
        let events = publisher.commit(&mut registry, &agent, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].scope(), "global");
        let expected = serde_json::to_value(&events[0]).unwrap();
        hub.dispatch(&publisher, &events);
        hub.close();
        resume.send(()).unwrap();

        let last = tokio::time::timeout(Duration::from_secs(5), consumer)
            .await
            .expect("the closed stream drains without waiting for another notification")
            .expect("the stream did not return EOF before the final event");
        assert_eq!(serde_json::to_value(last).unwrap(), expected);
    }
}
