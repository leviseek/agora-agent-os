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
    MessageRole, MigrationReport, NewEvent, Principal, PrincipalRef, SessionGrant, SessionMessage as TranscriptMessage,
    SessionRecord, SessionRole, TaskGraphRecord, WorkspaceRecord,
};

use agentos_core::state::{ActorState, SessionState, StateMachine};
use agentos_core::telemetry::Correlation;
use agentos_core::{now_ms, ActorId, SessionId, WorkspaceId};
use agentos_control_plane::directory::{ActorDirectory, DirectoryEntry};
use agentos_control_plane::placement::{PlacementService, PlacementStrategy};
use agentos_event_bus::EventBus;
use agentos_storage::store::{collections, Collection, Store};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    /// The workspace this session belongs to. None only for records written before workspaces
    /// existed; a console groups by this, so it has to travel with the summary.
    #[serde(default)]
    pub workspace_id: Option<WorkspaceId>,
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
            workspace_id: r.workspace_id.clone(),
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

/// A path as a person should read it.
///
/// `canonicalize` returns a verbatim path on Windows (`\?\D:\...`), which is right for comparing
/// and wrong for showing: the prefix is a filesystem detail, not part of where somebody's folder is.
fn printable_path(path: &Path) -> String {
    let text = path.to_string_lossy().to_string();
    match text.strip_prefix(r"\\?\") {
        Some(rest) => match rest.strip_prefix(r"UNC\") {
            Some(share) => format!(r"\\{share}"),
            None => rest.to_string(),
        },
        None => text,
    }
}

/// One folder in the workspace-root picker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceDirectory {
    pub name: String,
    /// The path relative to the node's workspace root, with `/` separators - what a create request
    /// takes back.
    pub path: String,
    /// Already the directory of a workspace: choosing it again is refused, so the picker says so
    /// rather than letting somebody walk into a dead end.
    pub taken: bool,
    /// Which workspace, when the caller may see it. Absent for a workspace they have no role in and
    /// for a folder nobody claimed.
    #[serde(default)]
    pub workspace_name: Option<String>,
}

