//! gRPC server and clients over the generated Protobuf stubs.
//!
//! Handlers are traits so this crate never depends on the kernel: the kernel implements them and
//! the composition root decides what to serve. Errors cross the wire as a structured ErrorInfo
//! rather than as a gRPC status, so a remote capability failure keeps its retry semantics.

use crate::rpc;
use agentos_core::error::{ErrorKind, Result, RuntimeError};
use agentos_core::{ActorId, SessionId, WorkerId};
use async_trait::async_trait;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// One remote capability call, normalized.
#[derive(Debug, Clone)]
pub struct RemoteInvocation {
    pub capability: String,
    pub version: String,
    pub input: serde_json::Value,
    pub session_id: Option<SessionId>,
    pub actor_id: Option<ActorId>,
    pub task_id: Option<String>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone)]
pub struct RemoteInvocationResult {
    pub output: serde_json::Value,
    pub duration_ms: u64,
    pub attempts: u32,
    pub provider: String,
}

#[async_trait]
pub trait CapabilityHandler: Send + Sync + 'static {
    async fn invoke(&self, request: RemoteInvocation) -> Result<RemoteInvocationResult>;
    async fn list(&self) -> Result<Vec<rpc::CapabilityInfo>>;
}

#[async_trait]
pub trait ControlHandler: Send + Sync + 'static {
    async fn register_worker(&self, request: rpc::RegisterWorkerRequest) -> Result<rpc::WorkerReply>;
    async fn heartbeat(&self, request: rpc::HeartbeatRequest) -> Result<rpc::WorkerReply>;
    async fn deregister_worker(&self, worker_id: &WorkerId) -> Result<()>;
    async fn list_workers(&self) -> Result<serde_json::Value>;
    async fn register_actor(&self, request: rpc::RegisterActorRequest) -> Result<()>;
    async fn lookup_actor(&self, session: &SessionId) -> Result<Option<rpc::ActorReply>>;
    async fn unregister_actor(&self, session: &SessionId) -> Result<()>;
    async fn place(&self, request: rpc::PlaceRequest) -> Result<rpc::PlaceReply>;
}

#[async_trait]
pub trait AgentHandler: Send + Sync + 'static {
    async fn create_session(&self, user_id: &str, title: &str) -> Result<(SessionId, ActorId)>;
    async fn list_sessions(&self) -> Result<serde_json::Value>;
    async fn close_session(&self, session: &SessionId) -> Result<()>;
    async fn get_session(&self, session: &SessionId) -> Result<serde_json::Value>;
    async fn post_goal(&self, session: &SessionId, goal: &str, wait: bool) -> Result<serde_json::Value>;
    async fn cancel(&self, session: &SessionId) -> Result<bool>;
    async fn snapshot(&self, session: &SessionId) -> Result<serde_json::Value>;
    async fn restore(&self, checkpoint: serde_json::Value) -> Result<(SessionId, ActorId)>;
    async fn migrate(&self, session: &SessionId, target_worker: Option<String>) -> Result<serde_json::Value>;
    async fn list_events(&self, query: rpc::EventQuery) -> Result<serde_json::Value>;
    async fn health(&self) -> Result<serde_json::Value>;
}

// ---------------------------------------------------------------------------------------------
// server
// ---------------------------------------------------------------------------------------------

pub struct GrpcServer {
    addr: SocketAddr,
    capability: Option<Arc<dyn CapabilityHandler>>,
    control: Option<Arc<dyn ControlHandler>>,
    agent: Option<Arc<dyn AgentHandler>>,
}

impl GrpcServer {
    pub fn new(addr: SocketAddr) -> Self {
        Self { addr, capability: None, control: None, agent: None }
    }

    pub fn with_capability(mut self, handler: Arc<dyn CapabilityHandler>) -> Self {
        self.capability = Some(handler);
        self
    }

    pub fn with_control(mut self, handler: Arc<dyn ControlHandler>) -> Self {
        self.control = Some(handler);
        self
    }

