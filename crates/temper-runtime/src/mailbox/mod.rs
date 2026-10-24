//! Actor mailbox — the fundamental message queue primitive.
//!
//! Every actor has exactly one mailbox. Messages are enqueued by senders
//! (via ActorRef) and dequeued sequentially by the actor cell.
//!
//! TigerStyle: All mailboxes are BOUNDED. No unbounded queues.
//! The capacity is set at actor creation time and cannot grow.
//! When full, sends return MailboxFull immediately — no blocking, no OOM.
//!
//! A halt travels beside the queue, not in it: it needs no mailbox space, and
//! the receiver sees it before any queued message.

use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use crate::actor::actor_ref::{Envelope, SystemSignal};
use crate::actor::errors::ActorError;
use crate::actor::traits::Message;

/// Default mailbox capacity. Sized for typical entity actors.
/// TigerStyle: This is a budget, not a suggestion.
pub const DEFAULT_MAILBOX_CAPACITY: usize = 1_000;

/// The sender half of a mailbox. Held by ActorRef, cloneable.
pub struct MailboxSender<M: Message> {
    inner: mpsc::Sender<Envelope<M>>,
    capacity: usize,
    halt: Arc<watch::Sender<bool>>,
}

/// The receiver half of a mailbox. Held by ActorCell, not cloneable.
pub struct MailboxReceiver<M: Message> {
    inner: mpsc::Receiver<Envelope<M>>,
    halt: watch::Receiver<bool>,
}

/// Create a new bounded mailbox with the given capacity.
pub fn mailbox<M: Message>(capacity: usize) -> (MailboxSender<M>, MailboxReceiver<M>) {
    // TigerStyle: assert the budget is sane
    debug_assert!(capacity > 0, "mailbox capacity must be > 0");
    debug_assert!(
        capacity <= 100_000,
        "mailbox capacity {} exceeds max budget 100,000",
        capacity
    );

    let (tx, rx) = mpsc::channel(capacity);
    let (halt_tx, halt_rx) = watch::channel(false);
    (
        MailboxSender {
            inner: tx,
            capacity,
            halt: Arc::new(halt_tx),
        },
        MailboxReceiver {
            inner: rx,
            halt: halt_rx,
        },
    )
}

impl<M: Message> MailboxSender<M> {
    /// Whether the actor has permanently dropped its receiving mailbox.
    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    /// Send a message to the mailbox. Returns MailboxFull if at capacity.
    /// TigerStyle: This never blocks. Full is an error, not a wait condition.
    pub fn send(&self, msg: Envelope<M>) -> Result<(), ActorError> {
        self.inner.try_send(msg).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => ActorError::MailboxFull,
            mpsc::error::TrySendError::Closed(_) => ActorError::SendFailed,
        })
    }

    /// Ask the receiver to stop before any queued message. Needs no mailbox
    /// space, so it works on a full mailbox; asking twice is the same as once.
    pub fn halt(&self) {
        self.halt.send_replace(true);
    }

    /// Whether a halt was asked for.
    pub fn is_halted(&self) -> bool {
        *self.halt.borrow()
    }

    /// Wait until the receiver is dropped: the actor's task has ended.
    pub async fn closed(&self) {
        self.inner.closed().await;
    }

    /// Get the mailbox capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Current number of messages queued in the mailbox.
    ///
    /// Computed as `max_capacity - available_capacity`. DST-safe:
    /// exact under single-threaded simulation.
    pub fn depth(&self) -> usize {
        self.capacity.saturating_sub(self.inner.capacity())
    }

    /// Mailbox utilization as a fraction in [0.0, 1.0].
    pub fn utilization(&self) -> f64 {
        if self.capacity == 0 {
            return 0.0;
        }
        self.depth() as f64 / self.capacity as f64
    }
}

impl<M: Message> MailboxReceiver<M> {
    /// Receive the next message. Returns None if all senders dropped. Once a
    /// halt is asked for, returns a Stop signal ahead of anything queued.
    pub async fn recv(&mut self) -> Option<Envelope<M>> {
        tokio::select! {
            biased;
            () = halt_requested(&mut self.halt) => Some(Envelope::Signal(SystemSignal::Stop)),
            envelope = self.inner.recv() => envelope,
        }
    }
}

/// Resolves once a halt is asked for. Never resolves when every sender is gone
/// without one: the queue then drains and ends as before.
async fn halt_requested(halt: &mut watch::Receiver<bool>) {
    if halt.wait_for(|halted| *halted).await.is_err() {
        std::future::pending::<()>().await;
    }
}

