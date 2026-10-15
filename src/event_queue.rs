use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bridge_event::BridgeEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventSendError {
    Closed,
}

#[derive(Clone)]
pub(crate) struct EventSender {
    inner: EventSenderInner,
}

#[derive(Clone)]
enum EventSenderInner {
    Accounted {
        tx: mpsc::UnboundedSender<QueuedEvent>,
        queued_bytes: Arc<AtomicUsize>,
        cancellation: CancellationToken,
    },
    #[cfg(test)]
    Unbounded(mpsc::UnboundedSender<String>),
}

pub(crate) struct EventReceiver {
    inner: EventReceiverInner,
}

enum EventReceiverInner {
    Accounted(mpsc::UnboundedReceiver<QueuedEvent>),
    #[cfg(test)]
    Unbounded(mpsc::UnboundedReceiver<String>),
}

pub(crate) struct QueuedEvent {
    event: BridgeEvent,
    lease: EventByteLease,
}

#[derive(Default)]
pub(crate) struct EventByteLease {
    queued_bytes: Option<Arc<AtomicUsize>>,
    bytes: usize,
}

impl Drop for EventByteLease {
    fn drop(&mut self) {
        if let Some(queued_bytes) = &self.queued_bytes {
            queued_bytes.fetch_sub(self.bytes, Ordering::AcqRel);
        }
    }
}

impl QueuedEvent {
    pub(crate) fn into_parts(self) -> (BridgeEvent, EventByteLease) {
        (self.event, self.lease)
    }

    #[cfg(test)]
    pub(crate) fn into_json_parts(self) -> (String, EventByteLease) {
        (self.event.to_json(), self.lease)
    }
}

impl EventSender {
    /// Queue a JSON event built by the Bridge.
    pub(crate) fn send_value(&self, event: Value) -> Result<(), EventSendError> {
        match &self.inner {
            EventSenderInner::Accounted { .. } => self.send_event(BridgeEvent::from_value(event)),
            #[cfg(test)]
            _ => self.send_json(event.to_string()),
        }
    }

    pub(crate) fn send_event(&self, event: BridgeEvent) -> Result<(), EventSendError> {
        match &self.inner {
            EventSenderInner::Accounted {
                tx, queued_bytes, ..
            } => {
                let bytes = event.encoded_len();
                queued_bytes.fetch_add(bytes, Ordering::AcqRel);
                let queued = QueuedEvent {
                    event,
                    lease: EventByteLease {
                        queued_bytes: Some(queued_bytes.clone()),
                        bytes,
                    },
                };
                tx.send(queued).map_err(|_| EventSendError::Closed)
            }
            #[cfg(test)]
            _ => self.send_json(event.to_json()),
        }
    }

    #[cfg(test)]
    fn send_json(&self, event: String) -> Result<(), EventSendError> {
        match &self.inner {
            EventSenderInner::Accounted { .. } => {
                unreachable!("accounted queues carry typed events")
            }
            EventSenderInner::Unbounded(tx) => tx.send(event).map_err(|_| EventSendError::Closed),
        }
    }

    pub(crate) fn cancel_generation(&self) {
        match &self.inner {
            EventSenderInner::Accounted { cancellation, .. } => cancellation.cancel(),
            #[cfg(test)]
            EventSenderInner::Unbounded(_) => {}
        }
    }

    #[cfg(test)]
    pub(crate) fn queued_bytes(&self) -> usize {
        match &self.inner {
            EventSenderInner::Accounted { queued_bytes, .. } => {
                queued_bytes.load(Ordering::Acquire)
            }
            EventSenderInner::Unbounded(_) => 0,
        }
    }
}

impl EventReceiver {
    pub(crate) async fn recv(&mut self) -> Option<QueuedEvent> {
        match &mut self.inner {
            EventReceiverInner::Accounted(rx) => rx.recv().await,
            #[cfg(test)]
            EventReceiverInner::Unbounded(rx) => Some(QueuedEvent {
                event: BridgeEvent::from_json(&rx.recv().await?),
                lease: EventByteLease::default(),
            }),
        }
    }

    pub(crate) fn try_recv(&mut self) -> Result<QueuedEvent, mpsc::error::TryRecvError> {
        match &mut self.inner {
            EventReceiverInner::Accounted(rx) => rx.try_recv(),
            #[cfg(test)]
            EventReceiverInner::Unbounded(rx) => Ok(QueuedEvent {
                event: BridgeEvent::from_json(&rx.try_recv()?),
                lease: EventByteLease::default(),
            }),
        }
    }
}

pub(crate) fn channel(cancellation: CancellationToken) -> (EventSender, EventReceiver) {
    let (tx, rx) = mpsc::unbounded_channel();
    let queued_bytes = Arc::new(AtomicUsize::new(0));
    (
        EventSender {
            inner: EventSenderInner::Accounted {
                tx,
                queued_bytes: queued_bytes.clone(),
                cancellation,
            },
        },
        EventReceiver {
            inner: EventReceiverInner::Accounted(rx),
        },
    )
}

#[cfg(test)]
impl From<mpsc::UnboundedSender<String>> for EventSender {
    fn from(tx: mpsc::UnboundedSender<String>) -> Self {
        Self {
            inner: EventSenderInner::Unbounded(tx),
        }
    }
}

#[cfg(test)]
impl From<mpsc::UnboundedReceiver<String>> for EventReceiver {
    fn from(rx: mpsc::UnboundedReceiver<String>) -> Self {
        Self {
            inner: EventReceiverInner::Unbounded(rx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lifecycle(event: impl Into<String>) -> BridgeEvent {
        BridgeEvent::Lifecycle(event.into())
    }

    #[tokio::test]
    async fn bursts_preserve_order_and_never_cancel_the_generation() {
        let cancellation = CancellationToken::new();
        let (sender, mut receiver) = channel(cancellation.clone());
        for index in 0..4_096 {
            sender
                .send_event(lifecycle(format!("{index}:{}", "x".repeat(8_192))))
                .unwrap();
        }
        assert!(sender.queued_bytes() > 16 * 1024 * 1024);
        assert!(!cancellation.is_cancelled());
        for index in 0..4_096 {
            let (event, _lease) = receiver.recv().await.unwrap().into_json_parts();
            assert_eq!(event, format!("{index}:{}", "x".repeat(8_192)));
        }
        assert_eq!(sender.queued_bytes(), 0);
    }

    #[tokio::test]
    async fn receive_completion_and_receiver_drop_release_bytes() {
        let cancellation = CancellationToken::new();
        let (sender, mut receiver) = channel(cancellation.clone());
        sender.send_event(lifecycle("first")).unwrap();
        let (event, lease) = receiver.recv().await.unwrap().into_json_parts();
        assert_eq!(event, "first");
        assert_eq!(sender.queued_bytes(), 5);
        drop(lease);
        assert_eq!(sender.queued_bytes(), 0);
        sender.send_event(lifecycle("queued")).unwrap();
        drop(receiver);
        assert_eq!(sender.queued_bytes(), 0);
        assert_eq!(
            sender.send_event(lifecycle("closed")),
            Err(EventSendError::Closed)
        );
        assert_eq!(sender.queued_bytes(), 0);
        assert!(!cancellation.is_cancelled());
        sender.cancel_generation();
        assert!(cancellation.is_cancelled());
    }
}