    pub fn with_agent(mut self, handler: Arc<dyn AgentHandler>) -> Self {
        self.agent = Some(handler);
        self
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Serve until cancelled. Returns the bound address through the callback so tests can use
    /// port 0 and learn the real port.
    pub async fn serve(
        self,
        cancellation: CancellationToken,
        on_ready: Option<tokio::sync::oneshot::Sender<SocketAddr>>,
    ) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(self.addr)
            .await
            .map_err(|e| RuntimeError::network(format!("cannot bind gRPC listener on {}: {e}", self.addr)))?;
        let local = listener
            .local_addr()
            .map_err(|e| RuntimeError::network(format!("cannot read the gRPC listener address: {e}")))?;
        if let Some(tx) = on_ready {
            let _ = tx.send(local);
        }
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        let mut builder = tonic::transport::Server::builder();

        let shutdown = {
            let token = cancellation.clone();
            async move {
                token.cancelled().await;
            }
        };

        match (self.capability, self.control, self.agent) {
            (Some(cap), Some(control), Some(agent)) => builder
                .add_service(rpc::capability_service_server::CapabilityServiceServer::new(CapabilitySvc { inner: cap }))
                .add_service(rpc::control_service_server::ControlServiceServer::new(ControlSvc { inner: control }))
                .add_service(rpc::agent_service_server::AgentServiceServer::new(AgentSvc { inner: agent }))
                .serve_with_incoming_shutdown(incoming, shutdown)
                .await
                .map_err(|e| RuntimeError::network(format!("gRPC server failed: {e}"))),
            (Some(cap), Some(control), None) => builder
                .add_service(rpc::capability_service_server::CapabilityServiceServer::new(CapabilitySvc { inner: cap }))
                .add_service(rpc::control_service_server::ControlServiceServer::new(ControlSvc { inner: control }))
                .serve_with_incoming_shutdown(incoming, shutdown)
                .await
                .map_err(|e| RuntimeError::network(format!("gRPC server failed: {e}"))),
            (Some(cap), None, None) => builder
                .add_service(rpc::capability_service_server::CapabilityServiceServer::new(CapabilitySvc { inner: cap }))
                .serve_with_incoming_shutdown(incoming, shutdown)
                .await
                .map_err(|e| RuntimeError::network(format!("gRPC server failed: {e}"))),
            (None, Some(control), None) => builder
                .add_service(rpc::control_service_server::ControlServiceServer::new(ControlSvc { inner: control }))
                .serve_with_incoming_shutdown(incoming, shutdown)
                .await
                .map_err(|e| RuntimeError::network(format!("gRPC server failed: {e}"))),
            (None, None, Some(agent)) => builder
                .add_service(rpc::agent_service_server::AgentServiceServer::new(AgentSvc { inner: agent }))
                .serve_with_incoming_shutdown(incoming, shutdown)
                .await
                .map_err(|e| RuntimeError::network(format!("gRPC server failed: {e}"))),
            _ => Err(RuntimeError::invalid_input("no gRPC handler was configured")),
        }
    }
}

struct CapabilitySvc {
    inner: Arc<dyn CapabilityHandler>,
}

fn to_error_info(error: &RuntimeError) -> rpc::ErrorInfo {
    let mut details = std::collections::HashMap::new();
    if let Some(detail) = &error.detail {
        if let Some(map) = detail.as_object() {
            for (k, v) in map {
                details.insert(k.clone(), v.to_string());
            }
        }
    }
    rpc::ErrorInfo {
        code: error.code().to_string(),
        message: error.message.clone(),
        retryable: error.is_retryable(),
        details,
    }
}

fn error_from_info(info: rpc::ErrorInfo) -> RuntimeError {
    let kind = match info.code.as_str() {
        "invalid_input" => ErrorKind::InvalidInput,
        "not_found" => ErrorKind::NotFound,
        "conflict" => ErrorKind::Conflict,
        "unauthorized" => ErrorKind::Unauthorized,
        "policy_denied" => ErrorKind::PolicyDenied,
        "rate_limited" => ErrorKind::RateLimited,
        "timeout" => ErrorKind::Timeout,
        "cancelled" => ErrorKind::Cancelled,
        "capability" => ErrorKind::Capability,
        "model" => ErrorKind::Model,
        "storage" => ErrorKind::Storage,
        "network" => ErrorKind::Network,
        "sandbox" => ErrorKind::Sandbox,
        "migration" => ErrorKind::Migration,
        "unavailable" => ErrorKind::Unavailable,
        _ => ErrorKind::Internal,
    };
    RuntimeError::new(kind, info.message).retryable(info.retryable)
}

