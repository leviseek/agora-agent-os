use crate::ids::{ActorId, SessionId, WorkerId};
use crate::state::SessionState;
use crate::time::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Durable view of a user session. The live conversation state lives inside the Session Actor;
/// this record is what the directory, the gateway and the UI read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: SessionId,
    pub user_id: String,
    pub title: String,
    pub state: SessionState,
    pub actor_id: ActorId,
    pub worker_id: Option<WorkerId>,
    pub message_count: u64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub closed_at: Option<Timestamp>,
    pub metadata: BTreeMap<String, String>,
}

impl SessionRecord {
    pub fn new(user_id: impl Into<String>, title: impl Into<String>) -> Self {
        let now = crate::now_ms();
        Self {
            id: SessionId::new(),
            user_id: user_id.into(),
            title: title.into(),
            state: SessionState::Creating,
            actor_id: ActorId::new(),
            worker_id: None,
            message_count: 0,
            created_at: now,
            updated_at: now,
            closed_at: None,
            metadata: BTreeMap::new(),
        }
    }

    pub fn is_open(&self) -> bool {
        !self.state.is_terminal()
    }
}
