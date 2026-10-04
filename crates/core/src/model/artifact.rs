use crate::ids::{AgentId, ArtifactId, SessionId, TaskId};
use crate::time::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Text,
    Json,
    File,
    Binary,
    Log,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub id: ArtifactId,
    pub session_id: SessionId,
    pub task_id: Option<TaskId>,
    pub agent_id: Option<AgentId>,
    pub kind: ArtifactKind,
    pub name: String,
    pub content_type: String,
    pub size: u64,
    pub sha256: String,
    /// Backend-specific locator (file path, key, url). Never interpreted by upper layers.
    pub storage_ref: String,
    pub created_at: Timestamp,
    pub preview: Option<String>,
}

impl ArtifactRecord {
    pub fn new(session_id: SessionId, name: impl Into<String>, kind: ArtifactKind, size: u64) -> Self {
        Self {
            id: ArtifactId::new(),
            session_id,
            task_id: None,
            agent_id: None,
            kind,
            name: name.into(),
            content_type: "application/octet-stream".into(),
            size,
            sha256: String::new(),
            storage_ref: String::new(),
            created_at: crate::now_ms(),
            preview: None,
        }
    }
}