#[tonic::async_trait]
impl rpc::capability_service_server::CapabilityService for CapabilitySvc {
    async fn invoke(
        &self,
        request: tonic::Request<rpc::InvokeRequest>,
    ) -> std::result::Result<tonic::Response<rpc::InvokeResponse>, tonic::Status> {
        let req = request.into_inner();
        let input = serde_json::from_str(&req.input_json).unwrap_or(serde_json::Value::Null);
        let invocation = RemoteInvocation {
            capability: req.capability,
            version: req.version,
            input,
            session_id: if req.session_id.is_empty() { None } else { Some(SessionId::from_raw(req.session_id)) },
            actor_id: if req.actor_id.is_empty() { None } else { Some(ActorId::from_raw(req.actor_id)) },
            task_id: if req.task_id.is_empty() { None } else { Some(req.task_id) },
            timeout_ms: req.timeout_ms,
        };
        match self.inner.invoke(invocation).await {
            Ok(result) => Ok(tonic::Response::new(rpc::InvokeResponse {
                output_json: serde_json::to_string(&result.output).unwrap_or_default(),
                duration_ms: result.duration_ms,
                attempts: result.attempts,
                provider: result.provider,
                error: None,
            })),
            Err(e) => Ok(tonic::Response::new(rpc::InvokeResponse {
                output_json: String::new(),
                duration_ms: 0,
                attempts: 0,
                provider: String::new(),
                error: Some(to_error_info(&e)),
            })),
        }
    }

    async fn list(
        &self,
        request: tonic::Request<rpc::ListCapabilitiesRequest>,
    ) -> std::result::Result<tonic::Response<rpc::ListCapabilitiesResponse>, tonic::Status> {
        let query = request.into_inner();
        let mut capabilities = self
            .inner
            .list()
            .await
            .map_err(|e| tonic::Status::internal(e.to_string()))?;
        if !query.name_contains.is_empty() {
            let needle = query.name_contains.to_ascii_lowercase();
            capabilities.retain(|c| c.name.to_ascii_lowercase().contains(&needle));
        }
        if query.healthy_only {
            capabilities.retain(|c| c.health == "healthy");
        }
        if !query.tags.is_empty() {
            capabilities.retain(|c| query.tags.iter().any(|t| c.tags.contains(t)));
        }
        Ok(tonic::Response::new(rpc::ListCapabilitiesResponse { capabilities }))
    }

    async fn health(
        &self,
        _request: tonic::Request<rpc::Empty>,
    ) -> std::result::Result<tonic::Response<rpc::JsonPayload>, tonic::Status> {
        let capabilities = self.inner.list().await.map_err(|e| tonic::Status::internal(e.to_string()))?;
        let payload = serde_json::json!({ "capabilities": capabilities.len(), "status": "ok" });
        Ok(tonic::Response::new(rpc::JsonPayload { json: payload.to_string() }))
    }
}

struct ControlSvc {
    inner: Arc<dyn ControlHandler>,
}

#[tonic::async_trait]
impl rpc::control_service_server::ControlService for ControlSvc {
    async fn register_worker(
        &self,
        request: tonic::Request<rpc::RegisterWorkerRequest>,
    ) -> std::result::Result<tonic::Response<rpc::WorkerReply>, tonic::Status> {
        match self.inner.register_worker(request.into_inner()).await {
            Ok(reply) => Ok(tonic::Response::new(reply)),
            Err(e) => Ok(tonic::Response::new(rpc::WorkerReply {
                id: String::new(),
                name: String::new(),
                state: "lost".into(),
                error: Some(to_error_info(&e)),
            })),
        }
    }

    async fn heartbeat(
        &self,
        request: tonic::Request<rpc::HeartbeatRequest>,
    ) -> std::result::Result<tonic::Response<rpc::WorkerReply>, tonic::Status> {
        match self.inner.heartbeat(request.into_inner()).await {
            Ok(reply) => Ok(tonic::Response::new(reply)),
            Err(e) => Ok(tonic::Response::new(rpc::WorkerReply {
                id: String::new(),
                name: String::new(),
                state: "lost".into(),
                error: Some(to_error_info(&e)),
            })),
        }
    }

