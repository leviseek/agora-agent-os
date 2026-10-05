//! Bus contract and the stream handle handed to subscribers.

use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{EventFilter, EventRecord, NewEvent};
use async_trait::async_trait;
use tokio::sync::broadcast;

/// A live subscription. Lagging subscribers receive Lagged errors instead of stalling publishers.
pub struct Subscription {
    inner: broadcast::Receiver<EventRecord>,
    filter: EventFilter,
}

impl Subscription {
    pub fn new(inner: broadcast::Receiver<EventRecord>, filter: EventFilter) -> Self {
        Self { inner, filter }
    }

    /// Next event matching the subscription filter.
    pub async fn recv(&mut self) -> Result<EventRecord> {
        loop {
            match self.inner.recv().await {
                Ok(e) => {
                    if self.filter.matches(&e) {
                        return Ok(e);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    return Err(RuntimeError::unavailable(format!(
                        "event subscriber lagged by {n} events"
                    ))
                    .retryable(true));
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(RuntimeError::cancelled("event bus closed"));
                }
            }
        }
    }

    /// Non-blocking poll, used by the WebSocket fan-out loop.
    pub fn try_recv(&mut self) -> Option<EventRecord> {
        loop {
            match self.inner.try_recv() {
                Ok(e) => {
                    if self.filter.matches(&e) {
                        return Some(e);
                    }
                }
                Err(_) => return None,
            }
        }
    }
}

/// Type alias kept for readability at call sites.
pub type EventStream = Subscription;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BusStats {
    pub published: u64,
    pub subscribers: usize,
    pub last_seq: u64,
    pub buffered: usize,
}

/// The contract every plane depends on.
#[async_trait]
pub trait EventBus: Send + Sync + 'static {
    /// Persist and fan out. Returns the stored record including its assigned sequence.
    async fn publish(&self, event: NewEvent) -> Result<EventRecord>;

    /// Fan out without persisting.
    ///
    /// For a live preview: a caller that relays every token of a streaming answer needs its
    /// subscribers to see each one, and needs the log to stay a record of what happened rather
    /// than of how it looked while it was happening. Ephemeral events keep their own sequence so a
    /// client can still order them, and they are simply absent from replay.
    async fn publish_ephemeral(&self, event: NewEvent) -> Result<EventRecord> {
        self.publish(event).await
    }

    /// Live subscription with an optional filter.
    fn subscribe(&self, filter: EventFilter) -> Subscription;

    /// Hot ring buffer, newest last.
    async fn recent(&self, limit: usize) -> Result<Vec<EventRecord>>;

    /// Durable replay straight from the store.
    async fn replay(&self, filter: EventFilter) -> Result<Vec<EventRecord>>;

    async fn last_seq(&self) -> Result<u64>;

    fn stats(&self) -> BusStats;
}
