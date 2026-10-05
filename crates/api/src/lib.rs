//! agentos-api - the HTTP/WebSocket gateway.
//!
//! Responsibilities, and nothing else:
//!   * authentication (static bearer token in v1, pluggable),
//!   * per-client rate limiting,
//!   * request correlation ids and latency metrics,
//!   * translating HTTP/WS into SessionManager calls and event subscriptions.
//!
//! The gateway owns no session state. It routes to the kernel, which routes to actors.

pub mod error;
pub mod handlers;
pub mod middleware;
pub mod ws;

use agentos_core::config::RuntimeConfig;
use agentos_core::error::{Result, RuntimeError};
use agentos_kernel::Kernel;
use axum::routing::{get, post};
use axum::Router;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;

#[derive(Clone)]
pub struct ApiState {
    pub kernel: Arc<Kernel>,
    pub config: Arc<RuntimeConfig>,
    pub started_at: u64,
    pub limiter: Arc<middleware::RateLimiter>,
}

impl ApiState {
    pub fn new(kernel: Arc<Kernel>, config: Arc<RuntimeConfig>) -> Self {
        let limiter = Arc::new(middleware::RateLimiter::new(config.api.rate_limit_per_minute));
        Self { kernel, config, started_at: agentos_core::now_ms(), limiter }
    }
}

/// Build the router. Kept public so tests can drive the API without binding a port.
///
/// Every /v1 route goes through the guard: rate limiting applies to all of them, and
/// authentication applies to all of them except the two the client needs before it has a token
/// (/v1/meta to learn whether a token is required, and /v1/auth/login to exchange one).
pub fn router(state: ApiState) -> Router {
    let v1 = Router::new()
        .route("/v1/meta", get(handlers::meta))
        .route("/v1/auth/login", post(handlers::login))
        // Who am I: the fastest way to tell a wrong token from a missing permission.
        .route("/v1/auth/whoami", get(handlers::whoami))
        .route("/v1/ws", get(ws::upgrade))
        .route("/v1/sessions", get(handlers::list_sessions).post(handlers::create_session))
        .route(
            "/v1/sessions/{id}",
            get(handlers::get_session)
                .patch(handlers::configure_session)
                .delete(handlers::close_session),
        )
        .route("/v1/sessions/{id}/status", get(handlers::session_status))
        .route("/v1/sessions/{id}/messages", post(handlers::post_message))
        .route(
            "/v1/sessions/{id}/attachments",
            post(handlers::upload_attachment),
        )
        .route("/v1/sessions/{id}/cancel", post(handlers::cancel_session))
        // Closing stops the actor; opening takes it back. Neither touches ownership.
        .route("/v1/sessions/{id}/open", post(handlers::open_session))
        .route("/v1/sessions/{id}/close", post(handlers::close_session))
        .route(
            "/v1/sessions/{id}/access",
            post(handlers::grant_access).delete(handlers::revoke_access),
        )
        // What this session may use, and who else may take part.
        .route(
            "/v1/sessions/{id}/capabilities",
            get(handlers::session_capabilities).put(handlers::set_session_capabilities),
        )
        .route(
            "/v1/sessions/{id}/access-requests",
            get(handlers::list_session_access).post(handlers::request_session_access),
        )
        .route(
            "/v1/sessions/{id}/access-requests/{request_id}/decide",
            post(handlers::decide_session_access),
        )
        // Workspaces: the unit of ownership, sharing and filesystem isolation. Sessions and their
        // access hang off one; see docs/decisions.md D20.
        .route(
            "/v1/workspaces",
            get(handlers::list_workspaces).post(handlers::create_workspace),
        )
        // Static before dynamic on purpose: axum matches a literal segment ahead of `{id}`, and this
        // is the picker the console opens before it has an id to name.
        .route("/v1/workspaces/browse", get(handlers::browse_workspaces))
        .route(
            "/v1/workspaces/{id}",
            get(handlers::get_workspace).patch(handlers::rename_workspace),
        )
        .route(
            "/v1/workspaces/{id}/sessions",
            get(handlers::list_workspace_sessions).post(handlers::create_workspace_session),
        )
        .route(
            "/v1/workspaces/{id}/capabilities",
            get(handlers::workspace_capabilities).put(handlers::set_workspace_capabilities),
        )
        .route(
            "/v1/workspaces/{id}/access",
            post(handlers::grant_workspace_access).delete(handlers::revoke_workspace_access),
        )
        .route(
            "/v1/workspaces/{id}/access-requests",
            get(handlers::list_workspace_access).post(handlers::request_workspace_access),
        )
        .route(
            "/v1/workspaces/{id}/access-requests/{request_id}/decide",
            post(handlers::decide_workspace_access),
        )
        // Archiving: a conversation out of the hot store and into one package.
        .route("/v1/sessions/{id}/archive", post(handlers::archive_session))
        .route("/v1/access-requests", get(handlers::access_inbox))
        .route("/v1/archives", get(handlers::list_archives))
        .route(
            "/v1/archives/{id}",
            get(handlers::get_archive).delete(handlers::delete_archive),
        )
        .route("/v1/archives/{id}/restore", post(handlers::restore_archive))
        .route("/v1/sessions/{id}/transcript", get(handlers::session_transcript))
        .route("/v1/diagnostics", get(handlers::diagnostics))
        .route("/v1/approvals", get(handlers::list_approvals))
        .route("/v1/approvals/{id}", post(handlers::decide_approval))
        .route("/v1/artifacts/{id}", get(handlers::get_artifact))
        .route("/v1/sessions/{id}/export", get(handlers::session_export))
        .route("/v1/sessions/{id}/branch", post(handlers::session_branch))
        .route("/v1/sessions/{id}/events", get(handlers::session_events))
        .route("/v1/sessions/{id}/graph", get(handlers::session_graph))
        .route("/v1/sessions/{id}/snapshot", get(handlers::session_snapshot))
        .route("/v1/sessions/{id}/restore", post(handlers::session_restore))
        .route("/v1/sessions/{id}/migrate", post(handlers::session_migrate))
        .route("/v1/capabilities", get(handlers::list_capabilities))
        .route("/v1/capabilities/{name}/invoke", post(handlers::invoke_capability))
        .route("/v1/tasks", get(handlers::list_tasks))
        .route("/v1/workers", get(handlers::list_workers))
        .route("/v1/actors", get(handlers::list_actors))
        .route("/v1/events", get(handlers::list_events))
        .route("/v1/nodes", get(handlers::list_nodes))
        .route("/v1/models", get(handlers::list_models))
        .route("/v1/metrics", get(handlers::metrics))
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), middleware::guard));

    Router::new()
        .route("/healthz", get(handlers::healthz))
        .route("/readyz", get(handlers::readyz))
        .merge(v1)
        .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BYTES))
        // A 413 from the body limit is raised before any handler runs, so without this the caller
        // sees axum's "length limit exceeded" and has no way to tell what the limit is.
        .layer(axum::middleware::from_fn(request_too_large))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// The largest request the gateway will read.
