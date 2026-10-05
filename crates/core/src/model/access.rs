//! Who may do what with a session.
//!
//! A conversation is not a free-for-all. Three things decide access, in this order:
//!
//!  1. a configured **principal** - the identity behind a request. Without one, `user_id` is just a
//!     string a client typed, and any permission model built on it is decoration.
//!  2. **ownership**, a durable attribute of the record, set at creation and transferable only by its
//!     owner.
//!  3. **grants**, roles handed out per session by the owner.
//!
//! The rule that keeps this from rotting: a session grant can only ever *narrow* what the global
//! policy allows. Nothing here widens a permission - a grant is a filter, never a bypass.

use crate::ids::SessionId;
use crate::model::session::SessionRecord;
use crate::time::Timestamp;
use serde::{Deserialize, Serialize};

/// Who a request is, as far as the runtime can prove.
///
/// `node_id` is what makes this work across machines: the same person on two nodes is two
/// principals, and a grant can name either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    pub user_id: String,
    #[serde(default)]
    pub node_id: Option<String>,
    /// Global roles from configuration: "admin" acts on every session, everything else acts only
    /// where it owns or was granted.
    #[serde(default)]
    pub roles: Vec<String>,
}

impl Principal {
    pub fn new(user_id: impl Into<String>, node_id: Option<String>, roles: Vec<String>) -> Self {
        Self { user_id: user_id.into(), node_id, roles }
    }

    /// The implicit principal of a runtime with no principal table: whoever holds the token is the
    /// operator. This is what keeps every existing deployment working unchanged.
    pub fn operator() -> Self {
        Self { user_id: "operator".into(), node_id: None, roles: vec!["admin".into()] }
    }

    pub fn is_admin(&self) -> bool {
        self.roles.iter().any(|role| role.eq_ignore_ascii_case("admin"))
    }

    /// The stable spelling used in records and grants: `user@node`, or just `user`.
    pub fn as_ref(&self) -> PrincipalRef {
        PrincipalRef { user_id: self.user_id.clone(), node_id: self.node_id.clone() }
    }
}

/// A principal as it is written into a record. Deliberately not the same type as `Principal`: a
/// record keeps who did something, never what they were allowed to do at the time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrincipalRef {
    pub user_id: String,
    #[serde(default)]
    pub node_id: Option<String>,
}

impl PrincipalRef {
    pub fn new(user_id: impl Into<String>, node_id: Option<String>) -> Self {
        Self { user_id: user_id.into(), node_id }
    }

    /// Does this reference mean the same principal as that one?
    ///
    /// A reference without a node matches that user on any node, and the same the other way round:
    /// a grant written before nodes were named must not stop matching when they are.
    pub fn matches(&self, other: &PrincipalRef) -> bool {
        if self.user_id != other.user_id {
            return false;
        }
        match (&self.node_id, &other.node_id) {
            (Some(left), Some(right)) => left == right,
            _ => true,
        }
    }
}

impl std::fmt::Display for PrincipalRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.node_id {
            Some(node) => write!(f, "{}@{node}", self.user_id),
            None => f.write_str(&self.user_id),
        }
    }
}

/// What someone may do inside one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRole {
    /// Everything, including granting and deleting.
    Owner,
    /// Runs the conversation and its lifecycle, but cannot hand out access or destroy it.
    Editor,
    /// Takes part: sends goals and reads. Cannot open, close or archive.
    Participant,
    /// Reads and downloads. Cannot speak.
    Viewer,
}

impl SessionRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Editor => "editor",
            Self::Participant => "participant",
            Self::Viewer => "viewer",
        }
    }

    /// Parse the wire spelling. Returns None for anything unknown, so a typo cannot silently become
    /// the most powerful role.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "owner" => Some(Self::Owner),
            "editor" => Some(Self::Editor),
            "participant" | "member" => Some(Self::Participant),
            "viewer" | "read" | "readonly" => Some(Self::Viewer),
            _ => None,
        }
    }
}

