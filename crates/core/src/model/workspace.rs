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
use crate::model::access::{role_allows, Principal, PrincipalRef, SessionAccessRequest, SessionAction, SessionCapabilities, SessionGrant, SessionRole};
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
    /// What this workspace narrowed about capabilities. Default means "whatever the runtime allows".
    ///
    /// Narrowing lives here rather than on the session because it is part of what a workspace *is*:
    /// sharing a working unit should share its abilities, and a per-session copy would be the same
    /// decision written N times, drifting one session at a time.
    #[serde(default)]
    pub capabilities: SessionCapabilities,
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
            capabilities: SessionCapabilities::default(),
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

/// Why an action on a workspace was refused, in words a person can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceDenial {
    pub action: SessionAction,
    pub workspace_id: WorkspaceId,
    pub reason: String,
}

impl WorkspaceDenial {
    pub fn message(&self) -> String {
        format!(
            "{} on workspace {} was refused: {}",
            self.action.as_str(),
            self.workspace_id.as_str(),
            self.reason
        )
    }
}

/// May this principal act on this workspace?
///
/// The same shape as a session decision, and the same role table: the roles are the same four, and
/// a workspace only adds the actions that belong to a whole working unit (creating a session in it,
/// administering it) - they are mapped onto the existing table rather than given a second one, so
/// the two can never disagree about what an editor may do.
pub fn decide_workspace(
    record: &WorkspaceRecord,
    principal: &Principal,
    action: SessionAction,
) -> std::result::Result<(), WorkspaceDenial> {
    if principal.is_admin() {
        return Ok(());
    }
    let denial = |reason: String| WorkspaceDenial {
        action,
        workspace_id: record.id.clone(),
        reason,
    };
    match workspace_role(record, principal) {
        Some(role) if role_allows(role, action) => Ok(()),
        Some(role) => Err(denial(format!(
            "you are {} on this workspace, and {} is not part of that",
            role.as_str(),
            action.as_str()
        ))),
        None => Err(denial(format!(
            "you have no role on this workspace; it belongs to {} - ask them for access",
            record.owner
        ))),
    }
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

    #[test]
    fn a_stranger_may_look_but_only_a_member_may_work() {
        let mut workspace = record("alice");
        workspace.grants.push(SessionGrant::new("bob", None, SessionRole::Viewer));
        let bob = principal("bob", &[]);
        // A viewer reads the workspace, but does not create conversations in it.
        assert!(decide_workspace(&workspace, &bob, SessionAction::Read).is_ok());
        let denied = decide_workspace(&workspace, &bob, SessionAction::Chat).unwrap_err();
        assert!(denied.reason.contains("viewer"), "{}", denied.reason);

        let carol = principal("carol", &[]);
        let stranger = decide_workspace(&workspace, &carol, SessionAction::Read).unwrap_err();
        assert!(stranger.reason.contains("alice"), "{}", stranger.reason);

        // Administering a workspace is the owner's: an editor works in it, they do not rename it or
        // hand out membership.
        workspace.grants.push(SessionGrant::new("dan", None, SessionRole::Editor));
        let dan = principal("dan", &[]);
        assert!(decide_workspace(&workspace, &dan, SessionAction::Chat).is_ok());
        assert!(decide_workspace(&workspace, &dan, SessionAction::Grant).is_err());
        assert!(decide_workspace(&workspace, &principal("alice", &[]), SessionAction::Grant).is_ok());
    }
}
