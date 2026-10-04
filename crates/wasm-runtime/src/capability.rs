//! Wasm-backed capability: the sandbox side of the Capability trait.

use crate::abi::{status, WasmHostPolicy};
use crate::engine::{SandboxConfig, WasmEngine, WasmModule};
use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{
    CapabilityDescriptor, CapabilityKind, CapabilityPermission, CapabilityProvider,
};
use agentos_core::state::CapabilityHealth;
use agentos_core::telemetry::{metric_names, metrics};
use agentos_core::CapabilityId;
use agentos_capability_runtime::capability::{Capability, CapabilityContext};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use wasmtime::{Caller, Extern, Linker, Memory, Store, StoreLimits, StoreLimitsBuilder};

/// Everything needed to publish a wasm module as a capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmCapabilitySpec {
    pub name: String,
    pub version: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
    pub permission: CapabilityPermission,
    pub host_policy: WasmHostPolicy,
    pub timeout_ms: u64,
    pub idempotent: bool,
    pub tags: Vec<String>,
}

impl WasmCapabilitySpec {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            description: String::new(),
            input_schema: serde_json::json!({ "type": "object" }),
            output_schema: serde_json::json!({ "type": "object" }),
            permission: CapabilityPermission::pure(),
            host_policy: WasmHostPolicy::minimal(),
            timeout_ms: 1_000,
            idempotent: true,
            tags: vec!["wasm".into()],
        }
    }
}

/// State handed to every host function. The sandbox can only see this.
struct HostState {
    limits: StoreLimits,
    policy: WasmHostPolicy,
    host_calls: u64,
    logs: Vec<String>,
    emits: Vec<Value>,
}

pub struct WasmCapability {
    engine: Arc<WasmEngine>,
    module: Arc<WasmModule>,
    descriptor: CapabilityDescriptor,
    host_policy: WasmHostPolicy,
}

impl WasmCapability {
    /// Compile (or reuse) a module and wrap it as a capability.
    pub fn new(engine: Arc<WasmEngine>, spec: WasmCapabilitySpec, wasm: &[u8]) -> Result<Arc<Self>> {
        let module = engine.compile(&spec.name, &spec.version, wasm, spec.host_policy.clone())?;
        Ok(Self::from_module(engine, module, spec))
    }

    pub fn from_file(
        engine: Arc<WasmEngine>,
        spec: WasmCapabilitySpec,
        path: impl AsRef<std::path::Path>,
    ) -> Result<Arc<Self>> {
        let module = engine.compile_file(&spec.name, &spec.version, path, spec.host_policy.clone())?;
        Ok(Self::from_module(engine, module, spec))
    }

    pub fn from_module(engine: Arc<WasmEngine>, module: Arc<WasmModule>, spec: WasmCapabilitySpec) -> Arc<Self> {
        let descriptor = CapabilityDescriptor {
            id: CapabilityId::new(),
            name: spec.name.clone(),
            version: spec.version.clone(),
            description: if spec.description.is_empty() {
                format!("wasm capability {} ({ABI})", spec.name, ABI = crate::abi::ABI_VERSION)
            } else {
                spec.description.clone()
            },
            kind: CapabilityKind::Wasm,
            tags: spec.tags.clone(),
            input_schema: spec.input_schema.clone(),
            output_schema: spec.output_schema.clone(),
            permission: spec.permission.clone(),
            provider: CapabilityProvider::Local,
            timeout_ms: spec.timeout_ms,
            idempotent: spec.idempotent,
            health: CapabilityHealth::Healthy,
            load: None,
        };
        Arc::new(Self { engine, module, descriptor, host_policy: spec.host_policy })
    }

    pub fn module_name(&self) -> &str {
        &self.module.name
    }

    pub fn module_bytes(&self) -> usize {
        self.module.bytes
    }

