//! Bounded multi-producer multi-consumer channel on a
//! [`crossbeam_queue::ArrayQueue`].
//!
//! [`bounded`] returns a [`Sender`] and an [`Inbox`], both cloneable. Each
//! value sent is received by exactly one `Inbox` clone. [`Sender::send`] waits
//! while the queue holds `capacity` values; [`Inbox::recv`] waits while it is
//! empty. Once every `Sender` is dropped, `recv` returns the values still
//! queued and then [`Error::BusClosed`]. Once every `Inbox` is dropped, `send`
//! returns [`Error::BusNoReceivers`]. `send` and `recv` are cancellation-safe:
//! dropping either future before it completes neither loses a queued value nor
//! leaks a slot.

use std::num::NonZeroUsize;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

use crossbeam_queue::ArrayQueue;
use tokio::sync::{Notify, Semaphore};

use crate::error::{Error, Result};

/// State shared by every `Sender` and `Inbox` of one channel.
struct Shared<T> {
    queue: ArrayQueue<T>,
    /// One permit per free queue slot.
    slots: Semaphore,
    /// Signalled once per pushed value, and for every waiter on close.
    items: Notify,
    senders: AtomicUsize,
    receivers: AtomicUsize,
}

/// Create a channel holding at most `capacity` values.
pub fn bounded<T>(capacity: NonZeroUsize) -> (Sender<T>, Inbox<T>) {
    let capacity = capacity.get();
    let shared = Arc::new(Shared {
        queue: ArrayQueue::new(capacity),
        slots: Semaphore::new(capacity),
        items: Notify::new(),
        senders: AtomicUsize::new(1),
        receivers: AtomicUsize::new(1),
    });
    (
        Sender {
            shared: Arc::clone(&shared),
        },
        Inbox { shared },
    )
}

/// Sending half of a [`bounded`] channel.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Sender<T> {
    /// Push `value`, waiting while the queue is full. Returns
    /// [`Error::BusNoReceivers`], dropping `value`, once every [`Inbox`] is
    /// dropped.
    pub async fn send(&self, value: T) -> Result<()> {
        let permit = self
            .shared
            .slots
            .acquire()
            .await
            .map_err(|_| Error::BusNoReceivers)?;
        permit.forget();
        if self.shared.queue.push(value).is_err() {
            unreachable!("invariant: a slot permit reserves a free queue slot");
        }
        self.shared.items.notify_one();
        Ok(())
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.shared.senders.fetch_add(1, SeqCst);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.shared.senders.fetch_sub(1, SeqCst) == 1 {
            self.shared.items.notify_waiters();
        }
    }
}

/// Receiving half of a [`bounded`] channel.
pub struct Inbox<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Inbox<T> {
    /// Pop the next value, waiting while the queue is empty and a [`Sender`]
    /// exists. Returns [`Error::BusClosed`] once every `Sender` is dropped and
    /// the queue is empty.
    pub async fn recv(&self) -> Result<T> {
        loop {
            let mut notified = pin!(self.shared.items.notified());
            notified.as_mut().enable();
            let closed = self.shared.senders.load(SeqCst) == 0;
            if let Some(value) = self.shared.queue.pop() {
                self.shared.slots.add_permits(1);
                return Ok(value);
            }
            if closed {
                return Err(Error::BusClosed);
            }
            notified.await;
        }
    }

    /// [`Inbox::recv`] that blocks the calling thread. Call it from a thread
    /// outside the async runtime's workers, such as a
    /// `tokio::task::spawn_blocking` closure.
    pub fn blocking_recv(&self) -> Result<T> {
        futures::executor::block_on(self.recv())
    }
}