    async fn deregister_worker(
        &self,
        request: tonic::Request<rpc::WorkerIdRequest>,
    ) -> std::result::Result<tonic::Response<rpc::Ack>, tonic::Status> {
        let id = WorkerId::from_raw(request.into_inner().worker_id);
        match self.inner.deregister_worker(&id).await {
            Ok(()) => Ok(tonic::Response::new(rpc::Ack { ok: true, detail: String::new() })),
            Err(e) => Ok(tonic::Response::new(rpc::Ack { ok: false, detail: e.to_string() })),
        }
    }

    async fn list_workers(
        &self,
        _request: tonic::Request<rpc::Empty>,
    ) -> std::result::Result<tonic::Response<rpc::ListWorkersReply>, tonic::Status> {
        let value = self.inner.list_workers().await.map_err(|e| tonic::Status::internal(e.to_string()))?;
        Ok(tonic::Response::new(rpc::ListWorkersReply { workers_json: value.to_string() }))
    }

    async fn register_actor(
        &self,
        request: tonic::Request<rpc::RegisterActorRequest>,
    ) -> std::result::Result<tonic::Response<rpc::Ack>, tonic::Status> {
        match self.inner.register_actor(request.into_inner()).await {
            Ok(()) => Ok(tonic::Response::new(rpc::Ack { ok: true, detail: String::new() })),
            Err(e) => Ok(tonic::Response::new(rpc::Ack { ok: false, detail: e.to_string() })),
        }
    }

    async fn lookup_actor(
        &self,
        request: tonic::Request<rpc::LookupActorRequest>,
    ) -> std::result::Result<tonic::Response<rpc::ActorReply>, tonic::Status> {
        let session = SessionId::from_raw(request.into_inner().session_id);
        match self.inner.lookup_actor(&session).await {
            Ok(Some(reply)) => Ok(tonic::Response::new(reply)),
            Ok(None) => Ok(tonic::Response::new(rpc::ActorReply {
                found: false,
                session_id: session.into_string(),
                ..Default::default()
            })),
            Err(e) => Err(tonic::Status::internal(e.to_string())),
        }
    }

    async fn unregister_actor(
        &self,
        request: tonic::Request<rpc::SessionIdRequest>,
    ) -> std::result::Result<tonic::Response<rpc::Ack>, tonic::Status> {
        let session = SessionId::from_raw(request.into_inner().session_id);
        match self.inner.unregister_actor(&session).await {
            Ok(()) => Ok(tonic::Response::new(rpc::Ack { ok: true, detail: String::new() })),
            Err(e) => Ok(tonic::Response::new(rpc::Ack { ok: false, detail: e.to_string() })),
        }
    }

    async fn place(
        &self,
        request: tonic::Request<rpc::PlaceRequest>,
    ) -> std::result::Result<tonic::Response<rpc::PlaceReply>, tonic::Status> {
        match self.inner.place(request.into_inner()).await {
            Ok(reply) => Ok(tonic::Response::new(reply)),
            Err(e) => Ok(tonic::Response::new(rpc::PlaceReply {
                worker_id: String::new(),
                score: 0.0,
                reason: String::new(),
                error: Some(to_error_info(&e)),
            })),
        }
    }
}

struct AgentSvc {
    inner: Arc<dyn AgentHandler>,
}

#[tonic::async_trait]
impl rpc::agent_service_server::AgentService for AgentSvc {
    async fn create_session(
        &self,
        request: tonic::Request<rpc::CreateSessionRequest>,
    ) -> std::result::Result<tonic::Response<rpc::SessionReply>, tonic::Status> {
        let req = request.into_inner();
        match self.inner.create_session(&req.user_id, &req.title).await {
            Ok((session, actor)) => Ok(tonic::Response::new(rpc::SessionReply {
                session_id: session.into_string(),
                actor_id: actor.into_string(),
                state: "active".into(),
                error: None,
            })),
            Err(e) => Ok(tonic::Response::new(rpc::SessionReply {
                session_id: String::new(),
                actor_id: String::new(),
                state: "failed".into(),
                error: Some(to_error_info(&e)),
            })),
        }
    }

    async fn list_sessions(
        &self,
        _request: tonic::Request<rpc::Empty>,
    ) -> std::result::Result<tonic::Response<rpc::JsonPayload>, tonic::Status> {
        let value = self.inner.list_sessions().await.map_err(|e| tonic::Status::internal(e.to_string()))?;
        Ok(tonic::Response::new(rpc::JsonPayload { json: value.to_string() }))
    }

