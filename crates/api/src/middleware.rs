//! Authentication, rate limiting and request correlation.

use crate::ApiState;
use agentos_core::error::RuntimeError;
use agentos_core::telemetry::{metric_names, metrics};
use agentos_core::RequestId;
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Instant;

/// Sliding-window limiter. Deliberately simple: a HashMap of timestamps per client key.
pub struct RateLimiter {
    limit_per_minute: u32,
    hits: Mutex<HashMap<String, Vec<Instant>>>,
}

impl RateLimiter {
    pub fn new(limit_per_minute: u32) -> Self {
        Self { limit_per_minute, hits: Mutex::new(HashMap::new()) }
    }

    /// Returns Ok(remaining) or Err(retry_after_seconds).
    pub fn check(&self, key: &str) -> std::result::Result<u32, u64> {
        if self.limit_per_minute == 0 {
            return Ok(u32::MAX);
        }
        let now = Instant::now();
        let mut hits = self.hits.lock();
        let entry = hits.entry(key.to_string()).or_default();
        entry.retain(|t| now.duration_since(*t).as_secs() < 60);
        if entry.len() as u32 >= self.limit_per_minute {
            let oldest = entry.first().copied().unwrap_or(now);
            let retry = 60u64.saturating_sub(now.duration_since(oldest).as_secs()).max(1);
            return Err(retry);
        }
        entry.push(now);
        Ok(self.limit_per_minute - entry.len() as u32)
    }

    pub fn tracked_clients(&self) -> usize {
        self.hits.lock().len()
    }
}

/// What a request acts on, for the ACL to check.
///
/// Two scopes, because access is decided at two levels and they must not be conflated: a session
/// (read through its workspace) and a workspace itself (membership, capabilities, its own sessions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequiredAccess {
    Session(agentos_core::SessionId, agentos_core::model::SessionAction),
    Workspace(agentos_core::WorkspaceId, agentos_core::model::SessionAction),
}

/// The action a request performs, if it performs one.
///
/// A pure function of method and path, so the mapping can be tested without a runtime and read in
/// one place: an endpoint added without an entry here is an endpoint with no permission check, and
/// that is worth being able to see at a glance.
pub fn required_access(method: &axum::http::Method, path: &str) -> Option<RequiredAccess> {
    session_access(method, path).or_else(|| workspace_access(method, path))
}

fn session_access(method: &axum::http::Method, path: &str) -> Option<RequiredAccess> {
    use agentos_core::model::SessionAction;
    // /v1/sessions/{id}/... - the last segment decides, except for the detail routes.
    let rest = path.strip_prefix("/v1/sessions/")?;
    let mut segments = rest.split('/');
    let id = segments.next()?;
    if id.is_empty() {
        // /v1/sessions itself: creating is not an action on an existing session.
        return None;
    }
    let session = agentos_core::SessionId::from_raw(id);
    let tail = segments.next().unwrap_or("");
    let action = match (method.as_str(), tail) {
        // The tail decides first: /access is a grant however it is reached, and the generic
        // DELETE rule below would silently make revoking a role an act of closing.
        (_, "messages") => SessionAction::Chat,
        (_, "cancel") => SessionAction::Chat,
        (_, "attachments") => SessionAction::Chat,
        (_, "open") => SessionAction::Open,
        (_, "close") => SessionAction::Close,
        (_, "archive") => SessionAction::Archive,
        (_, "access") => SessionAction::Grant,
        // Narrowing what a session may use is the same authority as handing out a role in it.
        (_, "capabilities") => SessionAction::Grant,
        // Deciding is an act of authority over the session.
        ("POST", "access-requests") if path.ends_with("/decide") => SessionAction::Grant,
        // Asking is not. A request for access has to be reachable by somebody with no role at all -
        // that is the entire point of asking - so it is deliberately NOT an action on the session and
        // the ACL above checks nothing for it. Listing works the same way: the handler answers with
        // your own requests when you have no standing to see anyone else's.
        (_, "access-requests") => return None,
        (_, "migrate") => SessionAction::Open,
        (_, "branch") => SessionAction::Read,
        (_, "restore") => SessionAction::Read,
        // Export is a download: a viewer who may read a session may also take a copy of it.
        (_, "export") => SessionAction::Download,
        // Closing is a delete on the wire because that is the verb the console uses; the record
        // survives, and ownership stays where it was.
        ("DELETE", _) => SessionAction::Close,
        ("PATCH", _) => SessionAction::Close,
        // Everything else that touches a session - detail, transcript, events, graph, snapshot,
        // status - is a read.
        _ => SessionAction::Read,
    };
    Some(RequiredAccess::Session(session, action))
}

