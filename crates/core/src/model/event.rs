use crate::ids::{
    ActorId, AgentId, ArtifactId, CapabilityId, CorrelationId, EventId, SessionId, TaskId, WorkerId,
};
use crate::time::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSeverity {
    Debug,
    Info,
    Warn,
    Error,
}

impl EventSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            EventSeverity::Debug => "debug",
            EventSeverity::Info => "info",
            EventSeverity::Warn => "warn",
            EventSeverity::Error => "error",
        }
    }
}

/// The runtime event vocabulary. This is the audit log, the UI feed and the replay source for
/// actor recovery - one stream, three consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SessionCreated,
    SessionClosed,
    /// A closed session was opened again: the record is active and an actor holds the conversation.
    SessionOpened,
    /// The owner handed someone a role on a session.
    SessionAccessGranted,
    /// The owner took it back.
    SessionAccessRevoked,
    /// The conversation was written into an archive package and its record became a tombstone.
    SessionArchived,
    /// An archive package was deleted. The record keeps saying the conversation exists elsewhere.
    SessionArchiveDeleted,
    /// A conversation came back from a package, as a new session.
    SessionRestored,
    SessionMessageQueued,
    SessionMessageHandled,
    ActorSpawned,
    ActorStopped,
    ActorRestarted,
    ActorMigrated,
    ActorCloned,
    SnapshotCreated,
    SnapshotRestored,
    EventReplayed,
    RunCreated,
    RunCompleted,
    RunFailed,
    AgentStep,
    ModelCall,
    ModelResult,
    ToolCall,
    ToolResult,
    TaskCreated,
    TaskQueued,
    TaskStarted,
    TaskRetrying,
    TaskCompleted,
    TaskFailed,
    TaskCancelled,
    CapabilityRegistered,
    CapabilityInvoked,
    CapabilityDenied,
    WorkerRegistered,
    WorkerHeartbeat,
    WorkerOffline,
    ArtifactCreated,
    MemoryWritten,
    PolicyDenied,
    /// Turns that left the history window were summarised into one memory record.
    SessionCompacted,
    /// Project instruction files were read from the workspace into a prompt.
    ContextLoaded,
    /// A capability call is parked until a human decides.
    ApprovalRequested,
    ApprovalGranted,
    ApprovalDenied,
    /// Nobody decided within the configured window, so the call gave up.
    ApprovalExpired,
    /// A streamed fragment of an answer that is still being written. Best effort: a missing delta
    /// costs a redraw, never an answer.
    AgentDelta,
    /// A session was renamed.
    SessionRenamed,
    /// Memories were read back into a prompt. Deliberately not MemoryWritten: a recall is a read,
    /// and an operator watching the stream needs to tell the two apart.
    MemoryRecalled,
    /// Another node on this machine or network became visible.
    NodeDiscovered,
    /// A previously visible node stopped advertising.
    NodeLost,
    Error,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::SessionCreated => "session_created",
            EventKind::SessionClosed => "session_closed",
            EventKind::SessionOpened => "session_opened",
            EventKind::SessionAccessGranted => "session_access_granted",
            EventKind::SessionAccessRevoked => "session_access_revoked",
            EventKind::SessionArchived => "session_archived",
            EventKind::SessionArchiveDeleted => "session_archive_deleted",
            EventKind::SessionRestored => "session_restored",
            EventKind::SessionMessageQueued => "session_message_queued",
            EventKind::SessionMessageHandled => "session_message_handled",
            EventKind::ActorSpawned => "actor_spawned",
            EventKind::ActorStopped => "actor_stopped",
            EventKind::ActorRestarted => "actor_restarted",
            EventKind::ActorMigrated => "actor_migrated",
            EventKind::ActorCloned => "actor_cloned",
            EventKind::SnapshotCreated => "snapshot_created",
            EventKind::SnapshotRestored => "snapshot_restored",
            EventKind::EventReplayed => "event_replayed",
            EventKind::RunCreated => "run_created",
            EventKind::RunCompleted => "run_completed",
            EventKind::RunFailed => "run_failed",
            EventKind::AgentStep => "agent_step",
            EventKind::ModelCall => "model_call",
            EventKind::ModelResult => "model_result",
            EventKind::ToolCall => "tool_call",
            EventKind::ToolResult => "tool_result",
            EventKind::TaskCreated => "task_created",
            EventKind::TaskQueued => "task_queued",
            EventKind::TaskStarted => "task_started",
            EventKind::TaskRetrying => "task_retrying",
            EventKind::TaskCompleted => "task_completed",
            EventKind::TaskFailed => "task_failed",
            EventKind::TaskCancelled => "task_cancelled",
            EventKind::CapabilityRegistered => "capability_registered",
            EventKind::CapabilityInvoked => "capability_invoked",
            EventKind::CapabilityDenied => "capability_denied",
            EventKind::WorkerRegistered => "worker_registered",
            EventKind::WorkerHeartbeat => "worker_heartbeat",
            EventKind::WorkerOffline => "worker_offline",
            EventKind::ArtifactCreated => "artifact_created",
            EventKind::MemoryWritten => "memory_written",
            EventKind::MemoryRecalled => "memory_recalled",
            EventKind::ContextLoaded => "context_loaded",
            EventKind::SessionCompacted => "session_compacted",
            EventKind::SessionRenamed => "session_renamed",
            EventKind::AgentDelta => "agent_delta",
            EventKind::ApprovalRequested => "approval_requested",
            EventKind::ApprovalGranted => "approval_granted",
            EventKind::ApprovalDenied => "approval_denied",
            EventKind::ApprovalExpired => "approval_expired",
            EventKind::PolicyDenied => "policy_denied",
            EventKind::NodeDiscovered => "node_discovered",
            EventKind::NodeLost => "node_lost",
            EventKind::Error => "error",
        }
    }
}