impl<M: Message> Clone for MailboxSender<M> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            capacity: self.capacity,
            halt: Arc::clone(&self.halt),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct TestMsg(String);
    impl Message for TestMsg {}

    #[tokio::test]
    async fn test_bounded_mailbox_send_recv() {
        let (tx, mut rx) = mailbox::<TestMsg>(10);
        tx.send(Envelope::Tell(TestMsg("hello".into()))).unwrap();
        let msg = rx.recv().await.unwrap();
        match msg {
            Envelope::Tell(TestMsg(s)) => assert_eq!(s, "hello"),
            _ => panic!("expected Tell"),
        }
    }

    #[tokio::test]
    async fn test_bounded_mailbox_full() {
        let (tx, _rx) = mailbox::<TestMsg>(2);
        tx.send(Envelope::Tell(TestMsg("1".into()))).unwrap();
        tx.send(Envelope::Tell(TestMsg("2".into()))).unwrap();
        // Third send should fail — mailbox full
        let result = tx.send(Envelope::Tell(TestMsg("3".into())));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), ActorError::MailboxFull);
    }

    #[tokio::test]
    async fn test_mailbox_fifo_ordering() {
        let (tx, mut rx) = mailbox::<TestMsg>(10);
        for i in 0..5 {
            tx.send(Envelope::Tell(TestMsg(format!("msg-{i}"))))
                .unwrap();
        }
        for i in 0..5 {
            match rx.recv().await.unwrap() {
                Envelope::Tell(TestMsg(s)) => assert_eq!(s, format!("msg-{i}")),
                _ => panic!("expected Tell"),
            }
        }
    }

    #[tokio::test]
    async fn test_mailbox_sender_clone() {
        let (tx1, mut rx) = mailbox::<TestMsg>(10);
        let tx2 = tx1.clone();
        tx1.send(Envelope::Tell(TestMsg("from-1".into()))).unwrap();
        tx2.send(Envelope::Tell(TestMsg("from-2".into()))).unwrap();

        let m1 = rx.recv().await.unwrap();
        let m2 = rx.recv().await.unwrap();
        match (m1, m2) {
            (Envelope::Tell(TestMsg(a)), Envelope::Tell(TestMsg(b))) => {
                assert_eq!(a, "from-1");
                assert_eq!(b, "from-2");
            }
            _ => panic!("expected Tell"),
        }
    }

    #[tokio::test]
    async fn test_mailbox_closed_on_receiver_drop() {
        let (tx, rx) = mailbox::<TestMsg>(10);
        drop(rx);
        let result = tx.send(Envelope::Tell(TestMsg("orphan".into())));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), ActorError::SendFailed);
    }

    #[tokio::test]
    async fn a_halt_on_a_full_mailbox_is_received_before_the_queue() {
        let (tx, mut rx) = mailbox::<TestMsg>(1);
        tx.send(Envelope::Tell(TestMsg("queued".into()))).unwrap();
        assert_eq!(
            tx.send(Envelope::Tell(TestMsg("no room".into())))
                .unwrap_err(),
            ActorError::MailboxFull
        );
        tx.halt();
        assert!(tx.is_halted());
        assert!(matches!(
            rx.recv().await,
            Some(Envelope::Signal(SystemSignal::Stop))
        ));
        drop(rx);
        tx.closed().await;
    }

    #[tokio::test]
    async fn test_mailbox_depth_empty() {
        let (tx, _rx) = mailbox::<TestMsg>(10);
        assert_eq!(tx.depth(), 0);
        assert_eq!(tx.utilization(), 0.0);
    }

    #[tokio::test]
    async fn test_mailbox_depth_after_sends() {
        let (tx, _rx) = mailbox::<TestMsg>(10);
        tx.send(Envelope::Tell(TestMsg("a".into()))).unwrap();
        tx.send(Envelope::Tell(TestMsg("b".into()))).unwrap();
        tx.send(Envelope::Tell(TestMsg("c".into()))).unwrap();
        assert_eq!(tx.depth(), 3);
        assert!((tx.utilization() - 0.3).abs() < 0.01);
    }

    #[tokio::test]
    async fn test_mailbox_depth_full() {
        let (tx, _rx) = mailbox::<TestMsg>(3);
        tx.send(Envelope::Tell(TestMsg("1".into()))).unwrap();
        tx.send(Envelope::Tell(TestMsg("2".into()))).unwrap();
        tx.send(Envelope::Tell(TestMsg("3".into()))).unwrap();
        assert_eq!(tx.depth(), 3);
        assert_eq!(tx.utilization(), 1.0);
    }

    #[tokio::test]
    async fn test_mailbox_depth_after_recv() {
        let (tx, mut rx) = mailbox::<TestMsg>(10);
        tx.send(Envelope::Tell(TestMsg("a".into()))).unwrap();
        tx.send(Envelope::Tell(TestMsg("b".into()))).unwrap();
        let _ = rx.recv().await;
        assert_eq!(tx.depth(), 1);
    }
}
