//! A workspace: the unit of ownership, sharing and filesystem isolation.
//!
//! Sessions belong to a workspace rather than directly to a person. That is what makes sharing a
//! whole working unit - every conversation, every file it can reach - one decision instead of one
//! per session, and it is what lets filesystem isolation follow ownership: each workspace has its
//! own directory beneath the node's workspace root, and a session's capabilities can only reach its
//! workspace's directory.
//!
//! Access is decided exactly as it used to be per session: an owner, and grants. The difference is
//! scope. A role held here is held in every session of the workspace, so a session resolves its
//! access through its workspace (see `effective_role`).

use crate::ids::WorkspaceId;
use crate::model::access::{Principal, PrincipalRef, SessionAccessRequest, SessionGrant, SessionRole};
use crate::time::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Durable view of a workspace: who owns it, who else may work in it, and what it is called.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub id: WorkspaceId,
    /// What a human reads. Editable, and never used as a path.
    pub name: String,
    /// Who created it, and who decides about it afterwards. Transferable only by its owner.
    pub owner: PrincipalRef,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    #[serde(default)]
    pub archived_at: Option<Timestamp>,
    /// Roles handed out for the whole workspace.
    ///
    /// The type is shared with sessions on purpose - a grant is a grant, and the record it lives on
    /// decides its scope. What changed is only where a grant is written.
    #[serde(default)]
    pub grants: Vec<SessionGrant>,
    /// People asking for access to this workspace, and what was decided. Kept here so the question
    /// and the answer cannot drift apart.
    #[serde(default)]
    pub access_requests: Vec<SessionAccessRequest>,
    /// Free-form annotations kept with the workspace (a project, a repository, a note).
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl WorkspaceRecord {
    pub fn new(owner: PrincipalRef, name: impl Into<String>) -> Self {
        let now = crate::now_ms();
        Self {
            id: WorkspaceId::new(),
            name: name.into(),
            owner,
            created_at: now,
            updated_at: now,
            archived_at: None,
            grants: Vec::new(),
            access_requests: Vec::new(),
            metadata: BTreeMap::new(),
        }
    }

    pub fn owner_label(&self) -> String {
        self.owner.to_string()
    }

    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }

    /// The leaf directory name for this workspace.
    ///
    /// Deliberately the id and not the display name. A name is editable, and a directory that moved
    /// when somebody renamed a workspace would break every path the model had ever been handed -
    /// and, worse, could silently point a session at a sibling's files.
    pub fn directory_name(&self) -> &str {
        self.id.as_str()
    }
}

/// The role a principal holds on a workspace, if any.
pub fn workspace_role(record: &WorkspaceRecord, principal: &Principal) -> Option<SessionRole> {
    let me = principal.as_ref();
    if record.owner.matches(&me) {
        return Some(SessionRole::Owner);
    }
    // The best grant wins, exactly as it does for a session: two grants for one person is a
    // configuration mistake, and the reading that matches what the owner last intended is the
    // generous one.
    record
        .grants
        .iter()
        .filter(|grant| grant.as_ref().matches(&me))
        .map(|grant| grant.role)
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(owner: &str) -> WorkspaceRecord {
        WorkspaceRecord::new(PrincipalRef::new(owner, Some("node-a".into())), "a workspace")
    }

    fn principal(user: &str, roles: &[&str]) -> Principal {
        Principal::new(user, Some("node-a".into()), roles.iter().map(|r| r.to_string()).collect())
    }

    #[test]
    fn the_owner_holds_every_role_and_a_stranger_none() {
        let workspace = record("alice");
        assert_eq!(workspace_role(&workspace, &principal("alice", &[])), Some(SessionRole::Owner));
        assert_eq!(workspace_role(&workspace, &principal("mallory", &[])), None);
    }

    #[test]
    fn the_best_of_several_grants_wins() {
        let mut workspace = record("alice");
        workspace.grants.push(SessionGrant::new("bob", None, SessionRole::Viewer));
        workspace.grants.push(SessionGrant::new("bob", None, SessionRole::Editor));
        assert_eq!(workspace_role(&workspace, &principal("bob", &[])), Some(SessionRole::Editor));
    }

    #[test]
    fn a_grant_that_names_a_node_only_matches_that_node() {
        let mut workspace = record("alice");
        workspace.grants.push(SessionGrant::new("bob", Some("node-a".into()), SessionRole::Participant));
        assert_eq!(workspace_role(&workspace, &principal("bob", &[])), Some(SessionRole::Participant));
        let elsewhere = Principal::new("bob", Some("node-z".into()), vec![]);
        assert_eq!(workspace_role(&workspace, &elsewhere), None);
    }

    #[test]
    fn the_directory_is_the_id_so_renaming_never_moves_files() {
        let mut workspace = record("alice");
        let before = workspace.directory_name().to_string();
        workspace.name = "renamed".into();
        assert_eq!(workspace.directory_name(), before);
        assert!(before.starts_with("ws_"));
    }
}
