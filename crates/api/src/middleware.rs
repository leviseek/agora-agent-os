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
pub async fn guard(State(state): State<ApiState>, req: Request, next: Next) -> Response {
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

    if let Some(expected) = state.config.api.auth_token().filter(|_| !open_without_token) {
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
        match presented {
            Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => {}
            _ => {
                metrics().inc_by(metric_names::HTTP_REQUESTS, &[("status", "401")], 1);
                return crate::error::ApiError(RuntimeError::unauthorized("missing or invalid bearer token"))
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
}
