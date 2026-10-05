//! The Capability trait and its invocation context.

use crate::workspace::Workspace;
use agentos_core::error::Result;
use agentos_core::model::{CapabilityDescriptor, CapabilityPermission};
use agentos_core::telemetry::Correlation;
use agentos_core::{ActorId, CapabilityId, SessionId, TaskId, WorkspaceId};
use agentos_storage::artifact::ArtifactStore;
use async_trait::async_trait;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Who is asking. Used by policy, by event correlation and by auditing.
#[derive(Debug, Clone)]
pub struct CallerContext {
    pub session_id: SessionId,
    pub actor_id: Option<ActorId>,
    pub task_id: Option<TaskId>,
    /// The workspace this session belongs to.
    ///
    /// The jail a capability may touch is derived from this and never from a path the model wrote: a
    /// path is input, a workspace is identity, and only the second decides what is reachable.
    pub workspace: Option<WorkspaceId>,
    pub correlation: Correlation,
    pub cancellation: CancellationToken,
}

impl CallerContext {
    pub fn new(session_id: SessionId) -> Self {
        Self {
            correlation: Correlation::new().with_session(&session_id),
            session_id,
            actor_id: None,
            task_id: None,
            workspace: None,
            cancellation: CancellationToken::new(),
        }
    }

    pub fn with_task(mut self, task: TaskId) -> Self {
        self.task_id = Some(task);
        self
    }

    pub fn with_actor(mut self, actor: ActorId) -> Self {
        self.actor_id = Some(actor);
        self
    }

    pub fn with_workspace(mut self, workspace: Option<WorkspaceId>) -> Self {
        self.workspace = workspace;
        self
    }
}

/// Everything an implementation may touch. Note there is no ambient access to the store, the
/// network or the process: a capability can only use what is handed to it here.
#[derive(Clone)]
pub struct CapabilityContext {
    pub capability_id: CapabilityId,
    pub caller: CallerContext,
    /// Permission set granted for THIS call, already intersected with policy.
    pub permission: CapabilityPermission,
    pub workspace: Arc<Workspace>,
    pub artifacts: Option<Arc<dyn ArtifactStore>>,
    pub timeout_ms: u64,
}

impl CapabilityContext {
    pub fn is_cancelled(&self) -> bool {
        self.caller.cancellation.is_cancelled()
    }
}

impl std::fmt::Debug for CapabilityContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilityContext")
            .field("capability_id", &self.capability_id)
            .field("session_id", &self.caller.session_id)
            .field("permission", &self.permission)
            .field("timeout_ms", &self.timeout_ms)
            .finish()
    }
}

/// The one interface every capability implements, regardless of where it executes.
#[async_trait]
pub trait Capability: Send + Sync + 'static {
    fn descriptor(&self) -> CapabilityDescriptor;
    async fn invoke(&self, input: serde_json::Value, ctx: CapabilityContext) -> Result<serde_json::Value>;
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InvocationResult {
    pub capability_id: CapabilityId,
    pub name: String,
    pub version: String,
    pub output: serde_json::Value,
    pub duration_ms: u64,
    pub attempts: u32,
    pub provider: String,
}
