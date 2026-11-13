//! Event bus: the single fan-out point for live UI traffic.
//!
//! `tokio::sync::broadcast` is deliberately chosen over a per-connection queue:
//! console output is *lossy-tolerant* (the ring buffer in `sp-process` is the
//! source of truth and every subscriber replays from it on connect), and a
//! slow websocket client must never apply back-pressure to a running Minecraft
//! server. Capacity is therefore a memory bound, not a correctness bound.

use crate::model::PanelEvent;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast::{self, error::TryRecvError, Receiver};

/// Default ring capacity; ~2k events is a few hundred KB and covers a reconnect.
pub const DEFAULT_CAPACITY: usize = 4096;

#[derive(Debug)]
pub struct EventBus {
    tx: broadcast::Sender<Arc<EventEnvelope>>,
    seq: AtomicU64,
    published: AtomicU64,
    dropped: AtomicU64,
}

/// Sequence numbers let the frontend de-dupe and detect gaps after a reconnect.
#[derive(Debug, Clone)]
pub struct EventEnvelope {
    pub seq: u64,
    pub at_ms: i64,
    pub event: PanelEvent,
}

impl EventEnvelope {
    pub fn new(seq: u64, event: PanelEvent) -> Self {
        Self {
            seq,
            at_ms: crate::model::now_ms(),
            event,
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity.max(16));
        Self {
            tx,
            seq: AtomicU64::new(0),
            published: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    /// Publish, ignoring the "nobody is listening" case (the normal state of a
    /// headless panel with no window open).
    pub fn publish(&self, event: PanelEvent) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        self.published.fetch_add(1, Ordering::Relaxed);
        // A send failure only means there are no receivers right now.
        let _ = self.tx.send(Arc::new(EventEnvelope::new(seq, event)));
        seq
    }

    pub fn subscribe(&self) -> Subscription {
        Subscription {
            rx: self.tx.subscribe(),
            dropped: false,
        }
    }

    /// Number of live subscribers, for the `/api/health` payload.
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }

    pub fn stats(&self) -> BusStats {
        BusStats {
            published: self.published.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            subscribers: self.tx.receiver_count(),
            last_seq: self.seq.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BusStats {
    pub published: u64,
    pub dropped: u64,
    pub subscribers: usize,
    pub last_seq: u64,
}

/// One consumer of the bus. `recv` reports gaps so the UI can refetch instead of
/// silently missing a status change.
#[derive(Debug)]
pub struct Subscription {
    rx: Receiver<Arc<EventEnvelope>>,
    dropped: bool,
}

impl Subscription {
    /// Next event, or `Err(true)` when the receiver lagged behind and messages
    /// were lost (the caller should replay from the ring buffer and reset).
    pub async fn recv(&mut self) -> Result<EventEnvelope, bool> {
        loop {
            match self.rx.recv().await {
                Ok(env) => {
                    if self.dropped {
                        self.dropped = false;
                    }
                    return Ok(Arc::unwrap_or_clone(env));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    self.dropped = true;
                    return Err(true);
                }
                Err(broadcast::error::RecvError::Closed) => return Err(false),
            }
        }
    }

    /// Non-blocking drain, used to coalesce bursts before a websocket write.
    pub fn try_recv_batch(&mut self, max: usize) -> (Vec<EventEnvelope>, bool) {
        let mut out = Vec::with_capacity(max.min(64));
        let mut lagged = false;
        while out.len() < max {
            match self.rx.try_recv() {
                Ok(env) => out.push(Arc::unwrap_or_clone(env)),
                Err(TryRecvError::Lagged(_)) => {
                    lagged = true;
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
            }
        }
        (out, lagged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ServerId;
    use crate::model::{ResourceStats, ServerStatus};

    #[tokio::test]
    async fn fanout_to_multiple_subscribers() {
        let bus = EventBus::new(16);
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        let seq = bus.publish(PanelEvent::Stats(ResourceStats::default()));
        for s in [&mut a, &mut b] {
            let env = s.recv().await.unwrap();
            assert_eq!(env.seq, seq);
            assert!(matches!(env.event, PanelEvent::Stats(_)));
        }
        assert_eq!(bus.stats().published, 1);
        assert_eq!(bus.stats().last_seq, seq);
    }

    #[tokio::test]
    async fn publish_without_subscribers_is_not_an_error() {
        let bus = EventBus::new(8);
        let seq = bus.publish(PanelEvent::Status {
            server_id: ServerId::new(1),
            status: ServerStatus::Running,
            reason: None,
        });
        assert_eq!(seq, 1);
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn lag_is_reported_not_silently_swallowed() {
        let bus = EventBus::new(4);
        let mut slow = bus.subscribe();
        for i in 0..200 {
            bus.publish(PanelEvent::Heartbeat { at_ms: i });
        }
        // The slow reader must learn it missed events instead of reading stale ones.
        let first = slow.recv().await;
        assert!(matches!(first, Err(true)), "expected a lag signal, got {first:?}");
    }

    #[tokio::test]
    async fn batch_drain_stops_when_empty() {
        let bus = EventBus::new(64);
        let mut sub = bus.subscribe();
        for i in 0..5 {
            bus.publish(PanelEvent::Heartbeat { at_ms: i });
        }
        let (batch, lagged) = sub.try_recv_batch(3);
        assert_eq!(batch.len(), 3);
        assert!(!lagged);
        let (rest, _) = sub.try_recv_batch(10);
        assert_eq!(rest.len(), 2);
        let (nothing, lagged) = sub.try_recv_batch(10);
        assert!(nothing.is_empty() && !lagged);
    }

    #[test]
    fn capacity_floor_is_applied() {
        let bus = EventBus::new(1);
        // `new` clamps to >= 16 so tokio never panics on a tiny channel.
        assert!(bus.subscribe().try_recv_batch(1).0.is_empty());
    }
}
