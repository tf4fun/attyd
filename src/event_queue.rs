use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

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
    #[cfg(test)]
    FaultInjected {
        tx: mpsc::UnboundedSender<String>,
        fault: TestEventFault,
    },
}

/// Inject failures at the actual publication boundary without changing the
/// production queue, serialization, or journal behavior.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct TestEventFault {
    attempts: Arc<AtomicUsize>,
    delta_attempts: Arc<AtomicUsize>,
    fail_after: usize,
}

#[cfg(test)]
impl TestEventFault {
    pub(crate) fn delta_attempts(&self) -> usize {
        self.delta_attempts.load(Ordering::Acquire)
    }

    fn rejects(&self, event: &str) -> bool {
        if serde_json::from_str::<serde_json::Value>(event).is_ok_and(|value| {
            value["type"] == "bridge/internal_runtime_delta"
        }) {
            self.delta_attempts.fetch_add(1, Ordering::AcqRel);
        }
        self.attempts.fetch_add(1, Ordering::AcqRel) >= self.fail_after
    }
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
            EventSenderInner::Accounted {
                tx, queued_bytes, ..
            } => {
                let bytes = event.len();
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
            EventSenderInner::Unbounded(tx) => tx.send(event).map_err(|_| EventSendError::Closed),
            #[cfg(test)]
            EventSenderInner::FaultInjected { tx, fault } => {
                if fault.rejects(&event) {
                    Err(EventSendError::Closed)
                } else {
                    tx.send(event).map_err(|_| EventSendError::Closed)
                }
            }
        }
    }

    pub(crate) fn cancel_generation(&self) {
        match &self.inner {
            EventSenderInner::Accounted { cancellation, .. } => cancellation.cancel(),
            #[cfg(test)]
            EventSenderInner::Unbounded(_) => {}
            #[cfg(test)]
            EventSenderInner::FaultInjected { .. } => {}
        }
    }

    #[cfg(test)]
    pub(crate) fn queued_bytes(&self) -> usize {
        match &self.inner {
            EventSenderInner::Accounted { queued_bytes, .. } => {
                queued_bytes.load(Ordering::Acquire)
            }
            EventSenderInner::Unbounded(_) => 0,
            EventSenderInner::FaultInjected { .. } => 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_failing_after(successes: usize) -> (Self, EventReceiver, TestEventFault) {
        let (tx, rx) = mpsc::unbounded_channel();
        let fault = TestEventFault {
            attempts: Arc::new(AtomicUsize::new(0)),
            delta_attempts: Arc::new(AtomicUsize::new(0)),
            fail_after: successes,
        };
        (
            Self {
                inner: EventSenderInner::FaultInjected {
                    tx,
                    fault: fault.clone(),
                },
            },
            EventReceiver {
                inner: EventReceiverInner::Unbounded(rx),
            },
            fault,
        )
    }
}

impl EventReceiver {
    pub(crate) async fn recv(&mut self) -> Option<QueuedEvent> {
        match &mut self.inner {
            EventReceiverInner::Accounted(rx) => rx.recv().await,
            #[cfg(test)]
            EventReceiverInner::Unbounded(rx) => rx.recv().await.map(|event| QueuedEvent {
                event,
                lease: EventByteLease::default(),
            }),
        }
    }

    pub(crate) fn try_recv(&mut self) -> Result<QueuedEvent, mpsc::error::TryRecvError> {
        match &mut self.inner {
            EventReceiverInner::Accounted(rx) => rx.try_recv(),
            #[cfg(test)]
            EventReceiverInner::Unbounded(rx) => rx.try_recv().map(|event| QueuedEvent {
                event,
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

    #[tokio::test]
    async fn bursts_preserve_order_and_never_cancel_the_generation() {
        let cancellation = CancellationToken::new();
        let (sender, mut receiver) = channel(cancellation.clone());
        for index in 0..4_096 {
            sender
                .send(format!("{index}:{}", "x".repeat(8_192)))
                .unwrap();
        }
        assert!(sender.queued_bytes() > 16 * 1024 * 1024);
        assert!(!cancellation.is_cancelled());
        for index in 0..4_096 {
            let (event, _lease) = receiver.recv().await.unwrap().into_parts();
            assert_eq!(event, format!("{index}:{}", "x".repeat(8_192)));
        }
        assert_eq!(sender.queued_bytes(), 0);
    }

    #[tokio::test]
    async fn receive_completion_and_receiver_drop_release_bytes() {
        let cancellation = CancellationToken::new();
        let (sender, mut receiver) = channel(cancellation.clone());
        sender.send("first".into()).unwrap();
        let (event, lease) = receiver.recv().await.unwrap().into_parts();
        assert_eq!(event, "first");
        assert_eq!(sender.queued_bytes(), 5);
        drop(lease);
        assert_eq!(sender.queued_bytes(), 0);
        sender.send("queued".into()).unwrap();
        drop(receiver);
        assert_eq!(sender.queued_bytes(), 0);
        assert_eq!(sender.send("closed".into()), Err(EventSendError::Closed));
        assert_eq!(sender.queued_bytes(), 0);
        assert!(!cancellation.is_cancelled());
        sender.cancel_generation();
        assert!(cancellation.is_cancelled());
    }
}
