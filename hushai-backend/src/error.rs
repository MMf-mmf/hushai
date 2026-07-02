//! Typed ingest errors and their HTTP status mapping (contract §6).

use axum::extract::multipart::MultipartError;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    // ---- 400 Bad Request: malformed request the client must fix ----
    #[error("missing multipart part: {0}")]
    MissingPart(&'static str),
    #[error("malformed multipart request: {0}")]
    MalformedMultipart(String),
    #[error("invalid {field} length: expected {expected} bytes, got {got}")]
    InvalidIdLength {
        field: &'static str,
        expected: usize,
        got: usize,
    },
    #[error("manifest field {0} out of representable range")]
    ValueOutOfRange(&'static str),
    #[error("empty required manifest field: {0}")]
    EmptyField(&'static str),
    #[error("failed to decode SegmentManifest: {0}")]
    ManifestDecode(#[from] prost::DecodeError),

    #[error("bad request: {0}")]
    BadRequest(String),

    // ---- 401 Unauthorized ----
    #[error("missing or invalid bearer token")]
    Unauthorized,

    // ---- 404 Not Found ----
    #[error("not found: {0}")]
    NotFound(&'static str),

    // ---- 413 Payload Too Large ----
    #[error("request body exceeds the configured limit")]
    PayloadTooLarge,

    // ---- 422 Unprocessable: well-formed but cannot be accepted as sent ----
    #[error("integrity mismatch on {0}")]
    IntegrityMismatch(&'static str),
    #[error("segment_id reused for different bytes")]
    IdempotencyConflict,

    // ---- 409 Conflict: collides with immutable stored state; re-sending cannot help ----
    #[error("(session_id, stream_id, sequence) already used by a different segment_id")]
    SequenceConflict,

    // ---- 429 Too Many Requests ----
    #[error("backend overloaded")]
    Overloaded,

    // ---- 507 Insufficient Storage ----
    #[error("storage pressure: insufficient free space")]
    StoragePressure,
    // ---- 503 Service Unavailable (transient overload) ----
    #[error("database pool exhausted")]
    PoolExhausted,

    // ---- 500 ----
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl IngestError {
    pub fn status(&self) -> StatusCode {
        use IngestError::*;
        match self {
            MissingPart(_)
            | MalformedMultipart(_)
            | InvalidIdLength { .. }
            | ValueOutOfRange(_)
            | EmptyField(_)
            | ManifestDecode(_)
            | BadRequest(_) => StatusCode::BAD_REQUEST,
            Unauthorized => StatusCode::UNAUTHORIZED,
            NotFound(_) => StatusCode::NOT_FOUND,
            PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            IntegrityMismatch(_) | IdempotencyConflict => StatusCode::UNPROCESSABLE_ENTITY,
            // A new segment_id reusing a taken (session, stream, sequence) is a
            // permanent client ordering error, not a re-send-able 422.
            SequenceConflict => StatusCode::CONFLICT,
            Overloaded => StatusCode::TOO_MANY_REQUESTS,
            // Disk-full is genuinely 507; a pool timeout is transient overload → 503 (not 507, which
            // would collide with real StoragePressure on the ops dashboard). Client treats both as retryable.
            StoragePressure => StatusCode::INSUFFICIENT_STORAGE,
            PoolExhausted => StatusCode::SERVICE_UNAVAILABLE,
            Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for IngestError {
    fn into_response(self) -> Response {
        let status = self.status();
        // 5xx is our fault; log the detail but don't leak it. 4xx is the client's
        // fault; the message is safe and useful to return.
        if status.is_server_error() {
            tracing::error!(error = %self, "request failed (server error)");
            (status, "internal error").into_response()
        } else {
            tracing::warn!(error = %self, %status, "request rejected");
            (status, self.to_string()).into_response()
        }
    }
}

impl From<MultipartError> for IngestError {
    fn from(e: MultipartError) -> Self {
        // axum surfaces the body-limit breach as a 413-status MultipartError;
        // preserve that, otherwise treat as a malformed multipart (400).
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            IngestError::PayloadTooLarge
        } else {
            IngestError::MalformedMultipart(e.to_string())
        }
    }
}

impl From<sqlx::Error> for IngestError {
    fn from(e: sqlx::Error) -> Self {
        match e {
            sqlx::Error::PoolTimedOut => IngestError::PoolExhausted,
            other => IngestError::Internal(other.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping_covers_contract_codes() {
        let cases: &[(IngestError, StatusCode)] = &[
            (IngestError::MissingPart("body"), StatusCode::BAD_REQUEST),
            (
                IngestError::MalformedMultipart("x".into()),
                StatusCode::BAD_REQUEST,
            ),
            (
                IngestError::InvalidIdLength {
                    field: "segment_id",
                    expected: 16,
                    got: 15,
                },
                StatusCode::BAD_REQUEST,
            ),
            (
                IngestError::ValueOutOfRange("sequence"),
                StatusCode::BAD_REQUEST,
            ),
            (
                IngestError::EmptyField("device_id"),
                StatusCode::BAD_REQUEST,
            ),
            (IngestError::Unauthorized, StatusCode::UNAUTHORIZED),
            (IngestError::NotFound("speaker"), StatusCode::NOT_FOUND),
            (
                IngestError::BadRequest("bad".into()),
                StatusCode::BAD_REQUEST,
            ),
            (IngestError::PayloadTooLarge, StatusCode::PAYLOAD_TOO_LARGE),
            (
                IngestError::IntegrityMismatch("content_sha256"),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                IngestError::IdempotencyConflict,
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (IngestError::SequenceConflict, StatusCode::CONFLICT),
            (IngestError::Overloaded, StatusCode::TOO_MANY_REQUESTS),
            (
                IngestError::StoragePressure,
                StatusCode::INSUFFICIENT_STORAGE,
            ),
            (IngestError::PoolExhausted, StatusCode::SERVICE_UNAVAILABLE),
        ];
        for (err, want) in cases {
            assert_eq!(err.status(), *want, "wrong status for {err:?}");
        }
        assert_eq!(
            IngestError::Internal(anyhow::anyhow!("boom")).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
