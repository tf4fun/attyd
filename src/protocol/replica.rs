//! The convergence gate (plan §4.1): replicas fed from the log must equal
//! fresh projections after every step (S2), whatever cut they start from
//! (S3) and however their queues are coalesced (S4).
use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::diff::apply;
use super::log::{Change, LogEntry};
use super::{AgentState, GLOBAL, Replication, project_global, project_session, session_scope};
use crate::session_registry::SessionRegistry;

/// Every document the bridge publishes now, by scope.
pub(crate) fn fresh(
    agent: &AgentState,
    catalog_revision: u64,
    registry: &SessionRegistry,
) -> BTreeMap<String, Value> {
    let mut documents = BTreeMap::from([(
        GLOBAL.to_string(),
        json!(project_global(agent, catalog_revision, registry)),
    )]);
    for session_id in registry.sessions.keys() {
        if let Some(session) = project_session(registry, session_id) {
            documents.insert(session_scope(session_id), json!(session));
        }
    }
    documents
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// Applies every entry in order.
    Follow,
    /// Lags, then replaces a random subset of each scope's queued entries
    /// with the latest snapshot.
    Coalesce,
    /// Lags, then reconnects: it resumes from the retained log, or starts
    /// over from snapshots when the log no longer holds its position.
    Cuts,
}

#[derive(Debug)]
struct Document {
    /// The `seq` of the last entry applied, or the snapshot's position.
    seq: u64,
    /// Entries at or before a snapshot's position are covered (protocol §4).
    covered: u64,
    value: Value,
}

#[derive(Debug)]
pub(crate) struct Replica {
    delivery: Delivery,
    received: u64,
    documents: BTreeMap<String, Document>,
    random: u64,
    pub(crate) snapshots: usize,
    pub(crate) applied_entries: usize,
}

impl Replica {
    pub(crate) fn new(delivery: Delivery, seed: u64) -> Self {
        Self {
            delivery,
            received: 0,
            documents: BTreeMap::new(),
            random: seed | 1,
            snapshots: 0,
            applied_entries: 0,
        }
    }

    fn next(&mut self) -> u64 {
        // xorshift64: deterministic and dependency-free.
        self.random ^= self.random << 13;
        self.random ^= self.random >> 7;
        self.random ^= self.random << 17;
        self.random
    }

    fn install(&mut self, scope: &str, replication: &Replication, fresh: &BTreeMap<String, Value>) {
        self.snapshots += 1;
        match (fresh.get(scope), replication.scope_seq(scope)) {
            (Some(value), Some(seq)) => {
                self.documents.insert(
                    scope.to_string(),
                    Document {
                        seq,
                        covered: replication.log().seq(),
                        value: value.clone(),
                    },
                );
            }
            _ => {
                self.documents.remove(scope);
            }
        }
    }

    fn restart(&mut self, replication: &Replication, fresh: &BTreeMap<String, Value>) {
        self.documents.clear();
        for scope in fresh.keys() {
            self.install(scope, replication, fresh);
        }
        self.received = replication.log().seq();
    }

    fn deliver(
        &mut self,
        entry: &LogEntry,
        replication: &Replication,
        fresh: &BTreeMap<String, Value>,
    ) {
        if self
            .documents
            .get(&entry.scope)
            .is_some_and(|document| entry.seq <= document.covered)
        {
            return;
        }
        match &entry.change {
            Change::End => {
                self.documents.remove(&entry.scope);
            }
            Change::Ops(ops) => {
                let mut document = match self.documents.remove(&entry.scope) {
                    Some(document) if document.seq == entry.prev => document,
                    None if entry.prev == 0 => Document {
                        seq: 0,
                        covered: 0,
                        value: json!({}),
                    },
                    // A continuity break resets the scope (protocol §4).
                    _ => return self.install(&entry.scope, replication, fresh),
                };
                if ops.iter().any(|op| apply(&mut document.value, op).is_err()) {
                    return self.install(&entry.scope, replication, fresh);
                }
                self.applied_entries += 1;
                document.seq = entry.seq;
                self.documents.insert(entry.scope.clone(), document);
            }
        }
    }

