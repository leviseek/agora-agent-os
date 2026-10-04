//! Uniform API error surface: one JSON shape, one status mapping.

use agentos_core::error::RuntimeError;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

pub struct ApiError(pub RuntimeError);

impl From<RuntimeError> for ApiError {
    fn from(e: RuntimeError) -> Self {
        Self(e)
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        Self(RuntimeError::internal(format!("serialization failed: {e}")))
    }
}

impl From<agentos_core::state::TransitionError> for ApiError {
    fn from(e: agentos_core::state::TransitionError) -> Self {
        Self(RuntimeError::conflict(e.to_string()))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = Json(serde_json::json!({
            "error": {
                "code": self.0.code(),
                "message": self.0.message,
                "retryable": self.0.is_retryable(),
                "details": self.0.detail,
            }
        }));
        (status, body).into_response()
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;
