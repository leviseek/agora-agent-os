//! HTTP handlers. Thin by design: parse, call the kernel, shape the response.

use crate::error::{ApiError, ApiResult};
use crate::ApiState;
use agentos_core::error::RuntimeError;
use crate::middleware::{IdentitySource, Principal};
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
        // Which identity mode this runtime is in, because it decides what ownership can mean. With no
        // principal table, every request is the same operator with admin rights: a session's owner is
        // recorded, and it separates nobody. A console that does not say so leaves two people
        // wondering why they can both read each other's conversations.
        "identity": {
            "mode": if cfg.api.principals.is_empty() { "single-principal" } else { "principals" },
            "principals": cfg.api.principals.len(),
            // Whether this node proves identity at all. False is the open case, and the only one
            // where a declared name is considered.
            "authenticated": cfg.api.has_authenticated_identities(),
            // What a declared name does here: a mode on an open node, and "ignored" wherever a token
            // exists - because there it would be a way around the token.
            "asserted": if cfg.api.has_authenticated_identities() {
                "ignored"
            } else {
                cfg.api.asserted_identity.as_str()
            },
            "separation": if cfg.api.has_authenticated_identities() {
                "each request presents a configured token: ownership and grants separate people"
            } else {
                match cfg.api.asserted_identity {
                    agentos_core::config::AssertedIdentityMode::Off =>
                        "every request is the same operator: ownership is recorded but separates nobody",
                    agentos_core::config::AssertedIdentityMode::Optional =>
                        "a caller may declare who it is; a request that declares nothing is the operator",
                    agentos_core::config::AssertedIdentityMode::Required =>
                        "a caller must declare who it is; the name is declared, not authenticated",
                }
            },
        },
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
        // What this binary is, so "am I running the fixed build?" is answerable without guessing.
        "build": build_info(),
    })))
}

/// The running binary's identity and the behaviours it is known to have.
///
/// A console that keeps calling a route an older runtime does not serve, or a fix that is written
/// but not built, both look like "it is broken again" from the outside. Reporting the build makes
/// that question answerable from one request.
fn build_info() -> Value {
    let exe = std::env::current_exe().ok();
    let (path, built_at) = match &exe {
        Some(path) => {
            let built_at = std::fs::metadata(path)
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|since| since.as_millis() as u64);
            (Some(path.display().to_string()), built_at)
        }
        None => (None, None),
    };
    json!({
        "id": option_env!("AGENTOS_BUILD_ID").unwrap_or("unset"),
        "exe": path,
        "built_at": built_at,
        "features": agentos_core::FEATURES,
    })
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

/// List sessions, optionally filtered by a search term.
///
/// The filter is a case-insensitive substring over the title and the user id. It runs here rather
/// than in the store because the store's contract is "list what exists", and a listing this small
/// costs nothing to filter - a query language can come later without moving the seam.
pub async fn list_sessions(
    State(state): State<ApiState>,
    principal: Option<Principal>,
    Query(q): Query<SessionListQuery>,
) -> ApiResult<Json<Value>> {
    let mut sessions = state.kernel.sessions.list().await?;
    let query = q.q.as_deref().map(str::trim).filter(|term| !term.is_empty());
    if let Some(term) = query {
        let needle = term.to_lowercase();
        sessions.retain(|session| {
            session.title.to_lowercase().contains(&needle)
                || session.user_id.to_lowercase().contains(&needle)
        });
    }
    // Every row carries the caller's own role, resolved from the record. The list itself stays open:
    // knowing that a conversation exists is not the same as being allowed into it, and a stranger
    // who cannot see what they might ask for access to has nothing to ask about.
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let mut mine = Vec::with_capacity(sessions.len());
    for session in sessions {
        let role = match state.kernel.sessions.get(&session.id).await? {
            Some(record) => state.kernel.sessions.role_on(&record, &me).await.unwrap_or(None),
            None => None,
        };
        let mut value = serde_json::to_value(&session)?;
        if let Some(object) = value.as_object_mut() {
            object.insert("my_role".into(), json!(role));
        }
        mine.push(value);
    }
    let total = mine.len();
    Ok(Json(json!({ "sessions": mine, "total": total, "query": query })))
}

#[derive(Debug, Deserialize)]
pub struct SessionListQuery {
    pub q: Option<String>,
}

/// Parse a thinking effort from a client, rejecting what we do not understand.
fn parse_effort(raw: Option<&str>) -> ApiResult<Option<agentos_core::model::ReasoningEffort>> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    agentos_core::model::ReasoningEffort::parse(raw)
        .map(Some)
        .ok_or_else(|| {
            ApiError(RuntimeError::invalid_input(format!(
                "unknown effort {raw:?}: use off, low, medium or high"
            )))
        })
}

/// Change a session's settings: its title, the provider it prefers, and its thinking effort.
///
/// Every field is optional and an absent field means "leave it alone". An empty string clears it,
/// which is how a client goes back to letting the router decide.
pub async fn configure_session(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    let title = body.get("title").and_then(|v| v.as_str()).map(|v| v.to_string());
    let model = body.get("model").and_then(|v| v.as_str()).map(|v| v.to_string());
    let effort = match body.get("effort") {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) => {
            if raw.trim().is_empty() {
                // Clearing the effort means "off", which the actor stores as no preference.
                Some(agentos_core::model::ReasoningEffort::Off)
            } else {
                Some(parse_effort(Some(raw))?.ok_or_else(|| {
                    ApiError(RuntimeError::invalid_input("effort must be a string"))
                })?)
            }
        }
        Some(_) => {
            return Err(ApiError(RuntimeError::invalid_input(
                "effort must be a string: off, low, medium or high",
            )))
        }
    };
    if title.is_none() && model.is_none() && effort.is_none() {
        return Err(ApiError(RuntimeError::invalid_input(
            "nothing to change: send title, model or effort",
        )));
    }
    let record = state.kernel.sessions.configure(&session, title, model, effort).await?;
    Ok(Json(json!({ "session": record })))
}

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// The workspace to create the session in. Omitted, the caller's own default workspace is used,
    /// created on first use - so every session has a workspace without a client having to know it.
    #[serde(default)]
    pub workspace_id: Option<String>,
}