    async fn close_session(
        &self,
        request: tonic::Request<rpc::SessionIdRequest>,
    ) -> std::result::Result<tonic::Response<rpc::Ack>, tonic::Status> {
        let session = SessionId::from_raw(request.into_inner().session_id);
        match self.inner.close_session(&session).await {
            Ok(()) => Ok(tonic::Response::new(rpc::Ack { ok: true, detail: String::new() })),
            Err(e) => Ok(tonic::Response::new(rpc::Ack { ok: false, detail: e.to_string() })),
        }
    }

    async fn get_session(
        &self,
        request: tonic::Request<rpc::SessionIdRequest>,
    ) -> std::result::Result<tonic::Response<rpc::JsonPayload>, tonic::Status> {
        let session = SessionId::from_raw(request.into_inner().session_id);
        let value = self.inner.get_session(&session).await.map_err(|e| tonic::Status::internal(e.to_string()))?;
        Ok(tonic::Response::new(rpc::JsonPayload { json: value.to_string() }))
    }

    async fn post_goal(
        &self,
        request: tonic::Request<rpc::PostGoalRequest>,
    ) -> std::result::Result<tonic::Response<rpc::JsonPayload>, tonic::Status> {
        let req = request.into_inner();
        let session = SessionId::from_raw(req.session_id);
        match self.inner.post_goal(&session, &req.goal, req.wait).await {
            Ok(value) => Ok(tonic::Response::new(rpc::JsonPayload { json: value.to_string() })),
            Err(e) => {
                let payload = serde_json::json!({
                    "error": {
                        "code": e.code(),
                        "message": e.message,
                        "retryable": e.is_retryable(),
                    }
                });
                Ok(tonic::Response::new(rpc::JsonPayload { json: payload.to_string() }))
            }
        }
    }

    async fn cancel(
        &self,
        request: tonic::Request<rpc::SessionIdRequest>,
    ) -> std::result::Result<tonic::Response<rpc::Ack>, tonic::Status> {
        let session = SessionId::from_raw(request.into_inner().session_id);
        match self.inner.cancel(&session).await {
            Ok(cancelled) => Ok(tonic::Response::new(rpc::Ack {
                ok: cancelled,
                detail: if cancelled { "cancellation requested".into() } else { "no run in flight".into() },
            })),
            Err(e) => Ok(tonic::Response::new(rpc::Ack { ok: false, detail: e.to_string() })),
        }
    }

    async fn snapshot(
        &self,
        request: tonic::Request<rpc::SessionIdRequest>,
    ) -> std::result::Result<tonic::Response<rpc::JsonPayload>, tonic::Status> {
        let session = SessionId::from_raw(request.into_inner().session_id);
        let value = self.inner.snapshot(&session).await.map_err(|e| tonic::Status::internal(e.to_string()))?;
        Ok(tonic::Response::new(rpc::JsonPayload { json: value.to_string() }))
    }

    async fn restore(
        &self,
        request: tonic::Request<rpc::JsonPayload>,
    ) -> std::result::Result<tonic::Response<rpc::SessionReply>, tonic::Status> {
        let checkpoint: serde_json::Value =
            serde_json::from_str(&request.into_inner().json).map_err(|e| tonic::Status::invalid_argument(e.to_string()))?;
        match self.inner.restore(checkpoint).await {
            Ok((session, actor)) => Ok(tonic::Response::new(rpc::SessionReply {
                session_id: session.into_string(),
                actor_id: actor.into_string(),
                state: "active".into(),
                error: None,
            })),
            Err(e) => Ok(tonic::Response::new(rpc::SessionReply {
                session_id: String::new(),
                actor_id: String::new(),
                state: "failed".into(),
                error: Some(to_error_info(&e)),
            })),
        }
    }

