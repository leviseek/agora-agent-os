//! Session manager: the only component the gateway talks to for session work.
//!
//! Routing rule that keeps the control plane off the hot path:
//!   1. look the actor up in the ACTOR RUNTIME (in-process map) - a cache hit,
//!   2. only on a miss consult the Actor DIRECTORY (control plane),
//!   3. if the directory knows the session but no actor is running, recover from the latest
//!      checkpoint and then route.
//! A cache miss is therefore the only case that touches the control plane, and it is measured
//! with the cache hit/miss counters.

use crate::session::{SessionActorHandle, SessionActorState, SessionDeps, SessionMessage};
use agentos_actor_runtime::actor::{ActorFactory, ActorHandle, ActorInit, ErasedActor};
use agentos_actor_runtime::migration::{MigrationCoordinator, TransferTarget};
use agentos_actor_runtime::runtime::ActorRuntime;
use agentos_actor_runtime::ActorTransfer;
use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{
    ActorRecord, Checkpoint, EventFilter, EventKind, EventRecord, MigrationReport, NewEvent,
    SessionRecord,
};
use agentos_core::state::{ActorState, SessionState, StateMachine};
use agentos_core::telemetry::Correlation;
use agentos_core::{now_ms, ActorId, SessionId};
use agentos_control_plane::directory::{ActorDirectory, DirectoryEntry};
use agentos_control_plane::placement::{PlacementService, PlacementStrategy};
use agentos_event_bus::EventBus;
use agentos_storage::store::{collections, Collection, Store};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Client-facing projection of a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub user_id: String,
    pub title: String,
    pub state: SessionState,
    pub actor_id: ActorId,
    pub created_at: u64,
    pub updated_at: u64,
    pub message_count: u64,
}

impl From<&SessionRecord> for SessionSummary {
    fn from(r: &SessionRecord) -> Self {
        Self {
            id: r.id.clone(),
            user_id: r.user_id.clone(),
            title: r.title.clone(),
            state: r.state,
            actor_id: r.actor_id.clone(),
            created_at: r.created_at,
            updated_at: r.updated_at,
            message_count: r.message_count,
        }
    }
}

pub struct SessionActorFactory {
    deps: Arc<SessionDeps>,
}

impl SessionActorFactory {
    pub fn new(deps: Arc<SessionDeps>) -> Self {
        Self { deps }
    }
}

impl ActorFactory for SessionActorFactory {
    fn kind(&self) -> &'static str {
        "session"
    }

    fn create(&self, init: &ActorInit) -> Result<Box<dyn ErasedActor>> {
        let session: SessionRecord = serde_json::from_value(init.params.clone()).unwrap_or_else(|_| {
            let mut record = SessionRecord::new("unknown", "restored session");
            record.id = init.session_id.clone();
            record.actor_id = init.actor_id.clone();
            record
        });
        let state = SessionActorState::new(session);
        Ok(SessionActorHandle::boot(self.deps.clone(), state).into_erased())
    }
}

pub struct SessionManager {
    store: Arc<dyn Store>,
    bus: Arc<dyn EventBus>,
    actors: Arc<ActorRuntime>,
    directory: Arc<ActorDirectory>,
    placement: Arc<PlacementService>,
    deps: Arc<SessionDeps>,
    factory: Arc<dyn ActorFactory>,
    transfer: Arc<dyn ActorTransfer>,
    node_id: String,
}

impl SessionManager {
    pub fn new(
        store: Arc<dyn Store>,
        bus: Arc<dyn EventBus>,
        actors: Arc<ActorRuntime>,
        directory: Arc<ActorDirectory>,
        placement: Arc<PlacementService>,
        deps: Arc<SessionDeps>,
        transfer: Arc<dyn ActorTransfer>,
        node_id: impl Into<String>,
    ) -> Self {
        let factory: Arc<dyn ActorFactory> = Arc::new(SessionActorFactory::new(deps.clone()));
        Self { store, bus, actors, directory, placement, deps, factory, transfer, node_id: node_id.into() }
    }

    pub fn deps(&self) -> Arc<SessionDeps> {
        self.deps.clone()
    }

    pub fn runtime(&self) -> Arc<ActorRuntime> {
        self.actors.clone()
    }

    pub fn directory(&self) -> Arc<ActorDirectory> {
        self.directory.clone()
    }

    pub fn placement(&self) -> Arc<PlacementService> {
        self.placement.clone()
    }