pub async fn create_session(
    State(state): State<ApiState>,
    principal: Option<Principal>,
    Json(body): Json<CreateSessionRequest>,
) -> ApiResult<Json<Value>> {
    // The owner is who is asking. A body that names someone else is honoured only for an admin:
    // otherwise "create a session as someone else" would be a way to take over their conversations.
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let (user, owner) = match body.user_id.clone() {
        Some(requested) if requested != me.user_id => {
            if !me.is_admin() {
                return Err(ApiError(
                    RuntimeError::policy_denied(format!(
                        "{} may not create sessions for {requested}",
                        me.user_id
                    ))
                    .with_detail("user_id", me.user_id.clone()),
                ));
            }
            // Creating on somebody's behalf makes them the owner. Recording them as a label and the
            // caller as the owner was a record that disagreed with itself: the list said one name and
            // the permission table said another, and the person it was created for could not be
            // granted anything without going through the caller first.
            (requested.clone(), agentos_core::model::PrincipalRef::new(requested, None))
        }
        _ => (me.user_id.clone(), me.as_ref()),
    };
    let title = body.title.unwrap_or_else(|| "untitled session".into());
    // Every session belongs to a workspace (D20). One may be named; without one the caller gets
    // their own default workspace, so "this is mine" and "this is in a workspace someone owns" are
    // the same statement and no session is an orphan.
    let workspace = match body.workspace_id.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
        Some(raw) => {
            let workspace_id = parse_workspace(raw)?;
            let record = state.kernel.sessions.get_workspace(&workspace_id).await?.ok_or_else(|| {
                ApiError(RuntimeError::not_found(format!("workspace {raw} does not exist")))
            })?;
            // Creating a session in someone else's workspace is speaking in it, so the same action
            // that lets a participant chat lets them start a conversation.
            if !me.is_admin() {
                let allowed = agentos_core::model::workspace_role(&record, &me)
                    .map(|role| {
                        agentos_core::model::role_allows(
                            role,
                            agentos_core::model::SessionAction::Chat,
                        )
                    })
                    .unwrap_or(false);
                if !allowed {
                    return Err(ApiError(
                        RuntimeError::policy_denied(format!(
                            "{} may not create sessions in workspace {}",
                            me.user_id, record.name
                        ))
                        .with_detail("workspace_id", workspace_id.as_str())
                        .with_detail("user_id", me.user_id.clone()),
                    ));
                }
            }
            workspace_id
        }
        // No workspace named: the session goes into the default workspace of whoever it is *for*.
        // That is what keeps "create one for bob" meaning bob owns it - an admin doing this on
        // somebody's behalf must not quietly make their own workspace the owner. The node is the
        // caller's, because that is the node this request is happening on.
        None => state
            .kernel
            .sessions
            .ensure_default_workspace(&agentos_core::model::Principal::new(
                user.clone(),
                me.node_id.clone(),
                Vec::new(),
            ))
            .await?
            .id,
    };
    let record = state
        .kernel
        .sessions
        .create_session_in(&workspace, &user, &title, Some(owner))
        .await?;
    Ok(Json(json!(record)))
}

pub async fn get_session(
    State(state): State<ApiState>,
    principal: Option<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    let record = state
        .kernel
        .sessions
        .get(&session)
        .await?
        .ok_or_else(|| ApiError(RuntimeError::not_found(format!("session {id} does not exist"))))?;
    let status = state.kernel.sessions.status(&session).await.unwrap_or(Value::Null);
    // The caller's own role, so a console can show what this person may do here instead of
    // repeating the permission table in TypeScript, where it would drift from the Rust one. Read
    // through the workspace (D20): a role there is held in every session of it.
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let workspace = state.kernel.sessions.workspace_of(&record).await?;
    let my_role = agentos_core::model::effective_role(&record, workspace.as_ref(), &me);
    let my_actions: Vec<&'static str> = ALL_SESSION_ACTIONS
        .iter()
        .filter(|action| match my_role {
            Some(role) => agentos_core::model::role_allows(role, **action),
            // No role at all: an admin still gets the list, everyone else gets nothing, which is
            // what the gateway will enforce anyway.
            None => me.is_admin(),
        })
        .map(|action| action.as_str())
        .collect();
    Ok(Json(json!({
        "session": record,
        "runtime": status,
        "you": {
            "user_id": me.user_id,
            "node_id": me.node_id,
            "roles": me.roles,
            "session_role": my_role,
            "workspace_id": workspace.as_ref().map(|workspace| workspace.id.as_str().to_string()),
            "can": my_actions,
        },
    })))
}

pub async fn close_session(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    state.kernel.sessions.close(&session).await?;
    Ok(Json(json!({ "closed": true, "session_id": id })))
}

/// Open a closed session again. The other half of close, and not a restore: the conversation, its
/// runs and its transcript were never gone - only the actor that held them.
pub async fn open_session(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    let record = state.kernel.sessions.open(&session).await?;
    Ok(Json(json!({ "opened": true, "session": record })))
}

#[derive(Debug, Deserialize)]
pub struct AccessRequest {
    pub user_id: String,
    #[serde(default)]
    pub node_id: Option<String>,
    /// owner, editor, participant or viewer.
    pub role: String,
}

