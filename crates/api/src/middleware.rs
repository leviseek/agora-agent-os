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

/// The action a request performs on a session, if it performs one.
///
/// A pure function of method and path, so the mapping can be tested without a runtime and read in
/// one place: an endpoint added without an entry here is an endpoint with no permission check, and
/// that is worth being able to see at a glance.
pub fn required_action(method: &axum::http::Method, path: &str) -> Option<(agentos_core::SessionId, agentos_core::model::SessionAction)> {
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
    Some((session, action))
}

/// Resolve the presented token into a principal.
///
/// Returns Ok(None) when the runtime has no principals at all (an open runtime, where every request
/// is the operator). With a table, an unrecognised token is refused rather than defaulted - the one
/// thing a permission model must never do is guess.
pub fn resolve_principal(
    config: &agentos_core::config::ApiConfig,
    presented: Option<&str>,
) -> std::result::Result<Option<agentos_core::model::Principal>, ()> {
    let table = config.resolved_principals();
    if table.is_empty() {
        return Ok(Some(agentos_core::model::Principal::operator()));
    }
    let Some(presented) = presented else {
        // A tokenless request against a runtime that has no tokenless principal.
        let open = table.iter().any(|(token, _)| token.is_empty());
        return if open {
            Ok(Some(table[0].1.clone()))
        } else {
            Err(())
        };
    };
    for (token, principal) in &table {
        if !token.is_empty() && constant_time_eq(presented.as_bytes(), token.as_bytes()) {
            return Ok(Some(principal.clone()));
        }
    }
    Err(())
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

    let principal = if open_without_token {
        None
    } else {
        match resolve_principal(&state.config.api, presented.as_deref()) {
            Ok(principal) => principal,
            Err(()) => {
                metrics().inc_by(metric_names::HTTP_REQUESTS, &[("status", "401")], 1);
                return crate::error::ApiError(RuntimeError::unauthorized(
                    "missing or invalid bearer token",
                ))
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
        if let Some((session, action)) = required_action(req.method(), req.uri().path()) {
            match state.kernel.sessions.get(&session).await {
                Ok(Some(record)) => {
                    if let Err(denial) = agentos_core::model::decide(&record, principal, action) {
                        metrics().inc_by(metric_names::HTTP_REQUESTS, &[("status", "403")], 1);
                        return crate::error::ApiError(
                            RuntimeError::policy_denied(denial.message())
                                .with_detail("action", action.as_str())
                                .with_detail("session_id", session.as_str())
                                .with_detail("owner", record.owner_label())
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
        ];
        for (method, path, expected) in cases {
            let (session, action) = required_action(&method, path)
                .unwrap_or_else(|| panic!("{method} {path} maps to nothing"));
            assert_eq!(session.as_str(), "ses_1");
            assert_eq!(action, expected, "{method} {path}");
        }
    }

    #[test]
    fn creating_a_session_is_not_an_action_on_one() {
        assert!(required_action(&Method::POST, "/v1/sessions").is_none());
        assert!(required_action(&Method::GET, "/v1/sessions").is_none());
        assert!(required_action(&Method::GET, "/v1/meta").is_none());
    }

    #[test]
    fn a_runtime_with_no_principal_table_resolves_every_request_to_the_operator() {
        let mut config = agentos_core::config::ApiConfig::default();
        config.auth_token_env = "AGENTOS_TEST_UNSET_TOKEN_VAR".into();
        std::env::remove_var("AGENTOS_TEST_UNSET_TOKEN_VAR");
        let principal = resolve_principal(&config, None).unwrap().unwrap();
        assert!(principal.is_admin());
        assert_eq!(principal.user_id, "operator");
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
        let alice = resolve_principal(&config, Some("alice-token")).unwrap().unwrap();
        assert_eq!(alice.user_id, "alice");
        assert_eq!(alice.node_id.as_deref(), Some("node-a"));
        // A wrong token is refused, not downgraded to the operator. Guessing is the one thing a
        // permission model must never do.
        assert!(resolve_principal(&config, Some("bob-token")).is_err());
        assert!(resolve_principal(&config, None).is_err());
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
        assert!(resolve_principal(&config, None).is_err());
        assert!(resolve_principal(&config, Some("")).is_err());
    }
}
