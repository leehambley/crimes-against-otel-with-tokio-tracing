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
    /// Low-cardinality `error.type` for spans and metrics.
    pub fn error_type(&self) -> String {
        match self {
            Self::NotFound => "not_found".into(),
            Self::BadRequest(_) => "bad_request".into(),
            Self::Chaos(c) => c.error_type.into(),
            Self::Valkey(e) => e
                .code()
                .map_or_else(|| format!("{:?}", e.kind()), str::to_owned),
            Self::Upstream { .. } => "upstream".into(),
        }
    }

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
            tracing::error!(error = %self, status = i64::from(status.as_u16()), "request failed");
        } else {
            tracing::debug!(error = %self, status = i64::from(status.as_u16()), "request rejected");
        }
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}