/// Hand out a role on a session.
///
/// Since D20 access is decided at the workspace, so a session with one delegates: the grant lands on
/// the workspace and is felt by every session of it. The response keeps `session` for a client that
/// asked about a session and adds the workspace that actually changed, so nothing has to guess which
/// one moved. A session with no workspace (a record written before workspaces existed) still takes a
/// session-level grant.
pub async fn grant_access(
    State(state): State<ApiState>,
    principal: Option<Principal>,
    Path(id): Path<String>,
    Json(body): Json<AccessRequest>,
) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    let role = parse_role(&body.role)?;
    if body.user_id.trim().is_empty() {
        return Err(ApiError(RuntimeError::invalid_input("user_id must not be empty")));
    }
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let grant = agentos_core::model::SessionGrant::new(body.user_id.trim(), body.node_id, role);
    let record = state
        .kernel
        .sessions
        .get(&session)
        .await?
        .ok_or_else(|| ApiError(RuntimeError::not_found(format!("session {id} does not exist"))))?;
    match state.kernel.sessions.workspace_of(&record).await? {
        Some(workspace) => {
            let workspace = state
                .kernel
                .sessions
                .grant_workspace(&workspace.id, grant, &me)
                .await?;
            Ok(Json(json!({
                "session": record,
                "workspace": workspace,
                "scope": "workspace",
            })))
        }
        None => {
            let record = state.kernel.sessions.grant(&session, grant, &me).await?;
            Ok(Json(json!({ "session": record, "scope": "session" })))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RevokeRequest {
    pub user_id: String,
    #[serde(default)]
    pub node_id: Option<String>,
}

/// Take a role away again. The workspace's copy when the session has a workspace, for the same
/// reason as `grant_access`.
pub async fn revoke_access(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<RevokeRequest>,
) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    let who = agentos_core::model::PrincipalRef::new(body.user_id.trim(), body.node_id);
    let record = state
        .kernel
        .sessions
        .get(&session)
        .await?
        .ok_or_else(|| ApiError(RuntimeError::not_found(format!("session {id} does not exist"))))?;
    match state.kernel.sessions.workspace_of(&record).await? {
        Some(workspace) => {
            let workspace = state.kernel.sessions.revoke_workspace(&workspace.id, &who).await?;
            Ok(Json(json!({
                "session": record,
                "workspace": workspace,
                "scope": "workspace",
            })))
        }
        None => {
            let record = state.kernel.sessions.revoke(&session, &who).await?;
            Ok(Json(json!({ "session": record, "scope": "session" })))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// workspaces
// ---------------------------------------------------------------------------------------------
//
// The unit of ownership, sharing and filesystem isolation (docs/decisions.md D20). Access is decided
// here and inherited by every session of the workspace, which is why the session routes below
// delegate: two places that write a role are two places that can disagree about it.

#[derive(Debug, Deserialize)]
pub struct CreateWorkspaceRequest {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct RenameWorkspaceRequest {
    pub name: String,
}

/// A workspace as the console needs it: the record, plus what this caller may do in it.
fn workspace_view(
    record: &agentos_core::model::WorkspaceRecord,
    me: &agentos_core::model::Principal,
) -> Value {
    let role = agentos_core::model::workspace_role(record, me);
    let can: Vec<&'static str> = ALL_SESSION_ACTIONS
        .iter()
        .filter(|action| match role {
            Some(role) => agentos_core::model::role_allows(role, **action),
            None => me.is_admin(),
        })
        .map(|action| action.as_str())
        .collect();
    json!({ "workspace": record, "workspace_role": role, "can": can })
}

pub async fn list_workspaces(
    State(state): State<ApiState>,
    principal: Option<Principal>,
) -> ApiResult<Json<Value>> {
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let workspaces = state.kernel.sessions.list_workspaces().await?;
    let sessions = state.kernel.sessions.list().await?;
    let session_count = |workspace_id: &agentos_core::WorkspaceId| {
        sessions
            .iter()
            .filter(|session| session.workspace_id.as_ref() == Some(workspace_id))
            .count()
    };

    // Two lists, and the split is the privacy boundary. `workspaces` are the ones this caller has a
    // role in: the full record, including who else is a member and who has asked. `discoverable` is
    // an index of the rest - a name, an owner and a session count - because a person cannot ask for
    // access to something they cannot name. Neither list is a permission: every read, write and
    // membership change is still decided by the guard.
    let mut mine: Vec<Value> = Vec::new();
    let mut discoverable: Vec<Value> = Vec::new();
    for record in &workspaces {
        let role = agentos_core::model::workspace_role(record, &me);
        if role.is_some() || me.is_admin() {
            let mut view = workspace_view(record, &me);
            view["session_count"] = json!(session_count(&record.id));
            mine.push(view);
        } else {
            discoverable.push(json!({
                "id": record.id,
                "name": record.name,
                "owner": record.owner,
                "created_at": record.created_at,
                "session_count": session_count(&record.id),
            }));
        }
    }
    Ok(Json(json!({
        "workspaces": mine,
        "discoverable": discoverable,
        "total": mine.len(),
    })))
}

pub async fn create_workspace(
    State(state): State<ApiState>,
    principal: Option<Principal>,
    Json(body): Json<CreateWorkspaceRequest>,
) -> ApiResult<Json<Value>> {
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError(RuntimeError::invalid_input("a workspace name must not be empty")));
    }
    let record = state.kernel.sessions.create_workspace(name, &me).await?;
    Ok(Json(json!({ "workspace": record })))
}

pub async fn get_workspace(
    State(state): State<ApiState>,
    principal: Option<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let workspace = parse_workspace(&id)?;
    let record = state
        .kernel
        .sessions
        .get_workspace(&workspace)
        .await?
        .ok_or_else(|| ApiError(RuntimeError::not_found(format!("workspace {id} does not exist"))))?;
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    // Every session in this workspace carries the workspace role: with a workspace, a per-session
    // reading of access no longer exists.
    let role = agentos_core::model::workspace_role(&record, &me);
    let sessions: Vec<Value> = state
        .kernel
        .sessions
        .list()
        .await?
        .into_iter()
        .filter(|session| session.workspace_id.as_ref() == Some(&record.id))
        .map(|session| {
            let mut view = serde_json::to_value(&session).unwrap_or(Value::Null);
            if let Some(object) = view.as_object_mut() {
                object.insert("my_role".into(), json!(role));
            }
            view
        })
        .collect();
    let mut view = workspace_view(&record, &me);
    view["sessions"] = json!(sessions);
    Ok(Json(view))
}

pub async fn rename_workspace(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<RenameWorkspaceRequest>,
) -> ApiResult<Json<Value>> {
    let workspace = parse_workspace(&id)?;
    let record = state.kernel.sessions.rename_workspace(&workspace, &body.name).await?;
    Ok(Json(json!({ "workspace": record })))
}

/// The sessions of one workspace, newest first.
pub async fn list_workspace_sessions(
    State(state): State<ApiState>,
    principal: Option<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let workspace = parse_workspace(&id)?;
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let role = state
        .kernel
        .sessions
        .get_workspace(&workspace)
        .await?
        .and_then(|record| agentos_core::model::workspace_role(&record, &me));
    let sessions: Vec<Value> = state
        .kernel
        .sessions
        .list()
        .await?
        .into_iter()
        .filter(|session| session.workspace_id.as_ref() == Some(&workspace))
        .map(|session| {
            let mut view = serde_json::to_value(&session).unwrap_or(Value::Null);
            if let Some(object) = view.as_object_mut() {
                // Every session of a workspace carries the workspace role: there is no per-session
                // reading of access any more, and saying otherwise in two columns invites drift.
                object.insert("my_role".into(), json!(role));
            }
            view
        })
        .collect();
    Ok(Json(json!({ "workspace_id": id, "sessions": sessions, "total": sessions.len() })))
}

/// Create a conversation inside a workspace. The owner of the session is the workspace's owner.
pub async fn create_workspace_session(
    State(state): State<ApiState>,
    principal: Option<Principal>,
    Path(id): Path<String>,
    Json(body): Json<CreateSessionRequest>,
) -> ApiResult<Json<Value>> {
    let workspace = parse_workspace(&id)?;
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let record = state
        .kernel
        .sessions
        .get_workspace(&workspace)
        .await?
        .ok_or_else(|| ApiError(RuntimeError::not_found(format!("workspace {id} does not exist"))))?;
    let user = body.user_id.unwrap_or_else(|| me.user_id.clone());
    let title = body.title.unwrap_or_else(|| "untitled session".into());
    let session = state
        .kernel
        .sessions
        .create_session_in(&workspace, &user, &title, Some(record.owner.clone()))
        .await?;
    Ok(Json(json!(session)))
}

/// Grant a role on a workspace: in force in every session of it, now and later.
pub async fn grant_workspace_access(
    State(state): State<ApiState>,
    principal: Option<Principal>,
    Path(id): Path<String>,
    Json(body): Json<AccessRequest>,
) -> ApiResult<Json<Value>> {
    let workspace = parse_workspace(&id)?;
    let role = parse_role(&body.role)?;
    if body.user_id.trim().is_empty() {
        return Err(ApiError(RuntimeError::invalid_input("user_id must not be empty")));
    }
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let grant = agentos_core::model::SessionGrant::new(body.user_id.trim(), body.node_id, role);
    let record = state.kernel.sessions.grant_workspace(&workspace, grant, &me).await?;
    Ok(Json(json!({ "workspace": record })))
}

pub async fn revoke_workspace_access(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<RevokeRequest>,
) -> ApiResult<Json<Value>> {
    let workspace = parse_workspace(&id)?;
    let who = agentos_core::model::PrincipalRef::new(body.user_id.trim(), body.node_id);
    let record = state.kernel.sessions.revoke_workspace(&workspace, &who).await?;
    Ok(Json(json!({ "workspace": record })))
}

/// What this workspace narrowed, next to what the runtime offers.
pub async fn workspace_capabilities(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let workspace = parse_workspace(&id)?;
    let record = state
        .kernel
        .sessions
        .get_workspace(&workspace)
        .await?
        .ok_or_else(|| ApiError(RuntimeError::not_found(format!("workspace {id} does not exist"))))?;
    let registered: Vec<String> = state
        .kernel
        .registry
        .list()
        .into_iter()
        .map(|descriptor| descriptor.name)
        .collect();
    let effective: Vec<String> = registered
        .iter()
        .filter(|name| record.capabilities.permits(name).is_ok())
        .cloned()
        .collect();
    Ok(Json(json!({
        "workspace_id": id,
        "runtime": registered,
        "workspace": record.capabilities,
        "effective": effective,
    })))
}

pub async fn set_workspace_capabilities(
    State(state): State<ApiState>,
    principal: Option<Principal>,
    Path(id): Path<String>,
    Json(body): Json<CapabilitiesRequest>,
) -> ApiResult<Json<Value>> {
    let workspace = parse_workspace(&id)?;
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let current = state
        .kernel
        .sessions
        .get_workspace(&workspace)
        .await?
        .map(|record| record.capabilities)
        .unwrap_or_default();
    let next = agentos_core::model::SessionCapabilities {
        allow: match body.allow {
            Some(value) => value,
            None => current.allow,
        },
        deny: body.deny,
        approval_required: body.approval_required,
    };
    let known: Vec<String> = state
        .kernel
        .registry
        .list()
        .into_iter()
        .map(|descriptor| descriptor.name)
        .collect();
    let record = state
        .kernel
        .sessions
        .set_workspace_capabilities(&workspace, next, &known, &me)
        .await?;
    Ok(Json(json!({ "workspace": record })))
}

/// The pending requests for a workspace. Its owner sees all of them; anyone else sees their own.
pub async fn list_workspace_access(
    State(state): State<ApiState>,
    principal: Option<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let workspace = parse_workspace(&id)?;
    let record = state
        .kernel
        .sessions
        .get_workspace(&workspace)
        .await?
        .ok_or_else(|| ApiError(RuntimeError::not_found(format!("workspace {id} does not exist"))))?;
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let may_decide = may_grant_on_workspace(&record, &me);
    let requests: Vec<&agentos_core::model::SessionAccessRequest> = if may_decide {
        record.access_requests.iter().collect()
    } else {
        record
            .access_requests
            .iter()
            .filter(|request| request.principal.matches(&me.as_ref()))
            .collect()
    };
    Ok(Json(json!({
        "workspace_id": id,
        "may_decide": may_decide,
        "requests": requests,
    })))
}

/// Ask the owner for access to a workspace. Open to anyone the gateway authenticated: asking is
/// the point.
pub async fn request_workspace_access(
    State(state): State<ApiState>,
    principal: Option<Principal>,
    Path(id): Path<String>,
    Json(body): Json<AccessRequestBody>,
) -> ApiResult<Json<Value>> {
    let workspace = parse_workspace(&id)?;
    let role = parse_role(&body.role)?;
    if role == agentos_core::model::SessionRole::Owner {
        return Err(ApiError(RuntimeError::invalid_input(
            "ownership is not granted on request: ask for editor, participant or viewer",
        )));
    }
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let request = state
        .kernel
        .sessions
        .request_workspace_access(&workspace, &me, role, body.note)
        .await?;
    Ok(Json(json!({ "request": request })))
}

pub async fn decide_workspace_access(
    State(state): State<ApiState>,
    principal: Option<Principal>,
    Path((id, request_id)): Path<(String, String)>,
    Json(body): Json<DecisionBody>,
) -> ApiResult<Json<Value>> {
    let workspace = parse_workspace(&id)?;
    let role = match body.role.as_deref() {
        Some(raw) => Some(parse_role(raw)?),
        None => None,
    };
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let (record, decided) = state
        .kernel
        .sessions
        .decide_workspace_access_request(&workspace, &request_id, body.approve, role, &me)
        .await?;
    Ok(Json(json!({ "request": decided, "workspace": record })))
}

// ---------------------------------------------------------------------------------------------
// archiving
// ---------------------------------------------------------------------------------------------

/// Write a conversation into an archive package. Closing happens on the way if it is still open.
pub async fn archive_session(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    let bundle = state.kernel.sessions.archive(&session).await?;
    Ok(Json(json!({
        "archived": true,
        "archive_id": bundle.id,
        "path": bundle.path,
        "bytes": bundle.bytes,
        "manifest": bundle.manifest,
    })))
}

/// Every package under this node's archive root.
pub async fn list_archives(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    let archives = state.kernel.sessions.list_archives().await?;
    Ok(Json(json!({
        "root": state.config.storage.archive_dir.display().to_string(),
        "enabled": state.config.storage.archive_enabled,
        "archives": archives,
    })))
}

/// One package: its manifest, and the first turns of what was said.
pub async fn get_archive(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(state.kernel.sessions.preview_archive(&id).await?))
}

#[derive(Debug, Deserialize, Default)]
pub struct RestoreArchiveRequest {
    /// The name for the restored conversation. Defaults to the archived title plus "(restored)".
    #[serde(default)]
    pub title: Option<String>,
}

/// Bring a conversation back from a package, as a new session.
///
/// A new session on purpose: restoring over the original id would merge a conversation from an old
/// package with whatever the live session has become since.
pub async fn restore_archive(
    State(state): State<ApiState>,
    principal: Option<Principal>,
    Path(id): Path<String>,
    body: Option<Json<RestoreArchiveRequest>>,
) -> ApiResult<Json<Value>> {
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let entry = state.kernel.sessions.find_archive(&id).await?;
    // Who may bring a conversation back: an admin, or the person who owned it. The package is on
    // this node's disk, so the check is about the conversation, not about reachability.
    let owner = entry.manifest.owner.clone();
    if !me.is_admin() {
        let is_owner = owner
            .as_ref()
            .map(|owner| owner.matches(&me.as_ref()))
            .unwrap_or(false);
        if !is_owner {
            return Err(ApiError(
                RuntimeError::policy_denied(format!(
                    "archive {id} belongs to {}: ask them to restore it",
                    owner.map(|owner| owner.to_string()).unwrap_or_else(|| "nobody".into())
                ))
                .with_detail("archive_id", id.clone()),
            ));
        }
    }
    let title = body.and_then(|Json(body)| body.title);
    let record = state.kernel.sessions.restore_archive(&id, title, None).await?;
    Ok(Json(json!({ "restored": true, "session": record, "from_archive": id })))
}

/// Delete a package. Only the file: the record keeps saying the conversation exists elsewhere.
pub async fn delete_archive(
    State(state): State<ApiState>,
    principal: Option<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let entry = state.kernel.sessions.find_archive(&id).await?;
    if !me.is_admin() {
        let is_owner = entry
            .manifest
            .owner
            .as_ref()
            .map(|owner| owner.matches(&me.as_ref()))
            .unwrap_or(false);
        if !is_owner {
            return Err(ApiError(
                RuntimeError::policy_denied(format!("archive {id} is not yours to delete"))
                    .with_detail("archive_id", id.clone()),
            ));
        }
    }
    state.kernel.sessions.delete_archive(&id).await?;
    Ok(Json(json!({ "deleted": true, "archive_id": id })))
}
// ---------------------------------------------------------------------------------------------
// session capabilities and access requests
// ---------------------------------------------------------------------------------------------

/// What this session may use, next to what the runtime offers.
///
/// The three lists answer three different questions and are kept apart on purpose: `runtime` is
/// what this node has registered, `session` is the narrowing its owner wrote, and `effective` is
/// what a call would actually meet.
pub async fn session_capabilities(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    let record = state
        .kernel
        .sessions
        .get(&session)
        .await?
        .ok_or_else(|| ApiError(RuntimeError::not_found(format!("session {id} does not exist"))))?;
    let registered: Vec<String> = state
        .kernel
        .registry
        .list()
        .into_iter()
        .map(|descriptor| descriptor.name)
        .collect();
    // Narrowing lives on the workspace when the session has one (D20): sharing a working unit shares
    // its abilities. `scope` says which record answered, so a caller never has to guess which one to
    // write to when it wants a change.
    let workspace = state.kernel.sessions.workspace_of(&record).await?;
    let narrowing = match &workspace {
        Some(workspace) => workspace.capabilities.clone(),
        None => record.capabilities.clone(),
    };
    let effective: Vec<String> = registered
        .iter()
        .filter(|name| narrowing.permits(name).is_ok())
        .cloned()
        .collect();
    Ok(Json(json!({
        "session_id": id,
        "scope": if workspace.is_some() { "workspace" } else { "session" },
        "workspace_id": workspace.as_ref().map(|workspace| workspace.id.as_str().to_string()),
        "runtime": registered,
        // The narrowing in force for this session, whichever record holds it. The key name predates
        // workspaces; `scope` is what a client should branch on.
        "session": narrowing,
        "effective": effective,
    })))
}

#[derive(Debug, Deserialize)]
pub struct CapabilitiesRequest {
    /// null clears the allow list (back to "whatever the runtime allows").
    #[serde(default, deserialize_with = "double_option")]
    pub allow: Option<Option<Vec<String>>>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub approval_required: Vec<String>,
}

/// Distinguishes "absent" from "null": absent keeps the current list, null clears it.
fn double_option<'de, D, T>(deserializer: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    serde::Deserialize::deserialize(deserializer).map(Some)
}

/// Narrow what a session may use. Only the owner (or an admin) gets here: the gateway checked.
pub async fn set_session_capabilities(
    State(state): State<ApiState>,
    principal: Option<Principal>,
    Path(id): Path<String>,
    Json(body): Json<CapabilitiesRequest>,
) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let record = state
        .kernel
        .sessions
        .get(&session)
        .await?
        .ok_or_else(|| ApiError(RuntimeError::not_found(format!("session {id} does not exist"))))?;
    let workspace = state.kernel.sessions.workspace_of(&record).await?;
    // Read what is in force before writing, so an omitted `allow` means "keep the current list"
    // rather than "clear it" - and read it from the same record the write will go to.
    let current = match &workspace {
        Some(workspace) => workspace.capabilities.clone(),
        None => record.capabilities.clone(),
    };
    let next = agentos_core::model::SessionCapabilities {
        allow: match body.allow {
            Some(value) => value,
            None => current.allow,
        },
        deny: body.deny,
        approval_required: body.approval_required,
    };
    let known: Vec<String> = state
        .kernel
        .registry
        .list()
        .into_iter()
        .map(|descriptor| descriptor.name)
        .collect();
    match workspace {
        Some(workspace) => {
            let changed = state
                .kernel
                .sessions
                .set_workspace_capabilities(&workspace.id, next, &known, &me)
                .await?;
            Ok(Json(json!({
                "session": record,
                "workspace": changed,
                "scope": "workspace",
            })))
        }
        None => {
            let record = state
                .kernel
                .sessions
                .set_capabilities(&session, next, &known, &me)
                .await?;
            Ok(Json(json!({ "session": record, "scope": "session" })))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct AccessRequestBody {
    /// owner, editor, participant or viewer.
    pub role: String,
    #[serde(default)]
    pub note: Option<String>,
}

/// Ask the owner for access. Open to anyone the gateway authenticated: asking is the point.
///
/// A session with a workspace asks for the workspace (D20): a role is held in every session of it,
/// so that is the thing to be granted. A legacy session with none still takes a session request.
pub async fn request_session_access(
    State(state): State<ApiState>,
    principal: Option<Principal>,
    Path(id): Path<String>,
    Json(body): Json<AccessRequestBody>,
) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    let role = parse_role(&body.role)?;
    if role == agentos_core::model::SessionRole::Owner {
        // Ownership is not handed out by asking for it. Transfer is a decision, not a request.
        return Err(ApiError(RuntimeError::invalid_input(
            "ownership is not granted on request: ask for editor, participant or viewer",
        )));
    }
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let record = state
        .kernel
        .sessions
        .get(&session)
        .await?
        .ok_or_else(|| ApiError(RuntimeError::not_found(format!("session {id} does not exist"))))?;
    match state.kernel.sessions.workspace_of(&record).await? {
        Some(workspace) => {
            let request = state
                .kernel
                .sessions
                .request_workspace_access(&workspace.id, &me, role, body.note)
                .await?;
            Ok(Json(json!({
                "request": request,
                "workspace_id": workspace.id.as_str(),
                "scope": "workspace",
            })))
        }
        None => {
            let request = state
                .kernel
                .sessions
                .request_access(&session, &me, role, body.note)
                .await?;
            Ok(Json(json!({ "request": request, "scope": "session" })))
        }
    }
}

/// The pending requests that govern a session. The owner sees all of them; anyone else sees their own.
pub async fn list_session_access(
    State(state): State<ApiState>,
    principal: Option<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    let record = state
        .kernel
        .sessions
        .get(&session)
        .await?
        .ok_or_else(|| ApiError(RuntimeError::not_found(format!("session {id} does not exist"))))?;
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let workspace = state.kernel.sessions.workspace_of(&record).await?;
    if let Some(workspace) = workspace {
        let may_decide = may_grant_on_workspace(&workspace, &me);
        // A requester sees their own request and nothing else: the list of who else wants in is the
        // owner's business.
        let requests: Vec<&agentos_core::model::SessionAccessRequest> = if may_decide {
            workspace.access_requests.iter().collect()
        } else {
            workspace
                .access_requests
                .iter()
                .filter(|request| request.principal.matches(&me.as_ref()))
                .collect()
        };
        return Ok(Json(json!({
            "session_id": id,
            "workspace_id": workspace.id.as_str(),
            "scope": "workspace",
            "may_decide": may_decide,
            "requests": requests,
        })));
    }
    let mine = agentos_core::model::role_of(&record, &me);
    let may_decide = me.is_admin()
        || mine
            .map(|role| agentos_core::model::role_allows(role, agentos_core::model::SessionAction::Grant))
            .unwrap_or(false);
    let requests: Vec<&agentos_core::model::SessionAccessRequest> = if may_decide {
        record.access_requests.iter().collect()
    } else {
        record
            .access_requests
            .iter()
            .filter(|request| request.principal.matches(&me.as_ref()))
            .collect()
    };
    Ok(Json(json!({
        "session_id": id,
        "scope": "session",
        "may_decide": may_decide,
        "requests": requests,
    })))
}

#[derive(Debug, Deserialize)]
pub struct DecisionBody {
    pub approve: bool,
    /// What to hand out, when it differs from what was asked for.
    #[serde(default)]
    pub role: Option<String>,
}

/// Approve or reject a request. Approval hands out the role in the same step.
pub async fn decide_session_access(
    State(state): State<ApiState>,
    principal: Option<Principal>,
    Path((id, request_id)): Path<(String, String)>,
    Json(body): Json<DecisionBody>,
) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    let role = match body.role.as_deref() {
        Some(raw) => Some(parse_role(raw)?),
        None => None,
    };
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let record = state
        .kernel
        .sessions
        .get(&session)
        .await?
        .ok_or_else(|| ApiError(RuntimeError::not_found(format!("session {id} does not exist"))))?;
    match state.kernel.sessions.workspace_of(&record).await? {
        Some(workspace) => {
            let (workspace, decided) = state
                .kernel
                .sessions
                .decide_workspace_access_request(&workspace.id, &request_id, body.approve, role, &me)
                .await?;
            Ok(Json(json!({
                "request": decided,
                "session": record,
                "workspace": workspace,
                "scope": "workspace",
            })))
        }
        None => {
            let (record, decided) = state
                .kernel
                .sessions
                .decide_access_request(&session, &request_id, body.approve, role, &me)
                .await?;
            Ok(Json(json!({ "request": decided, "session": record, "scope": "session" })))
        }
    }
}
/// Everything waiting on this person, across every session.
///
/// The per-session route answers "what is happening here"; this one answers "what is waiting on
/// me", which is the question a person actually has when they open the console.
pub async fn access_inbox(
    State(state): State<ApiState>,
    principal: Option<Principal>,
) -> ApiResult<Json<Value>> {
    let me = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let inbox = state.kernel.sessions.access_inbox(&me).await?;
    Ok(Json(json!({
        "user_id": me.user_id,
        "to_decide": inbox.to_decide,
        "mine": inbox.mine,
    })))
}
/// Who the gateway thinks this caller is.
///
/// The console shows it, and it is the fastest way to tell a wrong token from a missing permission.
pub async fn whoami(
    principal: Option<Principal>,
    source: Option<axum::Extension<IdentitySource>>,
) -> ApiResult<Json<Value>> {
    let principal = principal.map(|value| value.0).unwrap_or_else(agentos_core::model::Principal::operator);
    let source = source.map(|value| value.0).unwrap_or(IdentitySource::Operator);
    Ok(Json(json!({
        "user_id": principal.user_id,
        "node_id": principal.node_id,
        "roles": principal.roles,
        "admin": principal.is_admin(),
        // Where this identity came from. A console that shows it can explain why two people see the
        // same thing on an open node instead of leaving them to guess.
        "source": source.as_str(),
    })))
}

/// The action names the console is told about, in the order the permission table lists them.
pub const ALL_SESSION_ACTIONS: [agentos_core::model::SessionAction; 8] = [
    agentos_core::model::SessionAction::Read,
    agentos_core::model::SessionAction::Chat,
    agentos_core::model::SessionAction::Open,
    agentos_core::model::SessionAction::Close,
    agentos_core::model::SessionAction::Archive,
    agentos_core::model::SessionAction::Download,
    agentos_core::model::SessionAction::Delete,
    agentos_core::model::SessionAction::Grant,
];
pub async fn session_status(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    Ok(Json(state.kernel.sessions.status(&session).await?))
}

#[derive(Debug, Deserialize)]
pub struct PostMessageRequest {
    pub text: String,
    #[serde(default = "default_true")]
    pub wait: bool,
    /// Workspace-relative paths of images to attach. Read through the workspace jail, verified by
    /// content and stored as artifacts before the run starts.
    #[serde(default)]
    pub images: Vec<String>,
    /// Artifact ids returned by POST /v1/sessions/{id}/attachments. This is how a browser attaches
    /// an image: it cannot reach the runtime's workspace, so it uploads the bytes and names them.
    #[serde(default)]
    pub attachments: Vec<String>,
    /// Provider to prefer for this one goal. Overrides the session setting, and does not persist.
    #[serde(default)]
    pub model: Option<String>,
    /// Thinking effort for this one goal: off, low, medium or high. An unknown value is a client
    /// error, never a silent fallback to the strongest setting.
    #[serde(default)]
    pub effort: Option<String>,
}

fn default_true() -> bool {
    true
}

pub async fn post_message(
    State(state): State<ApiState>,
    principal: Option<Principal>,
    Path(id): Path<String>,
    Json(body): Json<PostMessageRequest>,
) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    if body.text.trim().is_empty() {
        return Err(ApiError(RuntimeError::invalid_input("message text must not be empty")));
    }
    let effort = parse_effort(body.effort.as_deref())?;
    // Who is speaking. Recorded on the turn and on the run, so a shared conversation keeps saying
    // whose words are whose.
    let author = principal.map(|value| value.0.as_ref());
    if body.wait {
        Ok(Json(
            state
                .kernel
                .sessions
                .post_goal(
                    &session,
                    &body.text,
                    &body.images,
                    &body.attachments,
                    body.model.clone(),
                    effort,
                    author,
                )
                .await?,
        ))
    } else {
        state
            .kernel
            .sessions
            .post_goal_async(
                &session,
                &body.text,
                &body.images,
                &body.attachments,
                body.model.clone(),
                effort,
                author,
            )
            .await?;
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

/// Approvals waiting for a decision.
pub async fn list_approvals(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    let pending = state.kernel.approvals.pending();
    Ok(Json(json!({ "approvals": pending, "total": pending.len() })))
}

/// Decide one. The id is single use: deciding twice is a conflict, not a toggle.
pub async fn decide_approval(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let approved = body
        .get("approved")
        .and_then(|value| value.as_bool())
        .ok_or_else(|| ApiError(RuntimeError::invalid_input("approved is required and must be a boolean")))?;
    let reason = body
        .get("reason")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string());
    let decided_by = body
        .get("by")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string());
    let decision = if approved {
        agentos_capability_runtime::ApprovalDecision::approve(decided_by)
    } else {
        agentos_capability_runtime::ApprovalDecision::deny(
            reason.clone().unwrap_or_else(|| "denied by an operator".into()),
            decided_by,
        )
    };
    let request = state.kernel.approvals.decide(&id, decision)?;
    Ok(Json(json!({
        "approval": request,
        "approved": approved,
        "reason": reason,
    })))
}