    fn session_collection(&self) -> Collection<SessionRecord> {
        Collection::new(collections::SESSIONS)
    }

    /// Create a session, place it on a worker and spawn its actor.
    pub async fn create_session(&self, user_id: &str, title: &str) -> Result<SessionRecord> {
        let mut record = SessionRecord::new(user_id, title);
        self.session_collection().save(self.store.as_ref(), record.id.as_str(), &record).await?;

        let decision = self
            .placement
            .place_actor(&record.id, &record.actor_id, "session")
            .await?;
        record.worker_id = Some(decision.worker_id.clone());

        let init = ActorInit {
            actor_id: record.actor_id.clone(),
            session_id: record.id.clone(),
            generation: 0,
            params: serde_json::to_value(&record)?,
        };
        self.actors.spawn_boxed(init, self.factory.create(&ActorInit {
            actor_id: record.actor_id.clone(),
            session_id: record.id.clone(),
            generation: 0,
            params: serde_json::to_value(&record)?,
        })?).await?;

        let mut actor_record = ActorRecord::new(record.actor_id.clone(), record.id.clone(), "session");
        actor_record.worker_id = Some(decision.worker_id.clone());
        actor_record.state = ActorState::Active;

        self.directory
            .register(DirectoryEntry::from_record(&actor_record, Some(self.node_id.clone())))
            .await?;

        record.state = record.state.transition(SessionState::Active).unwrap_or(record.state);
        record.updated_at = now_ms();
        self.session_collection().save(self.store.as_ref(), record.id.as_str(), &record).await?;

        self.bus
            .publish(
                NewEvent::new(EventKind::SessionCreated, "session created")
                    .session(record.id.clone())
                    .actor(record.actor_id.clone())
                    .worker(decision.worker_id.clone())
                    .node(self.node_id.clone())
                    .payload(serde_json::json!({
                        "user_id": user_id,
                        "title": title,
                        "placement": decision.reason,
                    })),
            )
            .await?;

        Ok(record)
    }

    /// Hot path: resolve the actor for a session, consulting the control plane only on a miss.
    pub async fn actor_for(&self, session: &SessionId) -> Result<ActorHandle> {
        if let Some(handle) = self.actors.lookup_session(session) {
            return Ok(handle);
        }
        // Cache miss: ask the control plane.
        let entry = self.directory.lookup(session).await?;
        let Some(entry) = entry else {
            return Err(RuntimeError::not_found(format!("session {session} does not exist"))
                .with_detail("session_id", session.as_str()));
        };
        if !entry.is_routable() {
            return Err(RuntimeError::unavailable(format!(
                "session {session} is registered but its actor is {}",
                entry.state
            )));
        }
        // The directory knows about it but nothing is running here: recover from a checkpoint.
        match self.actors.recover(&entry.actor_id, self.factory.clone()).await? {
            Some(handle) => Ok(handle),
            None => Err(RuntimeError::unavailable(format!(
                "session {session} has no live actor and no checkpoint to recover from"
            ))),
        }
    }

    /// Send a goal to a session. Sessions are independent, so this awaits only this session.
    pub async fn post_goal(
        &self,
        session: &SessionId,
        text: &str,
        images: &[String],
    ) -> Result<serde_json::Value> {
        let handle = self.actor_for(session).await?;
        let correlation = Correlation::new().with_session(session).with_actor(&handle.id);
        let span = correlation.span("session.goal");
        let _guard = span.enter();
        handle
            .send(SessionMessage::UserGoal {
                text: text.to_string(),
                correlation: Some(correlation),
                images: images.to_vec(),
            })
            .await
    }

    /// Queue a goal without waiting for completion; the caller follows the event stream instead.
    pub async fn post_goal_async(&self, session: &SessionId, text: &str, images: &[String]) -> Result<()> {
        let handle = self.actor_for(session).await?;
        handle
            .cast(SessionMessage::UserGoal {
                text: text.to_string(),
                correlation: None,
                images: images.to_vec(),
            })
            .await
    }