fn workspace_access(method: &axum::http::Method, path: &str) -> Option<RequiredAccess> {
    use agentos_core::model::SessionAction;
    let rest = path.strip_prefix("/v1/workspaces/")?;
    let mut segments = rest.split('/');
    let id = segments.next()?;
    if id.is_empty() {
        // /v1/workspaces itself: listing and creating are not actions on an existing workspace.
        return None;
    }
    let workspace = agentos_core::WorkspaceId::from_raw(id);
    let tail = segments.next().unwrap_or("");
    // The workspace actions are mapped onto the one role table rather than given a second one:
    //   * reading it is Read (every role),
    //   * creating a session in it is Chat (owner, editor, participant - a viewer may not speak),
    //   * renaming it, narrowing it and handing out membership is Grant (owner only).
    // Two tables is exactly how "an editor may do X here but not there" starts to drift.
    let action = match (method.as_str(), tail) {
        ("POST", "sessions") => SessionAction::Chat,
        (_, "sessions") => SessionAction::Read,
        ("PATCH", "") => SessionAction::Grant,
        ("DELETE", "") => SessionAction::Delete,
        (_, "capabilities") => SessionAction::Grant,
        (_, "access") => SessionAction::Grant,
        ("POST", "access-requests") if path.ends_with("/decide") => SessionAction::Grant,
        // Asking is open here for the same reason it is open on a session.
        (_, "access-requests") => return None,
        _ => SessionAction::Read,
    };
    Some(RequiredAccess::Workspace(workspace, action))
}

/// How the principal on a request was established.
///
/// Carried alongside the principal so a console can say "you are the operator because this node has
/// no token" instead of leaving somebody to infer it from the fact that permissions feel wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentitySource {
    /// A bearer token matched the configured principal table.
    Token,
    /// The node has nothing to authenticate against: the caller declared who it is.
    Asserted,
    /// The node has nothing to authenticate against and nobody declared anything: the operator.
    Operator,
}

impl IdentitySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Token => "token",
            Self::Asserted => "asserted",
            Self::Operator => "operator",
        }
    }
}

/// A caller-declared identity, before validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssertedIdentity {
    pub user_id: String,
    pub node_id: Option<String>,
}

/// Why resolving an identity refused a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityError {
    /// Nothing identified the caller and this node requires it: 401. Carries the sentence to show.
    Missing(String),
    /// Something was declared but is not usable, with the reason to show: 400.
    Invalid(String),
}

/// A principal and how it was established.
#[derive(Debug, Clone)]
pub struct ResolvedIdentity {
    pub principal: agentos_core::model::Principal,
    pub source: IdentitySource,
}

/// The role an asserted identity carries. Named `creator` and not `admin` on purpose: on an open
/// node a name nobody checked can create and own things, but it cannot act as the operator.
pub const ASSERTED_ROLE: &str = "creator";

fn operator_identity() -> ResolvedIdentity {
    ResolvedIdentity { principal: agentos_core::model::Principal::operator(), source: IdentitySource::Operator }
}

/// Names are written into records, matched in grants and displayed as `user@node`, so the alphabet
/// is fixed and `@` is not in it: a name that could contain the separator would be ambiguous.
fn valid_asserted_name(value: &str, field: &str) -> std::result::Result<String, IdentityError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(IdentityError::Invalid(format!("{field} must not be empty")));
    }
    if trimmed.len() > 64 {
        return Err(IdentityError::Invalid(format!("{field} must be at most 64 characters")));
    }
    if !trimmed.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
        return Err(IdentityError::Invalid(format!(
            "{field} may only contain letters, digits, dot, underscore or dash"
        )));
    }
    Ok(trimmed.to_string())
}

