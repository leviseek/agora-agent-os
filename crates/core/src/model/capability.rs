use crate::ids::{CapabilityId, WorkerId};
use crate::state::CapabilityHealth;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    /// Compiled into the runtime process.
    Builtin,
    /// Executed inside the Wasmtime sandbox.
    Wasm,
    /// Lives on another worker or an external service; reached over RPC.
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityProvider {
    Local,
    Worker(WorkerId),
    Endpoint(String),
}

/// What a capability is allowed to touch. Policy is evaluated against this before every call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityPermission {
    pub fs_read: bool,
    pub fs_write: bool,
    pub network: bool,
    pub process_exec: bool,
    pub secret_names: Vec<String>,
}

impl CapabilityPermission {
    pub fn read_only_fs() -> Self {
        Self { fs_read: true, ..Default::default() }
    }
    pub fn pure() -> Self {
        Self::default()
    }
    pub fn with_fs_write(mut self) -> Self {
        self.fs_write = true;
        self
    }
    pub fn with_network(mut self) -> Self {
        self.network = true;
        self
    }
    pub fn summary(&self) -> String {
        let mut flags = Vec::new();
        if self.fs_read { flags.push("fs_read"); }
        if self.fs_write { flags.push("fs_write"); }
        if self.network { flags.push("network"); }
        if self.process_exec { flags.push("process_exec"); }
        if self.secret_names.is_empty() { flags.join("+") } else { format!("{}+secrets", flags.join("+")) }
    }
}

/// A loose semantic-version request: "1", "1.2", "1.2.3" or "*".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionReq(pub String);

impl VersionReq {
    pub fn any() -> Self {
        Self("*".into())
    }
    pub fn matches(&self, version: &str) -> bool {
        if self.0 == "*" || self.0.is_empty() {
            return true;
        }
        version == self.0 || version.starts_with(&format!("{}.", self.0))
    }
}

impl Default for VersionReq {
    fn default() -> Self {
        Self::any()
    }
}

/// Everything the mesh needs to describe, route and audit a capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityDescriptor {
    pub id: CapabilityId,
    pub name: String,
    pub version: String,
    pub description: String,
    pub kind: CapabilityKind,
    pub tags: Vec<String>,
    /// JSON Schema for the invocation input.
    pub input_schema: serde_json::Value,
    /// JSON Schema for the output.
    pub output_schema: serde_json::Value,
    pub permission: CapabilityPermission,
    pub provider: CapabilityProvider,
    pub timeout_ms: u64,
    pub idempotent: bool,
    pub health: CapabilityHealth,
    /// Load signal used by the mesh to pick between replicas.
    pub load: Option<CapabilityLoad>,
}

impl CapabilityDescriptor {
    /// Stable routing key: capabilities are addressed by name and resolved by version.
    pub fn key(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CapabilityLoad {
    pub inflight: u32,
    pub total_calls: u64,
    pub failures: u64,
    pub avg_latency_ms: f64,
}