    /// Synchronous execution. Always called through spawn_blocking so a guest can never stall an
    /// async worker: this is the boundary between the sandbox and the runtime.
    fn run(&self, input: &Value, timeout_ms: u64) -> Result<(Value, Vec<String>, Vec<Value>, u64)> {
        let engine = self.engine.engine();
        let limits = StoreLimitsBuilder::new()
            .memory_size(self.engine.config().memory_limit_bytes)
            .instances(self.engine.config().max_instances)
            .build();
        let mut store = Store::new(
            engine,
            HostState {
                limits,
                policy: self.host_policy.clone(),
                host_calls: 0,
                logs: vec![],
                emits: vec![],
            },
        );
        // Wall clock budget: the watchdog increments the epoch, the store traps when it is spent.
        store.set_epoch_deadline(self.engine.ticks_for(timeout_ms));
        store.limiter(|state| &mut state.limits);
        if let Some(fuel) = self.engine.config().fuel {
            store.set_fuel(fuel).map_err(|e| RuntimeError::sandbox(format!("cannot set fuel: {e}")))?;
        }

        let mut linker: Linker<HostState> = Linker::new(engine);
        // ---- host functions, each gated by the module policy -------------------------
        linker
            .func_wrap(
                "agentos",
                "host_log",
                |mut caller: Caller<'_, HostState>, level: i32, ptr: i32, len: i32| -> i32 {
                    if !caller.data().policy.allow_log {
                        return status::DENIED;
                    }
                    let Some(text) = read_guest_string(&mut caller, ptr, len) else {
                        return status::INVALID;
                    };
                    caller.data_mut().host_calls += 1;
                    caller.data_mut().logs.push(text.clone());
                    tracing::debug!(level, msg = %text, "wasm guest log");
                    status::OK
                },
            )
            .map_err(|e| RuntimeError::sandbox(format!("cannot define host_log: {e}")))?;
        linker
            .func_wrap("agentos", "host_now", |mut caller: Caller<'_, HostState>| -> i64 {
                if !caller.data().policy.allow_clock {
                    return -1;
                }
                caller.data_mut().host_calls += 1;
                agentos_core::now_ms() as i64
            })
            .map_err(|e| RuntimeError::sandbox(format!("cannot define host_now: {e}")))?;
        linker
            .func_wrap(
                "agentos",
                "host_emit",
                |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i32 {
                    if !caller.data().policy.allow_emit {
                        return status::DENIED;
                    }
                    let Some(text) = read_guest_string(&mut caller, ptr, len) else {
                        return status::INVALID;
                    };
                    let payload = serde_json::from_str::<Value>(&text)
                        .unwrap_or_else(|_| Value::String(text.clone()));
                    caller.data_mut().host_calls += 1;
                    caller.data_mut().emits.push(payload);
                    status::OK
                },
            )
            .map_err(|e| RuntimeError::sandbox(format!("cannot define host_emit: {e}")))?;

        let instance = linker
            .instantiate(&mut store, &self.module.module)
            .map_err(|e| RuntimeError::sandbox(format!("cannot instantiate {}: {e}", self.module.name)))?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| RuntimeError::sandbox(format!("module {} does not export memory", self.module.name)))?;

        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .map_err(|e| RuntimeError::sandbox(format!("module {} does not export alloc: {e}", self.module.name)))?;
        let invoke = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, "invoke")
            .map_err(|e| RuntimeError::sandbox(format!("module {} does not export invoke: {e}", self.module.name)))?;

        let payload = serde_json::to_vec(input)?;
        if payload.len() > 1024 * 1024 {
            return Err(RuntimeError::invalid_input("wasm input exceeds 1 MiB"));
        }
        let ptr = alloc
            .call(&mut store, payload.len() as i32)
            .map_err(|e| RuntimeError::sandbox(format!("alloc failed: {e}")))?;
        write_guest_bytes(&memory, &mut store, ptr as usize, &payload)?;

        let packed = match invoke.call(&mut store, (ptr, payload.len() as i32)) {
            Ok(v) => v,
            Err(e) => {
                let message = e.to_string();
                if message.contains("epoch") || message.contains("interrupt") {
                    return Err(RuntimeError::timeout(format!(
                        "wasm guest {} exceeded {} ms",
                        self.module.name, timeout_ms
                    ))
                    .with_detail("capability", self.module.name.clone()));
                }
                return Err(RuntimeError::sandbox(format!("wasm guest {} trapped: {message}", self.module.name)));
            }
        };

        let out_ptr = ((packed as u64) >> 32) as usize;
        let out_len = (packed as u64 & 0xffff_ffff) as usize;
        if out_len > 1024 * 1024 {
            return Err(RuntimeError::sandbox("wasm guest returned an oversized result"));
        }
        let bytes = {
            let data = memory.data(&store);
            if out_ptr + out_len > data.len() {
                return Err(RuntimeError::sandbox("wasm guest returned an out-of-bounds pointer"));
            }
            data[out_ptr..out_ptr + out_len].to_vec()
        };
        let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
            RuntimeError::sandbox(format!("wasm guest {} returned invalid JSON: {e}", self.module.name))
        })?;
        let host = &store.data();
        Ok((value, host.logs.clone(), host.emits.clone(), host.host_calls))
    }
}

#[async_trait]
impl Capability for WasmCapability {
    fn descriptor(&self) -> CapabilityDescriptor {
        self.descriptor.clone()
    }

