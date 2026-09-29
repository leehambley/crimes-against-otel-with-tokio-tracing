use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

use crate::chaos::ChaosError;

/// Handler error. Converting it into a response is the one place a failed
/// request is logged, so every failure produces exactly one ERROR line, and
/// that line is also a span event on the request span in Jaeger.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error(transparent)]
    Chaos(#[from] ChaosError),
    #[error("valkey: {0}")]
    Valkey(#[from] redis::RedisError),
    #[error("{peer} failed: {detail}")]
    Upstream { peer: &'static str, detail: String },
}

impl AppError {
    fn status(&self) -> StatusCode {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Chaos(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Valkey(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Upstream { .. } => StatusCode::BAD_GATEWAY,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status.is_server_error() {
            tracing::error!(error = %self, status = status.as_u16(), "request failed");
        } else {
            tracing::debug!(error = %self, status = status.as_u16(), "request rejected");
        }
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}
