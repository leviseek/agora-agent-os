//! HTTP handlers. Thin by design: parse, call the kernel, shape the response.

use crate::error::{ApiError, ApiResult};
use crate::ApiState;
use agentos_core::error::RuntimeError;
use agentos_core::model::{EventFilter, EventKind, TaskGraphRecord, TaskRecord};
use agentos_core::{now_ms, SessionId};
use agentos_storage::store::{collections, Collection};
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------------------------
// system
// ---------------------------------------------------------------------------------------------

pub async fn healthz() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "agentos",
        "domain_version": agentos_core::DOMAIN_VERSION,
    }))
}

pub async fn readyz(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    let health = state.kernel.health().await?;
    if !health.store.ok {
        return Err(ApiError(RuntimeError::unavailable("storage is not healthy")));
    }
    Ok(Json(json!({ "status": "ready", "health": health })))
}

pub async fn meta(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    let cfg = &state.config;
    Ok(Json(json!({
        "node": cfg.node.name,
        "region": cfg.node.region,
        "domain_version": agentos_core::DOMAIN_VERSION,
        "uptime_ms": now_ms().saturating_sub(state.started_at),
        "auth_required": cfg.api.auth_required(),
        "store_backend": state.kernel.store.backend_name(),
        "blob_backend": state.kernel.blobs.backend_name(),
        "ws_path": cfg.api.ws_path,
        "grpc_addr": cfg.api.grpc_addr,
        "p2p_enabled": cfg.p2p.enabled,
        "transfer": agentos_kernel::transfer_kind(&state.kernel),
        "capabilities": state.kernel.registry.len(),
        "workers_online": state.kernel.workers.online(),
        "sessions": state.kernel.sessions.list().await?.len(),
        "limits": {
            "max_steps_per_run": cfg.policy.max_steps_per_run,
            "max_concurrent_tasks": cfg.policy.max_concurrent_tasks,
            "capability_timeout_ms": cfg.policy.capability_timeout_ms,
            "session_queue_capacity": cfg.limits.session_queue_capacity,
        },
        "workspace_root": cfg.policy.workspace_root.display().to_string(),
    })))
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub token: Option<String>,
}

pub async fn login(State(state): State<ApiState>, Json(body): Json<LoginRequest>) -> ApiResult<Json<Value>> {
    match state.config.api.auth_token() {
        None => Ok(Json(json!({
            "ok": true,
            "auth_required": false,
            "note": "no token is configured on this node; the gateway is open"
        }))),
        Some(expected) => {
            let presented = body.token.unwrap_or_default();
            if presented == expected {
                Ok(Json(json!({ "ok": true, "auth_required": true })))
            } else {
                Err(ApiError(RuntimeError::unauthorized("invalid token")))
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// sessions
// ---------------------------------------------------------------------------------------------

pub async fn list_sessions(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "sessions": state.kernel.sessions.list().await? })))
}

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

pub async fn create_session(
    State(state): State<ApiState>,
    Json(body): Json<CreateSessionRequest>,
) -> ApiResult<Json<Value>> {
    let user = body.user_id.unwrap_or_else(|| "anonymous".into());
    let title = body.title.unwrap_or_else(|| "untitled session".into());
    let record = state.kernel.sessions.create_session(&user, &title).await?;
    Ok(Json(json!(record)))
}

pub async fn get_session(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    let record = state
        .kernel
        .sessions
        .get(&session)
        .await?
        .ok_or_else(|| ApiError(RuntimeError::not_found(format!("session {id} does not exist"))))?;
    let status = state.kernel.sessions.status(&session).await.unwrap_or(Value::Null);
    Ok(Json(json!({ "session": record, "runtime": status })))
}

pub async fn close_session(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    state.kernel.sessions.close(&session).await?;
    Ok(Json(json!({ "closed": true, "session_id": id })))
}

pub async fn session_status(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    Ok(Json(state.kernel.sessions.status(&session).await?))
}

#[derive(Debug, Deserialize)]
pub struct PostMessageRequest {
    pub text: String,
    #[serde(default = "default_true")]
    pub wait: bool,
}

fn default_true() -> bool {
    true
}

pub async fn post_message(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<PostMessageRequest>,
) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    if body.text.trim().is_empty() {
        return Err(ApiError(RuntimeError::invalid_input("message text must not be empty")));
    }
    if body.wait {
        Ok(Json(state.kernel.sessions.post_goal(&session, &body.text).await?))
    } else {
        state.kernel.sessions.post_goal_async(&session, &body.text).await?;
        Ok(Json(json!({ "accepted": true, "session_id": id })))
    }
}

pub async fn cancel_session(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    let cancelled = state.kernel.sessions.cancel(&session).await?;
    Ok(Json(json!({ "cancelled": cancelled })))
}

