//! Engine, module compilation and the epoch watchdog.

use crate::abi::{WasmHostPolicy, ABI_VERSION};
use agentos_core::error::{Result, RuntimeError};
use agentos_core::state::CapabilityHealth;
use agentos_core::telemetry::{metric_names, metrics};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use wasmtime::{Config, Engine, Module};

#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub timeout_ms: u64,
    /// How often the watchdog ticks the engine epoch. Smaller means finer timeout granularity.
    pub epoch_tick_ms: u64,
    pub memory_limit_bytes: usize,
    pub max_instances: usize,
    /// Reserved for the next iteration: fuel metering complements epoch interruption.
    pub fuel: Option<u64>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self { timeout_ms: 2_000, epoch_tick_ms: 50, memory_limit_bytes: 32 * 1024 * 1024, max_instances: 16, fuel: None }
    }
}

/// A compiled, reusable module plus its declared host policy.
pub struct WasmModule {
    pub name: String,
    pub version: String,
    pub module: Module,
    pub policy: WasmHostPolicy,
    pub bytes: usize,
}

pub struct WasmEngine {
    engine: Engine,
    cfg: SandboxConfig,
    live_instances: AtomicUsize,
}

impl WasmEngine {
    pub fn new(cfg: SandboxConfig) -> Result<Self> {
        let mut config = Config::new();
        // Epoch interruption is how we enforce wall-clock timeouts on untrusted guests.
        config.epoch_interruption(true);
        config.consume_fuel(cfg.fuel.is_some());
        let engine = Engine::new(&config)
            .map_err(|e| RuntimeError::sandbox(format!("cannot create the wasm engine: {e}")))?;
        Ok(Self { engine, cfg, live_instances: AtomicUsize::new(0) })
    }

    pub fn config(&self) -> &SandboxConfig {
        &self.cfg
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn abi_version(&self) -> &'static str {
        ABI_VERSION
    }

    /// Compile a module from bytes. Both wasm binaries and wat text are accepted, which keeps the
    /// built-in examples dependency-free.
    pub fn compile(&self, name: &str, version: &str, bytes: &[u8], policy: WasmHostPolicy) -> Result<Arc<WasmModule>> {
        let module = Module::new(&self.engine, bytes).map_err(|e| {
            RuntimeError::sandbox(format!("cannot compile wasm module {name}: {e}"))
        })?;
        Ok(Arc::new(WasmModule {
            name: name.to_string(),
            version: version.to_string(),
            module,
            policy,
            bytes: bytes.len(),
        }))
    }

    pub fn compile_file(
        &self,
        name: &str,
        version: &str,
        path: impl AsRef<Path>,
        policy: WasmHostPolicy,
    ) -> Result<Arc<WasmModule>> {
        let bytes = std::fs::read(path.as_ref()).map_err(|e| {
            RuntimeError::sandbox(format!("cannot read wasm module {}: {e}", path.as_ref().display()))
        })?;
        self.compile(name, version, &bytes, policy)
    }

    /// Start the epoch watchdog. Every tick advances the engine epoch, which trips the deadline
    /// of any store whose budget has expired.
    pub fn spawn_watchdog(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let engine = self.engine.clone();
        let tick = self.cfg.epoch_tick_ms.max(1);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(tick));
            loop {
                interval.tick().await;
                engine.increment_epoch();
            }
        })
    }

    /// Epoch ticks that correspond to a timeout in milliseconds.
    pub fn ticks_for(&self, timeout_ms: u64) -> u64 {
        let tick = self.cfg.epoch_tick_ms.max(1);
        ((timeout_ms.max(1) as f64) / (tick as f64)).ceil().max(1.0) as u64
    }

    pub fn enter_instance(&self) -> Result<()> {
        let live = self.live_instances.fetch_add(1, Ordering::SeqCst) + 1;
        if live > self.cfg.max_instances {
            self.live_instances.fetch_sub(1, Ordering::SeqCst);
            metrics().inc_by(metric_names::WASM_INVOCATIONS, &[("outcome", "rejected")], 1);
            return Err(RuntimeError::unavailable(format!(
                "wasm instance limit reached ({} concurrent)",
                self.cfg.max_instances
            )));
        }
        Ok(())
    }

    pub fn leave_instance(&self) {
        self.live_instances.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn live_instances(&self) -> usize {
        self.live_instances.load(Ordering::SeqCst)
    }

    /// Health of a module: compiling is proof enough for v1, but the shape is here for the day a
    /// module needs an active probe.
    pub fn health(&self, _module: &WasmModule) -> CapabilityHealth {
        CapabilityHealth::Healthy
    }
}