    async fn migrate(
        &self,
        request: tonic::Request<rpc::MigrateRequest>,
    ) -> std::result::Result<tonic::Response<rpc::JsonPayload>, tonic::Status> {
        let req = request.into_inner();
        let session = SessionId::from_raw(req.session_id);
        let target = if req.target_worker.is_empty() { None } else { Some(req.target_worker) };
        match self.inner.migrate(&session, target).await {
            Ok(value) => Ok(tonic::Response::new(rpc::JsonPayload { json: value.to_string() })),
            Err(e) => {
                let payload = serde_json::json!({
                    "error": {
                        "code": e.code(),
                        "message": e.message,
                        "retryable": e.is_retryable(),
                    }
                });
                Ok(tonic::Response::new(rpc::JsonPayload { json: payload.to_string() }))
            }
        }
    }

    async fn list_events(
        &self,
        request: tonic::Request<rpc::EventQuery>,
    ) -> std::result::Result<tonic::Response<rpc::JsonPayload>, tonic::Status> {
        let value = self
            .inner
            .list_events(request.into_inner())
            .await
            .map_err(|e| tonic::Status::internal(e.to_string()))?;
        Ok(tonic::Response::new(rpc::JsonPayload { json: value.to_string() }))
    }

    async fn health(
        &self,
        _request: tonic::Request<rpc::Empty>,
    ) -> std::result::Result<tonic::Response<rpc::JsonPayload>, tonic::Status> {
        let value = self.inner.health().await.map_err(|e| tonic::Status::internal(e.to_string()))?;
        Ok(tonic::Response::new(rpc::JsonPayload { json: value.to_string() }))
    }
}

// ---------------------------------------------------------------------------------------------
// clients
// ---------------------------------------------------------------------------------------------

pub struct GrpcCapabilityClient {
    endpoint: String,
    channel: tonic::transport::Channel,
}

impl GrpcCapabilityClient {
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        let channel = tonic::transport::Channel::from_shared(endpoint.clone())
            .map_err(|e| RuntimeError::invalid_input(format!("bad gRPC endpoint {endpoint}: {e}")))?
            .connect()
            .await
            .map_err(|e| RuntimeError::network(format!("cannot connect to {endpoint}: {e}")))?;
        Ok(Self { endpoint, channel })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub async fn invoke(&self, invocation: RemoteInvocation) -> Result<RemoteInvocationResult> {
        let mut client = rpc::capability_service_client::CapabilityServiceClient::new(self.channel.clone());
        let request = rpc::InvokeRequest {
            capability: invocation.capability,
            version: invocation.version,
            input_json: serde_json::to_string(&invocation.input)?,
            session_id: invocation.session_id.map(|s| s.into_string()).unwrap_or_default(),
            actor_id: invocation.actor_id.map(|a| a.into_string()).unwrap_or_default(),
            task_id: invocation.task_id.unwrap_or_default(),
            correlation: None,
            timeout_ms: invocation.timeout_ms,
        };
        let response = tokio::time::timeout(
            std::time::Duration::from_millis(if invocation.timeout_ms == 0 { 30_000 } else { invocation.timeout_ms + 1_000 }),
            client.invoke(request),
        )
        .await
        .map_err(|_| RuntimeError::timeout("gRPC capability call timed out"))?
        .map_err(|e| RuntimeError::network(format!("gRPC call failed: {e}")))?;
        let reply = response.into_inner();
        if let Some(error) = reply.error {
            return Err(error_from_info(error));
        }
        Ok(RemoteInvocationResult {
            output: serde_json::from_str(&reply.output_json).unwrap_or(serde_json::Value::Null),
            duration_ms: reply.duration_ms,
            attempts: reply.attempts,
            provider: reply.provider,
        })
    }

    pub async fn list(&self, name_contains: &str) -> Result<Vec<rpc::CapabilityInfo>> {
        let mut client = rpc::capability_service_client::CapabilityServiceClient::new(self.channel.clone());
        let response = client
            .list(rpc::ListCapabilitiesRequest {
                name_contains: name_contains.to_string(),
                tags: vec![],
                healthy_only: false,
            })
            .await
            .map_err(|e| RuntimeError::network(format!("gRPC list failed: {e}")))?;
        Ok(response.into_inner().capabilities)
    }
}

pub struct GrpcControlClient {
    channel: tonic::transport::Channel,
}

