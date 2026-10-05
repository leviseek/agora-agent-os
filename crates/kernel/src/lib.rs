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
use agentos_network::discovery::{
    DiscoveryEvent, LocalFileDiscovery, NodeDiscovery, NodeInfo, StubDiscovery,
};
use agentos_storage::artifact::{ArtifactStore, StoreArtifactStore};
use agentos_storage::blob::BlobStore;
use agentos_storage::store::Store;
use agentos_storage::{open_blobs, open_store};
use agentos_task_scheduler::scheduler::{Scheduler, SchedulerConfig};
use agentos_wasm_runtime::capability::WasmCapability;
use agentos_wasm_runtime::engine::WasmEngine;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub mod diagnostics;
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
    /// Discovery backend in use ("local-file", "stub", ...).
    pub discovery: String,
    /// Nodes currently visible besides this one.
    pub peers: usize,
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
    /// Makes this node visible to others and lists the ones it can see.
    pub discovery: Arc<dyn NodeDiscovery>,
    /// MCP servers this runtime started. Held so the child processes live exactly as long as the
    /// runtime does: dropping a client kills its server.
    pub mcp_clients: Vec<Arc<agentos_capability_runtime::mcp::StdioMcpClient>>,
    /// Pending operator decisions. Lives here because both the mesh and the gateway need it, and
    /// neither owns it.
    pub approvals: Arc<agentos_capability_runtime::ApprovalBroker>,
    pub started_at: Timestamp,
    shutdown: tokio_util::sync::CancellationToken,
}

impl Kernel {
    /// Build the whole runtime from configuration. Every failure here is fatal and explicit.
    pub async fn bootstrap(mut config: RuntimeConfig) -> Result<Arc<Self>> {
        config.validate()?;
        config.ensure_dirs()?;
        // Identity before anything else: every plane below stamps the resolved id, and discovery
        // cannot tell two nodes apart without it.
        let node_identity = config.resolve_node_identity()?;
        // Identity vs. label: node_id is the stable, unique identity that appears in events, the
        // actor directory and worker records; node.name is the human label. When no explicit id is
        // configured the name doubles as the id, which keeps single-instance setups zero-config and
        // lets several instances on one machine be told apart.
        let node = node_identity;

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
        // External tool servers come last, and their failure is never fatal: a runtime that cannot
        // start because an optional integration is missing would be a worse trade than one that
        // starts without it and says so.
        let mcp_clients = connect_mcp_servers(&config, &registry).await;

        // Approvals exist even when nothing asks for one: the broker is cheap, and a capability
        // that suddenly needs a decision should not require a restart to get one.
        let approvals = Arc::new(agentos_capability_runtime::ApprovalBroker::new(
            config.policy.max_pending_approvals,
        ));

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
            .with_artifacts(artifacts.clone())
            .with_approvals(
                approvals.clone(),
                config.policy.approval_timeout_ms,
                config.policy.max_pending_approvals,
            ),
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
                // Ceilings from policy: the last call of a run is the one that must not be starved
                // by a thinking model's own reasoning.
                final_answer_max_tokens: config.policy.final_answer_max_tokens,
                plan_max_tokens: config.policy.plan_max_tokens,
                ..AgentSpec::default()
            },
            node_id: node.clone(),
            run_timeout_ms: config.limits.default_task_timeout_ms * 4,
            history_messages: config.policy.history_messages,
            history_chars: config.policy.history_chars,
            memory_recall_limit: config.policy.memory_recall_limit,
            memory_recall_chars: config.policy.memory_recall_chars,
            context_files: config.policy.context_files.clone(),
            context_files_chars: config.policy.context_files_chars,
            compaction_enabled: config.policy.compaction_enabled,
            compaction_min_messages: config.policy.compaction_min_messages,
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

