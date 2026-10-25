//! The revision 4 stream (docs/protocol-r4-design.md §6): path subscriptions over the state
//! tree, shared by full frontends and clients that follow only selected paths.
#![cfg_attr(not(test), allow(dead_code))]

pub(crate) mod params;
pub(crate) mod segments;
pub(crate) mod snapshot;
pub(crate) mod subscriber;
pub(crate) mod windowed_ops;
pub(crate) mod wire;
#[cfg(test)]
pub(crate) mod client;
#[cfg(test)]
pub(crate) mod documents;

/// An idle SSE connection sends a heartbeat at this interval.
pub(crate) const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(15);

/// Above this queued payload budget a slow stream is realigned from its received versions.
pub(crate) const QUEUE_BUDGET: usize = 1 << 20;