/// The diagnostics bundle, redacted. Same content the CLI writes, for a remote console.
pub async fn diagnostics(
    State(state): State<ApiState>,
    Query(q): Query<DiagnosticsQuery>,
) -> ApiResult<Json<Value>> {
    let options = agentos_kernel::diagnostics::DiagnosticsOptions {
        events: q.events.unwrap_or(200).min(2_000),
        include_transcripts: q.transcripts.unwrap_or(false),
    };
    let bundle = agentos_kernel::diagnostics::build(&state.kernel, &options).await?;
    Ok(Json(bundle))
}

#[derive(Debug, Deserialize)]
pub struct DiagnosticsQuery {
    pub events: Option<usize>,
    pub transcripts: Option<bool>,
}

/// Fetch an artifact's bytes. This is how a client renders an image the transcript refers to.
pub async fn get_artifact(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<axum::response::Response> {
    use axum::response::IntoResponse;
    let artifact_id = agentos_core::ArtifactId::from_raw(id.clone());
    let record = state
        .kernel
        .artifacts
        .get(&artifact_id)
        .await?
        .ok_or_else(|| ApiError(RuntimeError::not_found(format!("artifact {id} not found"))))?;
    let bytes = state
        .kernel
        .artifacts
        .read(&artifact_id)
        .await?
        .ok_or_else(|| ApiError(RuntimeError::not_found(format!("artifact {id} has no bytes"))))?;
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, record.content_type.clone()),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("inline; filename=\"{}\"", record.name.replace('"', "")),
            ),
        ],
        bytes,
    )
        .into_response())
}

