//! Streaming deltas: a live preview of an answer that is still being written.
//!
//! Deltas are deliberately NOT part of the run. They are forwarded to the event bus as they
//! arrive, so a console can render them, but nothing in the run waits for that: a slow subscriber
//! slows nobody down, and a dropped delta costs a redraw, not an answer. The authoritative text is
//! always the response the model call returns.
//!
//! Every chunk is forwarded on arrival - no timer, no batching. A timer is what made a streaming
//! answer appear in lumps: the provider had already emitted several tokens while the publisher was
//! waiting for its next tick. What keeps that from flooding the log is that previews are published
//! as ephemeral events, which reach subscribers but are never written down.

use agentos_core::model::{EventKind, NewEvent};
use agentos_core::SessionId;
use agentos_event_bus::EventBus;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Somewhere to send a delta. Synchronous on purpose: a model call must never await a subscriber.
pub type DeltaSink = Arc<dyn Fn(String) + Send + Sync>;

/// Forwards one run's deltas to the bus, one event per chunk.
pub struct DeltaPublisher {
    sink: DeltaSink,
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl DeltaPublisher {
    /// Start forwarding for one run.
    pub fn start(
        bus: Arc<dyn EventBus>,
        session: SessionId,
        run_id: String,
        node: String,
    ) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let stop = CancellationToken::new();
        let task = {
            let stop = stop.clone();
            tokio::spawn(async move {
                let publish = |text: String| {
                    let bus = bus.clone();
                    let session = session.clone();
                    let node = node.clone();
                    let run_id = run_id.clone();
                    async move {
                        if text.is_empty() {
                            return;
                        }
                        let _ = bus
                            .publish_ephemeral(
                                NewEvent::new(EventKind::AgentDelta, "model delta")
                                    .session(session)
                                    .node(node)
                                    .payload(json!({ "run_id": run_id, "text": text })),
                            )
                            .await;
                    }
                };
                loop {
                    tokio::select! {
                        biased;
                        // Stopping is explicit. Waiting for the channel to close would wait forever:
                        // the sink that feeds it is held by the loop for the whole run, so the
                        // channel stays open and the run would never be allowed to finish.
                        _ = stop.cancelled() => {
                            while let Ok(text) = rx.try_recv() {
                                publish(text).await;
                            }
                            break;
                        }
                        received = rx.recv() => match received {
                            Some(text) => publish(text).await,
                            None => break,
                        },
                    }
                }
            })
        };
        let sink: DeltaSink = {
            let tx = tx.clone();
            Arc::new(move |text: String| {
                // The receiver is dropped only when the publisher finishes, so a failed send means
                // the run is over: dropping the preview is the correct response.
                let _ = tx.send(text);
            })
        };
        Self { sink, stop, task }
    }

    pub fn sink(&self) -> DeltaSink {
        self.sink.clone()
    }

    /// Stop forwarding, after letting whatever is queued go out, so the last words are not lost.
    pub async fn finish(self) {
        self.stop.cancel();
        let _ = self.task.await;
    }
}