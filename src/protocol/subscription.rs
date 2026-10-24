//! Subscriptions to a stream (docs/protocol.md §5.1): which scopes a stream
//! follows and what version of each a client already holds.
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use super::documents::{End, ErrorCode, ProtocolError, Scope};
pub(crate) use super::engine::Have;
use super::engine::{Engine, StreamEvent};

/// One scope a stream follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Subscribed {
    pub scope: Scope,
    /// The version of the replica the client holds; none without `have`.
    pub have: Option<Have>,
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
                    have: None,
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
    for have in haves {
        // `<scope>:<epoch>:<rev>`; a session ID may itself hold a colon, an
        // epoch (base64url) and a rev (a number) never do.
        let mut parts = have.rsplitn(3, ':');
        let (Some(rev), Some(epoch), Some(scope)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid(format!("have is not <scope>:<epoch>:<rev>: {have:?}")));
        };
        let rev: u64 = rev
            .parse()
            .map_err(|_| invalid(format!("have names no rev: {have:?}")))?;
        if epoch.is_empty() {
            return Err(invalid(format!("have names no epoch: {have:?}")));
        }
        let followed = scopes
            .iter_mut()
            .find(|followed| followed.scope.to_string() == scope)
            .ok_or_else(|| invalid(format!("have of a scope not followed: {have:?}")))?;
        if followed
            .have
            .replace(Have {
                epoch: epoch.to_string(),
                rev,
            })
            .is_some()
        {
            return Err(invalid(format!("have named twice for {scope}")));
        }
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

/// How often a stream that has nothing to send says it is alive.
pub(crate) const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(15);

/// A queue past this many bytes is replaced by snapshots (protocol §5.4).
pub(crate) const QUEUE_BUDGET: usize = 1 << 20;

/// What the stream of one scope holds on behalf of a subscriber.
struct ScopeState {
    /// The version of the replica the subscriber is known to hold: what the
    /// client said it held, then what was last written to its stream. An event
    /// still in the queue is not delivered and never advances it.
    written: Option<Have>,
    /// An `end` was queued: nothing follows it for this scope.
    ended: bool,
    /// The working directory to load the session with, if it is not held.
    cwd: Option<String>,
}

/// An event with its JSON, encoded once: every subscriber it is queued for shares
/// both, and the stream writes the JSON as it is.
#[derive(Clone)]
pub(crate) struct Encoded {
    event: Arc<StreamEvent>,
    json: Arc<str>,
}

impl Encoded {
    pub(crate) fn new(event: StreamEvent) -> Self {
        let json = serde_json::to_string(&event).expect("a protocol event serializes");
        Self {
            event: Arc::new(event),
            json: json.into(),
        }
    }

    #[cfg(test)]
    pub(crate) fn into_event(self) -> StreamEvent {
        Arc::unwrap_or_clone(self.event)
    }

    /// The event's JSON, as the stream writes it.
    pub(crate) fn json(&self) -> Arc<str> {
        Arc::clone(&self.json)
    }
}

impl From<&Encoded> for Encoded {
    fn from(encoded: &Encoded) -> Self {
        encoded.clone()
    }
}

impl From<&StreamEvent> for Encoded {
    fn from(event: &StreamEvent) -> Self {
        Self::new(event.clone())
    }
}

/// The events one stream has yet to write, per subscriber (protocol §5.1, §5.4).
///
/// It keeps no log: a subscriber that cannot keep up is not sent the events it
/// missed but a snapshot of each scope from the version it last was written.
pub(crate) struct Subscriber {
    scopes: BTreeMap<String, ScopeState>,
    queue: VecDeque<Encoded>,
    queued_bytes: usize,
}

impl Subscriber {
    pub(crate) fn new(follow: &[Subscribed]) -> Self {
        Self {
            scopes: follow
                .iter()
                .map(|subscribed| {
                    (
                        subscribed.scope.to_string(),
                        ScopeState {
                            written: subscribed.have.clone(),
                            ended: false,
                            cwd: subscribed.cwd.clone(),
                        },
                    )
                })
                .collect(),
            queue: VecDeque::new(),
            queued_bytes: 0,
        }
    }

    /// The scopes still followed (no `end` was queued for them) and the
    /// directory each was given for its load.
    pub(crate) fn followed(&self) -> impl Iterator<Item = (&str, Option<&str>)> {
        self.scopes
            .iter()
            .filter(|(_, state)| !state.ended)
            .map(|(scope, state)| (scope.as_str(), state.cwd.as_deref()))
    }

    fn push(&mut self, encoded: Encoded) {
        self.queued_bytes += encoded.json.len();
        self.queue.push_back(encoded);
    }

