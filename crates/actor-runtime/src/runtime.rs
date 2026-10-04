//! The actor runtime: mailbox scheduling, lifecycle, checkpoint and recovery.

use crate::actor::{Actor, ActorCommand, ActorContext, ActorFactory, ActorHandle, ActorInit, ErasedActor, TypedActor};
use crate::checkpoint::CheckpointStore;
use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{ActorRecord, Checkpoint, CheckpointMeta, EventFilter, EventKind, EventRecord, NewEvent};
use agentos_core::state::ActorState;
use agentos_core::telemetry::{metric_names, metrics, Correlation};
use agentos_core::{ActorId, CheckpointId, SessionId};
use agentos_event_bus::EventBus;
use agentos_storage::store::{collections, Collection, Store};
use futures::FutureExt;
use parking_lot::RwLock;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct ActorRuntimeConfig {
    pub mailbox_capacity: usize,
    pub node_id: String,
    /// How many historical events a recovered actor will replay at most.
    pub max_replay_events: usize,
}

impl Default for ActorRuntimeConfig {
    fn default() -> Self {
        Self { mailbox_capacity: 256, node_id: "local".into(), max_replay_events: 5_000 }
    }
}

pub struct ActorRuntime {
    store: Arc<dyn Store>,
    bus: Arc<dyn EventBus>,
    checkpoints: Arc<dyn CheckpointStore>,
    cfg: ActorRuntimeConfig,
    actors: RwLock<HashMap<ActorId, ActorHandle>>,
    sessions: RwLock<HashMap<SessionId, ActorId>>,
}

impl ActorRuntime {
    pub fn new(
        store: Arc<dyn Store>,
        bus: Arc<dyn EventBus>,
        checkpoints: Arc<dyn CheckpointStore>,
        cfg: ActorRuntimeConfig,
    ) -> Self {
        Self {
            store,
            bus,
            checkpoints,
            cfg,
            actors: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
        }
    }

    pub fn checkpoints(&self) -> Arc<dyn CheckpointStore> {
        self.checkpoints.clone()
    }

    pub fn bus(&self) -> Arc<dyn EventBus> {
        self.bus.clone()
    }

    pub fn config(&self) -> &ActorRuntimeConfig {
        &self.cfg
    }

    /// Spawn a typed actor. The returned handle is the only way to talk to it.
    pub async fn spawn<A: Actor>(&self, init: ActorInit, actor: A) -> Result<ActorHandle> {
        self.spawn_boxed(init, Box::new(TypedActor::new(actor))).await
    }

    /// Spawn any erased actor. Messages are processed one at a time, in arrival order.
    pub async fn spawn_boxed(&self, init: ActorInit, actor: Box<dyn ErasedActor>) -> Result<ActorHandle> {
        let kind: &'static str = {
            // The kind is a &'static str by contract; leak-free because callers pass literals.
            let k = actor.kind();
            Box::leak(k.to_string().into_boxed_str())
        };
        let (tx, rx) = mpsc::channel::<ActorCommand>(self.cfg.mailbox_capacity);
        let depth = Arc::new(AtomicUsize::new(0));
        let state = Arc::new(RwLock::new(ActorState::Spawning));
        let seq = Arc::new(AtomicU64::new(0));
        let cancellation = CancellationToken::new();

        let handle = ActorHandle::new(
            init.clone(),
            kind,
            tx,
            depth.clone(),
            state.clone(),
            seq.clone(),
            cancellation.clone(),
        );

        {
            let mut actors = self.actors.write();
            if actors.contains_key(&init.actor_id) {
                return Err(RuntimeError::conflict(format!("actor {} already exists", init.actor_id)));
            }
            actors.insert(init.actor_id.clone(), handle.clone());
        }
        self.sessions.write().insert(init.session_id.clone(), init.actor_id.clone());

        let mut record = ActorRecord::new(init.actor_id.clone(), init.session_id.clone(), kind);
        record.generation = init.generation;
        record.state = ActorState::Active;
        self.save_record(&record).await?;

