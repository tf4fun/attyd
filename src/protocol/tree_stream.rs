//! The revision 4 stream (docs/protocol-r4-design.md §6): path subscriptions over the state
//! tree, served beside revision 3's scope streams.
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
