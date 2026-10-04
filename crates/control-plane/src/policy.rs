//! Policy engine: the single place that answers "is this allowed?".

use agentos_core::config::PolicyConfig;
use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::CapabilityDescriptor;
use agentos_core::telemetry::{metric_names, metrics};
use agentos_capability_runtime::policy::{CapabilityPolicy, PolicyDecision, PolicyRequest};
use std::path::{Path, PathBuf};

pub struct PolicyEngine {
    cfg: PolicyConfig,
}

/// Does a policy list entry cover this capability name?
///
/// An entry is either exact, or a prefix ending in "*" - which exists because a single MCP server
/// can publish thirty tools, and listing them by hand is how allow lists rot. The wildcard is only
/// honoured at the end: "a*b" would invite the reader to guess, and guessing is what this gate is
/// supposed to prevent.
pub fn entry_matches(entry: &str, name: &str) -> bool {
    match entry.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => entry == name,
    }
}

impl PolicyEngine {
    pub fn new(cfg: PolicyConfig) -> Self {
        Self { cfg }
    }

    pub fn config(&self) -> &PolicyConfig {
        &self.cfg
    }

    pub fn workspace_root(&self) -> &Path {
        &self.cfg.workspace_root
    }

    pub fn max_steps(&self) -> u32 {
        self.cfg.max_steps_per_run
    }

    /// Capability authorization. Default deny for anything that mutates the host.
    pub fn authorize_capability(&self, descriptor: &CapabilityDescriptor) -> Result<PolicyDecision> {
        let name = descriptor.name.as_str();

        if self.cfg.denied_capabilities.iter().any(|d| entry_matches(d, name)) {
            return Ok(PolicyDecision::deny(format!("{name} is on the deny list")));
        }

        let explicitly_allowed = self.cfg.allowed_capabilities.iter().any(|a| entry_matches(a, name));
        if !self.cfg.allowed_capabilities.is_empty() && !explicitly_allowed {
            return Ok(PolicyDecision::deny(format!(
                "{name} is not on the allow list for this node"
            )));
        }

        if descriptor.permission.process_exec && !explicitly_allowed {
            return Ok(PolicyDecision::deny("process execution requires an explicit allow entry"));
        }

        if descriptor.permission.fs_write && !explicitly_allowed {
            return Ok(PolicyDecision::deny(format!(
                "{name} writes to the workspace and needs an explicit allow entry"
            )));
        }

        if descriptor.permission.network && !self.cfg.allow_network_capabilities && !explicitly_allowed {
            return Ok(PolicyDecision::deny(format!(
                "{name} needs network access, which is disabled on this node"
            )));
        }

        if self.cfg.approval_required.iter().any(|a| a == name) {
            let mut decision = PolicyDecision::deny(format!("{name} requires explicit approval"));
            decision.requires_approval = true;
            return Ok(decision);
        }

        Ok(PolicyDecision::allow(descriptor.permission.clone()))
    }

    /// Budget check for the agent loop.
    pub fn authorize_step(&self, steps_used: u32) -> Result<()> {
        if steps_used >= self.cfg.max_steps_per_run {
            return Err(RuntimeError::policy_denied(format!(
                "step budget exhausted ({} steps)",
                self.cfg.max_steps_per_run
            )));
        }
        Ok(())
    }

    /// Cheap pre-flight check on a requested path, before any capability is dispatched.
    pub fn looks_like_traversal(&self, requested: &str) -> bool {
        let p = PathBuf::from(requested);
        p.is_absolute() || p.components().any(|c| matches!(c, std::path::Component::ParentDir))
    }

    pub fn denied(&self, reason: impl Into<String>) {
        metrics().inc(metric_names::POLICY_DENIED, 1);
        tracing::warn!(reason = %reason.into(), "policy denied an operation");
    }
}

impl CapabilityPolicy for PolicyEngine {
    fn evaluate(&self, request: &PolicyRequest) -> Result<PolicyDecision> {
        let decision = self.authorize_capability(&request.capability)?;
        if !decision.allowed {
            self.denied(format!(
                "capability {} for session {}: {}",
                request.capability.name, request.session_id, decision.reason
            ));
        }
        Ok(decision)
    }
}

#[cfg(test)]
mod wildcard_tests {
    use super::*;

    #[test]
    fn an_exact_entry_matches_only_itself() {
        assert!(entry_matches("filesystem-read", "filesystem-read"));
        assert!(!entry_matches("filesystem-read", "filesystem-read-all"));
        assert!(!entry_matches("filesystem-read", "filesystem"));
    }

