//! agentos-kernel - the composition root.
//!
//! This is the ONLY crate that knows how the planes are wired together. It constructs concrete
//! implementations, injects them as traits, starts the background heartbeat/watchdog tasks and
//! hands out an immutable Kernel handle. Nothing downstream can reach a concrete backend, which
//! is what keeps "replace the store / the model / the transport" a wiring change.

use agentos_actor_runtime::checkpoint::{CheckpointStore, StoreCheckpointStore};
use agentos_actor_runtime::migration::LocalTransfer;
use agentos_actor_runtime::runtime::{ActorRuntime, ActorRuntimeConfig};
use agentos_actor_runtime::ActorTransfer;
use agentos_agent_runtime::memory::{MemoryStore, StoreMemoryStore};
use agentos_agent_runtime::session::SessionDeps;
use agentos_agent_runtime::session_manager::SessionActorFactory;
use agentos_agent_runtime::session_manager::SessionManager;
use agentos_capability_runtime::builtins::{
    CalculatorCapability, ClockCapability, EchoCapability, FilesystemListCapability,
    FilesystemReadCapability, FilesystemWriteCapability,
};
use agentos_capability_runtime::mesh::{CapabilityMesh, MeshConfig};
use agentos_capability_runtime::registry::CapabilityRegistry;
use agentos_capability_runtime::workspace::Workspace;
use agentos_core::config::RuntimeConfig;
use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{AgentSpec, EventKind, NewEvent, WorkerLoad, WorkerRecord};
use agentos_core::telemetry::{metrics, Correlation};
use agentos_core::{now_ms, Timestamp};
use agentos_control_plane::directory::ActorDirectory;
use agentos_control_plane::placement::{PlacementPolicy, PlacementService, WorkerRegistry};
use agentos_control_plane::policy::PolicyEngine;
use agentos_event_bus::{EventBus, LocalEventBus};
use agentos_model_router::router::{ModelRouter, ProviderInfo};
use agentos_storage::artifact::{ArtifactStore, StoreArtifactStore};
use agentos_storage::blob::BlobStore;
use agentos_storage::store::Store;
use agentos_storage::{open_blobs, open_store};
use agentos_task_scheduler::scheduler::{Scheduler, SchedulerConfig};
use agentos_wasm_runtime::capability::WasmCapability;
use agentos_wasm_runtime::engine::WasmEngine;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub mod transports;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelHealth {
    /// Human label, from AGENTOS_NODE_NAME.
    pub node: String,
    /// Stable identity used in events and the directory: AGENTOS_NODE_ID, falling back to the name.
    pub node_id: String,
    pub uptime_ms: u64,
    pub store: agentos_storage::store::StoreHealth,
    pub events: agentos_event_bus::bus::BusStats,
    pub workers_online: usize,
    pub actors_live: usize,
    pub capabilities: usize,
    pub sessions: usize,
    pub models: Vec<ProviderInfo>,
    pub wasm_instances: usize,
    pub directory_cache: (u64, u64),
}

/// The wired runtime. Cheap to clone behind an Arc; every field is an interface.
pub struct Kernel {
    pub config: RuntimeConfig,
    pub store: Arc<dyn Store>,
    pub blobs: Arc<dyn BlobStore>,
    pub artifacts: Arc<dyn ArtifactStore>,
    pub bus: Arc<dyn EventBus>,
    pub registry: Arc<CapabilityRegistry>,
    pub mesh: Arc<CapabilityMesh>,
    pub models: Arc<ModelRouter>,
    pub checkpoints: Arc<dyn CheckpointStore>,
    pub actors: Arc<ActorRuntime>,
    pub directory: Arc<ActorDirectory>,
    pub workers: Arc<WorkerRegistry>,
    pub placement: Arc<PlacementService>,
    pub policy: Arc<PolicyEngine>,
    pub scheduler: Arc<Scheduler>,
    pub memory: Arc<dyn MemoryStore>,
    pub sessions: Arc<SessionManager>,
    pub wasm: Arc<WasmEngine>,
    pub transfer: Arc<dyn ActorTransfer>,
    pub started_at: Timestamp,
    shutdown: tokio_util::sync::CancellationToken,
}