/// Resolve the request's identity.
///
/// Two regimes, and the boundary between them is `has_authenticated_identities`:
///   * a node with a principal table or a static token derives identity **only** from the presented
///     token - an unrecognised token is refused rather than defaulted, and a declared identity is
///     ignored entirely, because honouring it would make the table decorative;
///   * a node with neither is open, and there `asserted_identity` decides between one shared
///     operator and a name the caller declares.
///
/// Guessing is the one thing a permission model must never do, so every refusal here is explicit:
/// a wrong token gives `Missing`, a malformed name gives `Invalid` with the reason.
pub fn resolve_principal(
    config: &agentos_core::config::ApiConfig,
    presented: Option<&str>,
    asserted: Option<&AssertedIdentity>,
) -> std::result::Result<ResolvedIdentity, IdentityError> {
    use agentos_core::config::AssertedIdentityMode;

    if config.has_authenticated_identities() {
        let table = config.resolved_principals();
        // A table whose entries all lack a token resolves to nobody. That used to fall through to the
        // operator; keep that, because a misconfigured table turning a node open would be the worse
        // failure - but the operator path is taken explicitly, and the declared identity still does
        // not get a say.
        if table.is_empty() {
            return Ok(operator_identity());
        }
        let Some(presented) = presented else {
            // A tokenless request against a runtime that has no tokenless principal.
            let open = table.iter().any(|(token, _)| token.is_empty());
            return if open {
                Ok(ResolvedIdentity { principal: table[0].1.clone(), source: IdentitySource::Token })
            } else {
                Err(IdentityError::Missing("missing or invalid bearer token".into()))
            };
        };
        for (token, principal) in &table {
            if !token.is_empty() && constant_time_eq(presented.as_bytes(), token.as_bytes()) {
                return Ok(ResolvedIdentity { principal: principal.clone(), source: IdentitySource::Token });
            }
        }
        return Err(IdentityError::Missing("missing or invalid bearer token".into()));
    }

    match config.asserted_identity {
        AssertedIdentityMode::Off => Ok(operator_identity()),
        mode => match asserted {
            Some(asserted) => {
                let user_id = valid_asserted_name(&asserted.user_id, "user_id")?;
                if user_id.eq_ignore_ascii_case("operator") {
                    // Otherwise asserting the node's own name would hand out the operator's identity,
                    // and every record and grant written before would start matching a stranger.
                    return Err(IdentityError::Invalid(
                        "user_id \"operator\" is reserved for the node itself".into(),
                    ));
                }
                let node_id = match &asserted.node_id {
                    Some(node) if !node.trim().is_empty() => Some(valid_asserted_name(node, "node_id")?),
                    _ => None,
                };
                Ok(ResolvedIdentity {
                    principal: agentos_core::model::Principal::new(
                        user_id,
                        node_id,
                        vec![ASSERTED_ROLE.into()],
                    ),
                    source: IdentitySource::Asserted,
                })
            }
            None if mode == AssertedIdentityMode::Required => Err(IdentityError::Missing(
                "this node requires a caller identity: send X-Agora-User (or ?user=) to say who you are"
                    .into(),
            )),
            None => Ok(operator_identity()),
        },
    }
}

