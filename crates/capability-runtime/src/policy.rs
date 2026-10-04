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