    /// Delivers whatever the replica's delivery mode takes from the log now.
    pub(crate) fn sync(&mut self, replication: &Replication, fresh: &BTreeMap<String, Value>) {
        if self.received == 0 && self.documents.is_empty() {
            return self.restart(replication, fresh);
        }
        if self.delivery != Delivery::Follow && self.next() % 3 != 0 {
            return; // lagging
        }
        let Some(entries) = replication.log().after(self.received) else {
            return self.restart(replication, fresh);
        };
        let entries = entries.collect::<Vec<_>>();
        match self.delivery {
            Delivery::Follow => {
                for entry in entries {
                    self.deliver(entry, replication, fresh);
                }
            }
            Delivery::Coalesce => {
                let coalesced = entries
                    .iter()
                    .map(|entry| entry.scope.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .filter(|_| self.next() % 2 == 0)
                    .collect::<BTreeSet<_>>();
                for entry in entries {
                    if !coalesced.contains(&entry.scope) {
                        self.deliver(entry, replication, fresh);
                    }
                }
                for scope in coalesced {
                    self.install(&scope, replication, fresh);
                }
            }
            Delivery::Cuts => {
                if self.next() % 4 == 0 {
                    return self.restart(replication, fresh);
                }
                for entry in entries {
                    self.deliver(entry, replication, fresh);
                }
            }
        }
        self.received = replication.log().seq();
    }

    pub(crate) fn is_current(&self, replication: &Replication) -> bool {
        self.received == replication.log().seq()
    }

    /// The replica's documents, compared only once it has caught up.
    pub(crate) fn diverges(&self, fresh: &BTreeMap<String, Value>) -> Option<String> {
        let replica = self
            .documents
            .iter()
            .map(|(scope, document)| (scope.clone(), document.value.clone()))
            .collect::<BTreeMap<_, _>>();
        (&replica != fresh).then(|| {
            let scopes = replica
                .keys()
                .chain(fresh.keys())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .filter(|scope| replica.get(*scope) != fresh.get(*scope))
                .map(|scope| {
                    format!(
                        "{scope}:\n  replica: {}\n  fresh:   {}",
                        replica.get(scope).map_or("<none>".into(), Value::to_string),
                        fresh.get(scope).map_or("<none>".into(), Value::to_string),
                    )
                })
                .collect::<Vec<_>>();
            format!(
                "{:?} replica diverged\n{}",
                self.delivery,
                scopes.join("\n")
            )
        })
    }
}

/// One replica per delivery mode, checked after every step.
#[derive(Debug)]
pub(crate) struct Convergence {
    pub(crate) replicas: Vec<Replica>,
    pub(crate) steps: usize,
}

impl Convergence {
    pub(crate) fn new(seed: u64) -> Self {
        Self {
            replicas: vec![
                Replica::new(Delivery::Follow, seed),
                Replica::new(Delivery::Coalesce, seed.wrapping_mul(31)),
                Replica::new(Delivery::Cuts, seed.wrapping_mul(131)),
            ],
            steps: 0,
        }
    }

    /// Syncs every replica and compares those that caught up. The `Follow`
    /// replica always catches up, so every step is checked at least once.
    pub(crate) fn check(
        &mut self,
        replication: &Replication,
        fresh: &BTreeMap<String, Value>,
    ) -> Result<(), String> {
        self.steps += 1;
        for replica in &mut self.replicas {
            replica.sync(replication, fresh);
            if replica.is_current(replication)
                && let Some(divergence) = replica.diverges(fresh)
            {
                return Err(format!("step {}: {divergence}", self.steps));
            }
        }
        Ok(())
    }

    /// The lagging replicas catch up, then every replica must match.
    pub(crate) fn settle(
        &mut self,
        replication: &Replication,
        fresh: &BTreeMap<String, Value>,
    ) -> Result<(), String> {
        for _ in 0..64 {
            self.check(replication, fresh)?;
            if self
                .replicas
                .iter()
                .all(|replica| replica.is_current(replication))
            {
                return Ok(());
            }
        }
        Err("a lagging replica never caught up".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{AgentPhase, Op};

    fn initialized_gate() -> (AgentState, SessionRegistry, Replication, Convergence) {
        let agent = AgentState::default();
        let registry = SessionRegistry::new("replica-regression");
        let mut replication = Replication::default();
        replication.commit(&agent, 0, &registry);
        let mut convergence = Convergence::new(0x5eed);
        convergence
            .check(&replication, &fresh(&agent, 0, &registry))
            .unwrap();
        (agent, registry, replication, convergence)
    }

    #[test]
    fn follow_rejects_an_invalid_op_instead_of_repairing_from_fresh() {
        let (mut agent, registry, mut replication, mut convergence) = initialized_gate();
        agent.phase = AgentPhase::Initializing;
        let prev = replication.scope_seq(GLOBAL).unwrap();
        // The authoritative state changed, but the producer emitted a patch
        // whose parent does not exist. S2 must reject this log suffix even
        // though an independently fetched snapshot could repair the document.
        replication.log_mut_for_test().append(
            GLOBAL,
            prev,
            Change::Ops(
                vec![Op::Set {
                    path: "/missing/phase".into(),
                    value: json!("initializing"),
                }]
                .into(),
            ),
        );

        let result = convergence.check(&replication, &fresh(&agent, 0, &registry));
        assert!(
            result.is_err(),
            "Follow must fail on an invalid producer op instead of passing by installing the comparison snapshot"
        );
    }

    #[test]
    fn follow_rejects_a_wrong_prev_instead_of_repairing_from_fresh() {
        let (mut agent, registry, mut replication, mut convergence) = initialized_gate();
        agent.phase = AgentPhase::Initializing;
        let prev = replication.scope_seq(GLOBAL).unwrap();
        // Delivery is ordered and the log has not compacted. A broken prev is
        // a producer error, not one of the gate's deliberate reconnect cuts.
        replication.log_mut_for_test().append(
            GLOBAL,
            prev + 1,
            Change::Ops(
                vec![Op::Set {
                    path: "/agent/phase".into(),
                    value: json!("initializing"),
                }]
                .into(),
            ),
        );

        let result = convergence.check(&replication, &fresh(&agent, 0, &registry));
        assert!(
            result.is_err(),
            "Follow must fail on an unexpected continuity break instead of passing by installing the comparison snapshot"
        );
    }
}
