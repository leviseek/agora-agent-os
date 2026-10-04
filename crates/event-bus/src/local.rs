//! In-process bus backed by the Store log plus a broadcast fan-out.

use crate::bus::{BusStats, EventBus, Subscription};
use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{EventFilter, EventRecord, NewEvent};
use agentos_core::telemetry::{metric_names, metrics};
use agentos_core::{now_ms, EventId};
use agentos_storage::store::{collections, Store};
use async_trait::async_trait;
use parking_lot::RwLock;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;

/// The largest window a selective replay will scan before giving up. Bounded so a filter that
/// matches nothing cannot turn into an unbounded scan of the log.
const MAX_REPLAY_WINDOW: usize = 50_000;

pub struct LocalEventBus {
    store: Arc<dyn Store>,
    tx: broadcast::Sender<EventRecord>,
    ring: RwLock<VecDeque<EventRecord>>,
    ring_capacity: usize,
    /// Cached last sequence; the store is the source of truth on restart.
    last_seq: AtomicU64,
    node_id: String,
    published: AtomicU64,
    /// Serialises sequence assignment with the append that makes it real.
    ///
    /// Assigning a number and then appending are two steps, and two publishers that interleave
    /// between them both believe they own the same number: the log then holds two events with the
    /// same seq while the in-memory copies hold corrected ones. An event log whose ordering
    /// metadata can collide cannot be resumed reliably, and publishing is already I/O-bound, so a
    /// lock here costs nothing worth measuring.
    publish_lock: tokio::sync::Mutex<()>,
}