impl GrpcControlClient {
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        let channel = tonic::transport::Channel::from_shared(endpoint.clone())
            .map_err(|e| RuntimeError::invalid_input(format!("bad gRPC endpoint {endpoint}: {e}")))?
            .connect()
            .await
            .map_err(|e| RuntimeError::network(format!("cannot connect to {endpoint}: {e}")))?;
        Ok(Self { channel })
    }

    pub async fn register_worker(&self, request: rpc::RegisterWorkerRequest) -> Result<rpc::WorkerReply> {
        let mut client = rpc::control_service_client::ControlServiceClient::new(self.channel.clone());
        let reply = client
            .register_worker(request)
            .await
            .map_err(|e| RuntimeError::network(format!("register_worker failed: {e}")))?
            .into_inner();
        if let Some(error) = reply.error.clone() {
            return Err(error_from_info(error));
        }
        Ok(reply)
    }

    pub async fn heartbeat(&self, request: rpc::HeartbeatRequest) -> Result<rpc::WorkerReply> {
        let mut client = rpc::control_service_client::ControlServiceClient::new(self.channel.clone());
        let reply = client
            .heartbeat(request)
            .await
            .map_err(|e| RuntimeError::network(format!("heartbeat failed: {e}")))?
            .into_inner();
        if let Some(error) = reply.error.clone() {
            return Err(error_from_info(error));
        }
        Ok(reply)
    }

    pub async fn lookup_actor(&self, session: &SessionId) -> Result<Option<rpc::ActorReply>> {
        let mut client = rpc::control_service_client::ControlServiceClient::new(self.channel.clone());
        let reply = client
            .lookup_actor(rpc::LookupActorRequest { session_id: session.as_str().to_string() })
            .await
            .map_err(|e| RuntimeError::network(format!("lookup_actor failed: {e}")))?
            .into_inner();
        Ok(if reply.found { Some(reply) } else { None })
    }

    pub async fn list_workers(&self) -> Result<serde_json::Value> {
        let mut client = rpc::control_service_client::ControlServiceClient::new(self.channel.clone());
        let reply = client
            .list_workers(rpc::Empty {})
            .await
            .map_err(|e| RuntimeError::network(format!("list_workers failed: {e}")))?
            .into_inner();
        Ok(serde_json::from_str(&reply.workers_json).unwrap_or(serde_json::Value::Null))
    }
}

pub struct GrpcAgentClient {
    channel: tonic::transport::Channel,
}

