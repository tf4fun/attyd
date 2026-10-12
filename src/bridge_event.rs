//! Typed events on the Bridge → Hub queue.
//!
//! The Bridge builds browser-facing session events once, with their routing
//! identity, and hands canonical runtime state over without serializing it.
//! The Hub routes on these fields instead of re-parsing JSON. Rare connection
//! lifecycle events keep their wire form because only the bootstrap reads them.

use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};

use crate::runtime_state::{RuntimeDelta, RuntimeSnapshot};

/// Session delivery identity: bridge epoch and session incarnation.
pub(crate) type SessionOwner = (String, u64);
/// Coalescing position: bridge epoch, session incarnation and view revision.
pub(crate) type SessionPosition = (String, u64, u64);

pub(crate) enum BridgeEvent {
    /// Connection, authentication and catalog events in wire form.
    Lifecycle(String),
    Session(SessionEvent),
    ObserverReady {
        observer_id: u64,
        session_id: String,
        reset: Option<SessionEvent>,
    },
    ObserverEnd {
        observer_id: u64,
        session_id: String,
        owner: Option<SessionOwner>,
    },
    RuntimeSnapshot(Box<RuntimeSnapshot>),
    RuntimeDelta(Box<RuntimeDelta>),
}

/// A browser-facing session event, serialized once with its routing fields.
#[derive(Clone)]
pub(crate) struct SessionEvent {
    pub(crate) session_id: String,
    pub(crate) owner: Option<SessionOwner>,
    /// Present for reconstructible state (deltas and resets), which slow
    /// subscribers may coalesce into the latest reset.
    pub(crate) position: Option<SessionPosition>,
    pub(crate) reset: bool,
    pub(crate) payload: Arc<str>,
}

impl SessionEvent {
    /// Reduce a business session event to its delivery form. A full session
    /// view becomes a reset token: subscribers refetch the view over REST.
    fn business(mut value: Value) -> Option<Self> {
        let session_id = value.get("sessionId")?.as_str()?.to_string();
        match value.get("type")?.as_str()? {
            "bridge/session_view" => {
                Self::reset(session_reset_value(&session_id, value.get("view")?)?)
            }
            "bridge/session_delta" => {
                crate::attachments::project(&mut value);
                let position = position(&value);
                Some(Self::new(session_id, value, position, false))
            }
            "bridge/session_retired"
            | "bridge/session_turn_complete"
            | "bridge/session_turn_failed" => {
                crate::attachments::project(&mut value);
                Some(Self::new(session_id, value, None, false))
            }
            _ => None,
        }
    }

    pub(crate) fn reset(value: Value) -> Option<Self> {
        if value.get("type")?.as_str()? != "bridge/session_reset" {
            return None;
        }
        let session_id = value.get("sessionId")?.as_str()?.to_string();
        let position = Some(position(&value)?);
        Some(Self::new(session_id, value, position, true))
    }

    fn new(
        session_id: String,
        value: Value,
        position: Option<SessionPosition>,
        reset: bool,
    ) -> Self {
        Self {
            owner: owner(&value),
            session_id,
            position,
            reset,
            payload: Arc::from(value.to_string()),
        }
    }
}

