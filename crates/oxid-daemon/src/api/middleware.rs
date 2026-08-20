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
use super::ApiState;
use super::REQUEST_ID_HEADER;
use super::error::{ApiError, ApiResult};

/// Reads (or, if absent/blank, generates) this request's `X-Request-Id`,
/// makes it available for the rest of the request's execution via
/// [`current_request_id`] (see `request_context`'s doc comment for why a
/// `tokio::task_local!` rather than an explicit parameter threaded through
/// every `ControlPlane` method), records it into a `tracing` span covering
/// the whole request/response, and echoes it back as a response header —
/// the three places an operator correlates a single request: the response
/// itself, structured logs (`grep request_id=<id>`), and the audit trail
/// (`WHERE request_id = '<id>'`).
pub(crate) async fn request_id_middleware(request: Request, next: Next) -> Response {
    let request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned);

    let method = request.method().clone();
    let uri = request.uri().clone();
    let span = tracing::info_span!("http_request", request_id = %request_id, %method, %uri);

    let mut response = {
        use tracing::Instrument;
        crate::request_context::scope(request_id.clone(), next.run(request))
            .instrument(span)
            .await
    };

    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    response
}

/// Converts a panicking handler into a `500` with the same `{"error": ...}`
/// shape as [`ApiError`], instead of `tower-http`'s default plain-text body
/// (or, without this layer at all, a dropped connection the client sees as
/// a reset). Logs the panic message via `tracing::error!` tagged with this
/// request's id (if any — [`current_request_id`] reads it from the span
/// [`request_id_middleware`] set up, which wraps this layer) so it shows up
/// in the same place every other request failure does.
// `tower_http::catch_panic::ResponseForPanic`'s blanket impl requires this
// exact `Fn(Box<dyn Any + Send>) -> Response` signature (taking the box by
// value) — can't take `&Box<..>` instead as clippy's `pedantic` lint would
// otherwise suggest.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn handle_panic(err: Box<dyn Any + Send + 'static>) -> Response {
    let detail = if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = err.downcast_ref::<&str>() {
        (*s).to_owned()
    } else {
        "unknown panic".to_owned()
    };
    let request_id = current_request_id();
    tracing::error!(
        request_id = request_id.as_deref().unwrap_or("-"),
        panic = %detail,
        "panic in request handler"
    );
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal server error", "request_id": request_id })),
    )
        .into_response()
}

/// Which credential a request authenticated with — attached to
/// [`Request::extensions`] by [`require_bearer_token`] so handlers can
/// attribute audit events to a named operator, or restrict
/// token-management endpoints to the master credential only.
#[derive(Debug, Clone)]
pub enum AuthedAs {
    /// The single shared `OXID_API_TOKEN` — anonymous by design (it's
    /// meant to be one credential for the whole team/CI, not a person).
    Master,
    /// A named, database-issued token (see [`create_token`]).
    Operator(String),
}

/// Rejects requests missing a valid `Authorization: Bearer <token>` header
/// when `state.api_token` is configured; passes everything through
/// unchanged otherwise (open by default, see [`ApiState::api_token`]). A
/// valid token is either the master `OXID_API_TOKEN` or any non-revoked
/// named token issued via `POST /api/v1/tokens`.
pub(crate) async fn require_bearer_token<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(expected) = state.api_token.as_deref() else {
        return next.run(request).await;
    };
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let Some(token) = provided else {
        return ApiError::new(StatusCode::UNAUTHORIZED, "missing or invalid bearer token")
            .into_response();
    };
    if constant_time_eq(token.as_bytes(), expected.as_bytes()) {
        request.extensions_mut().insert(AuthedAs::Master);
        return next.run(request).await;
    }
    match state.cp.find_operator_by_token(token).await {
        Ok(Some(name)) => {
            request.extensions_mut().insert(AuthedAs::Operator(name));
            next.run(request).await
        }
        _ => ApiError::new(StatusCode::UNAUTHORIZED, "missing or invalid bearer token")
            .into_response(),
    }
}

/// The operator name to attribute an audit event to: `None` for the master
/// token (anonymous by design) or an unauthenticated (open API) request.
pub(crate) fn operator_name(authed: Option<&Extension<AuthedAs>>) -> Option<String> {
    match authed {
        Some(Extension(AuthedAs::Operator(name))) => Some(name.clone()),
        _ => None,
    }
}

/// Restricts an endpoint to the master `OXID_API_TOKEN` — used for token
/// management, so any named operator can't mint or revoke other operators'
/// credentials. A no-op when the API has no token configured at all
/// (matches every other endpoint's "open by default" behavior).
pub(crate) fn require_master<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    state: &ApiState<G, O>,
    authed: Option<&Extension<AuthedAs>>,
) -> ApiResult<()> {
    if state.api_token.is_none() {
        return Ok(());
    }
    match authed {
        Some(Extension(AuthedAs::Master)) => Ok(()),
        _ => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "token management requires the master OXID_API_TOKEN",
        )),
    }
}

/// Constant-time byte comparison — a plain `==` on the token would let an
/// attacker learn how many leading bytes matched from response timing.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------------------------------------------------------------------------
// request/response types
// ---------------------------------------------------------------------------
