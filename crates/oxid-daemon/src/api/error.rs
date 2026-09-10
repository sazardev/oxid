#![allow(
    clippy::pedantic,
    clippy::nursery,
    clippy::too_many_lines,
    clippy::empty_line_after_doc_comments,
    clippy::duplicate_mod
)]

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use oxid_core::{PoolError, RepositoryError};
use serde_json::json;

use crate::CpError;
use crate::request_context::current_request_id;

/// Header carrying the per-request correlation id (see

/// Unified error response.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub(crate) fn from_validation(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    /// A 500 that logs the operator-actionable detail server-side and tells
    /// the client only what it can act on. `handle_panic` already does this
    /// for panics; this is the same contract for errors, so a SQL message or
    /// a filesystem path never crosses the API boundary. The `request_id` in
    /// the response body ties the two together.
    pub(crate) fn internal(detail: impl std::fmt::Display) -> Self {
        tracing::error!(error = %detail, "internal error");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
    }
}

impl From<CpError> for ApiError {
    fn from(err: CpError) -> Self {
        match err {
            CpError::NotFound(m) | CpError::Store(RepositoryError::NotFound(m)) => {
                Self::not_found(m)
            }
            CpError::Config(_)
            | CpError::Domain(_)
            | CpError::Pool(PoolError::NotConfigured(_)) => Self::from_validation(err.to_string()),
            CpError::Store(RepositoryError::Conflict(m)) => Self::new(StatusCode::CONFLICT, m),
            CpError::Store(RepositoryError::Storage(m)) => Self::internal(m),
            CpError::Git(_) | CpError::Oci(_) | CpError::Pool(PoolError::Failure(_)) => {
                Self::internal(err.to_string())
            }
            CpError::InsufficientCapacity(_) | CpError::DeployNotReady(_) => {
                Self::new(StatusCode::UNPROCESSABLE_ENTITY, err.to_string())
            }
            CpError::Validation(_) => Self::from_validation(err.to_string()),
            CpError::Proxy(_) => Self::internal(err.to_string()),
            // 503 for both, and never 404 or 422: the record is intact and
            // the request is fine — the fleet is not. A 404 would tell an
            // operator the branch is gone when its container is very
            // probably still serving traffic, and a 422 would read as "fix
            // your request", which is exactly what the caller must not do.
            // Waiting is the correct response to a drain, a partition or a
            // node that is full.
            CpError::UnknownNode(_) | CpError::NoNodeAvailable(_) => {
                Self::new(StatusCode::SERVICE_UNAVAILABLE, err.to_string())
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // An internal failure gets the same `request_id` correlation the
        // panic handler includes, since "internal server error" alone gives
        // the caller nothing to report.
        if self.status == StatusCode::INTERNAL_SERVER_ERROR {
            return (
                self.status,
                Json(json!({
                    "error": self.message,
                    "request_id": current_request_id(),
                })),
            )
                .into_response();
        }
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