/// Upload an image and get back an artifact id to attach to a goal.
///
/// The body is the image itself, not JSON: a browser has the bytes, and wrapping them in base64
/// would spend a third more of the request limit for nothing. The type is decided by content, the
/// size by policy.max_artifact_bytes, and a session that does not exist is refused before anything
/// is stored.
pub async fn upload_attachment(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    if state.kernel.sessions.get(&session).await?.is_none() {
        return Err(ApiError(RuntimeError::not_found(format!(
            "session {id} does not exist"
        ))));
    }
    // What the file is, decided once, by content. The two kinds have different caps because they
    // cost different things: an image goes to the model base64-encoded, a text file is pasted into
    // the prompt.
    let kind = agentos_agent_runtime::documents::classify_upload(&body);
    let limit = match kind {
        // A text file ends up in the prompt, and an .xlsx becomes text: both are capped at what a
        // prompt can carry, not at what the artifact store can hold.
        Some(agentos_agent_runtime::documents::UploadedKind::Text)
        | Some(agentos_agent_runtime::documents::UploadedKind::Spreadsheet) => {
            state.config.policy.max_artifact_bytes.min(agentos_agent_runtime::documents::MAX_DOCUMENT_BYTES)
        }
        _ => state.config.policy.max_artifact_bytes,
    };
    if body.len() as u64 > limit {
        return Err(ApiError(RuntimeError::invalid_input(format!(
            "the upload is {} bytes, the limit for this kind of file is {limit}",
            body.len()
        ))));
    }
    // A name is what a human reads in the transcript; the client may send one, and a missing one
    // must not become an empty file name.
    let name = headers
        .get("x-agentos-filename")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| value.rsplit(['/', '\\']).next().unwrap_or(value).to_string())
        .unwrap_or_else(|| "upload".to_string());
    let declared = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string());
    // The session manager owns the artifact store and the bus, so the gateway does not have to
    // know which crate a stored file belongs to.
    let deps = state.kernel.sessions.deps();
    let Some(kind) = kind else {
        return Err(ApiError(RuntimeError::invalid_input(format!(
            "{name} is neither an image (PNG, JPEG, GIF, WebP) nor a text file (CSV, TSV, Markdown, \
             JSON, plain text): the type is decided by content, and which one it is decides how the \
             model receives it."
        ))));
    };
    // A spreadsheet is read into text first: the model gets rows, and what is stored is the table
    // those rows make, so the artifact a client can fetch is the thing the model read.
    let mut spreadsheet_note = serde_json::Value::Null;
    let stored_body: Vec<u8> = if matches!(
        kind,
        agentos_agent_runtime::documents::UploadedKind::Spreadsheet
    ) {
        let workbook = agentos_agent_runtime::xlsx::workbook_from_bytes(&body)?.ok_or_else(|| {
            // Say why this one was refused *and* what would have been accepted: a refusal that
            // reports only the specific problem leaves the sender guessing about everything else.
            ApiError(RuntimeError::invalid_input(format!(
                "{name} is a zip archive but not a spreadsheet: no xl/workbook.xml inside it. \
                 Accepted: images (PNG, JPEG, GIF, WebP), text files (CSV, TSV, Markdown, JSON, \
                 plain text) and .xlsx workbooks"
            )))
        })?;
        spreadsheet_note = json!({
            "sheets": workbook.sheets.iter().map(|sheet| sheet.name.clone()).collect::<Vec<_>>(),
            "rows": workbook.sheets.iter().map(|sheet| sheet.rows.len()).sum::<usize>(),
        });
        workbook.csv.into_bytes()
    } else {
        body.to_vec()
    };
    if matches!(
        kind,
        agentos_agent_runtime::documents::UploadedKind::Text
            | agentos_agent_runtime::documents::UploadedKind::Spreadsheet
    ) {
        // Text, or a table read out of a spreadsheet: read into the prompt, which every model can do.
        // No vision check applies.
        // An .xlsx is stored as the table it was read into, so its content type describes the
        // bytes rather than the name.
        let record = if spreadsheet_note.is_null() {
            agentos_agent_runtime::documents::store_document(
                &deps.artifacts,
                &session,
                &name,
                &stored_body,
            )
            .await?
        } else {
            agentos_agent_runtime::documents::store_document_as(
                &deps.artifacts,
                &session,
                &name,
                &stored_body,
                "text/csv",
            )
            .await?
        };
        deps.bus
            .publish(
                agentos_core::model::NewEvent::new(
                    agentos_core::model::EventKind::ArtifactCreated,
                    "file uploaded",
                )
                .session(session)
                .node(deps.node_id.clone())
                .payload(json!({
                    "artifact_id": record.id.as_str(),
                    "name": record.name,
                    "bytes": record.size,
                    "content_type": record.content_type,
                })),
            )
            .await?;
        return Ok(Json(json!({
            "artifact_id": record.id.as_str(),
            "name": record.name,
            "content_type": record.content_type,
            "bytes": record.size,
            "kind": if spreadsheet_note.is_null() { "text" } else { "spreadsheet" },
            "spreadsheet": spreadsheet_note,
        })));
    }
    // An image. Refuse before storing, when nothing can look at it: the runtime would answer such a
    // goal with the placeholder's prose, which reads like a description of an image nobody saw.
    let sighted = state.kernel.models.vision_capable();
    if sighted.is_empty() {
        let blind = state
            .kernel
            .models
            .provider_vision()
            .into_iter()
            .map(|(name, vision)| format!("{name}={}", vision.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        // Two refusals that look alike: a model that cannot see, and a model that can see and has
        // no credential. Naming the variable is the whole value of the message - telling somebody
        // to switch to the model they already configured reads as a broken feature.
        let missing = state.config.vision_providers_without_a_key();
        let hint = if missing.is_empty() {
            "No enabled provider is declared able to see: mark one with vision: true, or use the default (deepseek with model deepseek-flash).".to_string()
        } else {
            format!(
                "{} can see but has no credential: set {} and restart the runtime.",
                missing
                    .iter()
                    .map(|(name, variable)| format!("{name} ({variable})"))
                    .collect::<Vec<_>>()
                    .join(", "),
                missing
                    .iter()
                    .map(|(_, variable)| variable.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let message = format!(
            "no configured model can be shown an image, so this upload would be answered by a model that cannot see it. Providers: {blind}. {hint}"
        );
        return Err(ApiError(RuntimeError::invalid_input(message)));
    }
    let record = agentos_agent_runtime::images::store_upload(
        &deps.artifacts,
        &session,
        &name,
        declared.as_deref(),
        &body,
    )
    .await?;
    deps.bus
        .publish(
            agentos_core::model::NewEvent::new(
                agentos_core::model::EventKind::ArtifactCreated,
                "image uploaded",
            )
            .session(session)
            .node(deps.node_id.clone())
            .payload(json!({
                "artifact_id": record.id.as_str(),
                "name": record.name,
                "bytes": record.size,
                "content_type": record.content_type,
            })),
        )
        .await?;
    Ok(Json(json!({
        "artifact_id": record.id.as_str(),
        "name": record.name,
        "content_type": record.content_type,
        "bytes": record.size,
        "kind": "image",
    })))
}

/// Fork a session. The fork inherits the conversation and the runs, with fresh identifiers.
pub async fn session_branch(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    let title = body
        .get("title")
        .and_then(|v| v.as_str())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let branch = state.kernel.sessions.branch(&session, title).await?;
    Ok(Json(json!({
        "session": branch,
        "forked_from": id,
    })))
}

/// Export a session as structured data or as a Markdown document.
pub async fn session_export(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(q): Query<ExportQuery>,
) -> ApiResult<axum::response::Response> {
    use axum::response::IntoResponse;
    let session = parse_session(&id)?;
    let (record, transcript, runs) = state.kernel.sessions.export_data(&session).await?;
    match q.format.as_deref().unwrap_or("json") {
        "markdown" | "md" => {
            let markdown = state.kernel.export_markdown(&session).await?;
            Ok((
                [(axum::http::header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
                markdown,
            )
                .into_response())
        }
        "json" => {
            let usage = runs.iter().fold(agentos_core::model::TokenUsage::default(), |mut total, run| {
                total.add(&run.usage);
                total
            });
            Ok(Json(json!({
                "exported_at": now_ms(),
                "node": state.kernel.config.effective_node_id(),
                "session": record,
                "messages": transcript,
                "runs": runs,
                "usage": usage,
            }))
            .into_response())
        }
        other => Err(ApiError(RuntimeError::invalid_input(format!(
            "unknown export format {other}: use json or markdown"
        )))),
    }
}

#[derive(Debug, Deserialize)]
pub struct ExportQuery {
    pub format: Option<String>,
}

/// The conversation: user goals and assistant replies, oldest first.
///
/// Posting a goal with "wait": false returns as soon as the goal is queued, so this is how a
/// client picks up the answer afterwards - the run completion event carries counters, not text.
pub async fn session_transcript(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(q): Query<LimitQuery>,
) -> ApiResult<Json<Value>> {
    let session = parse_session(&id)?;
    let transcript = state.kernel.sessions.transcript(&session, q.limit).await?;
    Ok(Json(transcript))
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
    // The named session decides the jail and the narrowing, so a direct invoke is as scoped as the
    // agent loop that would otherwise have made the call.
    let workspace = state
        .kernel
        .sessions
        .get(&session)
        .await?
        .and_then(|record| record.workspace_id);
    let caller = agentos_capability_runtime::capability::CallerContext::new(session)
        .with_workspace(workspace);
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
        // Who can be shown an image. A client asks this before offering a paperclip, so the answer
        // to "why can I not attach a screenshot" is available without trying one.
        "vision_capable": state.kernel.models.vision_capable(),
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

fn parse_workspace(id: &str) -> ApiResult<agentos_core::WorkspaceId> {
    agentos_core::WorkspaceId::parse(id).map_err(|e| ApiError(RuntimeError::invalid_input(e.to_string())))
}

/// Parse the wire spelling of a role once, so "unrecognised role" is one message everywhere.
fn parse_role(raw: &str) -> ApiResult<agentos_core::model::SessionRole> {
    agentos_core::model::SessionRole::parse(raw).ok_or_else(|| {
        ApiError(RuntimeError::invalid_input(format!(
            "unknown role {raw:?}: use owner, editor, participant or viewer"
        )))
    })
}

fn may_grant_on_workspace(
    record: &agentos_core::model::WorkspaceRecord,
    me: &agentos_core::model::Principal,
) -> bool {
    me.is_admin()
        || agentos_core::model::workspace_role(record, me)
            .map(|role| agentos_core::model::role_allows(role, agentos_core::model::SessionAction::Grant))
            .unwrap_or(false)
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
        EventKind::MemoryRecalled,
        EventKind::ContextLoaded,
        EventKind::SessionCompacted,
        EventKind::SessionRenamed,
        EventKind::AgentDelta,
        EventKind::ApprovalRequested,
        EventKind::ApprovalGranted,
        EventKind::ApprovalDenied,
        EventKind::ApprovalExpired,
        EventKind::NodeDiscovered,
        EventKind::NodeLost,
        EventKind::Error,
    ];
    all.into_iter().find(|k| k.as_str() == kind)
}
