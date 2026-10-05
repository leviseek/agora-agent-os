//! Transport adapters: the kernel implements the network handler traits.
//!
//! This is the only place where the composition root touches the RPC layer, which keeps the
//! dependency direction honest: network does not know the kernel, the kernel offers itself to
//! the network through small traits.

use crate::Kernel;
use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::WorkerLoad;
use agentos_core::state::{ActorState, StateMachine};
use agentos_core::{now_ms, ActorId, SessionId, WorkerId};
use agentos_control_plane::directory::DirectoryEntry;
use agentos_network::grpc::{
    AgentHandler, CapabilityHandler, ControlHandler, GrpcServer, RemoteInvocation,
    RemoteInvocationResult,
};
use agentos_network::rpc;
use agentos_capability_runtime::capability::CallerContext;
use async_trait::async_trait;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Build the gRPC server for a kernel: capability + control + agent services.
pub fn grpc_server(kernel: Arc<Kernel>, addr: SocketAddr) -> GrpcServer {
    GrpcServer::new(addr)
        .with_capability(Arc::new(KernelCapabilityHandler { kernel: kernel.clone() }))
        .with_control(Arc::new(KernelControlHandler { kernel: kernel.clone() }))
        .with_agent(Arc::new(KernelAgentHandler { kernel }))
}

/// Convenience used by the CLI and by the server binary.
pub async fn serve_grpc(
    kernel: Arc<Kernel>,
    addr: String,
    cancellation: CancellationToken,
) -> Result<SocketAddr> {
    let socket: SocketAddr = addr
        .parse()
        .map_err(|e| RuntimeError::invalid_input(format!("bad gRPC address {addr}: {e}")))?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let server = grpc_server(kernel, socket);
    let handle = tokio::spawn(async move {
        if let Err(e) = server.serve(cancellation, Some(tx)).await {
            tracing::error!(error = %e, "gRPC server stopped");
        }
    });
    let bound = rx
        .await
        .map_err(|_| RuntimeError::network("the gRPC server failed to start"))?;
    drop(handle);
    Ok(bound)
}

pub struct KernelCapabilityHandler {
    pub kernel: Arc<Kernel>,
}

#[async_trait]
impl CapabilityHandler for KernelCapabilityHandler {
    async fn invoke(&self, request: RemoteInvocation) -> Result<RemoteInvocationResult> {
        let session = request
            .session_id
            .clone()
            .unwrap_or_else(|| SessionId::new());
        let caller = CallerContext {
            session_id: session,
            actor_id: request.actor_id.clone(),
            task_id: None,
            // A remote capability call carries no workspace: the wire contract has no field for one
            // yet, so it lands in the node's legacy root. Phase 3 adds `workspace_id` to the gRPC
            // request; until then this is the one path that is not workspace-scoped.
            workspace: None,
            correlation: agentos_core::telemetry::Correlation::new(),
            cancellation: CancellationToken::new(),
        };
        let result = self
            .kernel
            .mesh
            .invoke(&request.capability, Some(&request.version), request.input, caller)
            .await?;
        Ok(RemoteInvocationResult {
            output: result.output,
            duration_ms: result.duration_ms,
            attempts: result.attempts,
            provider: result.provider,
        })
    }

    async fn list(&self) -> Result<Vec<rpc::CapabilityInfo>> {
        Ok(self
            .kernel
            .registry
            .list()
            .into_iter()
            .map(|d| rpc::CapabilityInfo {
                id: d.id.into_string(),
                name: d.name,
                version: d.version,
                description: d.description,
                kind: format!("{:?}", d.kind).to_lowercase(),
                tags: d.tags,
                input_schema_json: d.input_schema.to_string(),
                output_schema_json: d.output_schema.to_string(),
                permission: d.permission.summary(),
                health: format!("{:?}", d.health).to_lowercase(),
                inflight: d.load.as_ref().map(|l| l.inflight).unwrap_or(0),
                total_calls: d.load.as_ref().map(|l| l.total_calls).unwrap_or(0),
            })
            .collect())
    }
}

pub struct KernelControlHandler {
    pub kernel: Arc<Kernel>,
}

