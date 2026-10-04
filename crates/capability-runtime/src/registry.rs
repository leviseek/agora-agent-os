//! The capability registry: discovery, versioning and load accounting.

use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{CapabilityDescriptor, CapabilityKind, CapabilityLoad, VersionReq};
use agentos_core::state::{CapabilityHealth, StateMachine};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

use crate::capability::Capability;

/// A capability plus its live statistics.
pub struct RegisteredCapability {
    pub capability: Arc<dyn Capability>,
    pub descriptor: CapabilityDescriptor,
    pub stats: RwLock<CapabilityLoad>,
}

impl std::fmt::Debug for RegisteredCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisteredCapability")
            .field("descriptor", &self.descriptor)
            .field("stats", &*self.stats.read())
            .finish()
    }
}

impl RegisteredCapability {
    pub fn key(&self) -> String {
        self.descriptor.key()
    }

    pub fn record(&self, duration_ms: u64, ok: bool) {
        let mut s = self.stats.write();
        s.total_calls += 1;
        if !ok {
            s.failures += 1;
        }
        let n = s.total_calls as f64;
        s.avg_latency_ms = (s.avg_latency_ms * (n - 1.0) + duration_ms as f64) / n;
    }

    pub fn enter(&self) {
        self.stats.write().inflight += 1;
    }

    pub fn leave(&self) {
        let mut s = self.stats.write();
        s.inflight = s.inflight.saturating_sub(1);
    }

    pub fn descriptor_with_load(&self) -> CapabilityDescriptor {
        let mut d = self.descriptor.clone();
        d.load = Some(self.stats.read().clone());
        d
    }
}

#[derive(Debug, Clone, Default)]
pub struct DiscoveryQuery {
    /// Substring match on the capability name; empty means all.
    pub name_contains: String,
    pub tags: Vec<String>,
    pub kinds: Vec<CapabilityKind>,
    /// Only return capabilities with no recent failures.
    pub healthy_only: bool,
}

#[derive(Default)]
pub struct CapabilityRegistry {
    entries: RwLock<HashMap<String, Arc<RegisteredCapability>>>,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) a capability. Re-registration is how a node re-offers a capability
    /// after a restart, so it is an upsert rather than a conflict.
    pub fn register(&self, capability: Arc<dyn Capability>) -> Result<CapabilityDescriptor> {
        let mut descriptor = capability.descriptor();
        if descriptor.name.trim().is_empty() {
            return Err(RuntimeError::invalid_input("capability name must not be empty"));
        }
        if descriptor.version.trim().is_empty() {
            return Err(RuntimeError::invalid_input(format!(
                "capability {} must declare a version",
                descriptor.name
            )));
        }
        if descriptor.health == CapabilityHealth::Unknown {
            descriptor.health = CapabilityHealth::Healthy;
        }
        let key = descriptor.key();
        let entry = Arc::new(RegisteredCapability {
            capability,
            descriptor: descriptor.clone(),
            stats: RwLock::new(CapabilityLoad::default()),
        });
        self.entries.write().insert(key, entry);
        Ok(descriptor)
    }

    pub fn unregister(&self, name: &str, version: &str) -> Result<bool> {
        Ok(self.entries.write().remove(&format!("{name}@{version}")).is_some())
    }

    /// Resolve a name plus a loose version requirement. When several versions match, the highest
    /// version string wins and unhealthy replicas are skipped in favour of healthy ones.
    pub fn lookup(&self, name: &str, req: &VersionReq) -> Result<Arc<RegisteredCapability>> {
        let entries = self.entries.read();
        let mut candidates: Vec<&Arc<RegisteredCapability>> = entries
            .values()
            .filter(|e| e.descriptor.name == name && req.matches(&e.descriptor.version))
            .collect();
        if candidates.is_empty() {
            return Err(RuntimeError::not_found(format!(
                "capability {name} (version {}) is not registered",
                req.0
            ))
            .with_detail("capability", name));
        }
        candidates.sort_by(|a, b| {
            let health = |h: CapabilityHealth| match h {
                CapabilityHealth::Healthy => 0,
                CapabilityHealth::Unknown => 1,
                CapabilityHealth::Degraded => 2,
                CapabilityHealth::Unavailable => 3,
            };
            health(a.descriptor.health)
                .cmp(&health(b.descriptor.health))
                .then_with(|| b.descriptor.version.cmp(&a.descriptor.version))
                .then_with(|| {
                    let la = a.stats.read().inflight;
                    let lb = b.stats.read().inflight;
                    la.cmp(&lb)
                })
        });
        Ok(candidates[0].clone())
    }

    pub fn describe(&self, name: &str, req: &VersionReq) -> Result<CapabilityDescriptor> {
        Ok(self.lookup(name, req)?.descriptor_with_load())
    }

    pub fn list(&self) -> Vec<CapabilityDescriptor> {
        let mut out: Vec<CapabilityDescriptor> = self
            .entries
            .read()
            .values()
            .map(|e| e.descriptor_with_load())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.version.cmp(&b.version)));
        out
    }

    pub fn discover(&self, query: &DiscoveryQuery) -> Vec<CapabilityDescriptor> {
        let mut out: Vec<CapabilityDescriptor> = self
            .entries
            .read()
            .values()
            .filter(|e| {
                let d = &e.descriptor;
                if !query.name_contains.is_empty()
                    && !d.name.to_ascii_lowercase().contains(&query.name_contains.to_ascii_lowercase())
                {
                    return false;
                }
                if !query.kinds.is_empty() && !query.kinds.contains(&d.kind) {
                    return false;
                }
                if !query.tags.is_empty() && !query.tags.iter().any(|t| d.tags.contains(t)) {
                    return false;
                }
                if query.healthy_only && d.health != CapabilityHealth::Healthy {
                    return false;
                }
                true
            })
            .map(|e| e.descriptor_with_load())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn set_health(&self, name: &str, version: &str, health: CapabilityHealth) -> Result<()> {
        let mut entries = self.entries.write();
        let entry = entries
            .get_mut(&format!("{name}@{version}"))
            .ok_or_else(|| RuntimeError::not_found(format!("capability {name}@{version}")))?;
        let applied = Arc::get_mut(entry)
            .ok_or_else(|| RuntimeError::internal("capability is shared and cannot be mutated"))?
            .descriptor
            .health
            .transition(health)
            .map_err(|e| RuntimeError::conflict(e.to_string()))?;
        if let Some(e) = Arc::get_mut(entry) {
            e.descriptor.health = applied;
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.entries.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::EchoCapability;

    fn registry() -> CapabilityRegistry {
        let r = CapabilityRegistry::new();
        r.register(Arc::new(EchoCapability::new())).unwrap();
        r
    }

    #[test]
    fn discovery_finds_registered_capabilities() {
        let r = registry();
        assert_eq!(r.list().len(), 1);
        let found = r.discover(&DiscoveryQuery { name_contains: "ech".into(), ..Default::default() });
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "echo");
    }

    #[test]
    fn unknown_capability_is_not_found() {
        let r = registry();
        let err = r.lookup("nope", &VersionReq::any()).unwrap_err();
        assert_eq!(err.kind, agentos_core::ErrorKind::NotFound);
    }

    #[test]
    fn version_requirements_are_honoured() {
        let r = registry();
        assert!(r.lookup("echo", &VersionReq("1".into())).is_ok());
        assert!(r.lookup("echo", &VersionReq("9".into())).is_err());
    }
}