/// Read a caller-declared identity from the request, header first and query second.
///
/// The query form exists for the WebSocket handshake, where a browser cannot set a header. Both are
/// the same claim, so they must not be allowed to disagree: the header wins, and a request that
/// carries both is the header's reading.
fn asserted_identity<T>(req: &axum::http::Request<T>) -> Option<AssertedIdentity> {
    let header = |name: &str| {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let query = |name: &str| {
        req.uri().query().and_then(|q| {
            q.split('&')
                .find_map(|p| p.strip_prefix(&format!("{name}=")))
                .map(|v| v.to_string())
                .filter(|v| !v.is_empty())
        })
    };
    let user_id = header("x-agora-user").or_else(|| query("user"))?;
    let node_id = header("x-agora-node").or_else(|| query("node"));
    Some(AssertedIdentity { user_id, node_id })
}

/// The resolved caller, as an axum extractor.
///
/// Handlers take `Principal` when a person is required and `Option<Principal>` when the request is
/// allowed to be anonymous (the two routes that run before a token exists). It is the same value
/// the guard resolved, read back off the request - never re-resolved, so a handler cannot end up
/// disagreeing with the check that let the request through.
#[derive(Clone, Debug)]
pub struct Principal(pub agentos_core::model::Principal);

impl Principal {
    /// The identity a runtime with no principal table resolves every request to.
    pub fn operator() -> Self {
        Self(agentos_core::model::Principal::operator())
    }
}

impl std::ops::Deref for Principal {
    type Target = agentos_core::model::Principal;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<S> axum::extract::FromRequestParts<S> for Principal
where
    S: Send + Sync,
{
    type Rejection = crate::error::ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> std::result::Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<agentos_core::model::Principal>()
            .cloned()
            .map(Principal)
            .ok_or_else(|| {
                crate::error::ApiError(RuntimeError::unauthorized(
                    "this request carries no principal: it did not pass the gateway guard",
                ))
            })
    }
}

impl<S> axum::extract::OptionalFromRequestParts<S> for Principal
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> std::result::Result<Option<Self>, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<agentos_core::model::Principal>()
            .cloned()
            .map(Principal))
    }
}

