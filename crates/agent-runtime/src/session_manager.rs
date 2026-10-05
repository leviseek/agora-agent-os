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
use crate::archive::ArchivedBundle;
use agentos_core::model::{
    AgentRun, ActorRecord, Checkpoint, CheckpointMeta, EventFilter, EventKind, EventRecord,
    MessageRole, MigrationReport, NewEvent, SessionMessage as TranscriptMessage, SessionRecord,
    TaskGraphRecord,
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
    /// Who owns this conversation, so a list can say whose it is - and so another node's operator can
    /// see what they would be asking for access to.
    #[serde(default)]
    pub owner: Option<agentos_core::model::PrincipalRef>,
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
            owner: r.owner.clone(),
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
    ///
    /// The owner is the principal behind the request, not just a string in the body: `user_id` is
    /// who the session is *for*, and the owner is who decides about it afterwards. They default to
    /// the same person, and an admin creating for someone else can say so.
    pub async fn create_session(&self, user_id: &str, title: &str) -> Result<SessionRecord> {
        self.create_session_for(user_id, title, None).await
    }

    /// Create a session owned by an explicit principal.
    pub async fn create_session_for(
        &self,
        user_id: &str,
        title: &str,
        owner: Option<agentos_core::model::PrincipalRef>,
    ) -> Result<SessionRecord> {
        let mut record = SessionRecord::new(user_id, title);
        if let Some(owner) = owner {
            record.owner = Some(owner);
        }
        // Name this node: a record that says only "alice" is a different principal from "alice on
        // laptop", and the record should say which one it means.
        if let Some(current) = record.owner.take() {
            record.owner = Some(agentos_core::model::PrincipalRef::new(
                current.user_id,
                current.node_id.or_else(|| Some(self.node_id.as_str().to_string())),
            ));
        }
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


    /// Open a session again: the other half of close.
    ///
    /// Closing stops the actor and freezes the content; it does not delete anything, and the
    /// record, the runs and the transcript all survive it. Opening therefore does three things and
    /// no more: move the state back to active, take a lease on the conversation by bringing an
    /// actor back, and say so on the bus.
    ///
    /// The conversation continues where it stopped. The exact transcript comes back when a
    /// snapshot survived; otherwise it is rebuilt from the runs, turn by turn - the same path a
    /// restart takes, and the reason it is worth having one path instead of two.
    pub async fn open(&self, session: &SessionId) -> Result<SessionRecord> {
        let Some(mut record) = self.get(session).await? else {
            return Err(RuntimeError::not_found(format!("session {session} does not exist"))
                .with_detail("session_id", session.as_str()));
        };
        if !record.state.is_terminal() {
            // Already open. Opening twice is not an error: it is a client that is not sure.
            if self.actors.lookup_session(session).is_none() {
                self.spawn_from_history(session, &record).await?;
            }
            return Ok(record);
        }
        if record.state != SessionState::Closed {
            return Err(RuntimeError::conflict(format!(
                "session {session} is {}: only a closed session can be opened again",
                record.state.as_str()
            ))
            .with_detail("state", record.state.as_str()));
        }
        record.state = record
            .state
            .transition(SessionState::Active)
            .map_err(|error| RuntimeError::conflict(format!("cannot open {session}: {error}")))?;
        record.closed_at = None;
        record.updated_at = now_ms();
        self.session_collection()
            .save(self.store.as_ref(), record.id.as_str(), &record)
            .await?;

        self.spawn_from_history(session, &record).await?;

        self.bus
            .publish(
                NewEvent::new(EventKind::SessionOpened, "session opened")
                    .session(session.clone())
                    .actor(record.actor_id.clone())
                    .node(self.node_id.clone())
                    .payload(serde_json::json!({ "owner": record.owner_label() })),
            )
            .await?;
        Ok(record)
    }

    /// Bring an actor back for a session that has none, from what the store kept.
    async fn spawn_from_history(&self, session: &SessionId, record: &SessionRecord) -> Result<()> {
        let Some(handle) = self.actors.lookup_session(session) else {
            let Some(state) = self.rebuild_state(session).await? else {
                return Err(RuntimeError::not_found(format!(
                    "session {session} has a record but no history to rebuild from"
                ))
                .with_detail("session_id", session.as_str()));
            };
            let checkpoint = self
                .synthetic_checkpoint(&record.actor_id, session, &state)
                .await?;
            self.restore(checkpoint).await?;
            return Ok(());
        };
        // Registered but not running here (a restart, or another node's actor): recover it.
        let entry = self.directory.lookup(session).await?;
        match entry {
            Some(entry) => {
                self.actors.recover(&entry.actor_id, self.factory.clone()).await?;
                Ok(())
            }
            None => {
                let Some(state) = self.rebuild_state(session).await? else {
                    return Err(RuntimeError::not_found(format!(
                        "session {session} has a record but no history to rebuild from"
                    )));
                };
                let checkpoint = self
                    .synthetic_checkpoint(&handle.id, session, &state)
                    .await?;
                self.restore(checkpoint).await?;
                Ok(())
            }
        }
    }

    /// Hand out a role on a session. Only the owner (or an admin) may do this, and the caller has
    /// already been checked by the gateway: this is the write.
    pub async fn grant(
        &self,
        session: &SessionId,
        grant: agentos_core::model::SessionGrant,
        granted_by: &agentos_core::model::Principal,
    ) -> Result<SessionRecord> {
        let Some(mut record) = self.get(session).await? else {
            return Err(RuntimeError::not_found(format!("session {session} does not exist")));
        };
        // A grant for the owner's own id is a no-op that would also be confusing to read back.
        if let Some(owner) = &record.owner {
            if owner.matches(&grant.as_ref()) && grant.role != agentos_core::model::SessionRole::Owner {
                return Err(RuntimeError::invalid_input(format!(
                    "{owner} owns this session: they hold every right already"
                )));
            }
        }
        let mut grant = grant;
        grant.granted_by = Some(granted_by.as_ref().to_string());
        grant.granted_at = Some(now_ms());
        record
            .grants
            .retain(|existing| !existing.as_ref().matches(&grant.as_ref()));
        record.grants.push(grant.clone());
        record.updated_at = now_ms();
        self.session_collection()
            .save(self.store.as_ref(), record.id.as_str(), &record)
            .await?;
        self.bus
            .publish(
                NewEvent::new(EventKind::SessionAccessGranted, "session access granted")
                    .session(session.clone())
                    .node(self.node_id.clone())
                    .payload(serde_json::json!({
                        "user_id": grant.user_id,
                        "node_id": grant.node_id,
                        "role": grant.role.as_str(),
                        "granted_by": grant.granted_by,
                    })),
            )
            .await?;
        Ok(record)
    }

    /// Take a role away.
    pub async fn revoke(
        &self,
        session: &SessionId,
        who: &agentos_core::model::PrincipalRef,
    ) -> Result<SessionRecord> {
        let Some(mut record) = self.get(session).await? else {
            return Err(RuntimeError::not_found(format!("session {session} does not exist")));
        };
        let before = record.grants.len();
        record.grants.retain(|grant| !grant.as_ref().matches(who));
        if record.grants.len() == before {
            return Err(RuntimeError::not_found(format!(
                "{who} holds no role on session {session}"
            )));
        }
        record.updated_at = now_ms();
        self.session_collection()
            .save(self.store.as_ref(), record.id.as_str(), &record)
            .await?;
        self.bus
            .publish(
                NewEvent::new(EventKind::SessionAccessRevoked, "session access revoked")
                    .session(session.clone())
                    .node(self.node_id.clone())
                    .payload(serde_json::json!({ "user_id": who.user_id, "node_id": who.node_id })),
            )
            .await?;
        Ok(record)
    }

    // ---------------------------------------------------------------------------------------
    // archiving: out of the hot store, into one package
    // ---------------------------------------------------------------------------------------

    /// Write a conversation into an archive package and mark its record archived.
    ///
    /// Order matters here. The actor is stopped first (a package written while a run is in flight
    /// would be missing that run), the package is written and verified by its own write path, and
    /// only then does the record become `archived`. A failure anywhere before that last step leaves
    /// a closed session and a stray package, which is recoverable; the reverse - archived with no
    /// package - would be a conversation that is gone.
    pub async fn archive(&self, session: &SessionId) -> Result<ArchivedBundle> {
        if !self.deps.archive_enabled {
            return Err(RuntimeError::policy_denied(
                "archiving is switched off on this runtime (storage.archive_enabled)",
            ));
        }
        let Some(record) = self.get(session).await? else {
            return Err(RuntimeError::not_found(format!("session {session} does not exist")));
        };
        // Only a closed session is archived, and the refusal says what to do instead.
        //
        // Closing is the deliberate step that says "we are done with this for now"; archiving is the
        // step that moves it out of the hot store. Closing silently on the way would archive a
        // conversation that was still being had, hide the fact that its actor had just been stopped,
        // and leave the person who meant to keep talking to discover it later.
        if record.state != SessionState::Closed {
            return Err(RuntimeError::conflict(format!(
                "session {session} is {}: close it before archiving it",
                record.state.as_str()
            ))
            .with_detail("state", record.state.as_str()));
        }

        // The exact conversation when an actor still holds it, the rebuilt one otherwise. An
        // archive should carry what was actually said when that is available, and the runs are
        // always available.
        let state = match self.actors.lookup_session(session) {
            Some(handle) => match self.actors.checkpoint(&handle.id).await {
                Ok(checkpoint) => serde_json::from_value::<SessionActorState>(checkpoint.state.clone())
                    .unwrap_or_else(|_| SessionActorState::new(record.clone())),
                Err(_) => self
                    .rebuild_state(session)
                    .await?
                    .unwrap_or_else(|| SessionActorState::new(record.clone())),
            },
            None => self
                .rebuild_state(session)
                .await?
                .unwrap_or_else(|| SessionActorState::new(record.clone())),
        };
        let bundle = crate::archive::write_package(
            &self.deps.archive_dir,
            &record,
            &state.runs,
            &state.transcript,
            &state.graphs,
            self.deps.artifacts.as_ref(),
            &self.node_id.as_str(),
        )
        .await?;

        let mut updated = record.clone();
        updated.state = updated
            .state
            .transition(SessionState::Archived)
            .map_err(|error| RuntimeError::conflict(format!("cannot archive {session}: {error}")))?;
        updated.updated_at = now_ms();
        self.session_collection()
            .save(self.store.as_ref(), updated.id.as_str(), &updated)
            .await?;
        // The directory entry is gone already (close deregisters); this is belt and braces for the
        // case where the record arrived here in a terminal state without one.
        let _ = self.directory.unregister(session).await;

        self.bus
            .publish(
                NewEvent::new(EventKind::SessionArchived, "session archived")
                    .session(session.clone())
                    .actor(updated.actor_id.clone())
                    .node(self.node_id.clone())
                    .payload(serde_json::json!({
                        "archive_id": bundle.id,
                        "path": bundle.path,
                        "bytes": bundle.bytes,
                        "runs": bundle.manifest.runs,
                        "messages": bundle.manifest.messages,
                        "artifacts": bundle.manifest.artifacts,
                    })),
            )
            .await?;
        Ok(bundle)
    }

    /// Every package under the archive root, newest first.
    pub async fn list_archives(&self) -> Result<Vec<crate::archive::ArchiveEntry>> {
        crate::archive::list_packages(&self.deps.archive_dir).await
    }

    pub async fn find_archive(&self, id: &str) -> Result<crate::archive::ArchiveEntry> {
        crate::archive::find_package(&self.deps.archive_dir, id).await
    }

    /// What a package holds, without restoring it: the manifest, and the first and last turns so a
    /// person can recognise the conversation before bringing it back.
    pub async fn preview_archive(&self, id: &str) -> Result<serde_json::Value> {
        let entry = self.find_archive(id).await?;
        let package = crate::archive::read_package(std::path::Path::new(&entry.path)).await?;
        Ok(serde_json::json!({
            "id": entry.id,
            "path": entry.path,
            "bytes": entry.bytes,
            "manifest": entry.manifest,
            // The first few turns, so a person can recognise the conversation before restoring it.
            "preview": package.preview(6),
        }))
    }

    /// Delete a package. Only the file: the tombstone record keeps saying the conversation exists
    /// somewhere, which is the honest state of affairs.
    pub async fn delete_archive(&self, id: &str) -> Result<()> {
        let entry = self.find_archive(id).await?;
        tokio::fs::remove_file(&entry.path)
            .await
            .map_err(|error| RuntimeError::unavailable(format!("could not delete {}: {error}", entry.path)))?;
        self.bus
            .publish(
                NewEvent::new(EventKind::SessionArchiveDeleted, "archive deleted")
                    .node(self.node_id.clone())
                    .payload(serde_json::json!({ "archive_id": id, "path": entry.path })),
            )
            .await?;
        Ok(())
    }

    /// Bring an archived conversation back, as a new session.
    ///
    /// A new id by default, and that default is the whole design: restoring over the original id
    /// would silently merge a conversation restored from an old package with whatever the live
    /// session has become since. A restore produces a conversation that continues from the package,
    /// and the archived one stays archived.
    pub async fn restore_archive(
        &self,
        id: &str,
        title: Option<String>,
        owner: Option<agentos_core::model::PrincipalRef>,
    ) -> Result<SessionRecord> {
        let entry = self.find_archive(id).await?;
        let package = crate::archive::read_package(std::path::Path::new(&entry.path)).await?;
        let crate::archive::PackageContents { record, runs, graphs, artifacts, .. } = package;

        let mut restored = SessionRecord::new(
            record.user_id.clone(),
            title.unwrap_or_else(|| entry.manifest.restored_title()),
        );
        // The owner travels with the conversation unless the caller claims it.
        restored.owner = owner.or(record.owner.clone()).or(restored.owner);
        restored.model_hint = record.model_hint.clone();
        restored.reasoning_effort = record.reasoning_effort;
        restored.metadata = record.metadata.clone();
        restored.state = SessionState::Active;

        // Artifacts first: the runs refer to them, and an id in a run has to point at bytes that
        // are already there.
        let mut remapped: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for (artifact, bytes) in &artifacts {
            let stored = self
                .deps
                .artifacts
                .put(
                    restored.id.clone(),
                    &artifact.name,
                    artifact.kind,
                    &artifact.content_type,
                    bytes,
                )
                .await?;
            remapped.insert(artifact.id.as_str().to_string(), stored.id.as_str().to_string());
        }

        self.session_collection()
            .save(self.store.as_ref(), restored.id.as_str(), &restored)
            .await?;

        let run_collection: Collection<AgentRun> = Collection::new(collections::RUNS);
        for run in &runs {
            let mut run = run.clone();
            run.session_id = restored.id.clone();
            run.id = agentos_core::AgentId::new();
            for attachment in &mut run.attachments {
                if let Some(new_id) = remapped.get(&attachment.artifact_id) {
                    attachment.artifact_id = new_id.clone();
                }
            }
            run_collection
                .save(self.store.as_ref(), run.id.as_str(), &run)
                .await?;
        }
        let graph_collection: Collection<TaskGraphRecord> = Collection::new(collections::GRAPHS);
        for graph in &graphs {
            let mut graph = graph.clone();
            graph.id = agentos_core::TaskId::new();
            graph.session_id = restored.id.clone();
            graph_collection
                .save(self.store.as_ref(), graph.id.as_str(), &graph)
                .await?;
        }

        // Open, not merely "not closed": a restore produces a conversation somebody is about to
        // carry on with, so the actor comes up here rather than on the first message. What comes back
        // is a session you can keep talking in, which is the whole point of restoring one.
        if let Err(error) = self.spawn_from_history(&restored.id, &restored).await {
            // The session is usable either way: the first goal rebuilds the actor from the runs that
            // were just written. Worth saying out loud, though, because "restored but not live" is a
            // symptom of a store that is not keeping up.
            tracing::warn!(session = %restored.id, %error, "restored session could not be opened immediately");
        }

        self.bus
            .publish(
                NewEvent::new(EventKind::SessionRestored, "session restored from an archive")
                    .session(restored.id.clone())
                    .node(self.node_id.clone())
                    .payload(serde_json::json!({
                        "archive_id": id,
                        "source_session": entry.manifest.session_id,
                        "title": restored.title,
                        "runs": runs.len(),
                        "artifacts": artifacts.len(),
                    })),
            )
            .await?;
        Ok(restored)
    }
    /// Hot path: resolve the actor for a session, consulting the control plane only on a miss.
    pub async fn actor_for(&self, session: &SessionId) -> Result<ActorHandle> {
        if let Some(handle) = self.actors.lookup_session(session) {
            return Ok(handle);
        }
        // Cache miss: ask the control plane.
        let entry = self.directory.lookup(session).await?;
        let Some(entry) = entry else {
            // No directory entry. Either the session does not exist at all, or it exists without a
            // registered actor: one the user closed (closing deregisters it on purpose), or one
            // whose entry never reached the store before a restart. The record decides which.
            let Some(record) = self.get(session).await? else {
                return Err(RuntimeError::not_found(format!("session {session} does not exist"))
                    .with_detail("session_id", session.as_str()));
            };
            if record.state.is_terminal() {
                return Err(RuntimeError::conflict(format!(
                    "session {session} is {}: its history can be read, but it accepts no new goals",
                    record.state.as_str()
                ))
                .with_detail("session_id", session.as_str())
                .with_detail("state", record.state.as_str()));
            }
            // Open, but nothing is running here: rebuild it from its record and runs, so a restart
            // does not turn a usable session into one that answers "does not exist".
            let actor_id = record.actor_id.clone();
            let Some(state) = self.rebuild_state(session).await? else {
                return Err(RuntimeError::not_found(format!("session {session} does not exist"))
                    .with_detail("session_id", session.as_str()));
            };
            let checkpoint = self.synthetic_checkpoint(&actor_id, session, &state).await?;
            self.restore(checkpoint).await?;
            return self.actors.lookup_session(session).ok_or_else(|| {
                RuntimeError::unavailable(format!(
                    "session {session} was rebuilt but its actor is not registered"
                ))
            });
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
            None => {
                // No snapshot survived the restart. Refusing to serve a session that is still in
                // the list is the wrong answer: the session record and its runs are durable, so the
                // actor is rebuilt from those. The conversation returns as goal/answer turns rather
                // than message by message, because only a snapshot could preserve the exact
                // transcript - and a session you can use beats a session that answers 503.
                let Some(state) = self.rebuild_state(session).await? else {
                    return Err(RuntimeError::unavailable(format!(
                        "session {session} has no live actor and no record to rebuild from"
                    )));
                };
                let runs = state.runs.len();
                let checkpoint = self.synthetic_checkpoint(&entry.actor_id, session, &state).await?;
                tracing::info!(
                    session = %session,
                    runs,
                    "rebuilt a session actor from its record and runs (no snapshot survived)"
                );
                self.restore(checkpoint).await?;
                self.actors.lookup_session(session).ok_or_else(|| {
                    RuntimeError::unavailable(format!(
                        "session {session} was rebuilt but its actor is not registered"
                    ))
                })
            }
        }
    }

/// The runtime view of a session that has no live actor, spelled exactly like the actor's own
/// status so a client cannot tell the difference - except that `active_run` is always null.
fn durable_status_json(session: &SessionId, state: &SessionActorState) -> serde_json::Value {
    serde_json::json!({
        "session_id": session.as_str(),
        "state": state.session.state.as_str(),
        "title": state.session.title,
        "messages": state.transcript.len(),
        "goals_handled": state.goals_handled,
        "active_run": serde_json::Value::Null,
        "runs": state
            .runs
            .iter()
            .map(|run| serde_json::json!({
                "agent_id": run.id.as_str(),
                "state": run.state.as_str(),
                "goal": run.goal,
                "steps": run.steps.len(),
                "provider": run.provider,
                "model": run.model,
                "model_hint": run.model_hint,
                "reasoning_effort": run.reasoning_effort,
                "final_answer": run.final_answer,
                "error": run.error,
                "degraded": run.degraded,
                "attachments": run.attachments,
                "author": run.author,
                "usage": run.usage,
            }))
            .collect::<Vec<_>>(),
        "compaction_usage": state.compaction_usage,
        "compacted_through": state.compacted_through,
        "usage": state.runs.iter().fold(state.compaction_usage, |mut total, run| {
            total.add(&run.usage);
            total
        }),
        "graphs": Vec::<serde_json::Value>::new(),
        // Said out loud: this view was rebuilt from durable facts, not read from a live actor.
        "rebuilt_from_history": true,
    })
}

/// The same shape the actor returns for a transcript, rebuilt from durable facts.
fn durable_transcript_json(
    state: &SessionActorState,
    limit: Option<usize>,
) -> serde_json::Value {
    let all = &state.transcript;
    let start = match limit {
        Some(limit) if limit > 0 && all.len() > limit => all.len() - limit,
        _ => 0,
    };
    serde_json::json!({
        "messages": &all[start..],
        "total": all.len(),
        "truncated": start > 0,
    })
}

/// A checkpoint built from rebuilt state, for an actor that has to be brought back without one.
///
/// Nothing is replayed: the state already contains every durable fact, so the event offset is
/// where the log currently ends.
async fn synthetic_checkpoint(
    &self,
    actor_id: &ActorId,
    session: &SessionId,
    state: &SessionActorState,
) -> Result<Checkpoint> {
        Ok(Checkpoint {
            meta: CheckpointMeta {
                id: agentos_core::CheckpointId::new(),
                actor_id: actor_id.clone(),
                session_id: session.clone(),
                generation: 0,
                applied_seq: 0,
                event_offset: self.bus.last_seq().await.unwrap_or(0),
                bytes: 0,
                state_hash: String::new(),
                domain_version: agentos_core::DOMAIN_VERSION.to_string(),
                created_at: now_ms(),
            },
            state: serde_json::to_value(state)?,
        })
    }

/// Reconstruct a session actor from what is durable: its record and its runs.
async fn rebuild_state(&self, session: &SessionId) -> Result<Option<SessionActorState>> {
        let record = self
            .session_collection()
            .load(self.store.as_ref(), session.as_str())
            .await?;
        let Some(record) = record else {
            return Ok(None);
        };
        let stored: Vec<AgentRun> = Collection::new(collections::RUNS)
            .list(self.store.as_ref(), 10_000)
            .await?;
        let mut runs: Vec<AgentRun> = stored
            .into_iter()
            .filter(|run: &AgentRun| &run.session_id == session)
            .collect();
        runs.sort_by_key(|run| run.created_at);

        let mut state = SessionActorState::new(record);
        for run in &runs {
            let mut goal = TranscriptMessage::text(
                session.clone(),
                MessageRole::User,
                run.goal.clone(),
            );
            goal.created_at = run.created_at;
            goal.agent_id = Some(run.id.as_str().to_string());
            // Whose turn it was, rebuilt from the run: a shared conversation that loses its authors
            // when it is restored reads as if one person said everything.
            goal.author = run.author.clone();
            // The files this goal carried, rebuilt from the run record: without them a restored
            // conversation shows the question and the answer but not what was attached to it.
            for attachment in &run.attachments {
                goal.parts.push(match attachment.kind.as_str() {
                    "image" => agentos_core::model::ContentPart::Image {
                        artifact_id: attachment.artifact_id.clone(),
                        name: attachment.name.clone(),
                        mime: attachment
                            .content_type
                            .clone()
                            .unwrap_or_else(|| "application/octet-stream".into()),
                    },
                    _ => agentos_core::model::ContentPart::Artifact {
                        artifact_id: attachment.artifact_id.clone(),
                        name: attachment.name.clone(),
                    },
                });
            }
            state.transcript.push(goal);
            if let Some(answer) = &run.final_answer {
                let mut reply = TranscriptMessage::text(
                    session.clone(),
                    MessageRole::Assistant,
                    answer.clone(),
                );
                reply.created_at = run.finished_at.unwrap_or(run.updated_at);
                reply.agent_id = Some(run.id.as_str().to_string());
                state.transcript.push(reply);
            }
        }
        state.goals_handled = runs.len() as u64;
        state.runs = runs;
        state.session.message_count = state.transcript.len() as u64;
        Ok(Some(state))
    }

    /// Send a goal to a session. Sessions are independent, so this awaits only this session.
    pub async fn post_goal(
        &self,
        session: &SessionId,
        text: &str,
        images: &[String],
        attachments: &[String],
        model: Option<String>,
        reasoning_effort: Option<agentos_core::model::ReasoningEffort>,
        author: Option<agentos_core::model::PrincipalRef>,
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
                attachments: attachments.to_vec(),
                model,
                reasoning_effort,
                author,
            })
            .await
    }

    /// Queue a goal without waiting for completion; the caller follows the event stream instead.
    pub async fn post_goal_async(
        &self,
        session: &SessionId,
        text: &str,
        images: &[String],
        attachments: &[String],
        model: Option<String>,
        reasoning_effort: Option<agentos_core::model::ReasoningEffort>,
        author: Option<agentos_core::model::PrincipalRef>,
    ) -> Result<()> {
        let handle = self.actor_for(session).await?;
        handle
            .cast(SessionMessage::UserGoal {
                text: text.to_string(),
                correlation: None,
                images: images.to_vec(),
                attachments: attachments.to_vec(),
                model,
                reasoning_effort,
                author,
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

    /// The session's runtime view: what it is doing, and what it has done.
    ///
    /// A session with no actor is not an error. One the user closed has had its actor stopped and
    /// deregistered on purpose, and its history is still durable - a console that lists it must be
    /// able to open it, so this answers from the record and the runs instead of refusing.
    pub async fn status(&self, session: &SessionId) -> Result<serde_json::Value> {
        match self.actor_for(session).await {
            Ok(handle) => handle.send(SessionMessage::Status).await,
            Err(error) => match self.durable_view(session).await? {
                Some((state, _runs)) => Ok(Self::durable_status_json(session, &state)),
                None => Err(error),
            },
        }
    }

    /// The session record, its runs and its rebuilt conversation, read straight from the store.
    ///
    /// Returns None when there is no record at all, which is the only case that is genuinely a
    /// missing session.
    async fn durable_view(
        &self,
        session: &SessionId,
    ) -> Result<Option<(SessionActorState, Vec<AgentRun>)>> {
        let Some(state) = self.rebuild_state(session).await? else {
            return Ok(None);
        };
        let runs = state.runs.clone();
        Ok(Some((state, runs)))
    }

    pub async fn last_run(&self, session: &SessionId) -> Result<serde_json::Value> {
        let handle = self.actor_for(session).await?;
        handle.send(SessionMessage::LastRun).await
    }

    /// The conversation so far. This is how a caller that posted a goal without waiting picks up
    /// the answer: the reply is appended to the transcript when the run finishes, and the run
    /// completion event carries only a summary.
    pub async fn transcript(&self, session: &SessionId, limit: Option<usize>) -> Result<serde_json::Value> {
        match self.actor_for(session).await {
            Ok(handle) => handle.send(SessionMessage::Transcript { limit }).await,
            Err(error) => match self.durable_view(session).await? {
                Some((state, _runs)) => Ok(Self::durable_transcript_json(&state, limit)),
                None => Err(error),
            },
        }
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
    pub async fn configure(
        &self,
        session: &SessionId,
        title: Option<String>,
        model: Option<String>,
        reasoning_effort: Option<agentos_core::model::ReasoningEffort>,
    ) -> Result<SessionRecord> {
        let handle = self.actor_for(session).await?;
        handle
            .send(SessionMessage::Configure { title, model, reasoning_effort })
            .await?;
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