///
/// Sized for an image, not for a JSON payload: the runtime accepts images up to 5 MiB
/// (`agent_runtime::images::MAX_IMAGE_BYTES`) and base64 inflates them by a third. The old 4 MiB
/// limit rejected a legal image before any handler could explain why.
pub const MAX_REQUEST_BYTES: usize = agentos_agent_runtime::images::MAX_IMAGE_BYTES as usize * 2 + 1024 * 1024;

/// Serve until cancelled. Returns the bound address.
pub async fn serve(
    kernel: Arc<Kernel>,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<SocketAddr> {
    let config = Arc::new(kernel.config.clone());
    let addr: SocketAddr = config
        .api
        .http_addr
        .parse()
        .map_err(|e| RuntimeError::invalid_input(format!("bad api.http_addr: {e}")))?;
    let state = ApiState::new(kernel, config);
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| RuntimeError::network(format!("cannot bind {addr}: {e}")))?;
    let bound = listener
        .local_addr()
        .map_err(|e| RuntimeError::network(format!("cannot read the bound address: {e}")))?;
    tracing::info!(%bound, "gateway listening");
    // Connect info gives the rate limiter a real client address instead of "unknown".
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move { cancellation.cancelled().await })
    .await
    .map_err(|e| RuntimeError::network(format!("gateway failed: {e}")))?;
    Ok(bound)
}

/// Turn the body limiter's refusal into the same error shape as every other route.
async fn request_too_large(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let response = next.run(request).await;
    if response.status() != axum::http::StatusCode::PAYLOAD_TOO_LARGE {
        return response;
    }
    let body = serde_json::json!({
        "error": {
            "code": "invalid_input",
            "message": format!(
                "the request body is larger than this gateway reads ({MAX_REQUEST_BYTES} bytes). An image is capped at {} bytes before encoding.",
                agentos_agent_runtime::images::MAX_IMAGE_BYTES,
            ),
            "retryable": false,
            "details": null,
        }
    });
    (axum::http::StatusCode::PAYLOAD_TOO_LARGE, axum::Json(body)).into_response()
}

/// Serve on an ephemeral port and return it. Used by integration tests.
pub async fn serve_test(kernel: Arc<Kernel>) -> Result<(SocketAddr, tokio_util::sync::CancellationToken)> {
    let config = Arc::new(kernel.config.clone());
    let state = ApiState::new(kernel, config);
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| RuntimeError::network(format!("cannot bind: {e}")))?;
    let addr = listener.local_addr().map_err(|e| RuntimeError::network(e.to_string()))?;
    let token = tokio_util::sync::CancellationToken::new();
    let shutdown = token.clone();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await;
    });
    Ok((addr, token))
}