    async fn invoke(&self, input: Value, ctx: CapabilityContext) -> Result<Value> {
        self.engine.enter_instance()?;
        let started = agentos_core::now_ms();
        let timeout_ms = if ctx.timeout_ms == 0 { self.descriptor.timeout_ms } else { ctx.timeout_ms };

        let engine = self.engine.clone();
        let engine_for_task = self.engine.clone();
        let module = self.module.clone();
        let policy = self.host_policy.clone();
        let descriptor_timeout = self.descriptor.timeout_ms;
        let name = self.descriptor.name.clone();
        let input_for_task = input.clone();

        // Run the sandbox on a blocking thread: wasm execution is synchronous.
        let joined = tokio::task::spawn_blocking(move || {
            let capability = SandboxRunner { engine: engine_for_task, module, policy, timeout_ms: descriptor_timeout };
            capability.run(&input_for_task, timeout_ms)
        })
        .await;
        engine.leave_instance();

        let (value, logs, emits, host_calls) = match joined {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                metrics().inc_by(metric_names::WASM_INVOCATIONS, &[("capability", name.as_str()), ("outcome", "error")], 1);
                metrics().inc_by(metric_names::CAPABILITY_CALLS, &[("capability", name.as_str()), ("outcome", "error")], 1);
                return Err(e);
            }
            Err(e) => {
                return Err(RuntimeError::sandbox(format!("wasm task panicked: {e}")));
            }
        };

        if ctx.is_cancelled() {
            return Err(RuntimeError::cancelled("wasm invocation cancelled"));
        }
        for log in &logs {
            tracing::debug!(capability = %self.descriptor.name, guest_log = %log, "guest log");
        }
        if !emits.is_empty() {
            if let Some(artifacts) = ctx.artifacts.clone() {
                for emit in &emits {
                    let bytes = serde_json::to_vec(emit)?;
                    let _ = artifacts
                        .put(
                            ctx.caller.session_id.clone(),
                            format!("wasm-emit-{}.json", agentos_core::now_ms()).as_str(),
                            agentos_core::model::ArtifactKind::Json,
                            "application/json",
                            &bytes,
                        )
                        .await;
                }
            }
        }
        metrics().inc_by(metric_names::WASM_INVOCATIONS, &[("capability", name.as_str()), ("outcome", "ok")], 1);
        tracing::debug!(
            capability = %self.descriptor.name,
            host_calls,
            duration_ms = agentos_core::now_ms().saturating_sub(started),
            "wasm invocation finished"
        );
        Ok(value)
    }
}

/// Small helper so the blocking closure owns exactly what it needs.
struct SandboxRunner {
    engine: Arc<WasmEngine>,
    module: Arc<WasmModule>,
    policy: WasmHostPolicy,
    timeout_ms: u64,
}

impl SandboxRunner {
    fn run(&self, input: &Value, timeout_ms: u64) -> Result<(Value, Vec<String>, Vec<Value>, u64)> {
        let capability = WasmCapability {
            engine: self.engine.clone(),
            module: self.module.clone(),
            descriptor: CapabilityDescriptor {
                id: CapabilityId::new(),
                name: self.module.name.clone(),
                version: self.module.version.clone(),
                description: String::new(),
                kind: CapabilityKind::Wasm,
                tags: vec![],
                input_schema: Value::Null,
                output_schema: Value::Null,
                permission: CapabilityPermission::default(),
                provider: CapabilityProvider::Local,
                timeout_ms: self.timeout_ms,
                idempotent: true,
                health: CapabilityHealth::Healthy,
                load: None,
            },
            host_policy: self.policy.clone(),
        };
        capability.run(input, timeout_ms)
    }
}

fn read_guest_string(caller: &mut Caller<'_, HostState>, ptr: i32, len: i32) -> Option<String> {
    if ptr < 0 || len < 0 {
        return None;
    }
    let memory = match caller.get_export("memory") {
        Some(Extern::Memory(m)) => m,
        _ => return None,
    };
    let data = memory.data(&*caller);
    let start = ptr as usize;
    let end = start.checked_add(len as usize)?;
    if end > data.len() {
        return None;
    }
    Some(String::from_utf8_lossy(&data[start..end]).to_string())
}

fn write_guest_bytes(memory: &Memory, store: &mut Store<HostState>, offset: usize, bytes: &[u8]) -> Result<()> {
    let data = memory.data_mut(store);
    let end = offset
        .checked_add(bytes.len())
        .ok_or_else(|| RuntimeError::sandbox("wasm pointer overflow"))?;
    if end > data.len() {
        return Err(RuntimeError::sandbox("module alloc returned an out-of-bounds pointer"));
    }
    data[offset..end].copy_from_slice(bytes);
    Ok(())
}

/// Configuration used when the kernel registers wasm capabilities.
pub fn sandbox_config_from(limits: &agentos_core::config::RuntimeLimits) -> SandboxConfig {
    SandboxConfig {
        timeout_ms: limits.wasm_timeout_ms,
        epoch_tick_ms: limits.epoch_tick_ms,
        memory_limit_bytes: limits.wasm_memory_limit_bytes,
        max_instances: limits.wasm_max_instances,
        fuel: None,
    }
}
