use std::time::Duration;

use tokio::sync::mpsc;

use crate::process::{Message, SendError};

/// Inner sender type that handles both bounded and unbounded channels.
enum TxInner {
    Unbounded(mpsc::UnboundedSender<Message>),
    Bounded(mpsc::Sender<Message>),
}

/// Sender half of a mailbox channel.
///
/// Wraps either a bounded or unbounded tokio mpsc sender.
/// Implements `Clone` so multiple producers can send to the same mailbox.
#[derive(Clone)]
pub struct MailboxTx {
    inner: TxInner,
}

impl Clone for TxInner {
    fn clone(&self) -> Self {
        match self {
            Self::Unbounded(tx) => Self::Unbounded(tx.clone()),
            Self::Bounded(tx) => Self::Bounded(tx.clone()),
        }
    }
}

/// Inner receiver type that handles both bounded and unbounded channels.
enum RxInner {
    Unbounded(mpsc::UnboundedReceiver<Message>),
    Bounded(mpsc::Receiver<Message>),
}

/// Receiver half of a mailbox channel.
///
/// Provides `recv()` for blocking receive and `recv_timeout()` for
/// time-limited receive operations.
pub struct MailboxRx {
    inner: RxInner,
}

/// Factory for creating mailbox channel pairs.
pub struct Mailbox;

impl Mailbox {
    /// Create an unbounded mailbox channel pair.
    ///
    /// The sender will never block or fail due to capacity constraints.
    /// Messages are only lost if the receiver is dropped.
    #[must_use]
    pub fn unbounded() -> (MailboxTx, MailboxRx) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            MailboxTx {
                inner: TxInner::Unbounded(tx),
            },
            MailboxRx {
                inner: RxInner::Unbounded(rx),
            },
        )
    }

    /// Create a bounded mailbox channel pair with the given capacity.
    ///
    /// When the mailbox is full, `try_send` will return `SendError::MailboxFull`
    /// and `send` will also return an error for bounded channels at capacity.
    #[must_use]
    pub fn bounded(capacity: usize) -> (MailboxTx, MailboxRx) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            MailboxTx {
                inner: TxInner::Bounded(tx),
            },
            MailboxRx {
                inner: RxInner::Bounded(rx),
            },
        )
    }
}

impl MailboxTx {
    /// Send a message to the mailbox.
    ///
    /// For unbounded channels, this only fails if the receiver has been dropped
    /// (returns `SendError::ProcessDead`).
    ///
    /// For bounded channels, this uses `try_send` semantics: if the channel is
    /// full, returns `SendError::MailboxFull`; if the receiver is dropped,
    /// returns `SendError::ProcessDead`.
    ///
    /// # Errors
    ///
    /// Returns `SendError::ProcessDead` if the receiver has been dropped, or
    /// `SendError::MailboxFull` if the bounded channel is at capacity.
    pub fn send(&self, msg: Message) -> Result<(), SendError> {
        match &self.inner {
            TxInner::Unbounded(tx) => {
                let from = msg.from();
                tx.send(msg).map_err(|_| SendError::ProcessDead(from))
            }
            TxInner::Bounded(tx) => {
                let from = msg.from();
                tx.try_send(msg).map_err(|e| match e {
                    mpsc::error::TrySendError::Full(_) => SendError::MailboxFull(from),
                    mpsc::error::TrySendError::Closed(_) => SendError::ProcessDead(from),
                })
            }
        }
    }

    /// Try to send a message without blocking.
    ///
    /// For unbounded channels, behaves the same as `send`.
    /// For bounded channels, returns `SendError::MailboxFull` if the channel
    /// is at capacity.
    ///
    /// # Errors
    ///
    /// Returns `SendError::ProcessDead` if the receiver has been dropped, or
    /// `SendError::MailboxFull` if the bounded channel is at capacity.
    pub fn try_send(&self, msg: Message) -> Result<(), SendError> {
        match &self.inner {
            TxInner::Unbounded(tx) => {
                let from = msg.from();
                tx.send(msg).map_err(|_| SendError::ProcessDead(from))
            }
            TxInner::Bounded(tx) => {
                let from = msg.from();
                tx.try_send(msg).map_err(|e| match e {
                    mpsc::error::TrySendError::Full(_) => SendError::MailboxFull(from),
                    mpsc::error::TrySendError::Closed(_) => SendError::ProcessDead(from),
                })
            }
        }
    }
}

impl MailboxRx {
    /// Try to receive a message without blocking.
    ///
    /// Returns `Some(message)` if one is immediately available,
    /// or `None` if the channel is empty or closed.
    pub fn try_recv(&mut self) -> Option<Message> {
        match &mut self.inner {
            RxInner::Unbounded(rx) => rx.try_recv().ok(),
            RxInner::Bounded(rx) => rx.try_recv().ok(),
        }
    }