impl<T> Clone for Inbox<T> {
    fn clone(&self) -> Self {
        self.shared.receivers.fetch_add(1, SeqCst);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Drop for Inbox<T> {
    fn drop(&mut self) {
        if self.shared.receivers.fetch_sub(1, SeqCst) == 1 {
            self.shared.slots.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use tokio::time::{Duration, timeout};

    use super::*;

    const CAP: NonZeroUsize = NonZeroUsize::new(4).expect("4 is non-zero");
    const BLOCKED: Duration = Duration::from_millis(100);
    const BOUND: Duration = Duration::from_secs(5);

    /// A send at capacity stays pending until one `recv` frees a slot.
    #[tokio::test]
    async fn send_blocks_at_capacity_until_one_recv() {
        let (tx, rx) = bounded::<usize>(CAP);
        for i in 0..CAP.get() {
            tx.send(i).await.expect("send below capacity");
        }
        let blocked = timeout(BLOCKED, tx.send(99)).await;
        assert!(
            blocked.is_err(),
            "send at capacity {} must stay pending; it resolved to {blocked:?}",
            CAP.get()
        );

        let first = rx.recv().await.expect("recv a queued value");
        assert_eq!(first, 0, "values are received in send order");
        timeout(BOUND, tx.send(99))
            .await
            .expect("send must unblock after one recv frees a slot")
            .expect("receivers exist");
    }

    /// After the last sender drops, queued values drain in order and then
    /// `recv` returns `BusClosed`.
    #[tokio::test]
    async fn recv_drains_then_closes_after_last_sender_drops() {
        let (tx, rx) = bounded::<usize>(CAP);
        let tx2 = tx.clone();
        tx.send(1).await.expect("send");
        tx2.send(2).await.expect("send");
        drop(tx);
        drop(tx2);

        let mut drained = Vec::new();
        let end = loop {
            match timeout(BOUND, rx.recv()).await {
                Ok(Ok(v)) => drained.push(v),
                Ok(Err(e)) => break e,
                Err(_) => panic!("recv hung after close; drained {drained:?}"),
            }
        };
        assert_eq!(drained, vec![1, 2], "queued values must drain before close");
        assert!(
            matches!(end, Error::BusClosed),
            "expected BusClosed after draining, got {end:?}"
        );
    }

    /// A `recv` parked on an empty queue wakes with `BusClosed` when the last
    /// sender drops.
    #[tokio::test]
    async fn parked_recv_wakes_on_close() {
        let (tx, rx) = bounded::<usize>(CAP);
        let parked = tokio::spawn(async move { rx.recv().await });
        tokio::time::sleep(BLOCKED).await;
        drop(tx);
        let res = timeout(BOUND, parked)
            .await
            .expect("parked recv must wake on close")
            .expect("recv task");
        assert!(
            matches!(res, Err(Error::BusClosed)),
            "expected BusClosed, got {res:?}"
        );
    }

    /// A `recv` parked on an empty queue wakes when a value is sent.
    #[tokio::test]
    async fn parked_recv_wakes_on_send() {
        let (tx, rx) = bounded::<usize>(CAP);
        let parked = tokio::spawn(async move { rx.recv().await });
        tokio::time::sleep(BLOCKED).await;
        tx.send(7).await.expect("send");
        let res = timeout(BOUND, parked)
            .await
            .expect("parked recv must wake on send")
            .expect("recv task");
        assert!(matches!(res, Ok(7)), "expected Ok(7), got {res:?}");
    }

    /// `send` returns `BusNoReceivers` once every inbox is dropped, including
    /// a send already parked on a full queue.
    #[tokio::test]
    async fn send_fails_after_last_inbox_drops() {
        let (tx, rx) = bounded::<usize>(CAP);
        for i in 0..CAP.get() {
            tx.send(i).await.expect("send below capacity");
        }
        let tx2 = tx.clone();
        let parked = tokio::spawn(async move { tx2.send(99).await });
        tokio::time::sleep(BLOCKED).await;
        drop(rx);
        let res = timeout(BOUND, parked)
            .await
            .expect("parked send must wake when receivers drop")
            .expect("send task");
        assert!(
            matches!(res, Err(Error::BusNoReceivers)),
            "expected BusNoReceivers, got {res:?}"
        );
    }

    /// Under several producers and consumers on a multi-thread runtime, every
    /// value is received exactly once and every consumer ends on close.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mpmc_delivers_each_value_exactly_once() {
        const PRODUCERS: usize = 4;
        const CONSUMERS: usize = 4;
        const PER_PRODUCER: usize = 5_000;

        let (tx, rx) = bounded::<usize>(CAP);
        let consumers: Vec<_> = (0..CONSUMERS)
            .map(|_| {
                let rx = rx.clone();
                tokio::spawn(async move {
                    let mut got = Vec::new();
                    while let Ok(v) = rx.recv().await {
                        got.push(v);
                    }
                    got
                })
            })
            .collect();
        drop(rx);
        let producers: Vec<_> = (0..PRODUCERS)
            .map(|p| {
                let tx = tx.clone();
                tokio::spawn(async move {
                    for i in 0..PER_PRODUCER {
                        tx.send(p * PER_PRODUCER + i).await.expect("send");
                    }
                })
            })
            .collect();
        drop(tx);

        for p in producers {
            timeout(BOUND, p)
                .await
                .expect("producer stalled")
                .expect("producer task");
        }
        let mut seen = HashSet::new();
        let mut total = 0usize;
        for c in consumers {
            let got = timeout(BOUND, c)
                .await
                .expect("consumer stalled: a recv missed a wakeup or the close")
                .expect("consumer task");
            total += got.len();
            seen.extend(got);
        }
        let expected = PRODUCERS * PER_PRODUCER;
        assert_eq!(total, expected, "received {total} values, sent {expected}");
        assert_eq!(
            seen.len(),
            expected,
            "received {} distinct values, sent {expected}",
            seen.len()
        );
    }

    /// `blocking_recv` on an OS thread receives the async-sent values in order
    /// and then `BusClosed`.
    #[tokio::test]
    async fn blocking_recv_receives_then_closes() {
        let (tx, rx) = bounded::<usize>(CAP);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let mut got = Vec::new();
            let end = loop {
                match rx.blocking_recv() {
                    Ok(v) => got.push(v),
                    Err(e) => break e,
                }
            };
            let _ = done_tx.send((got, end));
        });
        for i in 0..10 {
            timeout(BOUND, tx.send(i))
                .await
                .unwrap_or_else(|_| panic!("send {i} stalled: blocking_recv freed no slot"))
                .expect("send");
        }
        drop(tx);
        let (got, end) = timeout(BOUND, done_rx)
            .await
            .expect("blocking worker stalled")
            .expect("blocking worker result");
        assert_eq!(got, (0..10).collect::<Vec<_>>(), "blocking_recv order");
        assert!(
            matches!(end, Error::BusClosed),
            "expected BusClosed, got {end:?}"
        );
    }
}