        // --- discovery: make this node visible, and see the others --------------------
        let discovery: Arc<dyn NodeDiscovery> = if config.discovery.enabled {
            Arc::new(LocalFileDiscovery::open(
                config.discovery.dir.clone(),
                config.discovery.ttl_ms,
            )?)
        } else {
            Arc::new(StubDiscovery::new())
        };

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
            discovery,
            mcp_clients,
            approvals,
            started_at: now_ms(),
            shutdown: tokio_util::sync::CancellationToken::new(),
        });

        kernel.spawn_heartbeat(local_worker.id.clone());
        kernel.spawn_lease_reaper();
        if let Err(error) = kernel.announce_self().await {
            // Discovery is a hint, never a startup requirement: warn and keep serving.
            tracing::warn!(error = %error, "cannot advertise this node for discovery");
        }
        kernel.spawn_discovery();

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

    /// What this node tells the others about itself.
    pub fn self_node_info(&self) -> NodeInfo {
        NodeInfo {
            node_id: agentos_core::NodeId::from_raw(self.config.effective_node_id()),
            name: self.config.node.name.clone(),
            address: advertised_http_url(&self.config),
            grpc_endpoint: Some(self.config.api.grpc_addr.clone()),
            version: agentos_core::DOMAIN_VERSION.to_string(),
            capabilities: self.registry.list().into_iter().map(|c| c.name).collect(),
            auth_required: self.config.api.auth_required(),
            transport: self.discovery.name().to_string(),
            discovered_at: now_ms(),
            last_seen: now_ms(),
        }
    }

    /// Publish (or keep fresh) this node's advertisement.
    pub async fn announce_self(&self) -> Result<()> {
        if !self.config.discovery.enabled || !self.config.discovery.advertise {
            return Ok(());
        }
        self.discovery.advertise(self.self_node_info())?;
        tracing::info!(
            node_id = %self.config.effective_node_id(),
            backend = self.discovery.name(),
            "node is discoverable"
        );
        Ok(())
    }

    /// Nodes visible besides this one.
    pub fn peers(&self) -> Vec<Arc<NodeInfo>> {
        self.discovery.nodes()
    }

    /// Discovery backend in use, for /v1/nodes and the console.
    pub fn discovery_backend(&self) -> &'static str {
        self.discovery.name()
    }

    /// Keep our advertisement fresh and turn peer changes into events, so every consumer (console,
    /// log, future placement) learns about a new workspace the moment it appears.
    fn spawn_discovery(self: &Arc<Self>) {
        if !self.config.discovery.enabled {
            return;
        }
        let kernel = self.clone();
        // Refresh comfortably inside the TTL: a third of it, bounded so we neither spin nor lag.
        let interval_ms = (kernel.config.discovery.ttl_ms / 3).clamp(500, 5_000);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
            loop {
                tokio::select! {
                    _ = kernel.shutdown.cancelled() => break,
                    _ = ticker.tick() => {
                        if kernel.config.discovery.advertise {
                            if let Err(error) = kernel.discovery.advertise(kernel.self_node_info()) {
                                tracing::warn!(error = %error, "cannot refresh the node advertisement");
                            }
                        }
                        let events = match kernel.discovery.refresh() {
                            Ok(events) => events,
                            Err(error) => {
                                tracing::warn!(error = %error, "discovery refresh failed");
                                continue;
                            }
                        };
                        for event in events {
                            let published = match event {
                                DiscoveryEvent::Joined(info) => {
                                    tracing::info!(node = %info.name, address = %info.address, "node discovered");
                                    kernel.bus.publish(
                                        NewEvent::new(
                                            EventKind::NodeDiscovered,
                                            format!("node {} is available", info.name),
                                        )
                                        .node(kernel.config.effective_node_id())
                                        .payload(serde_json::json!({
                                            "node_id": info.node_id.as_str(),
                                            "name": info.name,
                                            "address": info.address,
                                            "grpc": info.grpc_endpoint,
                                            "capabilities": info.capabilities.len(),
                                            "auth_required": info.auth_required,
                                            "transport": info.transport,
                                        })),
                                    ).await
                                }
                                DiscoveryEvent::Left(id) => {
                                    tracing::info!(node = %id, "node left");
                                    kernel.bus.publish(
                                        NewEvent::new(EventKind::NodeLost, format!("node {id} is gone"))
                                            .warn()
                                            .node(kernel.config.effective_node_id())
                                            .payload(serde_json::json!({ "node_id": id.as_str() })),
                                    ).await
                                }
                            };
                            if let Err(error) = published {
                                tracing::warn!(error = %error, "cannot publish a discovery event");
                            }
                        }
                    }
                }
            }
        });
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
        // Withdraw our advertisement so peers notice at once instead of waiting for the TTL.
        let _ = self.discovery.stop().await;
        for handle in self.actors.list() {
            let _ = self.actors.stop(&handle.id).await;
        }
        let _ = self.store.flush().await;
        let _ = self
            .bus
            .publish(NewEvent::new(EventKind::ActorStopped, "kernel shutting down"))
            .await;
    }

    /// Render a session as Markdown. The gateway stays free of rendering concerns: it asks the
    /// composition root, which owns the dependency on the agent runtime.
    pub async fn export_markdown(&self, session: &agentos_core::SessionId) -> Result<String> {
        let (record, transcript, runs) = self.sessions.export_data(session).await?;
        Ok(agentos_agent_runtime::to_markdown(&record, &transcript, &runs))
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
            discovery: self.discovery.name().to_string(),
            peers: self.discovery.nodes().len(),
        })
    }

    /// Convenience used by the CLI and the demo: run one goal end to end.
    pub async fn demo_goal(&self, goal: &str) -> Result<serde_json::Value> {
        let session = self.sessions.create_session("demo", "demo session").await?;
        self.sessions.post_goal(&session.id, goal, &[], &[], None, None).await
    }
}