    /// Drops what is queued of `scope`.
    fn clear(&mut self, scope: &str) {
        let mut removed = 0;
        self.queue.retain(|queued| {
            let keep = queued.event.scope() != scope;
            if !keep {
                removed += queued.json.len();
            }
            keep
        });
        self.queued_bytes -= removed;
    }

    /// Queues the snapshot `engine` has for `scope`, from the version held.
    fn queue_snapshot(&mut self, scope: &str, engine: &Engine) {
        let held = self.scopes.get(scope).and_then(|state| state.written.clone());
        if let Some(snapshot) = engine.snapshot(scope, held.as_ref()) {
            self.push(Encoded::new(snapshot));
        }
    }

    /// Aligns every scope published now (the cut); a scope not published yet is
    /// aligned when it is (`rebuilt`).
    pub(crate) fn align(&mut self, engine: &Engine) {
        let scopes: Vec<String> = self.scopes.keys().cloned().collect();
        for scope in scopes {
            self.queue_snapshot(&scope, engine);
        }
    }

    /// A commit's event: queued for a subscriber that follows its scope.
    pub(crate) fn publish(&mut self, event: impl Into<Encoded>) {
        let encoded = event.into();
        if self
            .scopes
            .get(encoded.event.scope())
            .is_some_and(|state| !state.ended)
        {
            self.push(encoded);
        }
    }

    /// `scope` was published anew (a first publication or a new epoch): what was
    /// queued of it is replaced by the whole document.
    pub(crate) fn rebuilt(&mut self, scope: &str, engine: &Engine) {
        let Some(state) = self.scopes.get_mut(scope) else {
            return;
        };
        if state.ended {
            return;
        }
        state.written = None;
        self.clear(scope);
        self.queue_snapshot(scope, engine);
    }

    /// The bridge no longer holds `scope` and says nothing of why (a release, S6): what was
    /// queued of it is dropped, the subscriber keeps what it holds and goes on following it,
    /// and is sent the whole document when it is published again. The session is loaded
    /// again in the directory the bridge knew it in, whatever the subscriber supplied.
    pub(crate) fn released(&mut self, scope: &str, cwd: Option<&str>) {
        if let Some(state) = self.scopes.get_mut(scope)
            && !state.ended
        {
            if let Some(cwd) = cwd {
                state.cwd = Some(cwd.to_string());
            }
            self.clear(scope);
        }
    }

    /// `scope` is over: what was queued of it is replaced by its end, and
    /// nothing follows.
    pub(crate) fn end(&mut self, scope: &str, end: End) {
        let Some(state) = self.scopes.get_mut(scope) else {
            return;
        };
        if state.ended {
            return;
        }
        state.ended = true;
        self.clear(scope);
        self.push(Encoded::new(StreamEvent::End {
            scope: scope.to_string(),
            end,
        }));
    }

    /// The next event to write, as an event.
    #[cfg(test)]
    pub(crate) fn next(&mut self) -> Option<StreamEvent> {
        self.next_encoded().map(Encoded::into_event)
    }

    /// The next event to write; a snapshot or a version advances what the
    /// subscriber is known to hold.
    pub(crate) fn next_encoded(&mut self) -> Option<Encoded> {
        let queued = self.queue.pop_front()?;
        self.queued_bytes -= queued.json.len();
        if let Some(state) = self.scopes.get_mut(queued.event.scope()) {
            match &*queued.event {
                StreamEvent::Snapshot { snapshot, .. } => {
                    state.written = Some(Have {
                        epoch: snapshot.epoch.clone(),
                        rev: snapshot.rev,
                    });
                }
                StreamEvent::Ops { rev, .. } => {
                    if let Some(written) = &mut state.written {
                        written.rev = *rev;
                    }
                }
                StreamEvent::Checkpoint { .. } | StreamEvent::End { .. } => {}
            }
        }
        Some(queued)
    }

    /// The subscriber could not keep up: for each scope still followed, what is
    /// queued is replaced by a snapshot from the version last written. A scope
    /// with an `end` keeps only the end, and one that no longer has a document
    /// gets nothing.
    pub(crate) fn coalesce(&mut self, engine: &Engine) {
        let scopes: Vec<(String, bool)> = self
            .scopes
            .iter()
            .map(|(scope, state)| (scope.clone(), state.ended))
            .collect();
        for (scope, ended) in scopes {
            if ended {
                continue;
            }
            self.clear(&scope);
            self.queue_snapshot(&scope, engine);
        }
    }

    pub(crate) fn queued(&self) -> usize {
        self.queue.len()
    }

    pub(crate) fn queued_bytes(&self) -> usize {
        self.queued_bytes
    }

    pub(crate) fn over_budget(&self) -> bool {
        self.queued_bytes > QUEUE_BUDGET
    }
}

#[cfg(test)]
mod queue_tests;
#[cfg(test)]
mod tests;
