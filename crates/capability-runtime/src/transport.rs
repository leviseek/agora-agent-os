//! Capability transport: how a remote capability gets called.
//!
//! The network crate implements this over gRPC. Keeping it as a trait means the mesh never
//! contains transport code and remote calls are testable with an in-process fake.

use agentos_core::error::Result;
use async_trait::async_trait;
use std::sync::Arc;

#[async_trait]
pub trait CapabilityTransport: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    /// Unary call. Implementations must honour the timeout themselves so they can produce a
    /// transport-specific error before the mesh deadline fires.
    async fn call(
        &self,
        endpoint: &str,
        capability: &str,
        version: &str,
        input: serde_json::Value,
        timeout_ms: u64,
    ) -> Result<serde_json::Value>;
}

/// In-process transport used by tests and by single-node deployments.
pub struct LocalTransport {
    handlers: parking_lot::RwLock<std::collections::HashMap<String, Handler>>,
}

type Handler = Arc<dyn Fn(serde_json::Value) -> Result<serde_json::Value> + Send + Sync>;

impl LocalTransport {
    pub fn new() -> Self {
        Self { handlers: parking_lot::RwLock::new(std::collections::HashMap::new()) }
    }

    pub fn register(&self, name: &str, f: impl Fn(serde_json::Value) -> Result<serde_json::Value> + Send + Sync + 'static) {
        self.handlers.write().insert(name.to_string(), Arc::new(f));
    }
}

impl Default for LocalTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CapabilityTransport for LocalTransport {
    fn name(&self) -> &'static str {
        "local"
    }

    async fn call(
        &self,
        _endpoint: &str,
        capability: &str,
        _version: &str,
        input: serde_json::Value,
        _timeout_ms: u64,
    ) -> Result<serde_json::Value> {
        let handler = self.handlers.read().get(capability).cloned();
        match handler {
            Some(f) => f(input),
            None => Err(agentos_core::RuntimeError::unavailable(format!(
                "no local transport handler for capability {capability}"
            ))),
        }
    }
}