    #[test]
    fn a_trailing_star_covers_a_family() {
        // The reason this exists: one MCP server, many tools.
        assert!(entry_matches("mcp.echo.*", "mcp.echo.echo"));
        assert!(entry_matches("mcp.echo.*", "mcp.echo.anything-at-all"));
        assert!(!entry_matches("mcp.echo.*", "mcp.other.echo"));
        assert!(entry_matches("mcp.*", "mcp.echo.echo"));
        assert!(entry_matches("*", "anything"));
    }

    #[test]
    fn a_star_elsewhere_is_not_a_wildcard() {
        // "a*b" is treated as a literal name: a matcher that guesses is worse than one that misses.
        assert!(!entry_matches("mcp.*.echo", "mcp.echo.echo"));
        assert!(entry_matches("mcp.*.echo", "mcp.*.echo"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::model::{CapabilityKind, CapabilityPermission, CapabilityProvider};
    use agentos_core::{CapabilityId, SessionId};

    fn descriptor(name: &str, permission: CapabilityPermission) -> CapabilityDescriptor {
        CapabilityDescriptor {
            id: CapabilityId::new(),
            name: name.into(),
            version: "1.0.0".into(),
            description: String::new(),
            kind: CapabilityKind::Builtin,
            tags: vec![],
            input_schema: serde_json::json!({}),
            output_schema: serde_json::json!({}),
            permission,
            provider: CapabilityProvider::Local,
            timeout_ms: 1000,
            idempotent: true,
            health: agentos_core::state::CapabilityHealth::Healthy,
            load: None,
        }
    }

    fn engine(allowed: Vec<&str>, denied: Vec<&str>) -> PolicyEngine {
        let mut cfg = agentos_core::config::RuntimeConfig::default().policy;
        cfg.allowed_capabilities = allowed.into_iter().map(|s| s.to_string()).collect();
        cfg.denied_capabilities = denied.into_iter().map(|s| s.to_string()).collect();
        PolicyEngine::new(cfg)
    }

    #[test]
    fn pure_capabilities_are_allowed_by_default() {
        let e = engine(vec![], vec![]);
        assert!(e.authorize_capability(&descriptor("echo", CapabilityPermission::pure())).unwrap().allowed);
    }

    #[test]
    fn deny_list_wins() {
        let e = engine(vec![], vec!["echo"]);
        let d = e.authorize_capability(&descriptor("echo", CapabilityPermission::pure())).unwrap();
        assert!(!d.allowed);
        assert!(d.reason.contains("deny list"));
    }

    #[test]
    fn allow_list_is_restrictive_once_set() {
        let e = engine(vec!["echo"], vec![]);
        assert!(e.authorize_capability(&descriptor("echo", CapabilityPermission::pure())).unwrap().allowed);
        assert!(!e.authorize_capability(&descriptor("other", CapabilityPermission::pure())).unwrap().allowed);
    }

    #[test]
    fn writes_are_denied_unless_explicitly_allowed() {
        let e = engine(vec![], vec![]);
        let d = e
            .authorize_capability(&descriptor("filesystem-write", CapabilityPermission::read_only_fs().with_fs_write()))
            .unwrap();
        assert!(!d.allowed);

        let e2 = engine(vec!["filesystem-write"], vec![]);
        let d2 = e2
            .authorize_capability(&descriptor("filesystem-write", CapabilityPermission::read_only_fs().with_fs_write()))
            .unwrap();
        assert!(d2.allowed);
    }

    #[test]
    fn network_capabilities_follow_the_node_switch() {
        let e = engine(vec![], vec![]);
        assert!(!e
            .authorize_capability(&descriptor("http-fetch", CapabilityPermission::pure().with_network()))
            .unwrap()
            .allowed);
    }

    #[test]
    fn step_budget_is_enforced() {
        let mut cfg = agentos_core::config::RuntimeConfig::default().policy;
        cfg.max_steps_per_run = 3;
        let e = PolicyEngine::new(cfg);
        assert!(e.authorize_step(0).is_ok());
        assert!(e.authorize_step(2).is_ok());
        assert!(e.authorize_step(3).is_err());
    }

    #[test]
    fn traversal_detection_is_cheap_and_conservative() {
        let e = engine(vec![], vec![]);
        assert!(e.looks_like_traversal("../secrets"));
        assert!(e.looks_like_traversal("C:\\Windows"));
        assert!(!e.looks_like_traversal("notes/a.txt"));
    }

    #[test]
    fn trait_impl_reports_the_decision() {
        let e = engine(vec![], vec![]);
        let req = PolicyRequest {
            capability: descriptor("echo", CapabilityPermission::pure()),
            session_id: SessionId::new(),
            actor_id: None,
            input_bytes: 10,
            workspace_root: PathBuf::from("."),
        };
        assert!(e.evaluate(&req).unwrap().allowed);
    }
}