/// The URL other processes should use to reach this node.
///
/// A wildcard bind (0.0.0.0 / ::) is not an address anyone can connect to, so it is advertised as
/// loopback: discovery here is same-machine first, and a cross-machine backend would advertise the
/// real interface instead.
fn advertised_http_url(config: &RuntimeConfig) -> String {
    let addr = config.api.http_addr.trim();
    let (host, port) = match addr.rsplit_once(':') {
        Some((host, port)) => (host.trim_start_matches('[').trim_end_matches(']'), port),
        None => (addr, "8788"),
    };
    if host == "0.0.0.0" || host == "::" || host.is_empty() {
        format!("http://127.0.0.1:{port}")
    } else {
        format!("http://{addr}")
    }
}

/// Start every enabled MCP server and register the tools it publishes.
///
/// A server that fails to start, or that answers the handshake with nonsense, is logged and
/// skipped: the rest of the runtime keeps working.
async fn connect_mcp_servers(
    config: &RuntimeConfig,
    registry: &Arc<CapabilityRegistry>,
) -> Vec<Arc<agentos_capability_runtime::mcp::StdioMcpClient>> {
    use agentos_capability_runtime::mcp::{connect_stdio, McpCapability};

    let mut clients = Vec::new();
    for server in config.mcp.servers.iter().filter(|server| server.enabled) {
        match connect_stdio(server, config.policy.capability_timeout_ms.max(1_000)).await {
            Ok(connection) => {
                let mut registered = 0usize;
                for tool in connection.tools {
                    let capability = McpCapability::new(
                        server.name.clone(),
                        tool,
                        connection.client.clone(),
                    );
                    match registry.register(Arc::new(capability)) {
                        Ok(_) => registered += 1,
                        Err(error) => {
                            tracing::warn!(server = %server.name, error = %error, "cannot register mcp tool")
                        }
                    }
                }
                tracing::info!(
                    server = %server.name,
                    tools = registered,
                    "mcp server connected"
                );
                clients.push(connection.client);
            }
            Err(error) => {
                tracing::warn!(server = %server.name, error = %error, "mcp server unavailable; continuing without it");
            }
        }
    }
    clients
}

fn register_builtins(registry: &Arc<CapabilityRegistry>) -> Result<()> {
    registry.register(Arc::new(EchoCapability::new()))?;
    registry.register(Arc::new(CalculatorCapability::new()))?;
    registry.register(Arc::new(FilesystemReadCapability::default()))?;
    registry.register(Arc::new(FilesystemListCapability))?;
    registry.register(Arc::new(FilesystemWriteCapability::new(1024 * 1024)))?;
    registry.register(Arc::new(agentos_capability_runtime::file_tools::FilesystemEditCapability::new(
        1024 * 1024,
    )))?;
    registry.register(Arc::new(
        agentos_capability_runtime::file_tools::FilesystemSearchCapability::default(),
    ))?;
    registry.register(Arc::new(ClockCapability))?;
    // Reading an attachment that was too long for the prompt. Registered here because the kernel is
    // where the artifact store is in scope.
    registry.register(Arc::new(
        agentos_agent_runtime::documents::DocumentReadCapability,
    ))?;
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