impl Kernel {
    /// Build the whole runtime from configuration. Every failure here is fatal and explicit.
    pub async fn bootstrap(config: RuntimeConfig) -> Result<Arc<Self>> {
        config.validate()?;
        config.ensure_dirs()?;
        // Identity vs. label: node_id is the stable, unique identity that appears in events, the
        // actor directory and worker records; node.name is the human label. When no explicit id is
        // configured the name doubles as the id, which keeps single-instance setups zero-config and
        // lets several instances on one machine be told apart.
        let node = config.effective_node_id();

        // --- data plane: storage ---------------------------------------------------
        let store = open_store(&config.storage).await?;
        let blobs = open_blobs(&config.storage).await?;
        let artifacts: Arc<dyn ArtifactStore> = Arc::new(StoreArtifactStore::new(
            store.clone(),
            blobs.clone(),
            config.policy.max_artifact_bytes,
        ));

        // --- data plane: events ----------------------------------------------------
        let local_bus = Arc::new(
            LocalEventBus::new(store.clone(), config.storage.event_log_retention, node.clone()).await,
        );
        let _ = local_bus.warm(500).await;
        let bus: Arc<dyn EventBus> = local_bus;

        // --- policy + capabilities -------------------------------------------------
        let workspace = Arc::new(Workspace::new(&config.policy.workspace_root)?);
        let policy = Arc::new(PolicyEngine::new(config.policy.clone()));
        let registry = Arc::new(CapabilityRegistry::new());
        register_builtins(&registry)?;

        let mesh = Arc::new(
            CapabilityMesh::new(
                registry.clone(),
                policy.clone(),
                bus.clone(),
                workspace.clone(),
                MeshConfig {
                    default_timeout_ms: config.policy.capability_timeout_ms,
                    max_retries: config.policy.capability_retries,
                    retry_backoff_ms: 25,
                },
                node.clone(),
            )
            .with_artifacts(artifacts.clone()),
        );

        // --- wasm sandbox ----------------------------------------------------------
        let wasm = Arc::new(WasmEngine::new(
            agentos_wasm_runtime::capability::sandbox_config_from(&config.limits),
        )?);
        let _watchdog = wasm.spawn_watchdog();

        // --- model router ----------------------------------------------------------
        let models = Arc::new(build_model_router(&config));

        // --- control plane ---------------------------------------------------------
        let checkpoints: Arc<dyn CheckpointStore> = Arc::new(StoreCheckpointStore::new(store.clone()));
        let actors = Arc::new(ActorRuntime::new(
            store.clone(),
            bus.clone(),
            checkpoints.clone(),
            ActorRuntimeConfig {
                mailbox_capacity: config.limits.session_queue_capacity,
                node_id: node.clone(),
                max_replay_events: 5_000,
            },
        ));
        let directory = Arc::new(ActorDirectory::new(store.clone(), node.clone()));
        let workers = Arc::new(WorkerRegistry::new(
            store.clone(),
            config.limits.worker_lease_ms,
            node.clone(),
        ));
        workers.restore().await?;
        let placement = Arc::new(PlacementService::new(
            workers.clone(),
            PlacementPolicy::default(),
            bus.clone(),
        ));

        // Register this process as a worker so single-node placement has a target.
        let mut local_worker = WorkerRecord::new(format!("{node}-local"), format!("inproc://{node}"));
        local_worker.labels.insert("role".into(), "all-in-one".into());
        local_worker.capabilities = registry.list().into_iter().map(|c| c.name).collect();
        let local_worker = workers.register(local_worker).await?;
        bus.publish(
            NewEvent::new(EventKind::WorkerRegistered, "local worker registered")
                .worker(local_worker.id.clone())
                .node(node.clone())
                .payload(serde_json::json!({
                    "name": local_worker.name,
                    "capacity": { "max_actors": local_worker.capacity.max_actors, "max_tasks": local_worker.capacity.max_tasks },
                })),
        )
        .await?;

        // --- agent runtime ---------------------------------------------------------
        let memory: Arc<dyn MemoryStore> = Arc::new(StoreMemoryStore::new(store.clone()));
        let transfer: Arc<dyn ActorTransfer> = Arc::new(LocalTransfer::new(checkpoints.clone()));

        // The kernel-level scheduler exists so every plane shares one policy object. The agent
        // loop builds a per-run scheduler with its own runner (a runner needs session context).
        let scheduler_runner = Arc::new(NoopTaskRunner);

        let deps = Arc::new(SessionDeps {
            store: store.clone(),
            bus: bus.clone(),
            models: models.clone(),
            mesh: mesh.clone(),
            scheduler: Arc::new(Scheduler::new(
                store.clone(),
                bus.clone(),
                scheduler_runner,
                SchedulerConfig {
                    max_concurrency: config.policy.max_concurrent_tasks,
                    default_max_attempts: config.policy.capability_retries + 1,
                    default_timeout_ms: config.limits.default_task_timeout_ms,
                    retry_backoff_ms: 50,
                },
            )),
            memory: memory.clone(),
            artifacts: artifacts.clone(),
            checkpoints: checkpoints.clone(),
            workspace: workspace.clone(),
            spec: AgentSpec {
                allowed_capabilities: vec![],
                max_steps: config.policy.max_steps_per_run,
                ..AgentSpec::default()
            },
            node_id: node.clone(),
            run_timeout_ms: config.limits.default_task_timeout_ms * 4,
            run_tokens: Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())),
        });

        let scheduler = deps.scheduler.clone();

        let sessions = Arc::new(SessionManager::new(
            store.clone(),
            bus.clone(),
            actors.clone(),
            directory.clone(),
            placement.clone(),
            deps.clone(),
            transfer.clone(),
            node.clone(),
        ));
        let warmed = sessions.warm().await.unwrap_or(0);
        if warmed > 0 {
            tracing::info!(sessions = warmed, "directory cache warmed from durable state");
        }

        let kernel = Arc::new(Self {
            config,
            store,
            blobs,
            artifacts,
            bus,
            registry,
            mesh,
            models,
            checkpoints,
            actors,
            directory,
            workers,
            placement,
            policy,
            scheduler,
            memory,
            sessions,
            wasm,
            transfer,
            started_at: now_ms(),
            shutdown: tokio_util::sync::CancellationToken::new(),
        });

        kernel.spawn_heartbeat(local_worker.id.clone());
        kernel.spawn_lease_reaper();

        kernel
            .bus
            .publish(
                NewEvent::new(EventKind::WorkerRegistered, "kernel bootstrap complete")
                    .node(kernel.config.node.name.clone())
                    .payload(serde_json::json!({
                        "capabilities": kernel.registry.len(),
                        "store_backend": kernel.store.backend_name(),
                        "domain_version": agentos_core::DOMAIN_VERSION,
                    })),
            )
            .await?;

        Ok(kernel)
    }

    /// Register a wasm module as a capability. Failure to compile is a startup error, not a
    /// runtime surprise.
    pub fn register_wasm_capability(
        &self,
        spec: agentos_wasm_runtime::capability::WasmCapabilitySpec,
        wasm: &[u8],
    ) -> Result<()> {
        let capability = WasmCapability::new(self.wasm.clone(), spec, wasm)?;
        self.registry.register(capability)?;
        Ok(())
    }

    pub fn correlation(&self) -> Correlation {
        Correlation::new()
    }

    fn spawn_heartbeat(self: &Arc<Self>, worker_id: agentos_core::WorkerId) {
        let kernel = self.clone();
        let interval = self.config.limits.heartbeat_interval_ms.max(250);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(interval));
            loop {
                tokio::select! {
                    _ = kernel.shutdown.cancelled() => break,
                    _ = ticker.tick() => {
                        let actors = kernel.actors.list().len() as u32;
                        let load = WorkerLoad {
                            actors,
                            running_tasks: kernel.scheduler_running_hint(),
                            cpu_percent: 0.0,
                            memory_bytes: 0,
                        };
                        match kernel.workers.heartbeat(&worker_id, load).await {
                            Ok(_) => {
                                metrics().gauge(
                                    agentos_core::telemetry::metric_names::WORKERS_ONLINE,
                                    kernel.workers.online() as f64,
                                );
                            }
                            Err(e) => tracing::warn!(error = %e, "worker heartbeat failed"),
                        }
                    }
                }
            }
        });
    }

    fn spawn_lease_reaper(self: &Arc<Self>) {
        let kernel = self.clone();
        let interval = self.config.limits.worker_lease_ms.max(500);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(interval));
            loop {
                tokio::select! {
                    _ = kernel.shutdown.cancelled() => break,
                    _ = ticker.tick() => {
                        let lost = kernel.workers.reap_expired().await;
                        for worker in lost {
                            tracing::warn!(worker = %worker.id, "worker lease expired");
                            let _ = kernel.bus.publish(
                                NewEvent::new(EventKind::WorkerOffline, "worker lease expired")
                                    .warn()
                                    .worker(worker.id.clone())
                                    .node(kernel.config.node.name.clone()),
                            ).await;
                        }
                    }
                }
            }
        });
    }

    fn scheduler_running_hint(&self) -> u32 {
        0
    }

    /// Stop accepting work and flush. Actors get a chance to persist their final state.
    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        for handle in self.actors.list() {
            let _ = self.actors.stop(&handle.id).await;
        }
        let _ = self.store.flush().await;
        let _ = self
            .bus
            .publish(NewEvent::new(EventKind::ActorStopped, "kernel shutting down"))
            .await;
    }

    pub async fn health(&self) -> Result<KernelHealth> {
        Ok(KernelHealth {
            node: self.config.node.name.clone(),
            node_id: self.config.effective_node_id(),
            uptime_ms: now_ms().saturating_sub(self.started_at),
            store: self.store.health().await?,
            events: self.bus.stats(),
            workers_online: self.workers.online(),
            actors_live: self.actors.list().len(),
            capabilities: self.registry.len(),
            sessions: self.sessions.list().await?.len(),
            models: self.models.provider_infos().await,
            wasm_instances: self.wasm.live_instances(),
            directory_cache: self.directory.cache_stats(),
        })
    }

    /// Convenience used by the CLI and the demo: run one goal end to end.
    pub async fn demo_goal(&self, goal: &str) -> Result<serde_json::Value> {
        let session = self.sessions.create_session("demo", "demo session").await?;
        self.sessions.post_goal(&session.id, goal).await
    }
}

