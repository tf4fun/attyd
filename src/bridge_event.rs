//! Typed events on the Bridge → Hub queue.
//!
//! Browsers read the published protocol documents; the Hub reads only the
//! connection lifecycle events the bootstrap needs, in their wire form.

use serde_json::Value;

pub(crate) enum BridgeEvent {
    /// Connection, authentication and catalog events in wire form.
    Lifecycle(String),
}

impl BridgeEvent {
    pub(crate) fn from_value(value: Value) -> Self {
        Self::Lifecycle(value.to_string())
    }

    #[cfg(test)]
    pub(crate) fn from_json(raw: &str) -> Self {
        Self::Lifecycle(raw.to_string())
    }

    /// The JSON form test channels carry and queue accounting measures.
    pub(crate) fn to_json(&self) -> String {
        match self {
            Self::Lifecycle(event) => event.clone(),
        }
    }

    /// Bytes this event occupies in wire form.
    pub(crate) fn encoded_len(&self) -> usize {
        match self {
            Self::Lifecycle(event) => event.len(),
        }
    }
}