#[async_trait]
impl ControlHandler for KernelControlHandler {
    async fn register_worker(&self, request: rpc::RegisterWorkerRequest) -> Result<rpc::WorkerReply> {
        let mut record = agentos_core::model::WorkerRecord::new(request.name.clone(), request.addr.clone());
        record.capacity.max_actors = request.max_actors.max(1);
        record.capacity.max_tasks = request.max_tasks.max(1);
        record.capabilities = request.capabilities.clone();
        for (k, v) in request.labels {
            record.labels.insert(k, v);
        }
        let registered = self.kernel.workers.register(record).await?;
        Ok(rpc::WorkerReply {
            id: registered.id.into_string(),
            name: registered.name,
            state: format!("{:?}", registered.state).to_lowercase(),
            error: None,
        })
    }

    async fn heartbeat(&self, request: rpc::HeartbeatRequest) -> Result<rpc::WorkerReply> {
        let id = WorkerId::from_raw(request.worker_id);
        let record = self
            .kernel
            .workers
            .heartbeat(
                &id,
                WorkerLoad {
                    actors: request.actors,
                    running_tasks: request.running_tasks,
                    cpu_percent: request.cpu_percent,
                    memory_bytes: request.memory_bytes,
                },
            )
            .await?;
        Ok(rpc::WorkerReply {
            id: record.id.into_string(),
            name: record.name,
            state: format!("{:?}", record.state).to_lowercase(),
            error: None,
        })
    }

    async fn deregister_worker(&self, worker_id: &WorkerId) -> Result<()> {
        self.kernel.workers.deregister(worker_id).await?;
        Ok(())
    }

    async fn list_workers(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(self.kernel.workers.list())?)
    }

    async fn register_actor(&self, request: rpc::RegisterActorRequest) -> Result<()> {
        let session = SessionId::from_raw(request.session_id);
        let actor = ActorId::from_raw(request.actor_id);
        let mut record = agentos_core::model::ActorRecord::new(actor, session, request.kind);
        record.generation = request.generation;
        record.state = match request.state.as_str() {
            "spawning" => ActorState::Spawning,
            "idle" => ActorState::Idle,
            "draining" => ActorState::Draining,
            "migrating" => ActorState::Migrating,
            "stopped" => ActorState::Stopped,
            "failed" => ActorState::Failed,
            _ => ActorState::Active,
        };
        let mut entry = DirectoryEntry::from_record(&record, Some(self.kernel.config.node.name.clone()));
        entry.worker_id = if request.worker_id.is_empty() {
            None
        } else {
            Some(WorkerId::from_raw(request.worker_id))
        };
        self.kernel.directory.register(entry).await?;
        Ok(())
    }

    async fn lookup_actor(&self, session: &SessionId) -> Result<Option<rpc::ActorReply>> {
        let entry = self.kernel.directory.lookup(session).await?;
        Ok(entry.map(|e| rpc::ActorReply {
            found: true,
            session_id: e.session_id.into_string(),
            actor_id: e.actor_id.into_string(),
            worker_id: e.worker_id.map(|w| w.into_string()).unwrap_or_default(),
            node_id: e.node_id.unwrap_or_default(),
            generation: e.generation,
            state: format!("{:?}", e.state).to_lowercase(),
            endpoints: e.endpoints,
        }))
    }

    async fn unregister_actor(&self, session: &SessionId) -> Result<()> {
        self.kernel.directory.unregister(session).await?;
        Ok(())
    }

    async fn place(&self, request: rpc::PlaceRequest) -> Result<rpc::PlaceReply> {
        let session = SessionId::from_raw(request.session_id);
        let actor = ActorId::from_raw(request.actor_id);
        let decision = self.kernel.placement.place_actor(&session, &actor, &request.kind).await?;
        Ok(rpc::PlaceReply {
            worker_id: decision.worker_id.into_string(),
            score: decision.score,
            reason: decision.reason,
            error: None,
        })
    }
}

pub struct KernelAgentHandler {
    pub kernel: Arc<Kernel>,
}

