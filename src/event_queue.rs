use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy)]
pub(crate) struct EventQueueLimits {
    pub max_items: usize,
    pub max_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventSendError {
    Full,
    Closed,
}

#[derive(Clone)]
pub(crate) struct EventSender {
    inner: EventSenderInner,
}

#[derive(Clone)]
enum EventSenderInner {
    Bounded {
        tx: mpsc::Sender<QueuedEvent>,
        queued_bytes: Arc<AtomicUsize>,
        max_bytes: usize,
        cancellation: CancellationToken,
    },
    #[cfg(test)]
    Unbounded(mpsc::UnboundedSender<String>),
}

pub(crate) struct EventReceiver {
    inner: EventReceiverInner,
}

enum EventReceiverInner {
    Bounded(mpsc::Receiver<QueuedEvent>),
    #[cfg(test)]
    Unbounded(mpsc::UnboundedReceiver<String>),
}

pub(crate) struct QueuedEvent {
    event: String,
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
    pub(crate) fn into_parts(mut self) -> (String, EventByteLease) {
        (
            std::mem::take(&mut self.event),
            std::mem::take(&mut self.lease),
        )
    }
}

impl EventSender {
    pub(crate) fn send(&self, event: String) -> Result<(), EventSendError> {
        match &self.inner {
            EventSenderInner::Bounded {
                tx,
                queued_bytes,
                max_bytes,
                cancellation,
            } => {
                let bytes = event.len();
                if queued_bytes
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                        current
                            .checked_add(bytes)
                            .filter(|next| *next <= *max_bytes)
                    })
                    .is_err()
                {
                    cancellation.cancel();
                    return Err(EventSendError::Full);
                }
                let queued = QueuedEvent {
                    event,
                    lease: EventByteLease {
                        queued_bytes: Some(queued_bytes.clone()),
                        bytes,
                    },
                };
                match tx.try_send(queued) {
                    Ok(()) => Ok(()),
                    Err(mpsc::error::TrySendError::Full(_queued)) => {
                        cancellation.cancel();
                        Err(EventSendError::Full)
                    }
                    Err(mpsc::error::TrySendError::Closed(_queued)) => Err(EventSendError::Closed),
                }
            }
            #[cfg(test)]
            EventSenderInner::Unbounded(tx) => tx.send(event).map_err(|_| EventSendError::Closed),
        }
    }

    pub(crate) fn cancel_generation(&self) {
        match &self.inner {
            EventSenderInner::Bounded { cancellation, .. } => cancellation.cancel(),
            #[cfg(test)]
            EventSenderInner::Unbounded(_) => {}
        }
    }

    #[cfg(test)]
    pub(crate) fn queued_bytes(&self) -> usize {
        match &self.inner {
            EventSenderInner::Bounded { queued_bytes, .. } => queued_bytes.load(Ordering::Acquire),
            EventSenderInner::Unbounded(_) => 0,
        }
    }
}

impl EventReceiver {
    pub(crate) async fn recv(&mut self) -> Option<QueuedEvent> {
        match &mut self.inner {
            EventReceiverInner::Bounded(rx) => rx.recv().await,
            #[cfg(test)]
            EventReceiverInner::Unbounded(rx) => rx.recv().await.map(|event| QueuedEvent {
                event,
                lease: EventByteLease::default(),
            }),
        }
    }

    pub(crate) fn try_recv(&mut self) -> Result<QueuedEvent, mpsc::error::TryRecvError> {
        match &mut self.inner {
            EventReceiverInner::Bounded(rx) => rx.try_recv(),
            #[cfg(test)]
            EventReceiverInner::Unbounded(rx) => rx.try_recv().map(|event| QueuedEvent {
                event,
                lease: EventByteLease::default(),
            }),
        }
    }
}

pub(crate) fn channel(
    limits: EventQueueLimits,
    cancellation: CancellationToken,
) -> (EventSender, EventReceiver) {
    assert!(
        limits.max_items > 0,
        "event queue item limit must be positive"
    );
    let (tx, rx) = mpsc::channel(limits.max_items);
    let queued_bytes = Arc::new(AtomicUsize::new(0));
    (
        EventSender {
            inner: EventSenderInner::Bounded {
                tx,
                queued_bytes: queued_bytes.clone(),
                max_bytes: limits.max_bytes,
                cancellation,
            },
        },
        EventReceiver {
            inner: EventReceiverInner::Bounded(rx),
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

    #[tokio::test]
    async fn queue_enforces_item_and_byte_limits_and_cancels_on_saturation() {
        let cancellation = CancellationToken::new();
        let (items, _rx) = channel(
            EventQueueLimits {
                max_items: 1,
                max_bytes: 16,
            },
            cancellation.clone(),
        );
        items.send("first".to_string()).unwrap();
        assert_eq!(items.send("second".to_string()), Err(EventSendError::Full));
        assert!(cancellation.is_cancelled());
        assert_eq!(items.queued_bytes(), 5);

        let cancellation = CancellationToken::new();
        let (bytes, _rx) = channel(
            EventQueueLimits {
                max_items: 4,
                max_bytes: 5,
            },
            cancellation.clone(),
        );
        bytes.send("12345".to_string()).unwrap();
        assert_eq!(bytes.send("6".to_string()), Err(EventSendError::Full));
        assert!(cancellation.is_cancelled());
        assert_eq!(bytes.queued_bytes(), 5);
    }

    #[tokio::test]
    async fn failed_send_receive_completion_and_receiver_drop_release_bytes() {
        let cancellation = CancellationToken::new();
        let (sender, mut receiver) = channel(
            EventQueueLimits {
                max_items: 1,
                max_bytes: 64,
            },
            cancellation,
        );
        sender.send("first".to_string()).unwrap();
        assert_eq!(
            sender.send("overflow".to_string()),
            Err(EventSendError::Full)
        );
        assert_eq!(sender.queued_bytes(), 5);

        let (event, lease) = receiver.recv().await.unwrap().into_parts();
        assert_eq!(event, "first");
        assert_eq!(sender.queued_bytes(), 5);
        drop(lease);
        assert_eq!(sender.queued_bytes(), 0);

        sender.send("queued".to_string()).unwrap();
        assert_eq!(sender.queued_bytes(), 6);
        drop(receiver);
        assert_eq!(sender.queued_bytes(), 0);
        assert_eq!(
            sender.send("closed".to_string()),
            Err(EventSendError::Closed)
        );
        assert_eq!(sender.queued_bytes(), 0);
    }
}
