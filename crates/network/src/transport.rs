//! gRPC implementation of the capability transport.
//!
//! A RemoteCapability wraps this, so from the mesh point of view a capability on another node is
//! indistinguishable from a local one.

use crate::grpc::{GrpcCapabilityClient, RemoteInvocation};
use agentos_capability_runtime::transport::CapabilityTransport;
use agentos_core::error::{Result, RuntimeError};
use async_trait::async_trait;
use std::time::Duration;

pub struct GrpcCapabilityTransport {
    client: GrpcCapabilityClient,
    /// Retries on transport-level failures only; the mesh owns capability-level retries.
    connect_timeout: Duration,
}

impl GrpcCapabilityTransport {
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self> {
        Ok(Self { client: GrpcCapabilityClient::connect(endpoint).await?, connect_timeout: Duration::from_secs(3) })
    }

    pub fn endpoint(&self) -> &str {
        self.client.endpoint()
    }

    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }
}

#[async_trait]
impl CapabilityTransport for GrpcCapabilityTransport {
    fn name(&self) -> &'static str {
        "grpc"
    }

    async fn call(
        &self,
        endpoint: &str,
        capability: &str,
        version: &str,
        input: serde_json::Value,
        timeout_ms: u64,
    ) -> Result<serde_json::Value> {
        if endpoint != self.client.endpoint() {
            // Endpoint mismatch means the descriptor points somewhere this transport cannot reach.
            return Err(RuntimeError::network(format!(
                "transport is bound to {} but the descriptor points at {endpoint}",
                self.client.endpoint()
            ))
            .retryable(false));
        }
        let result = self
            .client
            .invoke(RemoteInvocation {
                capability: capability.to_string(),
                version: version.to_string(),
                input,
                session_id: None,
                actor_id: None,
                task_id: None,
                // The transport seam carries no caller context today (not even a session id), so a
                // remote call made through it lands in the node's legacy root. Set here from the
                // caller once the seam is widened; the wire already has the field.
                workspace_id: None,
                timeout_ms,
            })
            .await?;
        Ok(result.output)
    }
}
