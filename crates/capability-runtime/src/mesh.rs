//! Capability mesh: one invocation path for local, wasm and remote capabilities.
//!
//! Order of operations for every call:
//!   1. resolve descriptor (version + health + load aware),
//!   2. policy gate (may narrow permissions or refuse),
//!   3. input schema validation,
//!   4. execute with timeout, retry and cancellation,
//!   5. record load, emit events, return a typed result.

use crate::capability::{CallerContext, CapabilityContext, InvocationResult};
use crate::policy::{CapabilityPolicy, PolicyRequest};
use crate::registry::CapabilityRegistry;
use crate::workspace::Workspace;
use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{CapabilityDescriptor, EventKind, NewEvent, VersionReq};
use agentos_core::telemetry::{metric_names, metrics};
use agentos_core::{now_ms, CapabilityId};
use agentos_event_bus::EventBus;
use agentos_storage::artifact::ArtifactStore;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct MeshConfig {
    pub default_timeout_ms: u64,
    pub max_retries: u32,
    pub retry_backoff_ms: u64,
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self { default_timeout_ms: 10_000, max_retries: 2, retry_backoff_ms: 25 }
    }
}

pub struct CapabilityMesh {
    registry: Arc<CapabilityRegistry>,
    policy: Arc<dyn CapabilityPolicy>,
    bus: Arc<dyn EventBus>,
    artifacts: Option<Arc<dyn ArtifactStore>>,
    workspace: Arc<Workspace>,
    cfg: MeshConfig,
    node_id: String,
}

impl CapabilityMesh {
    pub fn new(
        registry: Arc<CapabilityRegistry>,
        policy: Arc<dyn CapabilityPolicy>,
        bus: Arc<dyn EventBus>,
        workspace: Arc<Workspace>,
        cfg: MeshConfig,
        node_id: impl Into<String>,
    ) -> Self {
        Self { registry, policy, bus, artifacts: None, workspace, cfg, node_id: node_id.into() }
    }

    pub fn with_artifacts(mut self, artifacts: Arc<dyn ArtifactStore>) -> Self {
        self.artifacts = Some(artifacts);
        self
    }

    pub fn registry(&self) -> Arc<CapabilityRegistry> {
        self.registry.clone()
    }

    pub fn list(&self) -> Vec<CapabilityDescriptor> {
        self.registry.list()
    }