    /// Cancel the run in flight. This goes through the shared token registry, not the mailbox, so
    /// it works even while the actor is busy executing that very run.
    pub async fn cancel(&self, session: &SessionId) -> Result<bool> {
        let token: Option<CancellationToken> = self.deps.run_tokens.read().get(session).cloned();
        match token {
            Some(t) => {
                t.cancel();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub async fn status(&self, session: &SessionId) -> Result<serde_json::Value> {
        let handle = self.actor_for(session).await?;
        handle.send(SessionMessage::Status).await
    }

    pub async fn last_run(&self, session: &SessionId) -> Result<serde_json::Value> {
        let handle = self.actor_for(session).await?;
        handle.send(SessionMessage::LastRun).await
    }

    /// The conversation so far. This is how a caller that posted a goal without waiting picks up
    /// the answer: the reply is appended to the transcript when the run finishes, and the run
    /// completion event carries only a summary.
    pub async fn transcript(&self, session: &SessionId, limit: Option<usize>) -> Result<serde_json::Value> {
        let handle = self.actor_for(session).await?;
        handle.send(SessionMessage::Transcript { limit }).await
    }

    pub async fn list(&self) -> Result<Vec<SessionSummary>> {
        let mut records = self.session_collection().list(self.store.as_ref(), 10_000).await?;
        records.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(records.iter().map(SessionSummary::from).collect())
    }

    pub async fn get(&self, session: &SessionId) -> Result<Option<SessionRecord>> {
        self.session_collection().load(self.store.as_ref(), session.as_str()).await
    }

    /// Close a session: stop the actor, mark the record terminal, deregister from the directory.
    pub async fn close(&self, session: &SessionId) -> Result<()> {
        let Some(mut record) = self.get(session).await? else {
            return Err(RuntimeError::not_found(format!("session {session} does not exist")));
        };
        if let Some(handle) = self.actors.lookup_session(session) {
            let _ = self.actors.stop(&handle.id).await;
        }
        self.directory.unregister(session).await?;
        record.state = record.state.transition(SessionState::Closing).unwrap_or(record.state);
        record.state = record.state.transition(SessionState::Closed).unwrap_or(record.state);
        record.closed_at = Some(now_ms());
        record.updated_at = now_ms();
        self.session_collection().save(self.store.as_ref(), record.id.as_str(), &record).await?;
        self.bus
            .publish(
                NewEvent::new(EventKind::SessionClosed, "session closed")
                    .session(session.clone())
                    .actor(record.actor_id.clone())
                    .node(self.node_id.clone()),
            )
            .await?;
        Ok(())
    }

/// Fork a session: a new session that starts from this one and continues on its own.
    ///
    /// The fork inherits the transcript, the runs and the task graphs, with every session-scoped
    /// identifier rewritten so the two sessions never point at each other by accident. Memory is
    /// deliberately NOT copied: memory is per session, which is what makes a fork a clean place to
    /// try something without polluting what the original remembers.
    pub async fn branch(&self, session: &SessionId, title: Option<String>) -> Result<SessionRecord> {
        let source = self.session_collection().load(self.store.as_ref(), session.as_str()).await?;
        let source = source.ok_or_else(|| RuntimeError::not_found(format!("session {session} not found")))?;
        let handle = self.actor_for(session).await?;
        let checkpoint = self.actors.checkpoint(&handle.id).await?;

        // A run that was in flight in the source is not in flight in the fork.
        let mut state: SessionActorState = serde_json::from_value(checkpoint.state.clone())?;
        let mut record = SessionRecord::new(
            source.user_id.clone(),
            title.unwrap_or_else(|| format!("{} (branch)", source.title)),
        );
        record.state = record.state.transition(SessionState::Active).unwrap_or(record.state);
        state.session = record.clone();
        state.active_run = None;
        for message in &mut state.transcript {
            message.session_id = record.id.clone();
        }
        for run in &mut state.runs {
            run.session_id = record.id.clone();
        }
        for graph in &mut state.graphs {
            graph.session_id = record.id.clone();
        }

        let mut forked = checkpoint.clone();
        forked.meta.id = agentos_core::CheckpointId::new();
        forked.meta.actor_id = ActorId::new();
        forked.meta.session_id = record.id.clone();
        forked.meta.generation = 0;
        forked.meta.applied_seq = 0;
        forked.state = serde_json::to_value(&state)?;

        let (new_handle, _replayed) = self.actors.restore(forked, self.factory.clone()).await?;
        record.actor_id = new_handle.id.clone();
        record.updated_at = now_ms();
        self.session_collection()
            .save(self.store.as_ref(), record.id.as_str(), &record)
            .await?;

        let mut actor_record = ActorRecord::new(record.actor_id.clone(), record.id.clone(), "session");
        actor_record.state = ActorState::Active;
        self.directory
            .register(DirectoryEntry::from_record(&actor_record, Some(self.node_id.clone())))
            .await?;

        self.bus
            .publish(
                NewEvent::new(EventKind::SessionCreated, "session forked")
                    .session(record.id.clone())
                    .actor(record.actor_id.clone())
                    .node(self.node_id.clone())
                    .payload(serde_json::json!({
                        "forked_from": source.id.as_str(),
                        "turns": state.transcript.len(),
                        "runs": state.runs.len(),
                    })),
            )
            .await?;

        tracing::info!(source = %source.id, branch = %record.id, "session forked");
        Ok(record)
    }

    /// Rename a session through its actor, then read back what was stored.
    pub async fn rename(&self, session: &SessionId, title: &str) -> Result<SessionRecord> {
        let handle = self.actor_for(session).await?;
        handle.send(SessionMessage::Rename { title: title.to_string() }).await?;
        self.session_collection()
            .load(self.store.as_ref(), session.as_str())
            .await?
            .ok_or_else(|| RuntimeError::not_found(format!("session {session} not found")))
    }

    /// The typed material of an export: the live record, the conversation and the runs.
    pub async fn export_data(
        &self,
        session: &SessionId,
    ) -> Result<(SessionRecord, Vec<agentos_core::model::SessionMessage>, Vec<agentos_core::model::AgentRun>)> {
        let handle = self.actor_for(session).await?;
        let checkpoint = self.actors.checkpoint(&handle.id).await?;
        let state: SessionActorState = serde_json::from_value(checkpoint.state)?;
        Ok((state.session, state.transcript, state.runs))
    }

    /// Export a session actor snapshot.
    pub async fn snapshot(&self, session: &SessionId) -> Result<Checkpoint> {
        let handle = self.actor_for(session).await?;
        self.actors.checkpoint(&handle.id).await
    }

    /// Restore a session actor from a snapshot, replaying the events recorded after it.
    pub async fn restore(&self, checkpoint: Checkpoint) -> Result<ActorId> {
        let session_id = checkpoint.meta.session_id.clone();
        let (handle, replayed) = self.actors.restore(checkpoint.clone(), self.factory.clone()).await?;
        self.directory
            .register(DirectoryEntry {
                session_id: session_id.clone(),
                actor_id: handle.id.clone(),
                kind: "session".into(),
                worker_id: None,
                node_id: Some(self.node_id.clone()),
                generation: handle.init.generation,
                state: ActorState::Active,
                endpoints: vec![],
                updated_at: now_ms(),
            })
            .await?;
        tracing::info!(session = %session_id, replayed, "session actor restored");
        Ok(handle.id)
    }

    /// Migrate a session actor to another worker (v1 transfer is local, the pipeline is real).
    pub async fn migrate(&self, session: &SessionId, target_worker: Option<String>) -> Result<MigrationReport> {
        let handle = self.actor_for(session).await?;
        let coordinator = MigrationCoordinator::new(self.actors.clone(), self.transfer.clone());
        let target = TransferTarget {
            worker_id: target_worker.map(agentos_core::WorkerId::from_raw),
            endpoint: None,
        };
        coordinator.migrate(&handle.id, self.factory.clone(), target).await
    }

    pub async fn events(&self, session: &SessionId, limit: usize) -> Result<Vec<EventRecord>> {
        self.bus
            .replay(EventFilter {
                session_id: Some(session.clone()),
                limit: if limit == 0 { 200 } else { limit },
                ..Default::default()
            })
            .await
    }

    /// Placement view used by the UI topology panel.
    pub fn placement_strategy(&self) -> PlacementStrategy {
        self.placement.policy().strategy
    }

    /// Look up an actor handle by id, for actor-level operations such as migration.
    pub async fn actor_handle(&self, actor: &ActorId) -> Result<ActorHandle> {
        self.actors
            .lookup(actor)
            .ok_or_else(|| RuntimeError::not_found(format!("actor {actor} is not running")))
    }

    /// Sessions currently holding a live actor.
    pub fn live_actors(&self) -> Vec<ActorRecord> {
        self.actors.records()
    }

    /// Resume helper used at bootstrap: warm the directory cache and the event ring.
    pub async fn warm(&self) -> Result<usize> {
        let n = self.directory.warm().await?;
        Ok(n)
    }

    pub fn run_tokens(&self) -> Arc<RwLock<HashMap<SessionId, CancellationToken>>> {
        self.deps.run_tokens.clone()
    }
}
