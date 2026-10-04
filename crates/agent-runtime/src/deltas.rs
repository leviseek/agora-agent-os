//! Streaming deltas: a best-effort preview of an answer that is still being written.
//!
//! Deltas are deliberately NOT part of the run. They are published on the event bus as they
//! arrive, so a console can render them, but nothing in the run waits for that to happen: a slow
//! subscriber slows nobody down, and a dropped delta costs a redraw, not an answer. The
//! authoritative text is always the response the model call returns.
//!
//! They are batched on a timer rather than published one by one, because a language model emits
//! hundreds of tiny chunks and turning each into an event would fill the event log with noise.

use agentos_core::model::{EventKind, NewEvent};
use agentos_core::SessionId;
use agentos_event_bus::EventBus;
use parking_lot::Mutex;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Somewhere to send a delta. Synchronous on purpose: a model call must never await a subscriber.
pub type DeltaSink = Arc<dyn Fn(String) + Send + Sync>;

/// Publishes accumulated deltas for one run.
pub struct DeltaPublisher {
    sink: DeltaSink,
    buffer: Arc<Mutex<String>>,
    stop: CancellationToken,
    ticker: JoinHandle<()>,
    bus: Arc<dyn EventBus>,
    session: SessionId,
    run_id: String,
    node: String,
}

impl DeltaPublisher {
    /// Start publishing for one run. `interval_ms` is how long deltas accumulate before a batch
    /// goes out: short enough to look live, long enough not to flood the log.
    pub fn start(
        bus: Arc<dyn EventBus>,
        session: SessionId,
        run_id: String,
        node: String,
        interval_ms: u64,
    ) -> Self {
        let buffer = Arc::new(Mutex::new(String::new()));
        let stop = CancellationToken::new();
        let ticker = {
            let buffer = buffer.clone();
            let stop = stop.clone();
            let bus = bus.clone();
            let session = session.clone();
            let run_id = run_id.clone();
            let node = node.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => break,
                        _ = tokio::time::sleep(Duration::from_millis(interval_ms.max(10))) => {}
                    }
                    let text = {
                        let mut guard = buffer.lock();
                        if guard.is_empty() {
                            continue;
                        }
                        std::mem::take(&mut *guard)
                    };
                    let _ = bus
                        .publish(
                            NewEvent::new(EventKind::AgentDelta, "model delta")
                                .session(session.clone())
                                .node(node.clone())
                                .payload(json!({ "run_id": run_id, "text": text })),
                        )
                        .await;
                }
            })
        };
        let sink: DeltaSink = {
            let buffer = buffer.clone();
            Arc::new(move |text: String| buffer.lock().push_str(&text))
        };
        Self { sink, buffer, stop, ticker, bus, session, run_id, node }
    }

    pub fn sink(&self) -> DeltaSink {
        self.sink.clone()
    }

    /// Stop the ticker and publish whatever is left, so the last words are never lost.
    pub async fn finish(self) {
        self.stop.cancel();
        let _ = self.ticker.await;
        let text = {
            let mut guard = self.buffer.lock();
            std::mem::take(&mut *guard)
        };
        if !text.is_empty() {
            let _ = self
                .bus
                .publish(
                    NewEvent::new(EventKind::AgentDelta, "model delta")
                        .session(self.session.clone())
                        .node(self.node.clone())
                        .payload(json!({ "run_id": self.run_id, "text": text })),
                )
                .await;
        }
    }
}