//! The stream side of the convergence gate (docs/state-sync.md §4.1): a client
//! that takes a subscription's events one at a time, applies them at the wire
//! level, and checks each one against the published documents and the
//! subscription's baseline and queued tail (I2, I3, I4).
//!
//! The published documents are only compared with; a client never repairs its
//! copy from them. An event that does not apply fails the test.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::Value;

use super::Op;
use super::canon::{ChainHash, LogEntry};
use super::diff;
use super::documents::Scope;
use super::hub::{Head, Subscribed, Subscription};
use super::publish::Document;

/// Every published document after one commit.
pub(crate) type Documents = BTreeMap<String, Document>;

pub(crate) fn log_values(entries: &[Arc<LogEntry>]) -> Vec<Value> {
    serde_json::to_value(entries)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

/// A client's copy of one scope, built from wire events only.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Replica {
    pub live: Value,
    pub log: Vec<Value>,
}

impl Replica {
    pub(crate) fn hash_at(&self, index: usize) -> Option<String> {
        if index == 0 {
            return Some(ChainHash::ZERO.wire());
        }
        self.log
            .get(index - 1)
            .and_then(|entry| entry["hash"].as_str())
            .map(String::from)
    }

    pub(crate) fn head(&self) -> Head {
        Head {
            n: self.log.len(),
            hash: self.hash_at(self.log.len()).unwrap(),
        }
    }

    pub(crate) fn apply(&mut self, event: &Value) -> Result<(), String> {
        if let Some(log) = event.get("log") {
            let from = log["from"].as_u64().ok_or("log.from")? as usize;
            if from > self.log.len() || self.hash_at(from).as_deref() != log["prev"].as_str() {
                return Err(format!("log change does not follow the replica: {event}"));
            }
            self.log.truncate(from);
            self.log
                .extend(log["entries"].as_array().ok_or("entries")?.iter().cloned());
        }
        if let Some(live) = event.get("live") {
            if let Some(replace) = live.get("replace") {
                self.live = replace.clone();
            } else {
                let ops: Vec<Op> = serde_json::from_value(live["patch"].clone())
                    .map_err(|error| format!("patch: {error}"))?;
                for op in &ops {
                    diff::apply(&mut self.live, op)
                        .map_err(|error| format!("{op:?} does not apply: {error:?}"))?;
                }
            }
        }
        Ok(())
    }
}

fn is_session(scope: &str) -> bool {
    matches!(Scope::parse(scope), Some(Scope::Session(_)))
}

/// A client of one stream at a time: its replicas outlive a stream, as a
/// client keeps its copies when it subscribes again.
#[derive(Debug, Default)]
pub(crate) struct Reader {
    pub replicas: BTreeMap<String, Replica>,
    /// Scopes that ended, with their `end`.
    pub ended: BTreeMap<String, Value>,
    /// Every event taken, in order, across streams.
    pub received: Vec<Value>,
    /// Every scope's replica after each of its events.
    pub steps: Vec<(String, Replica)>,
    scopes: Vec<String>,
    /// Scopes aligned on the current stream.
    aligned: BTreeSet<String>,
    /// Per scope, the index of the published documents it last matched.
    cursor: BTreeMap<String, usize>,
}

impl Reader {
    pub(crate) fn new(scopes: &[&str]) -> Self {
        Self {
            scopes: scopes.iter().map(|scope| scope.to_string()).collect(),
            ..Self::default()
        }
    }

    /// The client already holds `log` for `scope`, unconfirmed.
    pub(crate) fn cache(mut self, scope: &str, log: Vec<Value>) -> Self {
        self.replicas.entry(scope.to_string()).or_default().log = log;
        self
    }

    pub(crate) fn head(&self, scope: &str) -> Head {
        self.replicas
            .get(scope)
            .map_or_else(Head::zero, Replica::head)
    }

    pub(crate) fn live(&self, scope: &str) -> Value {
        self.replicas
            .get(scope)
            .map_or(Value::Null, |replica| replica.live.clone())
    }

    pub(crate) fn log(&self, scope: &str) -> Vec<Value> {
        self.replicas
            .get(scope)
            .map_or_else(Vec::new, |replica| replica.log.clone())
    }

    /// The events `scope` received, in order.
    pub(crate) fn events_of(&self, scope: &str) -> Vec<Value> {
        self.received
            .iter()
            .filter(|event| event["scope"] == scope)
            .cloned()
            .collect()
    }

    /// The request of a new stream: every scope that has not ended, with the
    /// head the client holds now (S3).
    pub(crate) fn request(&mut self, cwd: Option<&str>) -> Vec<Subscribed> {
        self.aligned.clear();
        self.scopes
            .iter()
            .filter(|scope| !self.ended.contains_key(*scope))
            .map(|scope| Subscribed {
                scope: Scope::parse(scope).unwrap(),
                have: if is_session(scope) {
                    self.head(scope)
                } else {
                    Head::zero()
                },
                cwd: cwd.filter(|_| is_session(scope)).map(String::from),
            })
            .collect()
    }

