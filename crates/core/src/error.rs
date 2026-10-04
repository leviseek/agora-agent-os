//! One error taxonomy for the whole runtime.
//!
//! Every failure crossing a module boundary is normalized into RuntimeError. The kind drives
//! three decisions in exactly one place: whether to retry, what HTTP status to surface, and
//! whether the caller or the runtime is at fault.

use serde::{Deserialize, Serialize};
use std::fmt;

pub type Result<T, E = RuntimeError> = std::result::Result<T, E>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Caller supplied something we cannot accept. Never retry.
    InvalidInput,
    /// Referenced entity does not exist.
    NotFound,
    /// Optimistic-concurrency / duplicate-creation conflict.
    Conflict,
    /// Missing or invalid credentials.
    Unauthorized,
    /// Authenticated but not allowed by policy. Never retry, always audit.
    PolicyDenied,
    /// Edge throttling.
    RateLimited,
    /// Deadline exceeded. Retryable with backoff.
    Timeout,
    /// Cooperative cancellation (client disconnect, shutdown, supersede).
    Cancelled,
    /// A capability failed. Retryable only if the capability says so.
    Capability,
    /// A model provider failed. Retryable.
    Model,
    /// Storage backend failure. Retryable.
    Storage,
    /// Transport / peer failure. Retryable.
    Network,
    /// Wasm sandbox trap, resource limit or permission violation.
    Sandbox,
    /// Migration / checkpoint / replay failure.
    Migration,
    /// Invariant broken. Retryable only after restart.
    Internal,
    /// Dependency temporarily unavailable (worker offline, queue full).
    Unavailable,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::InvalidInput => "invalid_input",
            ErrorKind::NotFound => "not_found",
            ErrorKind::Conflict => "conflict",
            ErrorKind::Unauthorized => "unauthorized",
            ErrorKind::PolicyDenied => "policy_denied",
            ErrorKind::RateLimited => "rate_limited",
            ErrorKind::Timeout => "timeout",
            ErrorKind::Cancelled => "cancelled",
            ErrorKind::Capability => "capability",
            ErrorKind::Model => "model",
            ErrorKind::Storage => "storage",
            ErrorKind::Network => "network",
            ErrorKind::Sandbox => "sandbox",
            ErrorKind::Migration => "migration",
            ErrorKind::Internal => "internal",
            ErrorKind::Unavailable => "unavailable",
        }
    }

    /// Default retryability. Individual errors may override.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            ErrorKind::Timeout
                | ErrorKind::Model
                | ErrorKind::Storage
                | ErrorKind::Network
                | ErrorKind::Unavailable
                | ErrorKind::Capability
                | ErrorKind::Migration
        )
    }

    /// True when the caller can fix the problem by changing the request.
    pub fn client_fault(self) -> bool {
        matches!(
            self,
            ErrorKind::InvalidInput
                | ErrorKind::NotFound
                | ErrorKind::Conflict
                | ErrorKind::Unauthorized
                | ErrorKind::PolicyDenied
                | ErrorKind::RateLimited
        )
    }

    pub fn http_status(self) -> u16 {
        match self {
            ErrorKind::InvalidInput => 400,
            ErrorKind::Unauthorized => 401,
            ErrorKind::PolicyDenied => 403,
            ErrorKind::NotFound => 404,
            ErrorKind::Conflict => 409,
            ErrorKind::RateLimited => 429,
            ErrorKind::Cancelled => 499,
            ErrorKind::Timeout => 504,
            ErrorKind::Unavailable => 503,
            _ => 500,
        }
    }
}

