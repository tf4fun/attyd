//! A replica of the stream protocol (protocol §4, §4.1), for the SSE
//! convergence gate. It follows the replica rules strictly: every scope
//! starts pending, a `reset` waits for a fetched snapshot, covered entries
//! are ignored before `prev` is checked, and nothing follows `end`. A
//! continuity break, an op that does not apply, or an entry the protocol
//! forbids is an error, never repaired.
use std::collections::BTreeMap;

use serde_json::Value;

use super::diff::apply;
use super::{EndReason, Op, Position};

#[derive(Debug, Default)]
struct ScopeReplica {
    document: Option<Value>,
    /// The last `seq` applied: a snapshot's `at.seq` or the last `ops`.
    installed: u64,
    /// `installed` is a snapshot position, so the next entry's `prev` may be
    /// any earlier delivered position (protocol §4, "Continuity").
    from_snapshot: bool,
    /// Waiting for a snapshot at or after this position.
    reset_at: Option<u64>,
    buffer: Vec<(u64, u64, Vec<Op>)>,
    ended: Option<EndReason>,
}

impl ScopeReplica {
    fn pending(&self) -> bool {
        self.ended.is_none() && (self.document.is_none() || self.reset_at.is_some())
    }

    fn apply(&mut self, scope: &str, seq: u64, prev: u64, ops: &[Op]) -> Result<(), String> {
        if seq <= self.installed {
            // Covered by the installed snapshot, or replayed by a resumed
            // stream after this replica applied it.
            return Ok(());
        }
        let continuous = prev == self.installed || (self.from_snapshot && prev <= self.installed);
        if !continuous {
            return Err(format!(
                "{scope}: entry {seq} has prev {prev}, but the replica installed {}{}",
                self.installed,
                if self.from_snapshot {
                    " (snapshot)"
                } else {
                    ""
                },
            ));
        }
        let document = self
            .document
            .as_mut()
            .expect("installed scopes hold a document");
        for op in ops {
            apply(document, op)
                .map_err(|error| format!("{scope}: entry {seq} does not apply: {error:?}"))?;
        }
        self.installed = seq;
        self.from_snapshot = false;
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct StreamReplica {
    epoch: Option<String>,
    /// The `at.seq` of the last entry read (protocol §4.1).
    pub(crate) received: u64,
    scopes: BTreeMap<String, ScopeReplica>,
    pub(crate) resets: usize,
    pub(crate) snapshots: usize,
    pub(crate) covered: usize,
    pub(crate) entries: usize,
}

impl StreamReplica {
    /// A replica for one stream; every subscribed scope starts pending.
    pub(crate) fn new<'a>(scopes: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            epoch: None,
            received: 0,
            scopes: scopes
                .into_iter()
                .map(|scope| (scope.to_string(), ScopeReplica::default()))
                .collect(),
            resets: 0,
            snapshots: 0,
            covered: 0,
            entries: 0,
        }
    }

    fn scope(&mut self, scope: &str) -> Result<&mut ScopeReplica, String> {
        self.scopes
            .get_mut(scope)
            .ok_or_else(|| format!("entry for unsubscribed scope {scope}"))
    }

    fn position(&mut self, at: &Position) -> Result<(), String> {
        match &self.epoch {
            Some(epoch) if *epoch != at.epoch => {
                return Err(format!("entry from epoch {} in epoch {epoch}", at.epoch));
            }
            Some(_) => {}
            None => self.epoch = Some(at.epoch.clone()),
        }
        if at.seq < self.received {
            return Err(format!("entry at {} after {}", at.seq, self.received));
        }
        self.received = at.seq;
        Ok(())
    }