impl LocalEventBus {
    /// Create a bus. The ring capacity bounds the hot window used by the UI.
    pub async fn new(store: Arc<dyn Store>, ring_capacity: usize, node_id: impl Into<String>) -> Self {
        let last = store.last_seq(collections::LOG_EVENTS).await.unwrap_or(0);
        let (tx, _) = broadcast::channel(2048);
        Self {
            store,
            tx,
            ring: RwLock::new(VecDeque::with_capacity(ring_capacity.min(4096))),
            ring_capacity,
            last_seq: AtomicU64::new(last),
            node_id: node_id.into(),
            published: AtomicU64::new(0),
            publish_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Warm the in-memory ring from durable storage at startup.
    pub async fn warm(&self, limit: usize) -> Result<usize> {
        let from = self.last_seq.load(Ordering::SeqCst).saturating_sub(limit as u64) + 1;
        let rows = self.store.read_events(collections::LOG_EVENTS, from, limit).await?;
        let mut ring = self.ring.write();
        ring.clear();
        let mut n = 0;
        for (_, v) in rows {
            if let Ok(rec) = serde_json::from_value::<EventRecord>(v) {
                ring.push_back(rec);
                n += 1;
            }
        }
        Ok(n)
    }

    pub fn store(&self) -> Arc<dyn Store> {
        self.store.clone()
    }
}

#[async_trait]
impl EventBus for LocalEventBus {
    async fn publish(&self, event: NewEvent) -> Result<EventRecord> {
        let _guard = self.publish_lock.lock().await;
        let seq = self.last_seq.load(Ordering::SeqCst) + 1;
        let record = EventRecord {
            id: EventId::new(),
            seq,
            kind: event.kind,
            severity: event.severity,
            ts: now_ms(),
            message: event.message,
            payload: event.payload,
            node_id: Some(event.node_id.unwrap_or_else(|| self.node_id.clone())),
            correlation_id: event.correlation_id,
            session_id: event.session_id,
            actor_id: event.actor_id,
            agent_id: event.agent_id,
            task_id: event.task_id,
            capability_id: event.capability_id,
            worker_id: event.worker_id,
            artifact_id: event.artifact_id,
        };

        // The record is serialised with the sequence this publisher holds under the lock, so the
        // copy on disk and the copy subscribers see carry the same number. The previous version
        // wrote a pre-append guess to disk and corrected only the in-memory copy, which is how a
        // log ended up with two events sharing a seq.
        let value = serde_json::to_value(&record)
            .map_err(|e| RuntimeError::internal(format!("serialize event: {e}")))?;
        let stored_seq = self.store.append_event(collections::LOG_EVENTS, value).await?;
        if stored_seq != seq {
            // Only a second writer on the same log can cause this, which the file store does not
            // support anyway. Say so instead of silently renumbering.
            tracing::warn!(
                expected = seq,
                stored = stored_seq,
                "another writer advanced the event log; sequencing is not guaranteed"
            );
        }
        self.last_seq.store(stored_seq, Ordering::SeqCst);
        let mut record = record;
        record.seq = stored_seq;

        {
            let mut ring = self.ring.write();
            if ring.len() >= self.ring_capacity {
                ring.pop_front();
            }
            ring.push_back(record.clone());
        }

        self.published.fetch_add(1, Ordering::Relaxed);
        metrics().inc(metric_names::EVENTS_PUBLISHED, 1);
        // A send error only means nobody is listening, which is a normal condition.
        let _ = self.tx.send(record.clone());
        Ok(record)
    }

    fn subscribe(&self, filter: EventFilter) -> Subscription {
        Subscription::new(self.tx.subscribe(), filter)
    }

    async fn recent(&self, limit: usize) -> Result<Vec<EventRecord>> {
        let ring = self.ring.read();
        let skip = ring.len().saturating_sub(limit);
        Ok(ring.iter().skip(skip).cloned().collect())
    }

    async fn replay(&self, filter: EventFilter) -> Result<Vec<EventRecord>> {
        let limit = if filter.limit == 0 { 1000 } else { filter.limit };
        let from = filter.after_seq.map(|s| s + 1).unwrap_or(1);

        // The limit applies to what the caller asked FOR, not to how far we are willing to look.
        // Reading "the last N events overall, then filtering" silently returns too few: a session's
        // three events can sit just outside a window filled by heartbeats from other sessions. So
        // the window grows until the caller has what it asked for or the log runs out.
        let mut window = limit.max(256);
        loop {
            let rows = self.store.read_events(collections::LOG_EVENTS, from, window).await?;
            let exhausted = rows.len() < window;
            let mut out = Vec::new();
            for (_, value) in rows {
                let record: EventRecord = serde_json::from_value(value)
                    .map_err(|e| RuntimeError::storage(format!("corrupt event record: {e}")))?;
                if filter.matches(&record) {
                    out.push(record);
                    if out.len() >= limit {
                        return Ok(out);
                    }
                }
            }
            if exhausted || window >= MAX_REPLAY_WINDOW {
                return Ok(out);
            }
            window = (window * 4).min(MAX_REPLAY_WINDOW);
        }
    }

    async fn last_seq(&self) -> Result<u64> {
        Ok(self.last_seq.load(Ordering::SeqCst))
    }

    fn stats(&self) -> BusStats {
        BusStats {
            published: self.published.load(Ordering::Relaxed),
            subscribers: self.tx.receiver_count(),
            last_seq: self.last_seq.load(Ordering::SeqCst),
            buffered: self.ring.read().len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::model::{EventFilter, EventKind};
    use agentos_storage::memory::MemoryStore;

    async fn bus() -> LocalEventBus {
        LocalEventBus::new(Arc::new(MemoryStore::new()), 64, "node-test").await
    }

    #[tokio::test]
    async fn a_selective_replay_is_not_limited_by_unrelated_traffic() {
        // The bug this pins: three events for one session, buried under a thousand heartbeats from
        // everywhere else. Asking for that session's events with limit 10 must return three, not
        // "the last ten events overall, none of which happen to be yours".
        let b = bus().await;
        let wanted = agentos_core::SessionId::new();
        for index in 0..1_000 {
            b.publish(NewEvent::new(EventKind::WorkerHeartbeat, format!("noise {index}")))
                .await
                .unwrap();
        }
        for _ in 0..3 {
            b.publish(NewEvent::new(EventKind::SessionMessageHandled, "mine").session(wanted.clone()))
                .await
                .unwrap();
        }
        for index in 0..1_000 {
            b.publish(NewEvent::new(EventKind::WorkerHeartbeat, format!("noise after {index}")))
                .await
                .unwrap();
        }

        let found = b
            .replay(EventFilter {
                session_id: Some(wanted.clone()),
                kinds: vec![EventKind::SessionMessageHandled],
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(found.len(), 3, "the filter decides what is returned, not the window");

        // And the limit still limits what matches.
        let limited = b
            .replay(EventFilter {
                session_id: Some(wanted),
                limit: 2,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(limited.len(), 2);
    }


    #[tokio::test]
    async fn concurrent_publishes_get_distinct_sequences_on_disk_too() {
        // Ordering metadata that can collide cannot be resumed reliably: a caller resuming from
        // seq N would skip whichever event shares it. This used to happen because the sequence
        // was guessed before the append and only the in-memory copy was corrected.
        let b = Arc::new(bus().await);
        let mut tasks = Vec::new();
        for index in 0..50 {
            let bus = b.clone();
            tasks.push(tokio::spawn(async move {
                bus.publish(NewEvent::new(EventKind::WorkerHeartbeat, format!("n{index}")))
                    .await
                    .unwrap()
            }));
        }
        let mut seqs = Vec::new();
        for task in tasks {
            seqs.push(task.await.unwrap().seq);
        }
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 50, "every publisher got its own sequence: {seqs:?}");

        // And what was written to the log carries the same numbers as what publishers were told.
        let stored = b
            .replay(EventFilter { limit: 100, ..Default::default() })
            .await
            .unwrap();
        let mut stored_seqs: Vec<u64> = stored.iter().map(|event| event.seq).collect();
        stored_seqs.sort_unstable();
        stored_seqs.dedup();
        assert_eq!(
            stored_seqs.len(),
            50,
            "the durable log must not contain duplicate sequences either"
        );
    }

    #[tokio::test]
    async fn publish_assigns_monotonic_sequences() {
        let b = bus().await;
        let a = b.publish(NewEvent::new(EventKind::SessionCreated, "one")).await.unwrap();
        let c = b.publish(NewEvent::new(EventKind::SessionCreated, "two")).await.unwrap();
        assert_eq!(a.seq, 1);
        assert_eq!(c.seq, 2);
        assert!(a.id.as_str().starts_with("evt_"));
    }

    #[tokio::test]
    async fn live_subscribers_receive_events() {
        let b = bus().await;
        let mut sub = b.subscribe(EventFilter::default());
        b.publish(NewEvent::new(EventKind::TaskStarted, "go")).await.unwrap();
        let got = tokio::time::timeout(std::time::Duration::from_millis(500), sub.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.kind, EventKind::TaskStarted);
    }

    #[tokio::test]
    async fn filters_restrict_the_stream() {
        let b = bus().await;
        let mut sub = b.subscribe(EventFilter { kinds: vec![EventKind::ToolCall], ..Default::default() });
        b.publish(NewEvent::new(EventKind::TaskStarted, "ignored")).await.unwrap();
        b.publish(NewEvent::new(EventKind::ToolCall, "wanted")).await.unwrap();
        let got = tokio::time::timeout(std::time::Duration::from_millis(500), sub.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.message, "wanted");
    }

    #[tokio::test]
    async fn replay_reads_durable_history_after_restart() {
        let store = Arc::new(MemoryStore::new());
        {
            let b = LocalEventBus::new(store.clone(), 8, "n1").await;
            b.publish(NewEvent::new(EventKind::SessionCreated, "a")).await.unwrap();
            b.publish(NewEvent::new(EventKind::RunCompleted, "b")).await.unwrap();
        }
        let b2 = LocalEventBus::new(store.clone(), 8, "n1").await;
        assert_eq!(b2.last_seq().await.unwrap(), 2);
        let all = b2.replay(EventFilter { limit: 10, ..Default::default() }).await.unwrap();
        assert_eq!(all.len(), 2);
        let only = b2
            .replay(EventFilter { kinds: vec![EventKind::RunCompleted], limit: 10, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].message, "b");
    }
}