impl std::fmt::Display for SessionRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One action a session can be protected on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAction {
    Read,
    Chat,
    Open,
    Close,
    Archive,
    Download,
    Delete,
    Grant,
}

impl SessionAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Chat => "chat",
            Self::Open => "open",
            Self::Close => "close",
            Self::Archive => "archive",
            Self::Download => "download",
            Self::Delete => "delete",
            Self::Grant => "grant",
        }
    }
}

/// The whole permission table, in one place. A role is a set of actions; there is no other source.
pub fn role_allows(role: SessionRole, action: SessionAction) -> bool {
    use SessionAction::*;
    match role {
        SessionRole::Owner => true,
        SessionRole::Editor => matches!(action, Read | Chat | Open | Close | Archive | Download),
        SessionRole::Participant => matches!(action, Read | Chat | Download),
        SessionRole::Viewer => matches!(action, Read | Download),
    }
}

/// A role handed out for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGrant {
    pub user_id: String,
    #[serde(default)]
    pub node_id: Option<String>,
    pub role: SessionRole,
    /// Who handed it out. Kept so a surprising grant has an author.
    #[serde(default)]
    pub granted_by: Option<String>,
    #[serde(default)]
    pub granted_at: Option<Timestamp>,
}

impl SessionGrant {
    pub fn new(user_id: impl Into<String>, node_id: Option<String>, role: SessionRole) -> Self {
        Self { user_id: user_id.into(), node_id, role, granted_by: None, granted_at: None }
    }

    pub fn as_ref(&self) -> PrincipalRef {
        PrincipalRef { user_id: self.user_id.clone(), node_id: self.node_id.clone() }
    }
}

/// Why an action was refused, in words a person can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    pub action: SessionAction,
    pub session_id: SessionId,
    pub reason: String,
}

impl Denial {
    pub fn message(&self) -> String {
        format!(
            "{} on session {} was refused: {}",
            self.action.as_str(),
            self.session_id.as_str(),
            self.reason
        )
    }
}

/// What a session's owner narrowed about capabilities.
///
/// The rule this type exists to enforce: a session can only ever **narrow** what the runtime
/// allows. `allow: Some(list)` means "only these", never "also these" - the global policy stays
/// the ceiling, and nothing here can lift it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SessionCapabilities {
    /// None means "whatever the runtime allows". Some(list) is a whitelist for this session only.
    pub allow: Option<Vec<String>>,
    /// Never these, whatever the runtime says.
    pub deny: Vec<String>,
    /// These need an operator decision in this session, even where the runtime would allow them.
    pub approval_required: Vec<String>,
}

impl Default for SessionCapabilities {
    fn default() -> Self {
        Self { allow: None, deny: Vec::new(), approval_required: Vec::new() }
    }
}

impl SessionCapabilities {
    /// Is this the do-nothing policy? Lets a caller skip a store read on the hot path.
    pub fn is_unrestricted(&self) -> bool {
        self.allow.is_none() && self.deny.is_empty() && self.approval_required.is_empty()
    }

    /// Does this session's own policy allow the name? It says nothing about the global ceiling:
    /// that is the policy engine's business, and keeping the two apart is what stops a session
    /// grant from becoming a bypass.
    pub fn permits(&self, name: &str) -> std::result::Result<(), String> {
        if session_capability_matches(&self.deny, name) {
            return Err(format!("{name} is denied for this session by its owner"));
        }
        if let Some(allow) = &self.allow {
            if !session_capability_matches(allow, name) {
                return Err(format!("{name} is not granted to this session"));
            }
        }
        Ok(())
    }
}

/// Does a session-level list entry cover this capability name?
///
/// The same rule as the runtime's own lists, and deliberately the same shape: an entry is exact or
/// a trailing `*` prefix. Two spellings of one idea are one refactor away from disagreeing.
pub fn session_capability_matches(entries: &[String], name: &str) -> bool {
    entries.iter().any(|entry| match entry.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => entry == name,
    })
}

/// What somebody asked for, and what happened to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessRequestState {
    Pending,
    Approved,
    Rejected,
}

impl AccessRequestState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }
}