fn client_key(req: &Request) -> String {
    if let Some(forwarded) = req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = forwarded.split(',').next() {
            return first.trim().to_string();
        }
    }
    req.extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip())
        .or_else(|| req.headers().get("x-real-ip").and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<IpAddr>().ok()))
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Guard applied to every /v1 route: rate limit, then auth when a token is configured.
pub async fn guard(State(state): State<ApiState>, mut req: Request, next: Next) -> Response {
    let started = Instant::now();
    let key = client_key(&req);
    let path = req.uri().path().to_string();

    if let Err(retry_after) = state.limiter.check(&key) {
        metrics().inc_by(metric_names::HTTP_REQUESTS, &[("status", "429")], 1);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", retry_after.to_string())],
            axum::Json(serde_json::json!({
                "error": { "code": "rate_limited", "message": format!("retry in {retry_after}s"), "retryable": true }
            })),
        )
            .into_response();
    }

    // Two endpoints must work before the client holds a token. Rate limiting still applies.
    let open_without_token = path == "/v1/meta" || path == "/v1/auth/login";

    // One identity, resolved once, carried on the request: the handlers decide what to record, this
    // decides who is asking. A runtime with no principal table resolves every request to the
    // operator, which is how it behaved before principals existed.
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|v| v.trim().to_string())
        .or_else(|| {
            req.uri()
                .query()
                .and_then(|q| q.split('&').find_map(|p| p.strip_prefix("token=")))
                .map(|v| v.to_string())
        });

    let asserted = asserted_identity(&req);
    let principal = if open_without_token {
        None
    } else {
        match resolve_principal(&state.config.api, presented.as_deref(), asserted.as_ref()) {
            Ok(identity) => {
                req.extensions_mut().insert(identity.source);
                Some(identity.principal)
            }
            Err(IdentityError::Missing(reason)) => {
                metrics().inc_by(metric_names::HTTP_REQUESTS, &[("status", "401")], 1);
                return crate::error::ApiError(RuntimeError::unauthorized(reason)).into_response();
            }
            Err(IdentityError::Invalid(reason)) => {
                metrics().inc_by(metric_names::HTTP_REQUESTS, &[("status", "400")], 1);
                return crate::error::ApiError(
                    RuntimeError::invalid_input(format!("unusable caller identity: {reason}"))
                        .with_detail("field", "identity"),
                )
                .into_response();
            }
        }
    };
    if let Some(principal) = &principal {
        req.extensions_mut().insert(principal.clone());
    }

    // Per-session permissions, checked here and only here. A route that reaches a session without an
    // entry in required_action is a route nobody has thought about, which is why the mapping is a
    // pure function with a test.
    if let Some(principal) = &principal {
        if let Some(required) = required_access(req.method(), req.uri().path()) {
            match required {
                RequiredAccess::Session(session, action) => {
                    match state.kernel.sessions.get(&session).await {
                        Ok(Some(record)) => {
                            // The session's access is its workspace's access (D20). Loading the
                            // workspace here is what makes a grant on the workspace felt by every
                            // session of it, without any session record changing.
                            let workspace = state
                                .kernel
                                .sessions
                                .workspace_of(&record)
                                .await
                                .ok()
                                .flatten();
                            if let Err(denial) = agentos_core::model::decide_in(
                                &record,
                                workspace.as_ref(),
                                principal,
                                action,
                            ) {
                                metrics().inc_by(metric_names::HTTP_REQUESTS, &[("status", "403")], 1);
                                return crate::error::ApiError(
                                    RuntimeError::policy_denied(denial.message())
                                        .with_detail("action", action.as_str())
                                        .with_detail("session_id", session.as_str())
                                        .with_detail("owner", record.owner_label())
                                        .with_detail(
                                            "workspace_id",
                                            record
                                                .workspace_id
                                                .as_ref()
                                                .map(|id| id.as_str().to_string())
                                                .unwrap_or_default(),
                                        )
                                        .with_detail("user_id", principal.user_id.clone()),
                                )
                                .into_response();
                            }
                        }
                        // No record: the handler answers 404. Refusing here would turn "does not exist" into
                        // "you may not", which is a lie that costs an hour of debugging.
                        _ => {}
                    }
                }
                RequiredAccess::Workspace(workspace_id, action) => {
                    match state.kernel.sessions.get_workspace(&workspace_id).await {
                        Ok(Some(record)) => {
                            if let Err(denial) =
                                agentos_core::model::decide_workspace(&record, principal, action)
                            {
                                metrics().inc_by(metric_names::HTTP_REQUESTS, &[("status", "403")], 1);
                                return crate::error::ApiError(
                                    RuntimeError::policy_denied(denial.message())
                                        .with_detail("action", action.as_str())
                                        .with_detail("workspace_id", workspace_id.as_str())
                                        .with_detail("owner", record.owner_label())
                                        .with_detail("user_id", principal.user_id.clone()),
                                )
                                .into_response();
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // Creating a session is not an action on one, so it is checked separately: a principal has to be
    // someone the operator configured, which the resolution above already established.
    if let Some(principal) = &principal {
        if req.method() == axum::http::Method::POST && req.uri().path() == "/v1/sessions" {
            let allowed = principal.is_admin()
                || principal.roles.iter().any(|role| {
                    role.eq_ignore_ascii_case("operator") || role.eq_ignore_ascii_case("creator")
                });
            if !allowed {
                metrics().inc_by(metric_names::HTTP_REQUESTS, &[("status", "403")], 1);
                return crate::error::ApiError(
                    RuntimeError::policy_denied(format!(
                        "{} may not create sessions on this runtime",
                        principal.user_id
                    ))
                    .with_detail("user_id", principal.user_id.clone()),
                )
                .into_response();
            }
        }
    }

    let request_id = RequestId::new();
    let mut response = next.run(req).await;
    let elapsed = started.elapsed().as_millis() as f64;
    metrics().inc_by(
        metric_names::HTTP_REQUESTS,
        &[("status", &response.status().as_u16().to_string())],
        1,
    );
    metrics().observe(metric_names::HTTP_LATENCY_MS, elapsed);
    if let Ok(value) = header::HeaderValue::from_str(request_id.as_str()) {
        response.headers_mut().insert("x-request-id", value);
    }
    tracing::debug!(path = %path, status = response.status().as_u16(), latency_ms = elapsed, "http request");
    response
}

/// Length-independent comparison so a token cannot be guessed byte by byte.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_counts_and_blocks() {
        let limiter = RateLimiter::new(3);
        assert!(limiter.check("a").is_ok());
        assert!(limiter.check("a").is_ok());
        assert!(limiter.check("a").is_ok());
        assert!(limiter.check("a").is_err());
        assert!(limiter.check("b").is_ok(), "limits are per client");
    }

    #[test]
    fn zero_limit_disables_throttling() {
        let limiter = RateLimiter::new(0);
        for _ in 0..100 {
            assert!(limiter.check("a").is_ok());
        }
    }

    #[test]
    fn token_comparison_is_length_safe() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secre"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
    }

    use agentos_core::model::SessionAction;
    use axum::http::Method;


    #[test]
    fn every_session_route_maps_to_an_action() {
        let cases = [
            (Method::POST, "/v1/sessions/ses_1/messages", SessionAction::Chat),
            (Method::POST, "/v1/sessions/ses_1/cancel", SessionAction::Chat),
            (Method::POST, "/v1/sessions/ses_1/attachments", SessionAction::Chat),
            (Method::POST, "/v1/sessions/ses_1/open", SessionAction::Open),
            (Method::POST, "/v1/sessions/ses_1/close", SessionAction::Close),
            (Method::DELETE, "/v1/sessions/ses_1", SessionAction::Close),
            (Method::POST, "/v1/sessions/ses_1/access", SessionAction::Grant),
            (Method::DELETE, "/v1/sessions/ses_1/access", SessionAction::Grant),
            (Method::GET, "/v1/sessions/ses_1", SessionAction::Read),
            (Method::GET, "/v1/sessions/ses_1/transcript", SessionAction::Read),
            (Method::GET, "/v1/sessions/ses_1/graph", SessionAction::Read),
            // Export is a download: a viewer who may read a session may also take a copy of it.
            (Method::GET, "/v1/sessions/ses_1/export", SessionAction::Download),
            (Method::PATCH, "/v1/sessions/ses_1", SessionAction::Close),
            (Method::GET, "/v1/sessions/ses_1/capabilities", SessionAction::Grant),
            (Method::PUT, "/v1/sessions/ses_1/capabilities", SessionAction::Grant),
            (
                Method::POST,
                "/v1/sessions/ses_1/access-requests/req_1/decide",
                SessionAction::Grant,
            ),
        ];
        for (method, path, expected) in cases {
            match required_access(&method, path) {
                Some(RequiredAccess::Session(session, action)) => {
                    assert_eq!(session.as_str(), "ses_1");
                    assert_eq!(action, expected, "{method} {path}");
                }
                other => panic!("{method} {path} mapped to {other:?}"),
            }
        }
    }

    #[test]
    fn every_workspace_route_maps_to_an_action() {
        use agentos_core::model::SessionAction;
        let cases = [
            (Method::GET, "/v1/workspaces/ws_1", SessionAction::Read),
            (Method::PATCH, "/v1/workspaces/ws_1", SessionAction::Grant),
            // Creating a session in a workspace is speaking in it, so a viewer may not.
            (Method::POST, "/v1/workspaces/ws_1/sessions", SessionAction::Chat),
            (Method::GET, "/v1/workspaces/ws_1/sessions", SessionAction::Read),
            (Method::PUT, "/v1/workspaces/ws_1/capabilities", SessionAction::Grant),
            (Method::GET, "/v1/workspaces/ws_1/capabilities", SessionAction::Grant),
            (Method::POST, "/v1/workspaces/ws_1/access", SessionAction::Grant),
            (Method::DELETE, "/v1/workspaces/ws_1/access", SessionAction::Grant),
            (
                Method::POST,
                "/v1/workspaces/ws_1/access-requests/req_1/decide",
                SessionAction::Grant,
            ),
        ];
        for (method, path, expected) in cases {
            match required_access(&method, path) {
                Some(RequiredAccess::Workspace(workspace, action)) => {
                    assert_eq!(workspace.as_str(), "ws_1");
                    assert_eq!(action, expected, "{method} {path}");
                }
                other => panic!("{method} {path} mapped to {other:?}"),
            }
        }
    }

    #[test]
    fn asking_for_access_is_outside_the_acl() {
        // Deliberate: the endpoint exists for people who have no role, so it cannot require one. The
        // handler answers with the caller's own requests and refuses to decide anything.
        assert!(required_access(&Method::POST, "/v1/sessions/ses_1/access-requests").is_none());
        assert!(required_access(&Method::GET, "/v1/sessions/ses_1/access-requests").is_none());
        assert!(required_access(&Method::POST, "/v1/workspaces/ws_1/access-requests").is_none());
        assert!(required_access(&Method::GET, "/v1/workspaces/ws_1/access-requests").is_none());
    }

    #[test]
    fn creating_a_session_is_not_an_action_on_one() {
        assert!(required_access(&Method::POST, "/v1/sessions").is_none());
        assert!(required_access(&Method::GET, "/v1/sessions").is_none());
        assert!(required_access(&Method::GET, "/v1/meta").is_none());
        // Listing and creating workspaces are not actions on an existing workspace either.
        assert!(required_access(&Method::POST, "/v1/workspaces").is_none());
        assert!(required_access(&Method::GET, "/v1/workspaces").is_none());
    }

    #[test]
    fn a_runtime_with_no_principal_table_resolves_every_request_to_the_operator() {
        let mut config = agentos_core::config::ApiConfig::default();
        config.auth_token_env = "AGENTOS_TEST_UNSET_TOKEN_VAR".into();
        std::env::remove_var("AGENTOS_TEST_UNSET_TOKEN_VAR");
        let identity = resolve_principal(&config, None, None).unwrap();
        assert!(identity.principal.is_admin());
        assert_eq!(identity.principal.user_id, "operator");
        assert_eq!(identity.source, IdentitySource::Operator);
    }

    #[test]
    fn a_configured_table_admits_only_its_own_tokens() {
        let mut config = agentos_core::config::ApiConfig::default();
        config.principals = vec![agentos_core::config::PrincipalConfig {
            user_id: "alice".into(),
            node_id: Some("node-a".into()),
            roles: vec!["operator".into()],
            token_env: None,
            token: Some("alice-token".into()),
        }];
        let alice = resolve_principal(&config, Some("alice-token"), None).unwrap();
        assert_eq!(alice.principal.user_id, "alice");
        assert_eq!(alice.principal.node_id.as_deref(), Some("node-a"));
        assert_eq!(alice.source, IdentitySource::Token);
        // A wrong token is refused, not downgraded to the operator. Guessing is the one thing a
        // permission model must never do.
        assert!(resolve_principal(&config, Some("bob-token"), None).is_err());
        assert!(resolve_principal(&config, None, None).is_err());
    }

    #[test]
    fn a_principal_may_not_be_its_own_identity_by_typo() {
        // The table is matched on every entry, and an empty token matches nothing: a principal
        // configured without a secret cannot be logged into by presenting nothing.
        let mut config = agentos_core::config::ApiConfig::default();
        config.principals = vec![
            agentos_core::config::PrincipalConfig {
                user_id: "alice".into(),
                token: Some("alice-token".into()),
                ..Default::default()
            },
            agentos_core::config::PrincipalConfig {
                user_id: "ghost".into(),
                token: None,
                ..Default::default()
            },
        ];
        assert!(resolve_principal(&config, None, None).is_err());
        assert!(resolve_principal(&config, Some(""), None).is_err());
    }

    fn open_config(mode: agentos_core::config::AssertedIdentityMode) -> agentos_core::config::ApiConfig {
        let mut config = agentos_core::config::ApiConfig::default();
        config.auth_token_env = "AGENTOS_TEST_UNSET_TOKEN_VAR".into();
        std::env::remove_var("AGENTOS_TEST_UNSET_TOKEN_VAR");
        config.asserted_identity = mode;
        config
    }

    fn claimed(user: &str, node: Option<&str>) -> AssertedIdentity {
        AssertedIdentity { user_id: user.into(), node_id: node.map(|n| n.to_string()) }
    }

    #[test]
    fn an_open_node_honours_a_declared_identity_but_never_makes_it_admin() {
        let config = open_config(agentos_core::config::AssertedIdentityMode::Optional);
        let identity = resolve_principal(&config, None, Some(&claimed("bob", Some("node-b")))).unwrap();
        assert_eq!(identity.principal.user_id, "bob");
        assert_eq!(identity.principal.node_id.as_deref(), Some("node-b"));
        assert_eq!(identity.source, IdentitySource::Asserted);
        // Declared identities can create and own, but they are not the operator: a name nobody
        // checked must not widen what everyone else may do.
        assert!(!identity.principal.is_admin());
        assert!(identity.principal.roles.iter().any(|r| r == ASSERTED_ROLE));
    }

    #[test]
    fn optional_identity_stays_the_operator_when_nothing_is_declared() {
        let config = open_config(agentos_core::config::AssertedIdentityMode::Optional);
        let identity = resolve_principal(&config, None, None).unwrap();
        assert_eq!(identity.principal.user_id, "operator");
        assert_eq!(identity.source, IdentitySource::Operator);
    }

    #[test]
    fn off_ignores_a_declared_identity_entirely() {
        let config = open_config(agentos_core::config::AssertedIdentityMode::Off);
        let identity = resolve_principal(&config, None, Some(&claimed("bob", None))).unwrap();
        assert_eq!(identity.principal.user_id, "operator");
        assert_eq!(identity.source, IdentitySource::Operator);
    }

    #[test]
    fn a_node_that_requires_identity_refuses_a_silent_request() {
        let config = open_config(agentos_core::config::AssertedIdentityMode::Required);
        assert!(matches!(
            resolve_principal(&config, None, None),
            Err(IdentityError::Missing(_))
        ));
        assert!(resolve_principal(&config, None, Some(&claimed("bob", None))).is_ok());
    }

    #[test]
    fn a_declared_identity_is_ignored_when_the_node_has_a_principal_table() {
        // The whole point of the table: a node that can prove identity must not also accept a claim.
        // Otherwise anybody could present a name and skip the token check.
        let mut config = agentos_core::config::ApiConfig::default();
        config.principals = vec![agentos_core::config::PrincipalConfig {
            user_id: "alice".into(),
            token: Some("alice-token".into()),
            ..Default::default()
        }];
        let identity = resolve_principal(&config, Some("alice-token"), Some(&claimed("mallory", None))).unwrap();
        assert_eq!(identity.principal.user_id, "alice");
        assert_eq!(identity.source, IdentitySource::Token);
        // And a tokenless request cannot smuggle one in either.
        assert!(resolve_principal(&config, None, Some(&claimed("mallory", None))).is_err());
    }

    #[test]
    fn an_unusable_declared_name_is_refused_with_a_reason() {
        let config = open_config(agentos_core::config::AssertedIdentityMode::Optional);
        // Reserved: it is the node's own name, and matching it would reach the operator's records.
        assert!(matches!(
            resolve_principal(&config, None, Some(&claimed("Operator", None))),
            Err(IdentityError::Invalid(_))
        ));
        // `@` is the separator in `user@node`, so a name that contains it is ambiguous.
        assert!(resolve_principal(&config, None, Some(&claimed("a@b", None))).is_err());
        assert!(resolve_principal(&config, None, Some(&claimed("  ", None))).is_err());
        assert!(resolve_principal(&config, None, Some(&claimed(&"x".repeat(65), None))).is_err());
        assert!(resolve_principal(&config, None, Some(&claimed("bob", Some("a@b")))).is_err());
    }

    #[test]
    fn a_declared_identity_can_travel_in_the_query_for_the_socket() {
        // A browser cannot set a header on a WebSocket handshake, so the same claim also has to be
        // readable from the query string. The header wins when both are present: they must not be
        // allowed to disagree.
        let query_only = axum::http::Request::builder()
            .uri("/v1/ws?user=alice&node=node-a")
            .body(())
            .unwrap();
        let claimed = asserted_identity(&query_only).unwrap();
        assert_eq!(claimed.user_id, "alice");
        assert_eq!(claimed.node_id.as_deref(), Some("node-a"));

        let both = axum::http::Request::builder()
            .uri("/v1/ws?user=query-name")
            .header("x-agora-user", "header-name")
            .body(())
            .unwrap();
        assert_eq!(asserted_identity(&both).unwrap().user_id, "header-name");

        let neither = axum::http::Request::builder().uri("/v1/ws").body(()).unwrap();
        assert!(asserted_identity(&neither).is_none());
    }
}