/// Canonical runtime error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeError {
    pub kind: ErrorKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl RuntimeError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), detail: None, retryable: None, source: None }
    }

    pub fn invalid_input(m: impl Into<String>) -> Self { Self::new(ErrorKind::InvalidInput, m) }
    pub fn not_found(m: impl Into<String>) -> Self { Self::new(ErrorKind::NotFound, m) }
    pub fn conflict(m: impl Into<String>) -> Self { Self::new(ErrorKind::Conflict, m) }
    pub fn unauthorized(m: impl Into<String>) -> Self { Self::new(ErrorKind::Unauthorized, m) }
    pub fn policy_denied(m: impl Into<String>) -> Self { Self::new(ErrorKind::PolicyDenied, m) }
    pub fn rate_limited(m: impl Into<String>) -> Self { Self::new(ErrorKind::RateLimited, m) }
    pub fn timeout(m: impl Into<String>) -> Self { Self::new(ErrorKind::Timeout, m) }
    pub fn cancelled(m: impl Into<String>) -> Self { Self::new(ErrorKind::Cancelled, m) }
    pub fn capability(m: impl Into<String>) -> Self { Self::new(ErrorKind::Capability, m) }
    pub fn model(m: impl Into<String>) -> Self { Self::new(ErrorKind::Model, m) }
    pub fn storage(m: impl Into<String>) -> Self { Self::new(ErrorKind::Storage, m) }
    pub fn network(m: impl Into<String>) -> Self { Self::new(ErrorKind::Network, m) }
    pub fn sandbox(m: impl Into<String>) -> Self { Self::new(ErrorKind::Sandbox, m) }
    pub fn migration(m: impl Into<String>) -> Self { Self::new(ErrorKind::Migration, m) }
    pub fn internal(m: impl Into<String>) -> Self { Self::new(ErrorKind::Internal, m) }
    pub fn unavailable(m: impl Into<String>) -> Self { Self::new(ErrorKind::Unavailable, m) }

    pub fn with_detail(mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        let obj = self.detail.get_or_insert_with(|| serde_json::json!({}));
        if let Some(map) = obj.as_object_mut() {
            map.insert(key.into(), value.into());
        }
        self
    }

    pub fn with_source(mut self, src: impl fmt::Display) -> Self {
        self.source = Some(src.to_string());
        self
    }

    pub fn retryable(mut self, yes: bool) -> Self {
        self.retryable = Some(yes);
        self
    }

    pub fn is_retryable(&self) -> bool {
        self.retryable.unwrap_or_else(|| self.kind.retryable())
    }

    pub fn code(&self) -> &'static str {
        self.kind.as_str()
    }

    pub fn http_status(&self) -> u16 {
        self.kind.http_status()
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.message)?;
        if let Some(src) = &self.source {
            write!(f, " (caused by: {src})")?;
        }
        Ok(())
    }
}

impl std::error::Error for RuntimeError {}

impl From<std::io::Error> for RuntimeError {
    fn from(e: std::io::Error) -> Self {
        RuntimeError::storage(e.to_string()).with_source(e)
    }
}

impl From<serde_json::Error> for RuntimeError {
    fn from(e: serde_json::Error) -> Self {
        RuntimeError::internal(format!("json error: {e}"))
    }
}

impl From<String> for RuntimeError {
    fn from(e: String) -> Self {
        RuntimeError::internal(e)
    }
}

/// Attach human context to a fallible operation while preserving the machine-readable kind.
pub trait ResultExt<T> {
    fn ctx(self, msg: impl Into<String>) -> Result<T>;
    fn ctx_with(self, key: &str, value: impl Into<serde_json::Value>, msg: impl Into<String>) -> Result<T>;
}

impl<T> ResultExt<T> for Result<T> {
    fn ctx(self, msg: impl Into<String>) -> Result<T> {
        self.map_err(|e| {
            let msg = msg.into();
            RuntimeError { message: format!("{msg}: {}", e.message), ..e }
        })
    }
    fn ctx_with(self, key: &str, value: impl Into<serde_json::Value>, msg: impl Into<String>) -> Result<T> {
        self.map_err(|e| {
            let msg = msg.into();
            RuntimeError { message: format!("{msg}: {}", e.message), ..e }.with_detail(key, value)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_policy_is_category_driven() {
        assert!(RuntimeError::timeout("slow").is_retryable());
        assert!(!RuntimeError::invalid_input("bad").is_retryable());
        assert!(!RuntimeError::policy_denied("no").is_retryable());
        assert!(!RuntimeError::capability("x").retryable(false).is_retryable());
    }

    #[test]
    fn http_mapping_is_stable() {
        assert_eq!(RuntimeError::not_found("x").http_status(), 404);
        assert_eq!(RuntimeError::policy_denied("x").http_status(), 403);
        assert_eq!(RuntimeError::cancelled("x").http_status(), 499);
    }

    #[test]
    fn context_preserves_kind() {
        let e: Result<()> = Err(RuntimeError::not_found("session"));
        let e = e.ctx("lookup failed").unwrap_err();
        assert_eq!(e.kind, ErrorKind::NotFound);
        assert!(e.message.contains("lookup failed"));
    }
}