impl GrpcAgentClient {
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        let channel = tonic::transport::Channel::from_shared(endpoint.clone())
            .map_err(|e| RuntimeError::invalid_input(format!("bad gRPC endpoint {endpoint}: {e}")))?
            .connect()
            .await
            .map_err(|e| RuntimeError::network(format!("cannot connect to {endpoint}: {e}")))?;
        Ok(Self { channel })
    }

    pub async fn create_session(&self, user_id: &str, title: &str) -> Result<(SessionId, ActorId)> {
        let mut client = rpc::agent_service_client::AgentServiceClient::new(self.channel.clone());
        let reply = client
            .create_session(rpc::CreateSessionRequest { user_id: user_id.into(), title: title.into() })
            .await
            .map_err(|e| RuntimeError::network(format!("create_session failed: {e}")))?
            .into_inner();
        if let Some(error) = reply.error {
            return Err(error_from_info(error));
        }
        Ok((SessionId::parse(&reply.session_id).map_err(|e| RuntimeError::invalid_input(e.to_string()))?,
            ActorId::parse(&reply.actor_id).map_err(|e| RuntimeError::invalid_input(e.to_string()))?))
    }

    pub async fn post_goal(&self, session: &SessionId, goal: &str, wait: bool) -> Result<serde_json::Value> {
        let mut client = rpc::agent_service_client::AgentServiceClient::new(self.channel.clone());
        let reply = client
            .post_goal(rpc::PostGoalRequest {
                session_id: session.as_str().to_string(),
                goal: goal.to_string(),
                wait,
                correlation: None,
            })
            .await
            .map_err(|e| RuntimeError::network(format!("post_goal failed: {e}")))?
            .into_inner();
        Ok(serde_json::from_str(&reply.json).unwrap_or(serde_json::Value::Null))
    }

    pub async fn list_sessions(&self) -> Result<serde_json::Value> {
        let mut client = rpc::agent_service_client::AgentServiceClient::new(self.channel.clone());
        let reply = client
            .list_sessions(rpc::Empty {})
            .await
            .map_err(|e| RuntimeError::network(format!("list_sessions failed: {e}")))?
            .into_inner();
        Ok(serde_json::from_str(&reply.json).unwrap_or(serde_json::Value::Null))
    }

    pub async fn get_session(&self, session: &SessionId) -> Result<serde_json::Value> {
        let mut client = rpc::agent_service_client::AgentServiceClient::new(self.channel.clone());
        let reply = client
            .get_session(rpc::SessionIdRequest { session_id: session.as_str().to_string() })
            .await
            .map_err(|e| RuntimeError::network(format!("get_session failed: {e}")))?
            .into_inner();
        Ok(serde_json::from_str(&reply.json).unwrap_or(serde_json::Value::Null))
    }

    pub async fn snapshot(&self, session: &SessionId) -> Result<serde_json::Value> {
        let mut client = rpc::agent_service_client::AgentServiceClient::new(self.channel.clone());
        let reply = client
            .snapshot(rpc::SessionIdRequest { session_id: session.as_str().to_string() })
            .await
            .map_err(|e| RuntimeError::network(format!("snapshot failed: {e}")))?
            .into_inner();
        Ok(serde_json::from_str(&reply.json).unwrap_or(serde_json::Value::Null))
    }

    pub async fn restore(&self, checkpoint: serde_json::Value) -> Result<(SessionId, ActorId)> {
        let mut client = rpc::agent_service_client::AgentServiceClient::new(self.channel.clone());
        let reply = client
            .restore(rpc::JsonPayload { json: checkpoint.to_string() })
            .await
            .map_err(|e| RuntimeError::network(format!("restore failed: {e}")))?
            .into_inner();
        if let Some(error) = reply.error {
            return Err(error_from_info(error));
        }
        Ok((SessionId::from_raw(reply.session_id), ActorId::from_raw(reply.actor_id)))
    }

    pub async fn health(&self) -> Result<serde_json::Value> {
        let mut client = rpc::agent_service_client::AgentServiceClient::new(self.channel.clone());
        let reply = client
            .health(rpc::Empty {})
            .await
            .map_err(|e| RuntimeError::network(format!("health failed: {e}")))?
            .into_inner();
        Ok(serde_json::from_str(&reply.json).unwrap_or(serde_json::Value::Null))
    }

    pub async fn cancel(&self, session: &SessionId) -> Result<bool> {
        let mut client = rpc::agent_service_client::AgentServiceClient::new(self.channel.clone());
        let reply = client
            .cancel(rpc::SessionIdRequest { session_id: session.as_str().to_string() })
            .await
            .map_err(|e| RuntimeError::network(format!("cancel failed: {e}")))?
            .into_inner();
        Ok(reply.ok)
    }

    pub async fn close_session(&self, session: &SessionId) -> Result<()> {
        let mut client = rpc::agent_service_client::AgentServiceClient::new(self.channel.clone());
        let reply = client
            .close_session(rpc::SessionIdRequest { session_id: session.as_str().to_string() })
            .await
            .map_err(|e| RuntimeError::network(format!("close_session failed: {e}")))?
            .into_inner();
        if reply.ok {
            Ok(())
        } else {
            Err(RuntimeError::network(reply.detail))
        }
    }

    pub async fn list_events(&self, session: &SessionId, limit: u32) -> Result<serde_json::Value> {
        let mut client = rpc::agent_service_client::AgentServiceClient::new(self.channel.clone());
        let reply = client
            .list_events(rpc::EventQuery {
                session_id: session.as_str().to_string(),
                limit,
                after_seq: 0,
                kinds: vec![],
            })
            .await
            .map_err(|e| RuntimeError::network(format!("list_events failed: {e}")))?
            .into_inner();
        Ok(serde_json::from_str(&reply.json).unwrap_or(serde_json::Value::Null))
    }

    pub async fn migrate(&self, session: &SessionId, target_worker: Option<String>) -> Result<serde_json::Value> {
        let mut client = rpc::agent_service_client::AgentServiceClient::new(self.channel.clone());
        let reply = client
            .migrate(rpc::MigrateRequest {
                session_id: session.as_str().to_string(),
                target_worker: target_worker.unwrap_or_default(),
            })
            .await
            .map_err(|e| RuntimeError::network(format!("migrate failed: {e}")))?
            .into_inner();
        Ok(serde_json::from_str(&reply.json).unwrap_or(serde_json::Value::Null))
    }
}