    /// Takes one event, if one is queued, and checks it.
    pub(crate) fn take(
        &mut self,
        subscription: &Subscription,
        timeline: &[Documents],
    ) -> Option<Value> {
        self.check_baselines(subscription);
        let event = serde_json::to_value(subscription.try_next()?).unwrap();
        self.check(&event, timeline);
        self.check_baselines(subscription);
        Some(event)
    }

    /// Takes every queued event; the queue is then empty and every tail is
    /// its baseline.
    pub(crate) fn drain(
        &mut self,
        subscription: &Subscription,
        timeline: &[Documents],
    ) -> Vec<Value> {
        let mut events = Vec::new();
        while let Some(event) = self.take(subscription, timeline) {
            events.push(event);
        }
        assert_eq!(subscription.queued_bytes(), 0, "a drained queue is empty");
        for scope in self.scopes.iter().filter(|scope| is_session(scope)) {
            if self.ended.contains_key(scope) {
                continue;
            }
            assert_eq!(
                subscription.tail(scope),
                subscription.baseline(scope),
                "the tail of {scope} is its baseline once nothing is queued"
            );
        }
        events
    }

    /// The baseline of every followed session scope is the head this client
    /// holds: only an event handed to the output advances it.
    pub(crate) fn check_baselines(&self, subscription: &Subscription) {
        for scope in self.scopes.iter().filter(|scope| is_session(scope)) {
            if self.ended.contains_key(scope) {
                continue;
            }
            assert_eq!(
                subscription.baseline(scope),
                Some(self.head(scope)),
                "the baseline of {scope} is the head the client holds"
            );
        }
    }

    fn check(&mut self, event: &Value, timeline: &[Documents]) {
        let scope = event["scope"].as_str().expect("an event names its scope");
        assert!(
            self.scopes.iter().any(|followed| followed == scope),
            "an event of {scope}, which the stream does not follow: {event}"
        );
        assert!(
            !self.ended.contains_key(scope),
            "an event of {scope} after its end: {event}"
        );
        assert!(event.get("id").is_none(), "events carry no ID: {event}");
        self.received.push(event.clone());
        let start = self.cursor.get(scope).copied().unwrap_or(0);
        if let Some(end) = event.get("end") {
            let at = (start..timeline.len())
                .find(|index| !timeline[*index].contains_key(scope))
                .unwrap_or_else(|| panic!("{scope} ended but stayed published: {event}"));
            self.cursor.insert(scope.to_string(), at);
            self.ended.insert(scope.to_string(), end.clone());
            self.replicas.remove(scope);
            return;
        }
        if self.aligned.insert(scope.to_string()) {
            assert!(
                event["live"].get("replace").is_some(),
                "the first event of {scope} on a stream is its alignment: {event}"
            );
        }
        let replica = self.replicas.entry(scope.to_string()).or_default();
        let before = replica.log.clone();
        replica
            .apply(event)
            .unwrap_or_else(|error| panic!("{scope}: {error}"));
        if replica.live["op"].get("process").is_some() {
            assert_eq!(
                replica.live["op"]["status"], "running",
                "only a running prompt shows a process: {event}"
            );
        }
        let replica = replica.clone();
        self.steps.push((scope.to_string(), replica.clone()));
        let at = (start..timeline.len())
            .find(|index| {
                timeline[*index].get(scope).is_some_and(|document| {
                    document.live == replica.live
                        && match &document.log {
                            Some(log) => log_values(log) == replica.log,
                            None => event.get("log").is_none() && replica.log == before,
                        }
                })
            })
            .unwrap_or_else(|| {
                panic!(
                    "after {event}, {scope} is no document the bridge published from the \
                     last one it matched on; replica: {replica:#?}"
                )
            });
        self.cursor.insert(scope.to_string(), at);
    }

    /// Every followed scope that has not ended equals its published document,
    /// live and known log; a scope whose log is unknown keeps the client's.
    pub(crate) fn converged(&self, documents: &Documents) {
        for scope in &self.scopes {
            if self.ended.contains_key(scope) {
                assert!(!documents.contains_key(scope), "{scope} ended");
                continue;
            }
            let document = documents
                .get(scope)
                .unwrap_or_else(|| panic!("{scope} is not published"));
            let replica = &self.replicas[scope];
            assert_eq!(replica.live, document.live, "live of {scope}");
            if let Some(log) = &document.log {
                assert_eq!(replica.log, log_values(log), "log of {scope}");
            }
        }
    }
}