        self.bus
            .publish(
                NewEvent::new(EventKind::ActorSpawned, format!("actor {kind} spawned"))
                    .actor(init.actor_id.clone())
                    .session(init.session_id.clone())
                    .node(self.cfg.node_id.clone())
                    .payload(serde_json::json!({
                        "kind": kind,
                        "generation": init.generation,
                        "worker": self.cfg.node_id,
                    })),
            )
            .await?;

        let bus = self.bus.clone();
        let node = self.cfg.node_id.clone();
        let session_id = init.session_id.clone();
        let actor_id = init.actor_id.clone();
        let generation = init.generation;
        tokio::spawn(async move {
            drive(
                actor,
                rx,
                depth,
                state,
                seq,
                cancellation,
                bus,
                node,
                actor_id,
                session_id,
                generation,
            )
            .await;
        });

        Ok(handle)
    }

    pub fn lookup(&self, id: &ActorId) -> Option<ActorHandle> {
        self.actors.read().get(id).cloned()
    }

    pub fn lookup_session(&self, session: &SessionId) -> Option<ActorHandle> {
        let id = self.sessions.read().get(session).cloned()?;
        self.lookup(&id)
    }

    /// Live actor handles, one record per actor.
    pub fn list(&self) -> Vec<ActorHandle> {
        self.actors.read().values().cloned().collect()
    }

    pub fn records(&self) -> Vec<ActorRecord> {
        self.list().into_iter().map(|h| h.record()).collect()
    }

    pub async fn stop(&self, id: &ActorId) -> Result<()> {
        let handle = self
            .lookup(id)
            .ok_or_else(|| RuntimeError::not_found(format!("actor {id} is not running")))?;
        handle.stop().await?;
        self.actors.write().remove(id);
        self.sessions.write().remove(&handle.session_id);
        let mut record = handle.record();
        record.state = ActorState::Stopped;
        record.updated_at = agentos_core::now_ms();
        self.save_record(&record).await?;
        self.bus
            .publish(
                NewEvent::new(EventKind::ActorStopped, "actor stopped")
                    .actor(id.clone())
                    .session(handle.session_id.clone())
                    .node(self.cfg.node_id.clone()),
            )
            .await?;
        Ok(())
    }

    /// Ask an actor for its state and persist it as a checkpoint.
    pub async fn checkpoint(&self, id: &ActorId) -> Result<Checkpoint> {
        let handle = self
            .lookup(id)
            .ok_or_else(|| RuntimeError::not_found(format!("actor {id} is not running")))?;
        let state = handle.snapshot().await?;
        let bytes = serde_json::to_vec(&state)?.len() as u64;
        let mut hasher = Sha256::new();
        hasher.update(serde_json::to_vec(&state)?);
        let meta = CheckpointMeta {
            id: CheckpointId::new(),
            actor_id: id.clone(),
            session_id: handle.session_id.clone(),
            generation: handle.init.generation,
            applied_seq: handle.last_seq(),
            event_offset: self.bus.last_seq().await?,
            bytes,
            state_hash: hex::encode(hasher.finalize()),
            domain_version: agentos_core::DOMAIN_VERSION.to_string(),
            created_at: agentos_core::now_ms(),
        };
        let checkpoint = Checkpoint { meta, state };
        self.checkpoints.save(&checkpoint).await?;
        self.bus
            .publish(
                NewEvent::new(EventKind::SnapshotCreated, "actor snapshot persisted")
                    .actor(id.clone())
                    .session(handle.session_id.clone())
                    .node(self.cfg.node_id.clone())
                    .payload(serde_json::json!({
                        "checkpoint_id": checkpoint.meta.id.as_str(),
                        "bytes": checkpoint.meta.bytes,
                        "applied_seq": checkpoint.meta.applied_seq,
                        "event_offset": checkpoint.meta.event_offset,
                        "state_hash": checkpoint.meta.state_hash,
                    })),
            )
            .await?;
        Ok(checkpoint)
    }

    /// Restore an actor from a checkpoint: spawn from the factory, load state, replay the events
    /// that happened after the snapshot was taken.
    pub async fn restore(
        &self,
        checkpoint: Checkpoint,
        factory: Arc<dyn ActorFactory>,
    ) -> Result<(ActorHandle, u64)> {
        if let Some(existing) = self.lookup(&checkpoint.meta.actor_id) {
            existing.stop().await?;
            self.actors.write().remove(&checkpoint.meta.actor_id);
        }
        let init = ActorInit {
            actor_id: checkpoint.meta.actor_id.clone(),
            session_id: checkpoint.meta.session_id.clone(),
            generation: checkpoint.meta.generation + 1,
            params: serde_json::Value::Null,
        };
        let actor = factory.create(&init)?;
        let handle = self.spawn_boxed(init, actor).await?;
        handle.restore(checkpoint.state.clone(), checkpoint.meta.generation + 1).await?;

        let filter = EventFilter {
            session_id: Some(checkpoint.meta.session_id.clone()),
            after_seq: Some(checkpoint.meta.event_offset),
            limit: self.cfg.max_replay_events,
            ..Default::default()
        };
        let events = self.bus.replay(filter).await?;
        let target_actor = checkpoint.meta.actor_id.clone();
        let relevant: Vec<EventRecord> = events
            .into_iter()
            .filter(|e| e.actor_id.as_ref().map(|a| a == &target_actor).unwrap_or(false))
            .collect();
        let replayed = if relevant.is_empty() { 0 } else { handle.replay(relevant).await? };

        self.bus
            .publish(
                NewEvent::new(EventKind::SnapshotRestored, "actor restored from checkpoint")
                    .actor(checkpoint.meta.actor_id.clone())
                    .session(checkpoint.meta.session_id.clone())
                    .node(self.cfg.node_id.clone())
                    .payload(serde_json::json!({
                        "checkpoint_id": checkpoint.meta.id.as_str(),
                        "replayed_events": replayed,
                        "generation": checkpoint.meta.generation + 1,
                    })),
            )
            .await?;
        Ok((handle, replayed))
    }

    /// Latest checkpoint for an actor, if any.
    pub async fn latest_checkpoint(&self, id: &ActorId) -> Result<Option<Checkpoint>> {
        self.checkpoints.latest(id).await
    }

    /// Crash recovery: rebuild the actor from its most recent checkpoint.
    pub async fn recover(&self, id: &ActorId, factory: Arc<dyn ActorFactory>) -> Result<Option<ActorHandle>> {
        match self.latest_checkpoint(id).await? {
            None => Ok(None),
            Some(cp) => Ok(Some(self.restore(cp, factory).await?.0)),
        }
    }

    /// Remove actors that reached a terminal state from the live registry.
    pub fn reap(&self) -> usize {
        let mut actors = self.actors.write();
        let dead: Vec<ActorId> = actors
            .iter()
            .filter(|(_, h)| h.state().is_terminal())
            .map(|(id, _)| id.clone())
            .collect();
        for id in &dead {
            if let Some(h) = actors.remove(id) {
                self.sessions.write().remove(&h.session_id);
            }
        }
        dead.len()
    }

    async fn save_record(&self, record: &ActorRecord) -> Result<()> {
        let collection: Collection<ActorRecord> = Collection::new(collections::ACTORS);
        collection.save(self.store.as_ref(), record.id.as_str(), record).await
    }

    /// Metrics used by the UI topology view.
    pub fn snapshot_metrics(&self) {
        let actors = self.actors.read();
        let depth: i64 = actors.values().map(|h| h.depth() as i64).sum();
        metrics().gauge(metric_names::ACTOR_MAILBOX_DEPTH, depth as f64);
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    mut actor: Box<dyn ErasedActor>,
    mut rx: mpsc::Receiver<ActorCommand>,
    depth: Arc<AtomicUsize>,
    state: Arc<RwLock<ActorState>>,
    seq: Arc<AtomicU64>,
    cancellation: CancellationToken,
    bus: Arc<dyn EventBus>,
    node: String,
    actor_id: ActorId,
    session_id: SessionId,
    generation: u64,
) {
    let ctx = ActorContext {
        actor_id: actor_id.clone(),
        session_id: session_id.clone(),
        generation,
        seq: 0,
        correlation: Correlation::new().with_actor(&actor_id).with_session(&session_id),
        cancellation: cancellation.clone(),
    };
    if let Err(e) = actor.on_start(&ctx).await {
        tracing::error!(actor = %actor_id, error = %e, "actor on_start failed");
    }
    *state.write() = ActorState::Active;

    loop {
        let command = tokio::select! {
            biased;
            _ = cancellation.cancelled() => break,
            cmd = rx.recv() => match cmd {
                Some(c) => c,
                None => break,
            },
        };

        match command {
            ActorCommand::Message { payload, correlation, reply } => {
                let n = seq.fetch_add(1, Ordering::SeqCst) + 1;
                let ctx = ActorContext {
                    actor_id: actor_id.clone(),
                    session_id: session_id.clone(),
                    generation,
                    seq: n,
                    correlation,
                    cancellation: cancellation.clone(),
                };
                let outcome = AssertUnwindSafe(actor.handle(payload, &ctx)).catch_unwind().await;
                depth.fetch_sub(1, Ordering::SeqCst);
                match outcome {
                    Ok(result) => {
                        if let Err(e) = &result {
                            tracing::warn!(actor = %actor_id, seq = n, error = %e, "actor message failed");
                        }
                        if let Some(tx) = reply {
                            let _ = tx.send(result);
                        }
                    }
                    Err(panic) => {
                        let msg = panic_message(&panic);
                        tracing::error!(actor = %actor_id, seq = n, panic = %msg, "actor panicked");
                        *state.write() = ActorState::Failed;
                        metrics().inc(metric_names::ACTOR_RESTARTS, 1);
                        let _ = bus
                            .publish(
                                NewEvent::new(EventKind::Error, "actor panicked")
                                    .error()
                                    .actor(actor_id.clone())
                                    .session(session_id.clone())
                                    .node(node.clone())
                                    .payload(serde_json::json!({ "panic": msg, "seq": n })),
                            )
                            .await;
                        if let Some(tx) = reply {
                            let _ = tx.send(Err(RuntimeError::internal(format!("actor panicked: {msg}"))));
                        }
                        // Stop the loop: a panicked actor must be recovered from a checkpoint
                        // rather than continue with possibly corrupt state.
                        break;
                    }
                }
            }
            ActorCommand::Snapshot { reply } => {
                let _ = reply.send(Ok(actor.snapshot()));
            }
            ActorCommand::Restore { state: new_state, generation: g, reply } => {
                let result = actor.restore(new_state);
                if result.is_ok() {
                    *state.write() = ActorState::Active;
                    tracing::info!(actor = %actor_id, generation = g, "actor state restored");
                }
                let _ = reply.send(result);
            }
            ActorCommand::Replay { events, reply } => {
                let mut applied = 0u64;
                for event in &events {
                    if actor.on_replay(event).await.is_ok() {
                        applied += 1;
                    }
                }
                let _ = bus
                    .publish(
                        NewEvent::new(EventKind::EventReplayed, "actor replayed events")
                            .actor(actor_id.clone())
                            .session(session_id.clone())
                            .node(node.clone())
                            .payload(serde_json::json!({ "applied": applied })),
                    )
                    .await;
                let _ = reply.send(Ok(applied));
            }
            ActorCommand::Drain { reply } => {
                let _ = reply.send(());
            }
            ActorCommand::Stop { reply } => {
                let _ = reply.send(());
                break;
            }
        }
    }

    let end_ctx = ActorContext {
        actor_id: actor_id.clone(),
        session_id: session_id.clone(),
        generation,
        seq: seq.load(Ordering::SeqCst),
        correlation: Correlation::new().with_actor(&actor_id).with_session(&session_id),
        cancellation: cancellation.clone(),
    };
    if let Err(e) = actor.on_stop(&end_ctx).await {
        tracing::warn!(actor = %actor_id, error = %e, "actor on_stop failed");
    }
    let mut guard = state.write();
    if *guard != ActorState::Failed {
        *guard = ActorState::Stopped;
    }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}