/// A request for access to somebody else's conversation.
///
/// Kept on the session record rather than in a global queue: what is being decided is a
/// relationship between one person and one conversation, and a record that holds both cannot
/// drift out of step with itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionAccessRequest {
    pub id: String,
    /// Who is asking.
    pub principal: PrincipalRef,
    /// What they asked to be.
    pub role: SessionRole,
    #[serde(default)]
    pub note: Option<String>,
    pub created_at: Timestamp,
    pub state: AccessRequestState,
    /// Who decided, and when. Absent while it is pending.
    #[serde(default)]
    pub decided_by: Option<String>,
    #[serde(default)]
    pub decided_at: Option<Timestamp>,
    /// What they were actually given: an owner may hand out something other than what was asked.
    #[serde(default)]
    pub granted_role: Option<SessionRole>,
}
/// The role a principal holds on a session, if any.
pub fn role_of(record: &SessionRecord, principal: &Principal) -> Option<SessionRole> {
    let me = principal.as_ref();
    if let Some(owner) = &record.owner {
        if owner.matches(&me) {
            return Some(SessionRole::Owner);
        }
    }
    // The best grant wins: two grants for the same person is a configuration mistake, and the
    // generous reading of it is the one that matches what the owner last intended.
    record
        .grants
        .iter()
        .filter(|grant| grant.as_ref().matches(&me))
        .map(|grant| grant.role)
        .min()
}

/// The role a principal holds on a session, read through its workspace.
///
/// This is the rule the workspace model is built on: access belongs to the workspace, not to the
/// conversation. Someone with a role in a workspace holds that role in every session of it - which
/// is what makes "share this working unit" one decision rather than one per conversation - and a
/// session's own grants are no longer consulted once it has a workspace, because a per-session role
/// that could outlive the workspace's decision is exactly the drift workspaces exist to remove.
///
/// A record with no workspace (written before workspaces existed) keeps the old reading, so an
/// upgraded node still answers correctly about conversations it already had.
pub fn effective_role(
    record: &SessionRecord,
    workspace: Option<&crate::model::WorkspaceRecord>,
    principal: &Principal,
) -> Option<SessionRole> {
    match workspace {
        Some(workspace) => crate::model::workspace_role(workspace, principal),
        None => role_of(record, principal),
    }
}

/// May this principal do this to this session?
///
/// Admins pass everywhere - they are configured by the operator, they are the escape hatch - and
/// everyone else is decided by the role they hold. Kept as the no-workspace reading; the gateway and
/// the session manager use `decide_in` so that a session's workspace is taken into account.
pub fn decide(
    record: &SessionRecord,
    principal: &Principal,
    action: SessionAction,
) -> std::result::Result<(), Denial> {
    decide_in(record, None, principal, action)
}

