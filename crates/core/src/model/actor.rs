use crate::ids::{ActorId, CheckpointId, SessionId, WorkerId};
use crate::state::{ActorState, MigrationState};
use crate::time::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorRecord {
    pub id: ActorId,
    pub session_id: SessionId,
    pub kind: String,
    pub state: ActorState,
    pub worker_id: Option<WorkerId>,
    /// Incremented on every restart or migration. A stale generation must not be resumed.
    pub generation: u64,
    pub mailbox_depth: usize,
    /// Last fully applied sequence number for this actor.
    pub last_applied_seq: u64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl ActorRecord {
    pub fn new(id: ActorId, session_id: SessionId, kind: impl Into<String>) -> Self {
        let now = crate::now_ms();
        Self {
            id,
            session_id,
            kind: kind.into(),
            state: ActorState::Spawning,
            worker_id: None,
            generation: 0,
            mailbox_depth: 0,
            last_applied_seq: 0,
            created_at: now,
            updated_at: now,
        }
    }
}

/// Metadata describing a persisted actor checkpoint. The payload itself lives in the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointMeta {
    pub id: CheckpointId,
    pub actor_id: ActorId,
    pub session_id: SessionId,
    pub generation: u64,
    /// Number of mailbox messages already applied to the state.
    pub applied_seq: u64,
    /// Event log offset to replay after the snapshot is restored.
    pub event_offset: u64,
    pub bytes: u64,
    pub state_hash: String,
    pub domain_version: String,
    pub created_at: Timestamp,
}

/// A checkpoint plus its serialized state. Serializable by design: actor state is data, never a
/// process image, which is what allows cloning and migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub meta: CheckpointMeta,
    pub state: serde_json::Value,
}

/// Alias kept for readability in migration code.
pub type ActorSnapshotMeta = CheckpointMeta;

/// A clone request: same state, new identity. Cloning is defined in terms of a checkpoint, so it
/// works across workers without copying a process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneRequest {
    pub source_actor: ActorId,
    pub target_session: SessionId,
    pub target_worker: Option<WorkerId>,
}

/// Result of a migration pipeline run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationReport {
    pub actor_id: ActorId,
    pub session_id: SessionId,
    pub from_worker: Option<WorkerId>,
    pub to_worker: Option<WorkerId>,
    pub state: MigrationState,
    pub checkpoint_id: Option<CheckpointId>,
    pub replayed_events: u64,
    pub duration_ms: u64,
    pub error: Option<String>,
}