/// One level of the workspace-root picker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceDirectoryListing {
    /// The node's own workspace root, absolute - where browsing starts and where a relative path is
    /// resolved against.
    pub root: String,
    /// Every root a workspace may live under (`workspace_root` plus any configured extras), so the
    /// console can show why a folder somewhere else is or is not acceptable.
    pub roots: Vec<String>,
    /// The path being listed, relative to the root (`""` is the root itself).
    pub path: String,
    /// The parent to go up to, or `None` at the root.
    pub parent: Option<String>,
    /// Whether this folder itself can be chosen. The root cannot: a workspace rooted at the root would
    /// see every other workspace's files.
    pub selectable: bool,
    pub directories: Vec<WorkspaceDirectory>,
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
        self.create_session_scoped(None, user_id, title, owner).await
    }

    /// Create a session inside a workspace.
    ///
    /// Ownership follows the workspace rather than the caller's stated user: a workspace's sessions
    /// belong to whoever owns the workspace, which is what makes sharing the workspace enough to
    /// share everything inside it. A caller who is not the owner still gets the session - whether
    /// they may create one at all is the gateway's decision - but the record does not pretend they
    /// own it, because a session whose owner and workspace disagree is a record that cannot say who
    /// decides.
    pub async fn create_session_in(
        &self,
        workspace: &WorkspaceId,
        user_id: &str,
        title: &str,
        owner: Option<agentos_core::model::PrincipalRef>,
    ) -> Result<SessionRecord> {
        self.create_session_scoped(Some(workspace), user_id, title, owner).await
    }

    async fn create_session_scoped(
        &self,
        workspace: Option<&WorkspaceId>,
        user_id: &str,
        title: &str,
        owner: Option<agentos_core::model::PrincipalRef>,
    ) -> Result<SessionRecord> {
        // A session must belong to a workspace that exists: writing the id of a workspace nobody
        // created would produce a session whose jail is a directory with no owner, which is worse
        // than a refusal.
        let workspace_record = match workspace {
            Some(id) => Some(self.get_workspace(id).await?.ok_or_else(|| {
                RuntimeError::not_found(format!("workspace {id} does not exist"))
                    .with_detail("workspace_id", id.as_str())
            })?),
            None => None,
        };
        let mut record = SessionRecord::new(user_id, title);
        if let Some(owner) = owner {
            record.owner = Some(owner);
        }
        if let Some(workspace_record) = &workspace_record {
            record.workspace_id = Some(workspace_record.id.clone());
            record.owner = Some(workspace_record.owner.clone());
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
                        "workspace_id": record.workspace_id.as_ref().map(|id| id.as_str()),
                    })),
            )
            .await?;

        Ok(record)
    }

    // ---------------------------------------------------------------------------------------------
    // workspaces
    // ---------------------------------------------------------------------------------------------

    fn workspace_collection(&self) -> Collection<WorkspaceRecord> {
        Collection::new(collections::WORKSPACES)
    }

    /// Create a workspace. Ownership is the creator's, on this node unless they named another.
    ///
    /// The directory is required and is *chosen*, not derived from the name: a rename must never
    /// move files, and the path a session was handed must keep pointing at the same place. It must
    /// be inside the node's workspace root - a workspace rooted at the root itself, or outside it,
    /// would be able to read every other workspace's files.
    pub async fn create_workspace(
        &self,
        name: &str,
        directory: &str,
        owner: &Principal,
    ) -> Result<WorkspaceRecord> {
        let name = name.trim();
        if name.is_empty() {
            return Err(RuntimeError::invalid_input("a workspace name must not be empty"));
        }
        let directory = directory.trim();
        if directory.is_empty() {
            return Err(RuntimeError::invalid_input(
                "a workspace needs a directory: name the folder under the workspace root (or an absolute \
                 path inside it)",
            )
            .with_detail("field", "directory"));
        }
        let resolved = self.resolve_workspace_directory(directory).await?;

        // One directory, one workspace. Two workspaces sharing a jail would be one workspace's files
        // reachable from the other, which is precisely what workspaces exist to prevent - and which
        // nothing later in the runtime would catch, because both records look legitimate.
        for existing in self.list_workspaces().await? {
            if existing.directory_name() == directory {
                return Err(RuntimeError::conflict(format!(
                    "workspace {} already works in {directory}",
                    existing.name
                ))
                .with_detail("workspace_id", existing.id.as_str()));
            }
            let other = PathBuf::from(existing.directory_name());
            let other = if other.is_absolute() {
                other
            } else {
                self.deps.workspace_root.join(other)
            };
            if let Ok(canonical) = tokio::fs::canonicalize(&other).await {
                if canonical == resolved {
                    return Err(RuntimeError::conflict(format!(
                        "workspace {} already works in {} - one directory, one workspace",
                        existing.name,
                        resolved.display()
                    ))
                    .with_detail("workspace_id", existing.id.as_str()));
                }
            }
        }

        let owner_ref = PrincipalRef::new(
            owner.user_id.clone(),
            owner.node_id.clone().or_else(|| Some(self.node_id.clone())),
        );
        let record = WorkspaceRecord::new_in(owner_ref, name, Some(directory.to_string()));
        self.workspace_collection()
            .save(self.store.as_ref(), record.id.as_str(), &record)
            .await?;
        self.bus
            .publish(
                NewEvent::new(EventKind::WorkspaceCreated, "workspace created")
                    .node(self.node_id.clone())
                    .payload(serde_json::json!({
                        "workspace_id": record.id.as_str(),
                        "name": record.name,
                        "directory": directory,
                        "owner": record.owner_label(),
                    })),
            )
            .await?;
        Ok(record)
    }

    /// Turn a chosen directory into the path that must exist behind it.
    ///
    /// Creates it if it is not there (naming a folder is as good as making one), then refuses
    /// anything that is not a strict descendant of the node root.
    async fn resolve_workspace_directory(&self, directory: &str) -> Result<PathBuf> {
        let roots = self.allowed_workspace_roots().await?;
        let canonical = self.resolve_directory(directory, true).await?;
        // A workspace rooted *at* an allowed root would see every other workspace under it.
        if roots.contains(&canonical) {
            return Err(RuntimeError::invalid_input(format!(
                "a workspace needs a folder inside {} - the root itself would contain every other \
                 workspace",
                canonical.display()
            ))
            .with_detail("directory", directory));
        }
        Ok(canonical)
    }

    /// Every root a workspace directory may live under.
    ///
    /// `workspace_root` is where the runtime's own state lives; the extras are for an operator who
    /// wants workspaces over project folders somewhere else (`D:\projects`). Empty by default: a node
    /// that has not said so does not let a caller point a workspace at an arbitrary path.
    pub async fn allowed_workspace_roots(&self) -> Result<Vec<PathBuf>> {
        let mut roots: Vec<PathBuf> = Vec::new();
        let configured = std::iter::once(&self.deps.workspace_root)
            .chain(self.deps.extra_workspace_roots.iter());
        for candidate in configured {
            match tokio::fs::canonicalize(candidate).await {
                Ok(canonical) => {
                    if !roots.contains(&canonical) {
                        roots.push(canonical);
                    }
                }
                // A configured root that is not there is ignored rather than fatal: the runtime still
                // has its own workspace root, and a missing NAS mount should not stop it booting.
                Err(error) => tracing::warn!(
                    root = %candidate.display(),
                    %error,
                    "configured workspace root is unusable; ignored"
                ),
            }
        }
        if roots.is_empty() {
            return Err(RuntimeError::unavailable(format!(
                "workspace root {} is unusable",
                self.deps.workspace_root.display()
            )));
        }
        Ok(roots)
    }

    /// A directory inside one of the allowed roots, optionally created first.
    ///
    /// Refusals are checked after canonicalisation, so a symlink out of a root is caught too. The
    /// roots themselves are allowed here - they are legal places to *look*; refusing one as a
    /// *workspace* is `resolve_workspace_directory`'s job.
    async fn resolve_directory(&self, directory: &str, create: bool) -> Result<PathBuf> {
        let requested = Path::new(directory);
        if requested
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err(RuntimeError::invalid_input(format!(
                "a workspace directory may not climb out of a workspace root: {directory}"
            ))
            .with_detail("field", "directory"));
        }
        let roots = self.allowed_workspace_roots().await?;
        // Relative means "inside the node's own root", which is the default the console offers.
        let target = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            roots[0].join(requested)
        };
        if create {
            tokio::fs::create_dir_all(&target).await.map_err(|error| {
                RuntimeError::invalid_input(format!(
                    "cannot create workspace directory {}: {error}",
                    target.display()
                ))
            })?;
        }
        let canonical = tokio::fs::canonicalize(&target).await.map_err(|error| {
            RuntimeError::invalid_input(format!(
                "workspace directory {} is unusable: {error}",
                target.display()
            ))
        })?;
        if !roots.iter().any(|root| canonical.starts_with(root)) {
            return Err(RuntimeError::invalid_input(format!(
                "a workspace directory must live under one of {}: {}",
                roots
                    .iter()
                    .map(|root| root.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                canonical.display()
            ))
            .with_detail("directory", directory));
        }
        Ok(canonical)
    }

    /// The folders a person can pick a workspace from, one level at a time.
    ///
    /// Read-only and scoped to the node's own workspace root: a picker for the directories
    /// workspaces live in, never a file browser for the machine. A folder already claimed by a
    /// workspace is reported as taken, with the workspace's name only when the caller may see that
    /// workspace - a picker must not become the way around the discovery index beside it.
    pub async fn browse_workspace_root(
        &self,
        path: &str,
        principal: &Principal,
    ) -> Result<WorkspaceDirectoryListing> {
        let roots = self.allowed_workspace_roots().await?;
        let root = roots[0].clone();
        let current = if path.trim().is_empty() {
            root.clone()
        } else {
            // `false`: browsing must not create the folder it is looking at.
            self.resolve_directory(path, false).await?
        };
        // Under the node's own root a path comes back relative (what a create request takes); under an
        // extra root it comes back absolute, because there is no single root to be relative to.
        let relative = |target: &Path| -> String {
            match target.strip_prefix(&root) {
                Ok(rest) => rest.to_string_lossy().replace('\\', "/"),
                Err(_) => printable_path(target),
            }
        };

        // Who claims which directory, resolved once for the whole listing.
        let mut claimed: HashMap<String, Option<String>> = HashMap::new();
        for workspace in self.list_workspaces().await? {
            let named = PathBuf::from(workspace.directory_name());
            let target = if named.is_absolute() { named } else { root.join(named) };
            if let Ok(canonical) = tokio::fs::canonicalize(&target).await {
                let visible = principal.is_admin()
                    || agentos_core::model::workspace_role(&workspace, principal).is_some();
                let label = if visible { Some(workspace.name.clone()) } else { None };
                claimed.insert(canonical.to_string_lossy().to_string(), label);
            }
        }

        let mut directories: Vec<WorkspaceDirectory> = Vec::new();
        let mut reader = tokio::fs::read_dir(&current).await.map_err(|error| {
            RuntimeError::invalid_input(format!("cannot read {}: {error}", current.display()))
        })?;
        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|error| RuntimeError::internal(format!("reading {}: {error}", current.display())))?
        {
            let file_type = entry
                .file_type()
                .await
                .map_err(|error| RuntimeError::internal(format!("reading {}: {error}", current.display())))?;
            if !file_type.is_dir() {
                continue;
            }
            let Ok(canonical) = tokio::fs::canonicalize(entry.path()).await else {
                continue;
            };
            // A symlink pointing outside the root is not a place a workspace may live.
            if !canonical.starts_with(&root) {
                continue;
            }
            let claimed_by = claimed.get(&canonical.to_string_lossy().to_string()).cloned();
            directories.push(WorkspaceDirectory {
                name: entry.file_name().to_string_lossy().to_string(),
                path: relative(&canonical),
                taken: claimed_by.is_some(),
                workspace_name: claimed_by.flatten(),
            });
        }
        directories.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(WorkspaceDirectoryListing {
            root: printable_path(&root),
            roots: roots.iter().map(|root| printable_path(root)).collect(),
            path: relative(&current),
            parent: if current == root {
                None
            } else {
                Some(relative(current.parent().unwrap_or(&root)))
            },
            selectable: current != root,
            directories,
        })
    }

    pub async fn get_workspace(&self, id: &WorkspaceId) -> Result<Option<WorkspaceRecord>> {
        self.workspace_collection().load(self.store.as_ref(), id.as_str()).await
    }

    /// Newest first, so a console opens on what somebody was last working in.
    pub async fn list_workspaces(&self) -> Result<Vec<WorkspaceRecord>> {
        let mut records = self.workspace_collection().list(self.store.as_ref(), 10_000).await?;
        records.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(records)
    }

    pub async fn rename_workspace(&self, id: &WorkspaceId, name: &str) -> Result<WorkspaceRecord> {
        let Some(mut record) = self.get_workspace(id).await? else {
            return Err(RuntimeError::not_found(format!("workspace {id} does not exist")));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(RuntimeError::invalid_input("a workspace name must not be empty"));
        }
        record.name = name.to_string();
        record.updated_at = now_ms();
        self.workspace_collection()
            .save(self.store.as_ref(), record.id.as_str(), &record)
            .await?;
        self.bus
            .publish(
                NewEvent::new(EventKind::WorkspaceRenamed, "workspace renamed")
                    .node(self.node_id.clone())
                    .payload(serde_json::json!({
                        "workspace_id": record.id.as_str(),
                        "name": record.name,
                    })),
            )
            .await?;
        Ok(record)
    }

    /// Hand out a role on a workspace. Only its owner (or an admin) may do this, and the gateway
    /// checks that before calling here.
    pub async fn grant_workspace(
        &self,
        id: &WorkspaceId,
        mut grant: SessionGrant,
        granted_by: &Principal,
    ) -> Result<WorkspaceRecord> {
        let Some(mut record) = self.get_workspace(id).await? else {
            return Err(RuntimeError::not_found(format!("workspace {id} does not exist")));
        };
        if record.owner.matches(&grant.as_ref()) && grant.role != SessionRole::Owner {
            return Err(RuntimeError::invalid_input(format!(
                "{} owns this workspace: they hold every right already",
                record.owner
            )));
        }
        grant.granted_by = Some(granted_by.as_ref().to_string());
        grant.granted_at = Some(now_ms());
        record.grants.retain(|existing| !existing.as_ref().matches(&grant.as_ref()));
        record.grants.push(grant.clone());
        record.updated_at = now_ms();
        self.workspace_collection()
            .save(self.store.as_ref(), record.id.as_str(), &record)
            .await?;
        self.bus
            .publish(
                NewEvent::new(EventKind::WorkspaceAccessGranted, "workspace access granted")
                    .node(self.node_id.clone())
                    .payload(serde_json::json!({
                        "workspace_id": record.id.as_str(),
                        "principal": grant.as_ref().to_string(),
                        "role": grant.role.as_str(),
                        "granted_by": grant.granted_by,
                    })),
            )
            .await?;
        Ok(record)
    }

    /// Take a role away again.
    pub async fn revoke_workspace(&self, id: &WorkspaceId, who: &PrincipalRef) -> Result<WorkspaceRecord> {
        let Some(mut record) = self.get_workspace(id).await? else {
            return Err(RuntimeError::not_found(format!("workspace {id} does not exist")));
        };
        record.grants.retain(|grant| !grant.as_ref().matches(who));
        record.updated_at = now_ms();
        self.workspace_collection()
            .save(self.store.as_ref(), record.id.as_str(), &record)
            .await?;
        self.bus
            .publish(
                NewEvent::new(EventKind::WorkspaceAccessRevoked, "workspace access revoked")
                    .node(self.node_id.clone())
                    .payload(serde_json::json!({
                        "workspace_id": record.id.as_str(),
                        "principal": who.to_string(),
                    })),
            )
            .await?;
        Ok(record)
    }

    /// The workspace a principal falls back to when they create a session without naming one.
    ///
    /// One per owner, marked in metadata so it can be found without guessing at a name. It is a real
    /// workspace in every other respect - listed, shared, isolated - which is what makes "every
    /// session has a workspace" true without a special case anywhere else.
    pub async fn ensure_default_workspace(&self, principal: &Principal) -> Result<WorkspaceRecord> {
        let me = principal.as_ref();
        for record in self.list_workspaces().await? {
            if record.owner.matches(&me)
                && record.metadata.get("default").map(|value| value == "true").unwrap_or(false)
            {
                return Ok(record);
            }
        }
        let mut record = self
            .create_workspace(
                &format!("{}'s workspace", principal.user_id),
                &self.default_workspace_directory(principal).to_string_lossy(),
                principal,
            )
            .await?;
        record.metadata.insert("default".into(), "true".into());
        self.workspace_collection()
            .save(self.store.as_ref(), record.id.as_str(), &record)
            .await?;
        Ok(record)
    }

    /// Where a caller's default workspace lives: a folder named after them, under the node root.
    ///
    /// The default has to pick a directory without asking, and it must not collide between two
    /// people on one node - so it is their user id, sanitised to something a filesystem accepts.
    fn default_workspace_directory(&self, principal: &Principal) -> PathBuf {
        let slug: String = principal
            .user_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .collect();
        let slug = slug.trim_matches('-').to_string();
        let slug = if slug.is_empty() { "default".to_string() } else { slug };
        PathBuf::from(format!("{slug}-workspace"))
    }

    /// Replace a workspace's capability narrowing. Checked against what exists for the same reason a
    /// session's is: an allow list naming a capability nobody registered is a typo that would take
    /// the whole workspace down to nothing.
    pub async fn set_workspace_capabilities(
        &self,
        workspace: &WorkspaceId,
        capabilities: agentos_core::model::SessionCapabilities,
        known: &[String],
        by: &Principal,
    ) -> Result<WorkspaceRecord> {
        let Some(mut record) = self.get_workspace(workspace).await? else {
            return Err(RuntimeError::not_found(format!("workspace {workspace} does not exist")));
        };
        let mut unknown: Vec<String> = Vec::new();
        for name in capabilities
            .allow
            .iter()
            .flatten()
            .chain(capabilities.deny.iter())
            .chain(capabilities.approval_required.iter())
        {
            let covered = match name.strip_suffix('*') {
                Some(prefix) => known.iter().any(|capability| capability.starts_with(prefix)),
                None => known.iter().any(|capability| capability == name),
            };
            if !covered {
                unknown.push(name.clone());
            }
        }
        if !unknown.is_empty() {
            return Err(RuntimeError::invalid_input(format!(
                "this runtime has no capability called {}; it has: {}",
                unknown.join(", "),
                known.join(", ")
            ))
            .with_detail("unknown", unknown.join(", ")));
        }
        record.capabilities = capabilities;
        record.updated_at = now_ms();
        self.workspace_collection()
            .save(self.store.as_ref(), record.id.as_str(), &record)
            .await?;
        self.bus
            .publish(
                NewEvent::new(
                    EventKind::WorkspaceCapabilitiesChanged,
                    "workspace capabilities changed",
                )
                .node(self.node_id.clone())
                .payload(serde_json::json!({
                    "workspace_id": record.id.as_str(),
                    "allow": record.capabilities.allow,
                    "deny": record.capabilities.deny,
                    "approval_required": record.capabilities.approval_required,
                    "by": by.as_ref().to_string(),
                })),
            )
            .await?;
        Ok(record)
    }

    /// Somebody asks for access to a workspace that is not theirs.
    pub async fn request_workspace_access(
        &self,
        workspace: &WorkspaceId,
        principal: &Principal,
        role: SessionRole,
        note: Option<String>,
    ) -> Result<agentos_core::model::SessionAccessRequest> {
        let Some(mut record) = self.get_workspace(workspace).await? else {
            return Err(RuntimeError::not_found(format!("workspace {workspace} does not exist")));
        };
        if let Some(existing) = agentos_core::model::workspace_role(&record, principal) {
            return Err(RuntimeError::conflict(format!(
                "you already hold {} on this workspace",
                existing.as_str()
            )));
        }
        let me = principal.as_ref();
        record.access_requests.retain(|request| {
            !(request.principal.matches(&me)
                && request.state == agentos_core::model::AccessRequestState::Pending)
        });
        let request = agentos_core::model::SessionAccessRequest {
            id: format!("req_{}", agentos_core::now_ms()),
            principal: me.clone(),
            role,
            note,
            created_at: now_ms(),
            state: agentos_core::model::AccessRequestState::Pending,
            decided_by: None,
            decided_at: None,
            granted_role: None,
        };
        record.access_requests.push(request.clone());
        record.updated_at = now_ms();
        self.workspace_collection()
            .save(self.store.as_ref(), record.id.as_str(), &record)
            .await?;
        self.bus
            .publish(
                NewEvent::new(EventKind::WorkspaceAccessRequested, "workspace access requested")
                    .node(self.node_id.clone())
                    .payload(serde_json::json!({
                        "workspace_id": record.id.as_str(),
                        "request_id": request.id,
                        "user_id": request.principal.user_id,
                        "node_id": request.principal.node_id,
                        "role": request.role.as_str(),
                    })),
            )
            .await?;
        Ok(request)
    }

    /// The owner decides a workspace request. Approving hands out the role in the same breath.
    pub async fn decide_workspace_access_request(
        &self,
        workspace: &WorkspaceId,
        request_id: &str,
        approve: bool,
        role: Option<SessionRole>,
        by: &Principal,
    ) -> Result<(WorkspaceRecord, agentos_core::model::SessionAccessRequest)> {
        let Some(mut record) = self.get_workspace(workspace).await? else {
            return Err(RuntimeError::not_found(format!("workspace {workspace} does not exist")));
        };
        let index = record
            .access_requests
            .iter()
            .position(|request| request.id == request_id)
            .ok_or_else(|| RuntimeError::not_found(format!("request {request_id} does not exist")))?;
        let pending = record.access_requests[index].clone();
        if pending.state != agentos_core::model::AccessRequestState::Pending {
            return Err(RuntimeError::conflict(format!(
                "request {request_id} was already {}",
                pending.state.as_str()
            )));
        }
        let granted = role.unwrap_or(pending.role);
        record.access_requests[index].state = if approve {
            agentos_core::model::AccessRequestState::Approved
        } else {
            agentos_core::model::AccessRequestState::Rejected
        };
        record.access_requests[index].decided_by = Some(by.as_ref().to_string());
        record.access_requests[index].decided_at = Some(now_ms());
        if approve {
            record.access_requests[index].granted_role = Some(granted);
        }
        let decided = record.access_requests[index].clone();
        record.updated_at = now_ms();
        self.workspace_collection()
            .save(self.store.as_ref(), record.id.as_str(), &record)
            .await?;
        if approve {
            // Through the same door as a hand-written grant, so an approval is indistinguishable
            // from one in what it produces.
            let grant = SessionGrant::new(
                decided.principal.user_id.clone(),
                decided.principal.node_id.clone(),
                granted,
            );
            record = self.grant_workspace(workspace, grant, by).await?;
        }
        self.bus
            .publish(
                NewEvent::new(
                    EventKind::WorkspaceAccessDecided,
                    if approve { "workspace access approved" } else { "workspace access rejected" },
                )
                .node(self.node_id.clone())
                .payload(serde_json::json!({
                    "workspace_id": record.id.as_str(),
                    "request_id": decided.id,
                    "user_id": decided.principal.user_id,
                    "approved": approve,
                    "role": decided.granted_role,
                    "decided_by": decided.decided_by,
                })),
            )
            .await?;
        Ok((record, decided))
    }

    /// The workspace a session belongs to, if it has one. Legacy records do not.
    pub async fn workspace_of(&self, session: &SessionRecord) -> Result<Option<WorkspaceRecord>> {
        match &session.workspace_id {
            Some(id) => self.get_workspace(id).await,
            None => Ok(None),
        }
    }

    /// The role a principal holds on a session, read through its workspace.
    ///
    /// The one place the gateway and the session manager agree on what someone may do; keeping it
    /// here means a route cannot invent its own reading of the permission table.
    pub async fn role_on(
        &self,
        session: &SessionRecord,
        principal: &Principal,
    ) -> Result<Option<SessionRole>> {
        let workspace = self.workspace_of(session).await?;
        Ok(agentos_core::model::effective_role(
            session,
            workspace.as_ref(),
            principal,
        ))
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
        // A restored conversation is a new session on this node, and it belongs to a workspace on
        // this node too. It goes back into its own workspace when this node still has one; otherwise
        // into the owner's default - pointing at an id that only existed on the node the package came
        // from would leave the session owned by nobody.
        restored.workspace_id = match record.workspace_id.clone() {
            Some(id) if self.get_workspace(&id).await?.is_some() => Some(id),
            _ => match restored.owner.clone() {
                Some(owner) => Some(
                    self.ensure_default_workspace(&Principal::new(
                        owner.user_id,
                        owner.node_id,
                        Vec::new(),
                    ))
                    .await?
                    .id,
                ),
                None => None,
            },
        };
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

    /// The live view of the store: every session someone might work on.
    ///
    /// Archived conversations are deliberately absent. An archived one lives in its package; a row
    /// for it here would be a tombstone in the list people pick work from, and every action on it
    /// answers "no" or "close it first". The archive page is where it belongs, and `list_all` is
    /// there for the operator views that really do want the whole store.
    pub async fn list(&self) -> Result<Vec<SessionSummary>> {
        let mut summaries = self.list_all().await?;
        summaries.retain(|session| session.state != SessionState::Archived);
        Ok(summaries)
    }

    /// Everything in the store, archived records included.
    pub async fn list_all(&self) -> Result<Vec<SessionSummary>> {
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
        // A fork lives where the original lived: leaving it workspace-less would put it in the legacy
        // root and make it reachable only by an admin, which is not what "branch this conversation"
        // means to anybody.
        record.workspace_id = source.workspace_id.clone();
        record.owner = source.owner.clone();
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

/// Answers "what did this session narrow?" from the session record itself.
///
/// Straight from the store, with no cache on purpose: a cached copy of an authorization decision
/// that goes stale in the widening direction is worse than a key lookup on a path that runs once
/// per capability call.
pub struct StoreSessionCapabilities {
    store: Arc<dyn Store>,
}

impl StoreSessionCapabilities {
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl agentos_capability_runtime::policy::SessionCapabilitySource for StoreSessionCapabilities {
    async fn for_scope(
        &self,
        session: &SessionId,
        workspace: Option<&WorkspaceId>,
    ) -> Option<agentos_core::model::SessionCapabilities> {
        // A workspace's narrowing is what its sessions inherit; a session with no workspace keeps
        // its own. Never both: two narrowing sources is one refactor away from disagreeing, and the
        // safer reading of a disagreement is not obvious.
        if let Some(workspace_id) = workspace {
            let collection: Collection<WorkspaceRecord> = Collection::new(collections::WORKSPACES);
            return match collection.load(self.store.as_ref(), workspace_id.as_str()).await {
                Ok(Some(record)) if !record.capabilities.is_unrestricted() => Some(record.capabilities),
                _ => None,
            };
        }
        let collection: Collection<SessionRecord> = Collection::new(collections::SESSIONS);
        match collection.load(self.store.as_ref(), session.as_str()).await {
            Ok(Some(record)) if !record.capabilities.is_unrestricted() => Some(record.capabilities),
            // A store that cannot answer is not a reason to widen: no narrowing is returned, and
            // the node's own policy still applies to every call.
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// session capabilities and access requests
// ---------------------------------------------------------------------------------------------

impl SessionManager {
    /// Replace a session's capability narrowing.
    ///
    /// The names are checked against what the runtime actually has before they are stored: an
    /// allow list naming a capability nobody registered is a typo that would silently take the
    /// whole session down to nothing, and a deny list naming one is a spell that does nothing.
    pub async fn set_capabilities(
        &self,
        session: &SessionId,
        capabilities: agentos_core::model::SessionCapabilities,
        known: &[String],
        by: &agentos_core::model::Principal,
    ) -> Result<SessionRecord> {
        let mut record = self
            .get(session)
            .await?
            .ok_or_else(|| RuntimeError::not_found(format!("session {session} does not exist")))?;
        let mut unknown: Vec<String> = Vec::new();
        for name in capabilities
            .allow
            .iter()
            .flatten()
            .chain(capabilities.deny.iter())
            .chain(capabilities.approval_required.iter())
        {
            // A trailing wildcard names a family, so it is checked as a prefix against what exists.
            let covered = match name.strip_suffix('*') {
                Some(prefix) => known.iter().any(|capability| capability.starts_with(prefix)),
                None => known.iter().any(|capability| capability == name),
            };
            if !covered {
                unknown.push(name.clone());
            }
        }
        if !unknown.is_empty() {
            return Err(RuntimeError::invalid_input(format!(
                "this runtime has no capability called {}; it has: {}",
                unknown.join(", "),
                known.join(", ")
            ))
            .with_detail("unknown", unknown.join(", ")));
        }
        record.capabilities = capabilities;
        record.updated_at = now_ms();
        self.session_collection()
            .save(self.store.as_ref(), record.id.as_str(), &record)
            .await?;
        self.bus
            .publish(
                NewEvent::new(EventKind::SessionCapabilitiesChanged, "session capabilities changed")
                    .session(session.clone())
                    .node(self.node_id.clone())
                    .payload(serde_json::json!({
                        "allow": record.capabilities.allow,
                        "deny": record.capabilities.deny,
                        "approval_required": record.capabilities.approval_required,
                        "by": by.as_ref().to_string(),
                    })),
            )
            .await?;
        Ok(record)
    }

    /// Somebody asks for access to a conversation that is not theirs.
    ///
    /// The same person asking twice replaces their pending request rather than filling the owner's
    /// screen with duplicates: what changed is what they are asking for, not that they are asking.
    pub async fn request_access(
        &self,
        session: &SessionId,
        principal: &agentos_core::model::Principal,
        role: agentos_core::model::SessionRole,
        note: Option<String>,
    ) -> Result<agentos_core::model::SessionAccessRequest> {
        let mut record = self
            .get(session)
            .await?
            .ok_or_else(|| RuntimeError::not_found(format!("session {session} does not exist")))?;
        if let Some(existing) = agentos_core::model::role_of(&record, principal) {
            return Err(RuntimeError::conflict(format!(
                "you already hold {} on this session",
                existing.as_str()
            )));
        }
        let me = principal.as_ref();
        record
            .access_requests
            .retain(|request| {
                !(request.principal.matches(&me)
                    && request.state == agentos_core::model::AccessRequestState::Pending)
            });
        let request = agentos_core::model::SessionAccessRequest {
            id: format!("req_{}", agentos_core::now_ms()),
            principal: me.clone(),
            role,
            note,
            created_at: now_ms(),
            state: agentos_core::model::AccessRequestState::Pending,
            decided_by: None,
            decided_at: None,
            granted_role: None,
        };
        record.access_requests.push(request.clone());
        record.updated_at = now_ms();
        self.session_collection()
            .save(self.store.as_ref(), record.id.as_str(), &record)
            .await?;
        self.bus
            .publish(
                NewEvent::new(EventKind::SessionAccessRequested, "session access requested")
                    .session(session.clone())
                    .node(self.node_id.clone())
                    .payload(serde_json::json!({
                        "request_id": request.id,
                        "user_id": request.principal.user_id,
                        "node_id": request.principal.node_id,
                        "role": request.role.as_str(),
                    })),
            )
            .await?;
        Ok(request)
    }

    /// The owner decides a request. Approving hands out the role in the same breath.
    pub async fn decide_access_request(
        &self,
        session: &SessionId,
        request_id: &str,
        approve: bool,
        role: Option<agentos_core::model::SessionRole>,
        by: &agentos_core::model::Principal,
    ) -> Result<(agentos_core::model::SessionRecord, agentos_core::model::SessionAccessRequest)> {
        let mut record = self
            .get(session)
            .await?
            .ok_or_else(|| RuntimeError::not_found(format!("session {session} does not exist")))?;
        let index = record
            .access_requests
            .iter()
            .position(|request| request.id == request_id)
            .ok_or_else(|| RuntimeError::not_found(format!("request {request_id} does not exist")))?;
        let pending = record.access_requests[index].clone();
        if pending.state != agentos_core::model::AccessRequestState::Pending {
            return Err(RuntimeError::conflict(format!(
                "request {request_id} was already {}",
                pending.state.as_str()
            )));
        }
        let granted = role.unwrap_or(pending.role);
        record.access_requests[index].state = if approve {
            agentos_core::model::AccessRequestState::Approved
        } else {
            agentos_core::model::AccessRequestState::Rejected
        };
        record.access_requests[index].decided_by = Some(by.as_ref().to_string());
        record.access_requests[index].decided_at = Some(now_ms());
        if approve {
            record.access_requests[index].granted_role = Some(granted);
        }
        let decided = record.access_requests[index].clone();
        record.updated_at = now_ms();
        self.session_collection()
            .save(self.store.as_ref(), record.id.as_str(), &record)
            .await?;
        if approve {
            // Through the same door as a hand-written grant, so an approval is indistinguishable
            // from one in what it produces.
            let grant = agentos_core::model::SessionGrant::new(
                decided.principal.user_id.clone(),
                decided.principal.node_id.clone(),
                granted,
            );
            record = self.grant(session, grant, by).await?;
        }
        self.bus
            .publish(
                NewEvent::new(
                    EventKind::SessionAccessDecided,
                    if approve { "session access approved" } else { "session access rejected" },
                )
                .session(session.clone())
                .node(self.node_id.clone())
                .payload(serde_json::json!({
                    "request_id": decided.id,
                    "user_id": decided.principal.user_id,
                    "approved": approve,
                    "role": decided.granted_role,
                    "decided_by": decided.decided_by,
                })),
            )
            .await?;
        Ok((record, decided))
    }
}

// ---------------------------------------------------------------------------------------------
// the access inbox: everything waiting on this person, across every session
// ---------------------------------------------------------------------------------------------

/// One request, with just enough about where it was made to act on it without opening anything.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessInboxEntry {
    /// The workspace the request is about: the scope access is decided at since D20.
    #[serde(default)]
    pub workspace_id: Option<WorkspaceId>,
    #[serde(default)]
    pub workspace_name: Option<String>,
    /// The conversation, when the request was made on one. Present for records written before
    /// workspaces existed, and for the session routes' own requests.
    #[serde(default)]
    pub session_id: Option<SessionId>,
    #[serde(default)]
    pub session_title: Option<String>,
    #[serde(default)]
    pub session_owner: Option<agentos_core::model::PrincipalRef>,
    pub request: agentos_core::model::SessionAccessRequest,
}

/// What is waiting on this principal: requests they must answer, and requests they made.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccessInbox {
    /// Pending requests on sessions where this principal may decide.
    pub to_decide: Vec<AccessInboxEntry>,
    /// Everything they asked for, whatever came of it.
    pub mine: Vec<AccessInboxEntry>,
}

impl SessionManager {
    /// Read every session once and answer both questions from that single pass.
    ///
    /// A per-session walk would be N store reads for a page that exists to save the reader clicks;
    /// the whole collection is small by design (a session is one record) and is read at once here.
    pub async fn access_inbox(&self, principal: &agentos_core::model::Principal) -> Result<AccessInbox> {
        let me = principal.as_ref();
        let mut inbox = AccessInbox::default();

        // Workspaces first: that is where access is decided now, so that is where the requests are.
        for workspace in self.workspace_collection().list(self.store.as_ref(), 10_000).await? {
            let role = agentos_core::model::workspace_role(&workspace, principal);
            let may_decide = principal.is_admin()
                || role
                    .map(|role| agentos_core::model::role_allows(role, agentos_core::model::SessionAction::Grant))
                    .unwrap_or(false);
            for request in &workspace.access_requests {
                let entry = AccessInboxEntry {
                    workspace_id: Some(workspace.id.clone()),
                    workspace_name: Some(workspace.name.clone()),
                    session_id: None,
                    session_title: None,
                    session_owner: Some(workspace.owner.clone()),
                    request: request.clone(),
                };
                if request.principal.matches(&me) {
                    inbox.mine.push(entry.clone());
                }
                if may_decide && request.state == agentos_core::model::AccessRequestState::Pending {
                    inbox.to_decide.push(entry);
                }
            }
        }

        // Then the records that predate workspaces. They are the only ones that can still carry a
        // session-level request, because the session routes delegate to the workspace.
        for record in self.session_collection().list(self.store.as_ref(), 10_000).await? {
            if record.workspace_id.is_some() || record.access_requests.is_empty() {
                continue;
            }
            let role = agentos_core::model::role_of(&record, principal);
            let may_decide = principal.is_admin()
                || role
                    .map(|role| agentos_core::model::role_allows(role, agentos_core::model::SessionAction::Grant))
                    .unwrap_or(false);
            for request in &record.access_requests {
                let entry = AccessInboxEntry {
                    workspace_id: None,
                    workspace_name: None,
                    session_id: Some(record.id.clone()),
                    session_title: Some(record.title.clone()),
                    session_owner: record.owner.clone(),
                    request: request.clone(),
                };
                if request.principal.matches(&me) {
                    inbox.mine.push(entry.clone());
                }
                if may_decide && request.state == agentos_core::model::AccessRequestState::Pending {
                    inbox.to_decide.push(entry);
                }
            }
        }

        // Newest first, in both lists: a request is only interesting while it is fresh.
        inbox.to_decide.sort_by(|a, b| b.request.created_at.cmp(&a.request.created_at));
        inbox.mine.sort_by(|a, b| b.request.created_at.cmp(&a.request.created_at));
        Ok(inbox)
    }
}