#[derive(Debug, Deserialize)]
pub struct LimitQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub kinds: Option<String>,
}

pub async fn session_events(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(q): Query<LimitQuery>,
) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    let events = state.kernel.sessions.events(&session, q.limit.unwrap_or(200)).await?;
    Ok(Json(json!({ "events": events })))
}

pub async fn list_events(State(state): State<ApiState>, Query(q): Query<LimitQuery>) -> ApiResult<Json<Value>> {
    let kinds = q
        .kinds
        .as_deref()
        .map(|s| {
            s.split(',')
                .filter_map(parse_event_kind)
                .collect::<Vec<EventKind>>()
        })
        .unwrap_or_default();
    let filter = EventFilter {
        session_id: q.session_id.as_deref().map(|s| SessionId::from_raw(s.to_string())),
        kinds,
        limit: q.limit.unwrap_or(200),
        ..Default::default()
    };
    let events = state.kernel.bus.replay(filter).await?;
    Ok(Json(json!({ "events": events })))
}

pub async fn session_graph(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    let graphs: Collection<TaskGraphRecord> = Collection::new(collections::GRAPHS);
    let mut all = graphs.list(state.kernel.store.as_ref(), 10_000).await?;
    all.retain(|g| g.session_id == session);
    all.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(Json(json!({ "graphs": all })))
}

pub async fn list_tasks(State(state): State<ApiState>, Query(q): Query<LimitQuery>) -> ApiResult<Json<Value>> {
    let tasks: Collection<TaskRecord> = Collection::new(collections::TASKS);
    let mut all = tasks.list(state.kernel.store.as_ref(), 10_000).await?;
    if let Some(session) = &q.session_id {
        all.retain(|t| t.session_id.as_str() == session.as_str());
    }
    all.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    all.truncate(q.limit.unwrap_or(200));
    Ok(Json(json!({ "tasks": all })))
}

pub async fn session_snapshot(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    Ok(Json(serde_json::to_value(state.kernel.sessions.snapshot(&session).await?)?))
}

#[derive(Debug, Deserialize)]
pub struct RestoreRequest {
    /// Either the checkpoint itself or an envelope with a "checkpoint" field.
    #[serde(flatten)]
    pub payload: Value,
}

pub async fn session_restore(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let payload = body.get("checkpoint").cloned().unwrap_or(body);
    let checkpoint: agentos_core::model::Checkpoint = serde_json::from_value(payload)
        .map_err(|e| ApiError(RuntimeError::invalid_input(format!("invalid checkpoint payload: {e}"))))?;
    let session_id = checkpoint.meta.session_id.clone();
    let actor = state.kernel.sessions.restore(checkpoint).await?;
    Ok(Json(json!({ "restored": true, "session_id": session_id, "actor_id": actor })))
}

#[derive(Debug, Deserialize)]
pub struct MigrateRequest {
    #[serde(default)]
    pub target_worker: Option<String>,
}

pub async fn session_migrate(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<MigrateRequest>,
) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    let report = state.kernel.sessions.migrate(&session, body.target_worker).await?;
    Ok(Json(json!(report)))
}

// ---------------------------------------------------------------------------------------------
// capabilities, workers, actors, models, metrics
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CapabilityQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub tags: Option<String>,
    #[serde(default)]
    pub healthy_only: Option<bool>,
}

pub async fn list_capabilities(
    State(state): State<ApiState>,
    Query(q): Query<CapabilityQuery>,
) -> ApiResult<Json<Value>> {
    let query = agentos_capability_runtime::registry::DiscoveryQuery {
        name_contains: q.q.clone().unwrap_or_default(),
        tags: q
            .tags
            .as_deref()
            .map(|s| s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect())
            .unwrap_or_default(),
        kinds: vec![],
        healthy_only: q.healthy_only.unwrap_or(false),
    };
    let capabilities = state.kernel.registry.discover(&query);
    Ok(Json(json!({ "capabilities": capabilities })))
}

#[derive(Debug, Deserialize)]
pub struct InvokeRequest {
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

pub async fn invoke_capability(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    Json(body): Json<InvokeRequest>,
) -> ApiResult<Json<Value>> {
    let session = body
        .session_id
        .map(SessionId::from_raw)
        .unwrap_or_else(SessionId::new);
    let caller = agentos_capability_runtime::capability::CallerContext::new(session);
    let result = state
        .kernel
        .mesh
        .invoke(&name, body.version.as_deref(), body.input, caller)
        .await?;
    Ok(Json(json!({
        "capability": result.name,
        "version": result.version,
        "output": result.output,
        "duration_ms": result.duration_ms,
        "attempts": result.attempts,
        "provider": result.provider,
        "timeout_ms": body.timeout_ms,
    })))
}

pub async fn list_workers(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "workers": state.kernel.workers.list() })))
}