    /// Receive a message from the mailbox.
    ///
    /// Returns `None` if all senders have been dropped (channel is closed).
    pub async fn recv(&mut self) -> Option<Message> {
        match &mut self.inner {
            RxInner::Unbounded(rx) => rx.recv().await,
            RxInner::Bounded(rx) => rx.recv().await,
        }
    }

    /// Receive a message with a timeout.
    ///
    /// Returns `Some(message)` if a message arrives within the given duration,
    /// or `None` if the timeout expires or the channel is closed.
    pub async fn recv_timeout(&mut self, duration: Duration) -> Option<Message> {
        tokio::time::timeout(duration, self.recv())
            .await
            .ok()
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ProcessId;

    #[tokio::test]
    async fn unbounded_send_receive() {
        let (tx, mut rx) = Mailbox::unbounded();
        let msg = Message::new(ProcessId::new(1, 1), rmpv::Value::Nil);
        tx.send(msg).unwrap();
        let received = rx.recv().await.unwrap();
        assert_eq!(received.from(), ProcessId::new(1, 1));
    }

    #[tokio::test]
    async fn bounded_capacity_respected() {
        let (tx, _rx) = Mailbox::bounded(1);
        let msg1 = Message::new(ProcessId::new(1, 1), rmpv::Value::Nil);
        let msg2 = Message::new(ProcessId::new(1, 2), rmpv::Value::Nil);
        tx.send(msg1).unwrap();
        assert!(tx.try_send(msg2).is_err());
    }

    #[tokio::test]
    async fn recv_timeout_expires() {
        let (_tx, mut rx) = Mailbox::unbounded();
        let result = rx.recv_timeout(std::time::Duration::from_millis(10)).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn recv_timeout_receives_in_time() {
        let (tx, mut rx) = Mailbox::unbounded();
        let msg = Message::new(ProcessId::new(1, 1), rmpv::Value::Nil);
        tx.send(msg).unwrap();
        let result = rx.recv_timeout(std::time::Duration::from_millis(100)).await;
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn multiple_messages_fifo() {
        let (tx, mut rx) = Mailbox::unbounded();
        for i in 0..5u64 {
            tx.send(Message::new(
                ProcessId::new(1, i),
                rmpv::Value::Integer(i.into()),
            ))
            .unwrap();
        }
        for i in 0..5u64 {
            let msg = rx.recv().await.unwrap();
            assert_eq!(msg.from().local_id(), i);
        }
    }

    #[tokio::test]
    async fn dropped_sender_closes_receiver() {
        let (tx, mut rx) = Mailbox::unbounded();
        drop(tx);
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn bounded_send_after_drain() {
        let (tx, mut rx) = Mailbox::bounded(1);
        let msg1 = Message::new(ProcessId::new(1, 1), rmpv::Value::Nil);
        tx.send(msg1).unwrap();
        rx.recv().await.unwrap();
        let msg2 = Message::new(ProcessId::new(1, 2), rmpv::Value::Nil);
        assert!(tx.send(msg2).is_ok());
    }

    #[tokio::test]
    async fn send_to_closed_receiver_returns_error() {
        let (tx, rx) = Mailbox::unbounded();
        drop(rx);
        let msg = Message::new(ProcessId::new(1, 1), rmpv::Value::Nil);
        assert!(tx.send(msg).is_err());
    }

    #[tokio::test]
    async fn recv_timeout_zero_duration() {
        let (_tx, mut rx) = Mailbox::unbounded();
        let result = rx.recv_timeout(std::time::Duration::ZERO).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn large_message_throughput() {
        let (tx, mut rx) = Mailbox::unbounded();
        let count = 10_000;
        for i in 0..count {
            tx.send(Message::new(
                ProcessId::new(1, 1),
                rmpv::Value::Integer(i.into()),
            ))
            .unwrap();
        }
        for _ in 0..count {
            rx.recv().await.unwrap();
        }
    }

    #[tokio::test]
    async fn concurrent_senders() {
        let (tx, mut rx) = Mailbox::unbounded();
        let mut handles = Vec::new();
        for i in 0..10u64 {
            let tx = tx.clone();
            handles.push(tokio::spawn(async move {
                tx.send(Message::new(
                    ProcessId::new(1, i),
                    rmpv::Value::Integer(i.into()),
                ))
                .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let mut received = 0;
        while rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .await
            .is_some()
        {
            received += 1;
        }
        assert_eq!(received, 10);
    }

    #[tokio::test]
    async fn bounded_try_send_full_returns_mailbox_full() {
        let (tx, _rx) = Mailbox::bounded(1);
        tx.send(Message::new(ProcessId::new(1, 1), rmpv::Value::Nil))
            .unwrap();
        let result = tx.try_send(Message::new(ProcessId::new(1, 2), rmpv::Value::Nil));
        match result {
            Err(SendError::MailboxFull(_)) => {}
            other => panic!("expected MailboxFull, got {other:?}"),
        }
    }
}