/// The same decision, told which workspace the session belongs to.
pub fn decide_in(
    record: &SessionRecord,
    workspace: Option<&crate::model::WorkspaceRecord>,
    principal: &Principal,
    action: SessionAction,
) -> std::result::Result<(), Denial> {
    if principal.is_admin() {
        return Ok(());
    }
    let denial = |reason: String| Denial {
        action,
        session_id: record.id.clone(),
        reason,
    };
    match effective_role(record, workspace, principal) {
        Some(role) if role_allows(role, action) => Ok(()),
        Some(role) => Err(denial(format!(
            "you are {} on this session, and {} is not part of that",
            role.as_str(),
            action.as_str()
        ))),
        None => Err(denial(match workspace {
            // Naming the workspace, and not just the session, is what tells a stranger where to ask.
            Some(workspace) => format!(
                "you have no role on this session; it belongs to workspace {} ({}) - ask its owner for access",
                workspace.name, workspace.owner
            ),
            None => match &record.owner {
                Some(owner) => format!(
                    "you have no role on this session; it belongs to {owner} - ask them for access"
                ),
                None => "you have no role on this session".to_string(),
            },
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SessionState;

    fn record(owner: &str) -> SessionRecord {
        let mut record = SessionRecord::new(owner, "a conversation");
        record.state = SessionState::Active;
        record
    }

    fn principal(user: &str, roles: &[&str]) -> Principal {
        Principal::new(user, Some("node-a".into()), roles.iter().map(|r| r.to_string()).collect())
    }

    #[test]
    fn the_owner_may_do_anything_and_a_stranger_nothing() {
        let record = record("alice");
        let alice = principal("alice", &[]);
        for action in [SessionAction::Read, SessionAction::Chat, SessionAction::Delete, SessionAction::Grant] {
            assert!(decide(&record, &alice, action).is_ok(), "{action:?}");
        }
        let mallory = principal("mallory", &[]);
        let denial = decide(&record, &mallory, SessionAction::Read).unwrap_err();
        // The reason names the owner: "ask them for access" needs to say who them is.
        assert!(denial.reason.contains("alice"), "{}", denial.reason);
    }

    #[test]
    fn an_admin_stands_outside_the_table() {
        let record = record("alice");
        let root = principal("root", &["admin"]);
        assert!(decide(&record, &root, SessionAction::Delete).is_ok());
    }

    #[test]
    fn a_role_grants_exactly_its_own_actions() {
        let mut record = record("alice");
        record.grants.push(SessionGrant::new("bob", None, SessionRole::Participant));
        let bob = principal("bob", &[]);
        assert!(decide(&record, &bob, SessionAction::Chat).is_ok());
        assert!(decide(&record, &bob, SessionAction::Read).is_ok());
        for action in [SessionAction::Close, SessionAction::Open, SessionAction::Delete, SessionAction::Grant] {
            let denial = decide(&record, &bob, action).unwrap_err();
            assert!(denial.reason.contains("participant"), "{}", denial.reason);
        }
        record.grants.clear();
        record.grants.push(SessionGrant::new("bob", None, SessionRole::Editor));
        assert!(decide(&record, &bob, SessionAction::Close).is_ok());
        assert!(decide(&record, &bob, SessionAction::Delete).is_err(), "an editor is not an owner");
    }

    #[test]
    fn the_best_grant_wins_when_someone_holds_two() {
        let mut record = record("alice");
        record.grants.push(SessionGrant::new("bob", None, SessionRole::Viewer));
        record.grants.push(SessionGrant::new("bob", None, SessionRole::Participant));
        let bob = principal("bob", &[]);
        assert_eq!(role_of(&record, &bob), Some(SessionRole::Participant));
    }

    #[test]
    fn a_grant_without_a_node_matches_that_user_anywhere() {
        let mut record = record("alice");
        record.grants.push(SessionGrant::new("bob", None, SessionRole::Participant));
        let elsewhere = Principal::new("bob", Some("node-z".into()), vec![]);
        assert!(decide(&record, &elsewhere, SessionAction::Chat).is_ok());
        // Naming a node makes it specific: bob on node-z is not bob on node-a.
        let mut pinned = record.clone();
        pinned.grants.clear();
        pinned.grants.push(SessionGrant::new("bob", Some("node-a".into()), SessionRole::Participant));
        assert!(decide(&pinned, &elsewhere, SessionAction::Chat).is_err());
    }

    #[test]
    fn the_permission_table_is_the_one_that_is_documented() {
        for (role, allowed) in [
            (SessionRole::Owner, vec!["read", "chat", "open", "close", "archive", "download", "delete", "grant"]),
            (SessionRole::Editor, vec!["read", "chat", "open", "close", "archive", "download"]),
            (SessionRole::Participant, vec!["read", "chat", "download"]),
            (SessionRole::Viewer, vec!["read", "download"]),
        ] {
            for action in [
                SessionAction::Read,
                SessionAction::Chat,
                SessionAction::Open,
                SessionAction::Close,
                SessionAction::Archive,
                SessionAction::Download,
                SessionAction::Delete,
                SessionAction::Grant,
            ] {
                let expected = allowed.contains(&action.as_str());
                assert_eq!(role_allows(role, action), expected, "{} x {}", role.as_str(), action.as_str());
            }
        }
    }
}