pub async fn list_actors(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    let live = state.kernel.actors.records();
    let directory = state.kernel.directory.list().await?;
    Ok(Json(json!({
        "actors": live,
        "directory": directory,
        "cache": {
            "hits": state.kernel.directory.cache_stats().0,
            "misses": state.kernel.directory.cache_stats().1,
            "entries": state.kernel.directory.cache_len(),
        }
    })))
}

/// Who else is running: this node plus every node the discovery plane can see.
///
/// The console uses it to offer a one-click switch to another workspace without the user having to
/// know any port. Discovery never touches the request path - this endpoint reads a cached view.
pub async fn list_nodes(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    let mut nodes: Vec<Value> = state
        .kernel
        .peers()
        .iter()
        .map(|info| {
            json!({
                "node_id": info.node_id.as_str(),
                "name": info.name,
                "address": info.address,
                "grpc": info.grpc_endpoint,
                "version": info.version,
                "capabilities": info.capabilities,
                "auth_required": info.auth_required,
                "transport": info.transport,
                "last_seen": info.last_seen,
                "age_ms": now_ms().saturating_sub(info.last_seen),
                "self": false,
            })
        })
        .collect();
    nodes.sort_by(|a, b| a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or("")));

    Ok(Json(json!({
        "self": {
            "node_id": state.kernel.config.effective_node_id(),
            "name": state.kernel.config.node.name,
            "address": state.kernel.config.api.http_addr,
            "grpc": state.kernel.config.api.grpc_addr,
            "capabilities": state.kernel.registry.list().into_iter().map(|c| c.name).collect::<Vec<_>>(),
            "auth_required": state.kernel.config.api.auth_required(),
            "self": true,
        },
        "nodes": nodes,
        "discovery": {
            "backend": state.kernel.discovery_backend(),
            "enabled": state.kernel.config.discovery.enabled,
            "advertise": state.kernel.config.discovery.advertise,
            "dir": state.kernel.config.discovery.dir.display().to_string(),
            "ttl_ms": state.kernel.config.discovery.ttl_ms,
        },
    })))
}

pub async fn list_models(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "providers": state.kernel.models.provider_infos().await,
        "configured": state.config.model_summary(),
        "default": state.config.models.default_provider,
    })))
}

pub async fn metrics(State(state): State<ApiState>) -> ApiResult<String> {
    let mut text = agentos_core::telemetry::metrics().render_prometheus();
    let health = state.kernel.health().await?;
    text.push_str(&format!("agentos_sessions {}\n", health.sessions));
    text.push_str(&format!("agentos_actors_live {}\n", health.actors_live));
    text.push_str(&format!("agentos_capabilities {}\n", health.capabilities));
    text.push_str(&format!("agentos_uptime_ms {}\n", health.uptime_ms));
    text.push_str(&format!("agentos_events_last_seq {}\n", health.events.last_seq));
    Ok(text)
}

fn parse_session(id: &str) -> ApiResult<SessionId> {
    SessionId::parse(id).map_err(|e| ApiError(RuntimeError::invalid_input(e.to_string())))
}

fn parse_event_kind(kind: &str) -> Option<EventKind> {
    let all = [
        EventKind::SessionCreated,
        EventKind::SessionClosed,
        EventKind::SessionMessageQueued,
        EventKind::SessionMessageHandled,
        EventKind::ActorSpawned,
        EventKind::ActorStopped,
        EventKind::ActorRestarted,
        EventKind::ActorMigrated,
        EventKind::ActorCloned,
        EventKind::SnapshotCreated,
        EventKind::SnapshotRestored,
        EventKind::EventReplayed,
        EventKind::RunCreated,
        EventKind::RunCompleted,
        EventKind::RunFailed,
        EventKind::AgentStep,
        EventKind::ModelCall,
        EventKind::ModelResult,
        EventKind::ToolCall,
        EventKind::ToolResult,
        EventKind::TaskCreated,
        EventKind::TaskQueued,
        EventKind::TaskStarted,
        EventKind::TaskRetrying,
        EventKind::TaskCompleted,
        EventKind::TaskFailed,
        EventKind::TaskCancelled,
        EventKind::CapabilityRegistered,
        EventKind::CapabilityInvoked,
        EventKind::CapabilityDenied,
        EventKind::WorkerRegistered,
        EventKind::WorkerHeartbeat,
        EventKind::WorkerOffline,
        EventKind::ArtifactCreated,
        EventKind::MemoryWritten,
        EventKind::PolicyDenied,
        EventKind::NodeDiscovered,
        EventKind::NodeLost,
        EventKind::Error,
    ];
    all.into_iter().find(|k| k.as_str() == kind)
}