    /// Reads one SSE `data` payload whose `id` is `id`.
    pub(crate) fn receive(&mut self, id: &str, data: &str) -> Result<(), String> {
        let entry: Value =
            serde_json::from_str(data).map_err(|error| format!("{data}: {error}"))?;
        let field = |name: &str| {
            entry
                .get(name)
                .cloned()
                .ok_or_else(|| format!("{data}: no {name}"))
        };
        let at: Position =
            serde_json::from_value(field("at")?).map_err(|error| format!("{data}: {error}"))?;
        self.entries += 1;
        // An entry without an id shares its position with the next one.
        if !id.is_empty() && id != format!("{}:{}", at.epoch, at.seq) {
            return Err(format!("SSE id {id} for an entry at {at:?}"));
        }
        self.position(&at)?;
        let kind = entry["type"].as_str().unwrap_or_default();
        if kind == "position" {
            return Ok(());
        }
        let scope = entry["scope"]
            .as_str()
            .ok_or_else(|| format!("{data}: no scope"))?
            .to_string();
        match kind {
            "snapshot" => self.install(&scope, &at, field("state")?)?,
            "reset" => {
                self.resets += 1;
                let replica = self.scope(&scope)?;
                if replica.ended.is_some() {
                    return Err(format!("{scope}: reset after end"));
                }
                replica.reset_at = Some(at.seq);
                replica.buffer.clear();
            }
            "ops" => {
                let prev = entry["prev"]
                    .as_u64()
                    .ok_or_else(|| format!("{data}: no prev"))?;
                let ops: Vec<Op> = serde_json::from_value(field("ops")?)
                    .map_err(|error| format!("{data}: {error}"))?;
                let replica = self.scope(&scope)?;
                if replica.ended.is_some() {
                    return Err(format!("{scope}: ops after end"));
                }
                if replica.pending() {
                    if replica.reset_at.is_none() {
                        return Err(format!(
                            "{scope}: ops before the scope's first snapshot or reset"
                        ));
                    }
                    replica.buffer.push((at.seq, prev, ops));
                } else {
                    let covered = at.seq <= replica.installed;
                    replica.apply(&scope, at.seq, prev, &ops)?;
                    self.covered += usize::from(covered);
                }
            }
            "end" => {
                let reason: EndReason = serde_json::from_value(field("reason")?)
                    .map_err(|error| format!("{data}: {error}"))?;
                let replica = self.scope(&scope)?;
                if replica.ended.is_some() {
                    return Err(format!("{scope}: a second end"));
                }
                *replica = ScopeReplica {
                    ended: Some(reason),
                    ..ScopeReplica::default()
                };
            }
            other => return Err(format!("{data}: unknown entry type {other}")),
        }
        Ok(())
    }

    /// A new stream resumes from `last_id` (none: from scratch). Its entries
    /// can repeat positions this replica already read (protocol §4.1).
    pub(crate) fn reconnect(&mut self, last_id: Option<&str>) {
        self.received = last_id
            .and_then(super::hub::parse_event_id)
            .filter(|position| self.epoch.as_ref() == Some(&position.epoch))
            .map_or(0, |position| position.seq);
    }

    /// Installs a snapshot, inline or fetched over HTTP, then drains the
    /// scope's buffer. Covered entries are ignored.
    pub(crate) fn install(
        &mut self,
        scope: &str,
        at: &Position,
        state: Value,
    ) -> Result<(), String> {
        if self.epoch.as_ref().is_some_and(|epoch| *epoch != at.epoch) {
            return Err(format!("{scope}: snapshot from epoch {}", at.epoch));
        }
        self.snapshots += 1;
        let replica = self.scope(scope)?;
        if replica.ended.is_some() {
            return Err(format!("{scope}: snapshot after end"));
        }
        if let Some(reset) = replica.reset_at
            && at.seq < reset
        {
            return Err(format!(
                "{scope}: snapshot at {} before its reset at {reset}",
                at.seq
            ));
        }
        replica.document = Some(state);
        replica.installed = at.seq;
        replica.from_snapshot = true;
        replica.reset_at = None;
        let mut covered = 0;
        for (seq, prev, ops) in std::mem::take(&mut replica.buffer) {
            covered += usize::from(seq <= at.seq);
            replica.apply(scope, seq, prev, &ops)?;
        }
        self.covered += covered;
        Ok(())
    }

    /// Session scopes waiting for `GET /api/v2/sessions/{id}`.
    pub(crate) fn fetches(&self) -> Vec<String> {
        self.scopes
            .iter()
            .filter(|(_, replica)| replica.reset_at.is_some() && replica.ended.is_none())
            .map(|(scope, _)| scope.clone())
            .collect()
    }

    /// `min(received, installed of every pending scope)` (protocol §4.1).
    pub(crate) fn applied(&self) -> u64 {
        self.scopes
            .values()
            .filter(|replica| replica.pending())
            .map(|replica| replica.installed)
            .fold(self.received, u64::min)
    }

    /// Some scope holds a snapshot later than `seq`.
    pub(crate) fn ahead_of(&self, seq: u64) -> bool {
        self.scopes.values().any(|replica| replica.installed > seq)
    }

