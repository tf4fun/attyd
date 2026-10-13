//! The per-epoch replication log (plan §2.3): a bounded in-memory suffix,
//! accounted by bytes. Only the commit step appends to it (S1).
use std::collections::VecDeque;
use std::sync::Arc;

use super::{Entry, Op, Position};

/// The default retained bytes per epoch. A replica resuming from before the
/// retained suffix gets a `reset` (protocol §4).
pub(crate) const DEFAULT_LOG_BUDGET: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Change {
    Ops(Arc<[Op]>),
    /// The scope's document no longer exists: its session was retired or
    /// replaced by a new incarnation.
    End,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LogEntry {
    pub scope: String,
    pub seq: u64,
    /// The `seq` of the scope's previous entry, `0` for a new document.
    pub prev: u64,
    pub change: Change,
    bytes: usize,
}

impl LogEntry {
    /// The stream entry for a subscriber whose previous delivered entry for
    /// this scope is `prev`. `End` reasons come from the Hub (migration step 3).
    pub(crate) fn ops_entry(&self, epoch: &str, prev: u64) -> Option<Entry> {
        let Change::Ops(ops) = &self.change else {
            return None;
        };
        Some(Entry::Ops {
            scope: self.scope.clone(),
            at: Position {
                epoch: epoch.to_string(),
                seq: self.seq,
            },
            prev,
            ops: ops.to_vec(),
        })
    }
}

#[derive(Debug)]
pub(crate) struct Log {
    seq: u64,
    entries: VecDeque<LogEntry>,
    bytes: usize,
    budget: usize,
}

impl Default for Log {
    fn default() -> Self {
        Self::with_budget(DEFAULT_LOG_BUDGET)
    }
}

impl Log {
    pub(crate) fn with_budget(budget: usize) -> Self {
        Self {
            seq: 0,
            entries: VecDeque::new(),
            bytes: 0,
            budget,
        }
    }

    /// The `seq` of the last committed entry; `0` before the first.
    pub(crate) fn seq(&self) -> u64 {
        self.seq
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn append(&mut self, scope: &str, prev: u64, change: Change) -> u64 {
        self.seq += 1;
        let bytes = scope.len()
            + match &change {
                Change::Ops(ops) => serde_json::to_vec(&**ops).map_or(0, |encoded| encoded.len()),
                Change::End => 0,
            };
        self.entries.push_back(LogEntry {
            scope: scope.to_string(),
            seq: self.seq,
            prev,
            change,
            bytes,
        });
        self.bytes += bytes;
        // Compaction drops the oldest entries; the newest always stays, so a
        // replica at the previous position can still follow it.
        while self.bytes > self.budget && self.entries.len() > 1 {
            let dropped = self.entries.pop_front().expect("nonempty log");
            self.bytes -= dropped.bytes;
        }
        self.seq
    }

    /// Every entry after `seq`, or `None` when compaction dropped some of
    /// them and the replica must reset.
    pub(crate) fn after(&self, seq: u64) -> Option<impl Iterator<Item = &LogEntry>> {
        let first = self.entries.front().map_or(self.seq + 1, |entry| entry.seq);
        (seq + 1 >= first && seq <= self.seq).then(|| {
            self.entries
                .iter()
                .skip(usize::try_from(seq + 1 - first).unwrap_or(usize::MAX))
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn set(value: usize) -> Change {
        Change::Ops(
            vec![Op::Set {
                path: "/value".into(),
                value: json!("x".repeat(value)),
            }]
            .into(),
        )
    }

    #[test]
    fn positions_count_entries_across_scopes() {
        let mut log = Log::default();
        assert_eq!(log.after(0).unwrap().count(), 0);
        assert_eq!(log.append("global", 0, set(1)), 1);
        assert_eq!(log.append("session/a", 0, set(1)), 2);
        assert_eq!(log.append("global", 1, set(1)), 3);
        assert_eq!(log.append("session/a", 2, Change::End), 4);
        let after = log
            .after(1)
            .unwrap()
            .map(|entry| (entry.seq, entry.prev))
            .collect::<Vec<_>>();
        assert_eq!(after, [(2, 0), (3, 1), (4, 2)]);
        assert_eq!(log.after(4).unwrap().count(), 0);
        assert!(log.after(5).is_none(), "a position from the future");
        let entry = log.after(2).unwrap().next().unwrap();
        assert_eq!(
            serde_json::to_value(entry.ops_entry("e", 1).unwrap()).unwrap(),
            json!({
                "type": "ops", "scope": "global", "at": { "epoch": "e", "seq": 3 },
                "prev": 1, "ops": [{ "op": "set", "path": "/value", "value": "x" }],
            })
        );
        assert!(
            log.after(3)
                .unwrap()
                .next()
                .unwrap()
                .ops_entry("e", 2)
                .is_none()
        );
    }

    #[test]
    fn compaction_keeps_the_byte_budget_and_reports_dropped_suffixes() {
        let mut log = Log::with_budget(200);
        for _ in 0..10 {
            log.append("global", log.seq(), set(50));
        }
        assert!(log.bytes() <= 200, "{}", log.bytes());
        assert!(log.len() < 10);
        let first = 10 - log.len() as u64 + 1;
        assert!(log.after(first - 2).is_none());
        assert_eq!(log.after(first - 1).unwrap().next().unwrap().seq, first);

        // One oversized entry stays: the newest entry is never dropped.
        log.append("global", log.seq(), set(1_000));
        assert_eq!(log.len(), 1);
        assert_eq!(log.after(10).unwrap().count(), 1);
        assert!(log.after(9).is_none());
    }
}
