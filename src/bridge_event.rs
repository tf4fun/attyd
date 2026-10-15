//! What a bridge generation tells its host (`server.rs`).
//!
//! Browsers read the published protocol documents; the host acts only on
//! these two signals, in the order the generation sent them.

use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BridgeEvent {
    /// The Agent connection initialized: the next recovery starts at once.
    Initialized,
    /// A terminal login succeeded: the host restarts the connection (Q2).
    TerminalLoginSucceeded,
}

pub(crate) type EventSender = mpsc::UnboundedSender<BridgeEvent>;
pub(crate) type EventReceiver = mpsc::UnboundedReceiver<BridgeEvent>;