    pub(crate) fn ended(&self, scope: &str) -> Option<EndReason> {
        self.scopes.get(scope).and_then(|replica| replica.ended)
    }

    /// The installed documents of scopes that have not ended.
    pub(crate) fn documents(&self) -> BTreeMap<String, Value> {
        self.scopes
            .iter()
            .filter(|(_, replica)| replica.ended.is_none())
            .filter_map(|(scope, replica)| Some((scope.clone(), replica.document.clone()?)))
            .collect()
    }

    /// Compares every subscribed, live scope with the fresh documents.
    pub(crate) fn diverges(&self, fresh: &BTreeMap<String, Value>) -> Option<String> {
        let mut divergent = Vec::new();
        for (scope, replica) in &self.scopes {
            if replica.ended.is_some() {
                continue;
            }
            let expected = fresh.get(scope);
            if replica.document.as_ref() != expected {
                divergent.push(format!(
                    "{scope}:\n  replica: {}\n  fresh:   {}",
                    replica
                        .document
                        .as_ref()
                        .map_or("<none>".into(), Value::to_string),
                    expected.map_or("<none>".into(), Value::to_string),
                ));
            }
        }
        (!divergent.is_empty())
            .then(|| format!("stream replica diverged\n{}", divergent.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn at(seq: u64) -> Position {
        Position {
            epoch: "e".into(),
            seq,
        }
    }

    fn send(replica: &mut StreamReplica, entry: Value) -> Result<(), String> {
        let seq = entry["at"]["seq"].as_u64().unwrap();
        replica.receive(&format!("e:{seq}"), &entry.to_string())
    }

    fn set(seq: u64) -> Value {
        json!([{ "op": "set", "path": "/n", "value": seq }])
    }

    #[test]
    fn a_snapshot_that_overtakes_its_entries_ignores_them_without_a_second_reset() {
        let scope = "session/s";
        let mut replica = StreamReplica::new([scope]);
        send(
            &mut replica,
            json!({ "type": "reset", "scope": scope, "at": at(10) }),
        )
        .unwrap();
        assert_eq!(replica.fetches(), [scope]);
        assert_eq!(replica.applied(), 0, "a reset never advances applied");
        // The HTTP snapshot at 20 arrives before SSE entries 11–20.
        replica.install(scope, &at(20), json!({ "n": 20 })).unwrap();
        for seq in 11..=20 {
            send(
                &mut replica,
                json!({ "type": "ops", "scope": scope, "at": at(seq), "prev": seq - 1, "ops": set(seq) }),
            )
            .unwrap();
        }
        assert_eq!((replica.covered, replica.resets), (10, 1));
        send(
            &mut replica,
            json!({ "type": "ops", "scope": scope, "at": at(21), "prev": 20, "ops": set(21) }),
        )
        .unwrap();
        assert_eq!(replica.documents()[scope], json!({ "n": 21 }));
        assert_eq!(replica.applied(), 21);
    }

    #[test]
    fn every_scope_starts_pending_until_its_snapshot_is_installed() {
        let mut replica = StreamReplica::new(["global", "session/s"]);
        send(
            &mut replica,
            json!({ "type": "reset", "scope": "session/s", "at": at(20) }),
        )
        .unwrap();
        replica.install("global", &at(20), json!({})).unwrap();
        assert_eq!(replica.applied(), 0);
        replica.install("session/s", &at(25), json!({})).unwrap();
        assert_eq!(replica.applied(), 20, "received bounds applied");
    }

    #[test]
    fn broken_continuity_and_entries_after_end_are_errors() {
        let mut replica = StreamReplica::new(["global"]);
        replica
            .install("global", &at(5), json!({ "n": 0 }))
            .unwrap();
        send(
            &mut replica,
            json!({ "type": "ops", "scope": "global", "at": at(6), "prev": 4, "ops": set(6) }),
        )
        .unwrap();
        assert!(
            send(
                &mut replica,
                json!({ "type": "ops", "scope": "global", "at": at(8), "prev": 7, "ops": set(8) }),
            )
            .is_err()
        );

        let mut replica = StreamReplica::new(["session/s"]);
        send(
            &mut replica,
            json!({ "type": "end", "scope": "session/s", "at": at(3), "reason": "closed" }),
        )
        .unwrap();
        assert_eq!(replica.ended("session/s"), Some(EndReason::Closed));
        assert_eq!(replica.applied(), 3, "an ended scope is not pending");
        assert!(
            send(
                &mut replica,
                json!({ "type": "reset", "scope": "session/s", "at": at(4) }),
            )
            .is_err()
        );
    }

    #[test]
    fn an_overtaken_reset_does_not_make_an_installed_scope_pending_again() {
        let scope = "session/s";
        let mut replica = StreamReplica::new([scope]);
        send(
            &mut replica,
            json!({ "type": "reset", "scope": scope, "at": at(10) }),
        )
        .unwrap();
        replica.install(scope, &at(20), json!({ "n": 20 })).unwrap();

        // Coalescing queued this reset while the HTTP snapshot was in flight;
        // the response already covers it, just as it covers queued ops.
        send(
            &mut replica,
            json!({ "type": "reset", "scope": scope, "at": at(15) }),
        )
        .unwrap();
        assert!(
            replica.fetches().is_empty(),
            "a covered reset must not request the same snapshot again"
        );
        assert_eq!(replica.documents()[scope], json!({ "n": 20 }));

        // A genuinely newer reset must still suspend application.
        send(
            &mut replica,
            json!({ "type": "reset", "scope": scope, "at": at(21) }),
        )
        .unwrap();
        assert_eq!(replica.fetches(), [scope]);
        assert_eq!(replica.applied(), 20);
    }

    #[test]
    fn an_overtaken_inline_snapshot_does_not_rewind_the_installed_document() {
        let mut replica = StreamReplica::new(["global"]);
        send(
            &mut replica,
            json!({ "type": "snapshot", "scope": "global", "at": at(10), "state": { "n": 10 } }),
        )
        .unwrap();
        replica
            .install("global", &at(20), json!({ "n": 20 }))
            .unwrap();
        send(
            &mut replica,
            json!({ "type": "snapshot", "scope": "global", "at": at(15), "state": { "n": 15 } }),
        )
        .unwrap();
        assert_eq!(
            replica.documents()["global"],
            json!({ "n": 20 }),
            "an inline snapshot covered by a newer HTTP snapshot is ignored"
        );

        send(
            &mut replica,
            json!({ "type": "snapshot", "scope": "global", "at": at(21), "state": { "n": 21 } }),
        )
        .unwrap();
        assert_eq!(replica.documents()["global"], json!({ "n": 21 }));
    }

    #[test]
    fn an_epoch_change_requires_a_new_stream_and_discards_old_pending_state() {
        fn initial_replica() -> StreamReplica {
            let mut replica = StreamReplica::new(["global", "session/s"]);
            send(
                &mut replica,
                json!({ "type": "snapshot", "scope": "global", "at": at(10), "state": { "epoch": "old" } }),
            )
            .unwrap();
            send(
                &mut replica,
                json!({ "type": "reset", "scope": "session/s", "at": at(10) }),
            )
            .unwrap();
            send(
                &mut replica,
                json!({ "type": "ops", "scope": "session/s", "at": at(11), "prev": 10, "ops": set(11) }),
            )
            .unwrap();
            replica
        }

        let next_epoch = json!({
            "type": "snapshot", "scope": "global",
            "at": { "epoch": "next", "seq": 1 }, "state": { "epoch": "next" },
        })
        .to_string();
        let mut same_stream = initial_replica();
        assert!(
            same_stream.receive("", &next_epoch).is_err(),
            "an epoch cannot change inside one stream"
        );

        let mut reconnected = initial_replica();
        reconnected.reconnect(Some("e:11"));
        // The following End at this position carries the batch's SSE id.
        reconnected.receive("", &next_epoch).unwrap();
        assert!(
            reconnected.fetches().is_empty(),
            "the old epoch's reset and buffered ops must not survive"
        );
        reconnected
            .receive(
                "next:1",
                &json!({
                    "type": "end", "scope": "session/s",
                    "at": { "epoch": "next", "seq": 1 }, "reason": "replaced",
                })
                .to_string(),
            )
            .unwrap();
        assert_eq!(reconnected.documents().len(), 1);
        assert_eq!(reconnected.documents()["global"], json!({ "epoch": "next" }));
        assert_eq!(reconnected.ended("session/s"), Some(EndReason::Replaced));
        assert_eq!(reconnected.applied(), 1);
    }
}
