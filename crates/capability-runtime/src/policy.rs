//! Policy seam.
//!
//! The capability mesh never decides for itself whether a call is allowed: it asks the policy
//! gate. The default gate allows everything, which is only used in tests and in the desktop
//! demo; production wiring installs the control-plane policy engine.

use agentos_core::error::Result;
use agentos_core::model::{CapabilityDescriptor, CapabilityPermission};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct PolicyRequest {
    /// Capability the caller wants to run.
    pub capability: CapabilityDescriptor,
    pub session_id: agentos_core::SessionId,
    pub actor_id: Option<agentos_core::ActorId>,
    /// Input size is a useful abuse signal.
    pub input_bytes: usize,
    pub workspace_root: PathBuf,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PolicyDecision {
    pub allowed: bool,
    pub reason: String,
    /// Permissions actually granted, possibly narrower than the capability asks for.
    pub granted: CapabilityPermission,
    pub requires_approval: bool,
}

impl PolicyDecision {
    pub fn allow(granted: CapabilityPermission) -> Self {
        Self { allowed: true, reason: "allowed".into(), granted, requires_approval: false }
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            allowed: false,
            reason: reason.into(),
            granted: CapabilityPermission::default(),
            requires_approval: false,
        }
    }
}

/// Implemented by the control plane.
pub trait CapabilityPolicy: Send + Sync + 'static {
    fn evaluate(&self, request: &PolicyRequest) -> Result<PolicyDecision>;
}

/// Used by unit tests and by the offline demo mode.
pub struct AllowAllPolicy;

impl CapabilityPolicy for AllowAllPolicy {
    fn evaluate(&self, request: &PolicyRequest) -> Result<PolicyDecision> {
        Ok(PolicyDecision::allow(request.capability.permission.clone()))
    }
}

/// Where the mesh learns what a session narrowed about capabilities.
///
/// Async because the honest answer comes from the session record, which lives in a store. A cache
/// here would be a second copy of an authorization decision, and a stale copy of one fails in the
/// widening direction - the only direction that matters.
#[async_trait::async_trait]
pub trait SessionCapabilitySource: Send + Sync + 'static {
    /// None means the scope narrows nothing, or is not known here.
    ///
    /// `workspace` is where narrowing lives now: sharing a working unit should share its abilities.
    /// A session with no workspace (a record written before workspaces existed) still answers from
    /// its own narrowing, so an upgraded node keeps honouring what it already knew.
    async fn for_scope(
        &self,
        session: &agentos_core::SessionId,
        workspace: Option<&agentos_core::WorkspaceId>,
    ) -> Option<agentos_core::model::SessionCapabilities>;
}

/// A source that never narrows anything: what a mesh wired without a store gets. Not a hole - the
/// global policy still applies to every call.
pub struct NoSessionNarrowing;

#[async_trait::async_trait]
impl SessionCapabilitySource for NoSessionNarrowing {
    async fn for_scope(
        &self,
        _session: &agentos_core::SessionId,
        _workspace: Option<&agentos_core::WorkspaceId>,
    ) -> Option<agentos_core::model::SessionCapabilities> {
        None
    }
}

/// Fold a session's narrowing into the decision the runtime already made.
///
/// Written as a function so it can be read - and tested - on its own. This is the place where a
/// mistake becomes either a bypass or a session that can do nothing at all, and the two failure
/// modes deserve to be visible in one short function rather than implied by control flow.
pub fn apply_session_narrowing(
    decision: PolicyDecision,
    narrowing: &agentos_core::model::SessionCapabilities,
    name: &str,
) -> PolicyDecision {
    // Refused outright by the node: nothing a session says can change that. This early return is
    // the whole "narrow, never widen" rule, in one line.
    if !decision.allowed && !decision.requires_approval {
        return decision;
    }
    if let Err(reason) = narrowing.permits(name) {
        return PolicyDecision::deny(reason);
    }
    // A session can also ask for a human, on a call the node would have allowed by itself.
    if agentos_core::model::session_capability_matches(&narrowing.approval_required, name) {
        let mut parked = PolicyDecision::deny(format!(
            "{name} needs approval in this session: its owner asked for one"
        ));
        parked.requires_approval = true;
        return parked;
    }
    decision
}

#[cfg(test)]
mod session_narrowing_tests {
    use super::*;
    use agentos_core::model::{CapabilityPermission, SessionCapabilities};

    fn allowed() -> PolicyDecision {
        PolicyDecision::allow(CapabilityPermission::pure())
    }

    fn narrowing(allow: Option<&[&str]>, deny: &[&str], approval: &[&str]) -> SessionCapabilities {
        SessionCapabilities {
            allow: allow.map(|list| list.iter().map(|s| s.to_string()).collect()),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            approval_required: approval.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn a_session_cannot_permit_what_the_node_denied() {
        // The node said no. A session allow list that names the capability must not rescue it.
        let denied = PolicyDecision::deny("filesystem-write writes to the workspace");
        let permissive = narrowing(Some(&["filesystem-write"]), &[], &[]);
        let result = apply_session_narrowing(denied, &permissive, "filesystem-write");
        assert!(!result.allowed);
        assert!(result.reason.contains("writes to the workspace"), "{}", result.reason);
    }

    #[test]
    fn an_allow_list_is_a_whitelist() {
        let only = narrowing(Some(&["calculator", "clock"]), &[], &[]);
        assert!(apply_session_narrowing(allowed(), &only, "calculator").allowed);
        let refused = apply_session_narrowing(allowed(), &only, "filesystem-read");
        assert!(!refused.allowed);
        assert!(refused.reason.contains("not granted to this session"), "{}", refused.reason);
    }

    #[test]
    fn a_deny_list_beats_the_nodes_yes() {
        let none = narrowing(None, &["filesystem-*"], &[]);
        let refused = apply_session_narrowing(allowed(), &none, "filesystem-read");
        assert!(!refused.allowed, "a wildcard deny covers the family");
        assert!(apply_session_narrowing(allowed(), &none, "calculator").allowed);
    }

    #[test]
    fn a_session_can_ask_for_a_human_on_a_call_the_node_allowed() {
        let careful = narrowing(None, &[], &["filesystem-write"]);
        let parked = apply_session_narrowing(allowed(), &careful, "filesystem-write");
        assert!(!parked.allowed);
        assert!(parked.requires_approval, "the flag is what makes it a request, not a refusal");
        // Untouched capabilities are untouched.
        assert!(apply_session_narrowing(allowed(), &careful, "calculator").allowed);
    }
}