impl BridgeEvent {
    /// Classify one event produced by the Bridge. Unknown types are dropped:
    /// no Hub subscriber could receive them.
    pub(crate) fn from_value(value: Value) -> Option<Self> {
        let kind = value.get("type")?.as_str()?;
        match kind {
            "bridge/session_view"
            | "bridge/session_delta"
            | "bridge/session_retired"
            | "bridge/session_turn_complete"
            | "bridge/session_turn_failed" => SessionEvent::business(value).map(Self::Session),
            "bridge/internal_observer_ready" => Some(Self::ObserverReady {
                observer_id: value.get("observerId")?.as_u64()?,
                session_id: value.get("sessionId")?.as_str()?.to_string(),
                reset: value.get("reset").cloned().and_then(SessionEvent::reset),
            }),
            "bridge/internal_observer_end" => Some(Self::ObserverEnd {
                observer_id: value.get("observerId")?.as_u64()?,
                session_id: value.get("sessionId")?.as_str()?.to_string(),
                owner: owner(&value),
            }),
            "bridge/internal_runtime_snapshot" => Some(Self::RuntimeSnapshot(Box::new(
                serde_json::from_value(value.get("value")?.clone()).ok()?,
            ))),
            "bridge/internal_runtime_delta" => Some(Self::RuntimeDelta(Box::new(
                serde_json::from_value(value.get("value")?.clone()).ok()?,
            ))),
            _ => Some(Self::Lifecycle(value.to_string())),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_json(raw: &str) -> Option<Self> {
        Self::from_value(serde_json::from_str(raw).ok()?)
    }

    /// The JSON form test channels carry and queue accounting measures.
    pub(crate) fn to_json(&self) -> String {
        match self {
            Self::Lifecycle(event) => event.clone(),
            Self::Session(event) => event.payload.to_string(),
            Self::ObserverReady {
                observer_id,
                session_id,
                reset,
            } => json!({
                "type": "bridge/internal_observer_ready",
                "observerId": observer_id,
                "sessionId": session_id,
                "reset": reset
                    .as_ref()
                    .and_then(|reset| serde_json::from_str::<Value>(&reset.payload).ok()),
            })
            .to_string(),
            Self::ObserverEnd {
                observer_id,
                session_id,
                owner,
            } => json!({
                "type": "bridge/internal_observer_end",
                "observerId": observer_id,
                "sessionId": session_id,
                "bridgeEpoch": owner.as_ref().map(|owner| &owner.0),
                "sessionIncarnation": owner.as_ref().map(|owner| owner.1),
            })
            .to_string(),
            Self::RuntimeSnapshot(snapshot) => {
                runtime_envelope("bridge/internal_runtime_snapshot", snapshot)
            }
            Self::RuntimeDelta(delta) => runtime_envelope("bridge/internal_runtime_delta", delta),
        }
    }

    /// Bytes this event would occupy in wire form. Runtime state is measured
    /// without allocating its serialization.
    pub(crate) fn encoded_len(&self) -> usize {
        match self {
            Self::Lifecycle(event) => event.len(),
            Self::Session(event) => event.payload.len(),
            Self::ObserverReady { .. } | Self::ObserverEnd { .. } => self.to_json().len(),
            Self::RuntimeSnapshot(snapshot) => {
                runtime_envelope_len("bridge/internal_runtime_snapshot", snapshot)
            }
            Self::RuntimeDelta(delta) => {
                runtime_envelope_len("bridge/internal_runtime_delta", delta)
            }
        }
    }
}

#[derive(Serialize)]
struct RuntimeEnvelope<'a, T> {
    #[serde(rename = "type")]
    kind: &'a str,
    value: &'a T,
}

fn runtime_envelope<T: Serialize>(kind: &str, value: &T) -> String {
    serde_json::to_string(&RuntimeEnvelope { kind, value })
        .expect("canonical runtime state serializes")
}

fn runtime_envelope_len<T: Serialize>(kind: &str, value: &T) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    let _ = serde_json::to_writer(&mut counter, &RuntimeEnvelope { kind, value });
    counter.0
}

fn owner(value: &Value) -> Option<SessionOwner> {
    Some((
        value.get("bridgeEpoch")?.as_str()?.to_string(),
        value.get("sessionIncarnation")?.as_u64()?,
    ))
}

fn position(value: &Value) -> Option<SessionPosition> {
    Some((
        value.get("bridgeEpoch")?.as_str()?.to_string(),
        value.get("sessionIncarnation")?.as_u64()?,
        value.get("viewRevision")?.as_u64()?,
    ))
}

pub(crate) fn session_reset_value(session_id: &str, view: &Value) -> Option<Value> {
    Some(json!({
        "type": "bridge/session_reset",
        "bridgeEpoch": view.get("bridgeEpoch")?.as_str()?,
        "sessionId": session_id,
        "sessionIncarnation": view.pointer("/session/incarnation")?.as_u64()?,
        "viewRevision": view.pointer("/session/viewRevision")?.as_u64()?,
        "historyRevision": view.pointer("/session/historyRevision").cloned().unwrap_or(Value::Null),
        "phase": view.pointer("/session/phase").cloned().unwrap_or(Value::Null),
        "syncError": view.pointer("/session/syncError").cloned().unwrap_or(Value::Null),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_queue;
    use crate::runtime_state::RuntimeState;
    use crate::test_allocations::measure_allocations;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn runtime_transfer_shares_large_state_without_serializing_it() {
        const BODY_BYTES: usize = 1024 * 1024;
        let mut runtime = RuntimeState::new("epoch");
        let incarnation = runtime
            .open_new(
                "epoch",
                "session",
                "/workspace",
                json!({ "sessionId": "session" }),
            )
            .unwrap();
        runtime
            .start_prompt("epoch", "session", incarnation, "prompt", Vec::new())
            .unwrap();
        runtime
            .append_runtime_update_for_test(
                "epoch",
                "session",
                incarnation,
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "answer",
                    "content": { "type": "text", "text": "x".repeat(BODY_BYTES) },
                }),
            )
            .unwrap();
        let snapshot = runtime.snapshot();
        let (sender, mut receiver) = event_queue::channel(CancellationToken::new());

        let (event, stats) = measure_allocations(|| {
            sender
                .send_event(BridgeEvent::RuntimeSnapshot(Box::new(snapshot)))
                .unwrap();
            receiver.try_recv().unwrap().into_parts().0
        });

        let BridgeEvent::RuntimeSnapshot(delivered) = event else {
            panic!("runtime snapshot changed type in transit");
        };
        assert_eq!(delivered.sessions["session"].incarnation, incarnation);
        assert!(
            stats.requested_bytes < BODY_BYTES / 16,
            "handoff allocated like a serialization of the {BODY_BYTES}-byte body: {stats:?}"
        );
    }
}