#[async_trait]
impl AgentHandler for KernelAgentHandler {
    async fn create_session(&self, user_id: &str, title: &str) -> Result<(SessionId, ActorId)> {
        let record = self.kernel.sessions.create_session(user_id, title).await?;
        Ok((record.id, record.actor_id))
    }

    async fn list_sessions(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(self.kernel.sessions.list().await?)?)
    }

    async fn close_session(&self, session: &SessionId) -> Result<()> {
        self.kernel.sessions.close(session).await
    }

    async fn get_session(&self, session: &SessionId) -> Result<serde_json::Value> {
        match self.kernel.sessions.get(session).await? {
            Some(record) => Ok(serde_json::to_value(record)?),
            None => Err(RuntimeError::not_found(format!("session {session} does not exist"))),
        }
    }

    async fn post_goal(&self, session: &SessionId, goal: &str, wait: bool) -> Result<serde_json::Value> {
        // The gRPC/CLI transport in v1 carries text: images arrive over HTTP, where the bytes can
        // be uploaded and named. A gRPC caller that wants vision gets it the same way.
        if wait {
            // The transport carries text only and no principal: the goal is attributed to nobody,
            // which is what an unauthenticated internal call is.
            self.kernel.sessions.post_goal(session, goal, &[], &[], None, None, None).await
        } else {
            self.kernel.sessions.post_goal_async(session, goal, &[], &[], None, None, None).await?;
            Ok(serde_json::json!({ "accepted": true, "session_id": session.as_str() }))
        }
    }

    async fn cancel(&self, session: &SessionId) -> Result<bool> {
        self.kernel.sessions.cancel(session).await
    }

    async fn snapshot(&self, session: &SessionId) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(self.kernel.sessions.snapshot(session).await?)?)
    }

    async fn restore(&self, checkpoint: serde_json::Value) -> Result<(SessionId, ActorId)> {
        let checkpoint: agentos_core::model::Checkpoint = serde_json::from_value(checkpoint)
            .map_err(|e| RuntimeError::invalid_input(format!("invalid checkpoint payload: {e}")))?;
        let session = checkpoint.meta.session_id.clone();
        let actor = self.kernel.sessions.restore(checkpoint).await?;
        Ok((session, actor))
    }

    async fn migrate(&self, session: &SessionId, target_worker: Option<String>) -> Result<serde_json::Value> {
        let report = self.kernel.sessions.migrate(session, target_worker).await?;
        Ok(serde_json::to_value(report)?)
    }

    async fn list_events(&self, query: rpc::EventQuery) -> Result<serde_json::Value> {
        let session = if query.session_id.is_empty() {
            None
        } else {
            Some(SessionId::from_raw(query.session_id))
        };
        let kinds = query
            .kinds
            .iter()
            .filter_map(|k| match k.as_str() {
                "created" => Some(agentos_core::model::EventKind::SessionCreated),
                "queued" => Some(agentos_core::model::EventKind::SessionMessageQueued),
                "running" => Some(agentos_core::model::EventKind::TaskStarted),
                "tool_call" => Some(agentos_core::model::EventKind::ToolCall),
                "completed" => Some(agentos_core::model::EventKind::TaskCompleted),
                "failed" => Some(agentos_core::model::EventKind::TaskFailed),
                "migrated" => Some(agentos_core::model::EventKind::ActorMigrated),
                _ => None,
            })
            .collect();
        let events = self
            .kernel
            .bus
            .replay(agentos_core::model::EventFilter {
                session_id: session,
                kinds,
                after_seq: if query.after_seq == 0 { None } else { Some(query.after_seq) },
                limit: if query.limit == 0 { 200 } else { query.limit as usize },
                ..Default::default()
            })
            .await?;
        Ok(serde_json::to_value(events)?)
    }

    async fn health(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(self.kernel.health().await?)?)
    }
}

/// Held so the unused-import lint does not fire on the state machine import above.
#[allow(dead_code)]
fn state_machine_is_in_scope(state: ActorState) -> bool {
    state.can_transition_to(ActorState::Idle)
}

#[allow(dead_code)]
fn clock() -> u64 {
    now_ms()
}
