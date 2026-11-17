//! Chord v2 links (upstream `src/link/link.ts`): an ordered, bidirectional
//! channel of byte messages between two hosts. Adapters only move bytes:
//! a transport hands a link what arrived and sends what the link gives it.
//!
//! Closing either end closes both: the closing end at once, the peer when
//! its transport sees the end.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, watch};

static NEXT_LINK: AtomicU64 = AtomicU64::new(1);

struct Inner {
    id: u64,
    /// What this end sends; `None` once closed.
    outbound: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    /// What arrives, taken once by the host that runs the link.
    inbound: Mutex<Option<mpsc::UnboundedReceiver<Vec<u8>>>>,
    closed: watch::Sender<bool>,
}

/// One end of a link. Cloning shares the end.
#[derive(Clone)]
pub struct Link {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link")
            .field("id", &self.inner.id)
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl Link {
    /// A link over a transport: `outbound` takes what this end sends (its
    /// close is the transport's signal to end), `inbound` gives what arrived
    /// (its end closes the link).
    pub fn new(
        outbound: mpsc::UnboundedSender<Vec<u8>>,
        inbound: mpsc::UnboundedReceiver<Vec<u8>>,
    ) -> Link {
        Link {
            inner: Arc::new(Inner {
                id: NEXT_LINK.fetch_add(1, Ordering::Relaxed),
                outbound: Mutex::new(Some(outbound)),
                inbound: Mutex::new(Some(inbound)),
                closed: watch::channel(false).0,
            }),
        }
    }

    /// Two connected ends in one process.
    pub fn memory_pair() -> (Link, Link) {
        let (a_out, b_in) = mpsc::unbounded_channel();
        let (b_out, a_in) = mpsc::unbounded_channel();
        (Link::new(a_out, a_in), Link::new(b_out, b_in))
    }

    /// A process-unique id.
    pub fn id(&self) -> u64 {
        self.inner.id
    }

    pub fn is_closed(&self) -> bool {
        *self.inner.closed.borrow()
    }

    /// Sends one message; nothing once closed.
    pub fn send(&self, message: Vec<u8>) {
        if let Some(outbound) = &*self.inner.outbound.lock().expect("link") {
            let _ = outbound.send(message);
        }
    }

    /// Closes both ends; messages not delivered yet may be dropped.
    pub fn close(&self) {
        self.inner.outbound.lock().expect("link").take();
        self.inner.closed.send_replace(true);
    }

    /// Resolves once this end is closed.
    pub async fn closed(&self) {
        let mut closed = self.inner.closed.subscribe();
        let _ = closed.wait_for(|closed| *closed).await;
    }

    /// What arrives, taken once: by the host that runs the link, or by a
    /// peer that reads it itself (tests).
    pub fn take_inbound(&self) -> Option<mpsc::UnboundedReceiver<Vec<u8>>> {
        self.inner.inbound.lock().expect("link").take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bytes_cross_in_order_and_a_close_ends_the_peer() {
        let (a, b) = Link::memory_pair();
        a.send(vec![1]);
        a.send(vec![2, 3]);
        let mut inbound = b.take_inbound().unwrap();
        assert_eq!(inbound.recv().await, Some(vec![1]));
        assert_eq!(inbound.recv().await, Some(vec![2, 3]));
        a.close();
        assert!(a.is_closed());
        a.closed().await;
        assert_eq!(inbound.recv().await, None);
        a.send(vec![4]);
        assert!(b.take_inbound().is_none());
    }
}
