//! What became of a write whose answer was lost (docs/protocol-r4-design.md §9.3).
//!
//! A request `(x, e)` is operation ID `x` written with `expect = e` to a target `W`, while the
//! client held instance `i` and `W` at path version `t`. The rule reads only what the tree
//! publishes of `W`: its `op`, its `ops` and, for a session, `heldSince`. The same rule is in
//! `shared/outcome.ts`; both replay `tests/fixtures/protocol-r4-outcome-vectors.json`.
#![cfg_attr(not(test), allow(dead_code))]

use serde::Serialize;

/// One record of a target's log, as far as the rule reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Logged {
    pub id: String,
    pub after: Option<String>,
}

/// What the tree publishes of a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TargetState {
    pub instance: String,
    /// `op.id`.
    pub op: String,
    /// The revision at which the bridge started holding the session; none for `global` and
    /// `catalog`, which are held for the whole instance.
    pub held_since: Option<u64>,
    pub ops: Vec<Logged>,
}

/// The request and what the client held when it made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    pub id: String,
    pub expect: String,
    pub instance: String,
    /// The path version of the target the client held.
    pub at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reason {
    /// The bridge restarted: whether the request was admitted is lost.
    Restarted,
    /// The session was released and held again.
    Released,
    /// `e` is older than the log remembers.
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum Outcome {
    /// It took effect; `record` is its ID in the log.
    Admitted { record: String },
    /// Another operation replaced `e`: it can never be admitted.
    Superseded,
    /// Not admitted yet: a delayed copy may still be.
    Open,
    Unknown { reason: Reason },
}

/// The outcome of `request` against `target` (§9.3), the rows of the table in order.
pub(crate) fn outcome(target: &TargetState, request: &Request) -> Outcome {
    if target.instance != request.instance {
        return Outcome::Unknown { reason: Reason::Restarted };
    }
    if target.held_since.is_some_and(|since| since > request.at) {
        return Outcome::Unknown { reason: Reason::Released };
    }
    if let Some(record) = target.ops.iter().find(|record| record.id == request.id) {
        return Outcome::Admitted { record: record.id.clone() };
    }
    if target
        .ops
        .iter()
        .any(|record| record.after.as_deref() == Some(request.expect.as_str()))
    {
        return Outcome::Superseded;
    }
    if target.op == request.expect {
        return Outcome::Open;
    }
    Outcome::Unknown { reason: Reason::Expired }
}

#[cfg(test)]
mod tests;