fn register_builtins(registry: &Arc<CapabilityRegistry>) -> Result<()> {
    registry.register(Arc::new(EchoCapability::new()))?;
    registry.register(Arc::new(CalculatorCapability::new()))?;
    registry.register(Arc::new(FilesystemReadCapability::default()))?;
    registry.register(Arc::new(FilesystemListCapability))?;
    registry.register(Arc::new(FilesystemWriteCapability::new(1024 * 1024)))?;
    registry.register(Arc::new(ClockCapability))?;
    Ok(())
}

fn build_model_router(config: &RuntimeConfig) -> ModelRouter {
    ModelRouter::from_config(&config.models)
}

/// Re-export used by the HTTP layer for typed access to the session factory.
pub fn session_factory(deps: Arc<SessionDeps>) -> Arc<dyn agentos_actor_runtime::actor::ActorFactory> {
    Arc::new(SessionActorFactory::new(deps))
}

/// Reserved: replace the local transfer with a network transfer without touching the runtime.
pub fn transfer_kind(kernel: &Kernel) -> &'static str {
    kernel.transfer.name()
}

/// The kernel-level scheduler has no runner of its own: a runner needs the session context that
/// only the agent loop has. Calling it is a programming error, reported as an internal failure
/// rather than a panic.
struct NoopTaskRunner;

#[async_trait::async_trait]
impl agentos_task_scheduler::scheduler::TaskRunner for NoopTaskRunner {
    async fn run(
        &self,
        node: &agentos_core::model::TaskRecord,
        _ctx: agentos_task_scheduler::scheduler::TaskContext,
    ) -> Result<serde_json::Value> {
        Err(RuntimeError::internal(format!(
            "the kernel scheduler has no runner; task {} must be executed through the agent loop",
            node.id
        )))
    }
}