use crate::ids::{AgentId, EventId, MemoryId, SessionId};
use crate::time::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// Current conversation window.
    ShortTerm,
    /// What happened, summarized.
    Episode,
    /// Durable facts and preferences.
    Semantic,
    /// Documents and knowledge base entries.
    Knowledge,
    /// Reserved for embedding-based retrieval.
    Vector,
}

/// A memory record. Retrieval is deliberately a trait, so an embedding store or a vector DB can
/// replace the local implementation without touching the agent loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: MemoryId,
    pub session_id: SessionId,
    pub agent_id: Option<AgentId>,
    pub kind: MemoryKind,
    pub content: String,
    pub tags: Vec<String>,
    pub importance: f32,
    pub created_at: Timestamp,
    pub expires_at: Option<Timestamp>,
    /// Reserved: populated once an embedding provider is configured.
    pub embedding: Option<Vec<f32>>,
    pub source_event: Option<EventId>,
}

impl MemoryRecord {
    pub fn new(session_id: SessionId, kind: MemoryKind, content: impl Into<String>) -> Self {
        Self {
            id: MemoryId::new(),
            session_id,
            agent_id: None,
            kind,
            content: content.into(),
            tags: vec![],
            importance: 0.5,
            created_at: crate::now_ms(),
            expires_at: None,
            embedding: None,
            source_event: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryQuery {
    pub session_id: Option<SessionId>,
    pub text: Option<String>,
    pub tags: Vec<String>,
    pub kinds: Vec<MemoryKind>,
    pub limit: usize,
}

impl MemoryQuery {
    pub fn session(id: SessionId, limit: usize) -> Self {
        Self { session_id: Some(id), limit, ..Default::default() }
    }
}
