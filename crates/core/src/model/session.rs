use crate::ids::{ActorId, SessionId, WorkerId, WorkspaceId};
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
    /// The workspace this session belongs to.
    ///
    /// Optional only for records written before workspaces existed: a missing value reads as "no
    /// workspace", and access then falls back to the session's own owner and grants. Every session
    /// created since carries one, and that is the shape the permission model assumes.
    #[serde(default)]
    pub workspace_id: Option<WorkspaceId>,
    /// Provider this session prefers. None lets the router decide (its default and failover order).
    #[serde(default)]
    pub model_hint: Option<String>,
    /// How much thinking this session asks for. None means "whatever the provider defaults to".
    #[serde(default)]
    pub reasoning_effort: Option<crate::model::ReasoningEffort>,
    /// Who owns this conversation. Owners decide who else may take part; closing a session does not
    /// change this, because ownership is about the record and closing is about the actor.
    #[serde(default)]
    pub owner: Option<crate::model::PrincipalRef>,
    /// Roles handed out by the owner, one entry per person.
    #[serde(default)]
    pub grants: Vec<crate::model::SessionGrant>,
    /// What this session narrowed about capabilities. Default means "whatever the runtime allows".
    #[serde(default)]
    pub capabilities: crate::model::SessionCapabilities,
    /// People asking for access, and what was decided. Kept here so the answer and the question
    /// cannot drift apart.
    #[serde(default)]
    pub access_requests: Vec<crate::model::SessionAccessRequest>,
}

impl SessionRecord {
    pub fn new(user_id: impl Into<String>, title: impl Into<String>) -> Self {
        let now = crate::now_ms();
        let user_id = user_id.into();
        Self {
            id: SessionId::new(),
            // The owner defaults to the stated user: a record always has an owner, even one created
            // by a caller that did not name a principal.
            owner: Some(crate::model::PrincipalRef::new(user_id.clone(), None)),
            user_id,
            title: title.into(),
            state: SessionState::Creating,
            actor_id: ActorId::new(),
            worker_id: None,
            message_count: 0,
            created_at: now,
            updated_at: now,
            closed_at: None,
            metadata: BTreeMap::new(),
            workspace_id: None,
            model_hint: None,
            reasoning_effort: None,
            grants: Vec::new(),
            capabilities: crate::model::SessionCapabilities::default(),
            access_requests: Vec::new(),
        }
    }

    /// A session with no actor is still a session: closed ones are read, archived ones are restored.
    pub fn is_open(&self) -> bool {
        !self.state.is_terminal()
    }

    /// Who owns it, as a display string, for logs and listings.
    pub fn owner_label(&self) -> String {
        self.owner
            .as_ref()
            .map(|owner| owner.to_string())
            .unwrap_or_else(|| self.user_id.clone())
    }
}