/// Builder for publishing. Keeps call sites short and the payload shape consistent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewEvent {
    pub kind: EventKind,
    pub severity: EventSeverity,
    pub message: String,
    pub payload: serde_json::Value,
    pub session_id: Option<SessionId>,
    pub actor_id: Option<ActorId>,
    pub agent_id: Option<AgentId>,
    pub task_id: Option<TaskId>,
    pub capability_id: Option<CapabilityId>,
    pub worker_id: Option<WorkerId>,
    pub artifact_id: Option<ArtifactId>,
    pub correlation_id: Option<CorrelationId>,
    pub node_id: Option<String>,
}

impl NewEvent {
    pub fn new(kind: EventKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            severity: EventSeverity::Info,
            message: message.into(),
            payload: serde_json::Value::Null,
            session_id: None,
            actor_id: None,
            agent_id: None,
            task_id: None,
            capability_id: None,
            worker_id: None,
            artifact_id: None,
            correlation_id: None,
            node_id: None,
        }
    }

    pub fn warn(mut self) -> Self { self.severity = EventSeverity::Warn; self }
    pub fn error(mut self) -> Self { self.severity = EventSeverity::Error; self }
    pub fn debug(mut self) -> Self { self.severity = EventSeverity::Debug; self }
    pub fn payload(mut self, v: impl Into<serde_json::Value>) -> Self { self.payload = v.into(); self }
    pub fn session(mut self, id: SessionId) -> Self { self.session_id = Some(id); self }
    pub fn actor(mut self, id: ActorId) -> Self { self.actor_id = Some(id); self }
    pub fn agent(mut self, id: AgentId) -> Self { self.agent_id = Some(id); self }
    pub fn task(mut self, id: TaskId) -> Self { self.task_id = Some(id); self }
    pub fn capability(mut self, id: CapabilityId) -> Self { self.capability_id = Some(id); self }
    pub fn worker(mut self, id: WorkerId) -> Self { self.worker_id = Some(id); self }
    pub fn artifact(mut self, id: ArtifactId) -> Self { self.artifact_id = Some(id); self }
    pub fn correlation(mut self, id: CorrelationId) -> Self { self.correlation_id = Some(id); self }
    pub fn node(mut self, id: impl Into<String>) -> Self { self.node_id = Some(id.into()); self }
}

/// Persisted, replayable event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: EventId,
    /// Monotonic per-node sequence. The replay cursor for actor recovery.
    pub seq: u64,
    pub kind: EventKind,
    pub severity: EventSeverity,
    pub ts: Timestamp,
    pub message: String,
    pub payload: serde_json::Value,
    pub node_id: Option<String>,
    pub correlation_id: Option<CorrelationId>,
    pub session_id: Option<SessionId>,
    pub actor_id: Option<ActorId>,
    pub agent_id: Option<AgentId>,
    pub task_id: Option<TaskId>,
    pub capability_id: Option<CapabilityId>,
    pub worker_id: Option<WorkerId>,
    pub artifact_id: Option<ArtifactId>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventFilter {
    pub kinds: Vec<EventKind>,
    pub session_id: Option<SessionId>,
    pub actor_id: Option<ActorId>,
    pub task_id: Option<TaskId>,
    pub agent_id: Option<AgentId>,
    pub min_severity: Option<EventSeverity>,
    /// Only events with seq strictly greater than this.
    pub after_seq: Option<u64>,
    pub limit: usize,
}

impl EventFilter {
    pub fn matches(&self, e: &EventRecord) -> bool {
        if !self.kinds.is_empty() && !self.kinds.contains(&e.kind) {
            return false;
        }
        if let Some(s) = &self.session_id {
            if e.session_id.as_ref() != Some(s) { return false; }
        }
        if let Some(s) = &self.actor_id {
            if e.actor_id.as_ref() != Some(s) { return false; }
        }
        if let Some(s) = &self.task_id {
            if e.task_id.as_ref() != Some(s) { return false; }
        }
        if let Some(s) = &self.agent_id {
            if e.agent_id.as_ref() != Some(s) { return false; }
        }
        if let Some(min) = self.min_severity {
            let rank = |s: EventSeverity| match s {
                EventSeverity::Debug => 0,
                EventSeverity::Info => 1,
                EventSeverity::Warn => 2,
                EventSeverity::Error => 3,
            };
            if rank(e.severity) < rank(min) { return false; }
        }
        if let Some(after) = self.after_seq {
            if e.seq <= after { return false; }
        }
        true
    }
}
