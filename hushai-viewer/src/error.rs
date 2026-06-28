//! HTTP error type for the viewer. Maps internal failures to status codes; the
//! body is a small JSON `{ "error": "..." }` so the UI can surface a message.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum ViewerError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("ffmpeg busy")]
    Busy,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<sqlx::Error> for ViewerError {
    fn from(e: sqlx::Error) -> Self {
        ViewerError::Internal(e.into())
    }
}

impl IntoResponse for ViewerError {
    fn into_response(self) -> Response {
        let status = match &self {
            ViewerError::NotFound(_) => StatusCode::NOT_FOUND,
            ViewerError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ViewerError::Busy => StatusCode::SERVICE_UNAVAILABLE,
            ViewerError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if let ViewerError::Internal(e) = &self {
            tracing::error!(error = %e, "viewer internal error");
        }
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

pub type ViewerResult<T> = Result<T, ViewerError>;