    /// The single entry point for calling a capability.
    pub async fn invoke(
        &self,
        name: &str,
        version_req: Option<&str>,
        input: serde_json::Value,
        caller: CallerContext,
    ) -> Result<InvocationResult> {
        let req = match version_req {
            Some(v) if !v.is_empty() => VersionReq(v.to_string()),
            _ => VersionReq::any(),
        };
        let registered = self.registry.lookup(name, &req)?;
        let descriptor = registered.descriptor_with_load();

        // --- policy gate -------------------------------------------------------------
        let decision = self.policy.evaluate(&PolicyRequest {
            capability: descriptor.clone(),
            session_id: caller.session_id.clone(),
            actor_id: caller.actor_id.clone(),
            input_bytes: serde_json::to_vec(&input).map(|v| v.len()).unwrap_or(0),
            workspace_root: self.workspace.root().to_path_buf(),
        })?;
        if !decision.allowed {
            metrics().inc(metric_names::POLICY_DENIED, 1);
            self.bus
                .publish(
                    NewEvent::new(EventKind::CapabilityDenied, format!("capability {name} denied by policy"))
                        .warn()
                        .session(caller.session_id.clone())
                        .capability(descriptor.id.clone())
                        .node(self.node_id.clone())
                        .payload(serde_json::json!({ "reason": decision.reason })),
                )
                .await?;
            return Err(RuntimeError::policy_denied(format!(
                "capability {name} denied: {}",
                decision.reason
            )));
        }

        // --- input validation ---------------------------------------------------------
        crate::schema::validate(&descriptor.input_schema, &input, "input")?;

        let ctx = CapabilityContext {
            capability_id: descriptor.id.clone(),
            caller: caller.clone(),
            permission: decision.granted.clone(),
            workspace: self.workspace.clone(),
            artifacts: self.artifacts.clone(),
            timeout_ms: if descriptor.timeout_ms == 0 { self.cfg.default_timeout_ms } else { descriptor.timeout_ms },
        };

        self.bus
            .publish(
                NewEvent::new(EventKind::ToolCall, format!("invoke capability {name}"))
                    .session(caller.session_id.clone())
                    .capability(descriptor.id.clone())
                    .node(self.node_id.clone())
                    .payload(serde_json::json!({
                        "capability": descriptor.name,
                        "version": descriptor.version,
                        "permission": descriptor.permission.summary(),
                    })),
            )
            .await?;

        let started = now_ms();
        let max_attempts = self.cfg.max_retries + 1;
        let mut last_error: Option<RuntimeError> = None;
        let mut attempts = 0;

        for attempt in 1..=max_attempts {
            attempts = attempt;
            if ctx.is_cancelled() {
                return Err(RuntimeError::cancelled("capability call cancelled before dispatch"));
            }
            registered.enter();
            let fut = registered.capability.invoke(input.clone(), ctx.clone());
            let timeout = std::time::Duration::from_millis(ctx.timeout_ms);
            let outcome = tokio::select! {
                biased;
                _ = ctx.caller.cancellation.cancelled() => Err(RuntimeError::cancelled("capability call cancelled")),
                r = tokio::time::timeout(timeout, fut) => match r {
                    Ok(inner) => inner,
                    Err(_) => Err(RuntimeError::timeout(format!(
                        "capability {name} exceeded {} ms", ctx.timeout_ms
                    ))),
                },
            };
            registered.leave();

            match outcome {
                Ok(output) => {
                    let duration = now_ms().saturating_sub(started);
                    registered.record(duration, true);
                    metrics().inc_by(metric_names::CAPABILITY_CALLS, &[("capability", name), ("outcome", "ok")], 1);
                    metrics().observe(metric_names::CAPABILITY_LATENCY_MS, duration as f64);
                    crate::schema::validate(&descriptor.output_schema, &output, "output").map_err(|e| {
                        RuntimeError::capability(format!("capability {name} produced invalid output: {e}"))
                    })?;
                    self.bus
                        .publish(
                            NewEvent::new(EventKind::ToolResult, format!("capability {name} completed"))
                                .session(caller.session_id.clone())
                                .capability(descriptor.id.clone())
                                .node(self.node_id.clone())
                                .payload(serde_json::json!({
                                    "duration_ms": duration,
                                    "attempts": attempts,
                                })),
                        )
                        .await?;
                    return Ok(InvocationResult {
                        capability_id: descriptor.id.clone(),
                        name: descriptor.name.clone(),
                        version: descriptor.version.clone(),
                        output,
                        duration_ms: duration,
                        attempts,
                        provider: format!("{:?}", descriptor.provider),
                    });
                }
                Err(err) => {
                    registered.record(now_ms().saturating_sub(started), false);
                    metrics().inc_by(metric_names::CAPABILITY_CALLS, &[("capability", name), ("outcome", "error")], 1);
                    let retryable = err.is_retryable() && attempt < max_attempts;
                    tracing::warn!(
                        capability = name,
                        attempt,
                        retryable,
                        error = %err,
                        "capability invocation failed"
                    );
                    last_error = Some(err);
                    if !retryable {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(self.cfg.retry_backoff_ms * attempt as u64)).await;
                }
            }
        }

        let err = last_error.unwrap_or_else(|| RuntimeError::capability(format!("capability {name} failed")));
        self.bus
            .publish(
                NewEvent::new(EventKind::ToolResult, format!("capability {name} failed"))
                    .error()
                    .session(caller.session_id.clone())
                    .capability(descriptor.id.clone())
                    .node(self.node_id.clone())
                    .payload(serde_json::json!({ "error": err.to_string(), "attempts": attempts })),
            )
            .await?;
        Err(err.with_detail("capability", descriptor.name.clone()))
    }

    /// Convenience for internal callers that only need the raw output.
    pub async fn invoke_value(
        &self,
        name: &str,
        input: serde_json::Value,
        caller: CallerContext,
    ) -> Result<serde_json::Value> {
        Ok(self.invoke(name, None, input, caller).await?.output)
    }

    pub fn capability_id_of(&self, name: &str) -> Option<CapabilityId> {
        self.registry.lookup(name, &VersionReq::any()).ok().map(|c| c.descriptor.id.clone())
    }
}
