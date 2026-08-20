#![allow(
    unused_imports,
    clippy::pedantic,
    clippy::nursery,
    clippy::too_many_lines,
    clippy::empty_line_after_doc_comments,
    clippy::duplicate_mod
)]
//! `HTTP` API and webhook surface (SPEC.md §5: the shared internal API).
//!
//! The CLI, TUI, dashboard and desktop app all consume this router.

use std::any::Any;
use std::convert::Infallible;
use std::path::PathBuf;

use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use oxid_core::{
    AuditFilter, BranchName, ContainerPort, EnvVarScope, Environment, EnvironmentId,
    EnvironmentState, GitPort, PoolError, Project, ProjectId, RepositoryError, StateTransition,
    Ttl,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use tower_governor::GovernorLayer;
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::GlobalKeyExtractor;
use tower_http::catch_panic::CatchPanicLayer;

use crate::request_context::current_request_id;
use crate::{ControlPlane, CpError, DeployOutcome};

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
            CpError::Store(RepositoryError::Storage(m)) => {
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, m)
            }
            CpError::Git(_) | CpError::Oci(_) | CpError::Pool(PoolError::Failure(_)) => {
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
            }
            CpError::InsufficientCapacity(_) | CpError::DeployNotReady(_) => {
                Self::new(StatusCode::UNPROCESSABLE_ENTITY, err.to_string())
            }
            CpError::Proxy(_) => Self::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
