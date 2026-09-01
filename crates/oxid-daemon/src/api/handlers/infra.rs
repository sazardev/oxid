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

use crate::api::ApiState;
use crate::api::error::{ApiError, ApiResult};
use crate::api::middleware::AuthedAs;
use crate::api::middleware::authorize;
use crate::api::types::{
    AuditQuery, DeployBody, ListEnvironmentsQuery, RegisterBody, RollbackBody, SecretBody,
    SecretDeleteQuery, SecretListQuery,
};
use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self as axum_middleware, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use oxid_core::services::access::Capability;
use oxid_core::{
    AuditFilter, BranchName, ContainerPort, EnvVarScope, Environment, EnvironmentId,
    EnvironmentState, GitPort, PoolError, Project, ProjectId, RepositoryError, StateTransition,
    Ttl,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use tower_governor::GovernorLayer;

pub async fn stats<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    authed: Option<Extension<AuthedAs>>,
) -> ApiResult<Json<crate::NodeStats>> {
    authorize(&authed, Capability::ManageNode, None)?;
    Ok(Json(state.cp.node_stats().await?))
}

/// Traefik's HTTP provider polls this.
///
/// Authenticated like every other protected route — the document names
/// every branch on this daemon and where to reach it, so it is not public.
/// Traefik sends the header via `--providers.http.headers.Authorization`.
///
/// ETagged, because polling is what this endpoint is *for*: Traefik asks
/// every few seconds forever, and a fleet's worth of routers rebuilt and
/// re-serialised on each poll is real work to answer "nothing changed". A
/// hash of the rendered document rather than a version counter, so it stays
/// correct with several daemons and across a restart.
pub async fn traefik_config<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    authed: Option<Extension<AuthedAs>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    authorize(&authed, Capability::ManageNode, None)?;

    let config = state.cp.traefik_dynamic_config().await?;
    let body = serde_json::to_string(&config)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let etag = format!(
        "\"{:x}\"",
        <Sha256 as sha2::Digest>::digest(body.as_bytes())
    );
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == etag)
    {
        // 304 carries no body by definition, and Traefik keeps whatever it
        // already had — which is the correct configuration, since the hash
        // it sent is the hash of what we would have sent back.
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }

    Ok((
        StatusCode::OK,
        [
            (header::ETAG, etag),
            (header::CONTENT_TYPE, "application/json".to_owned()),
        ],
        body,
    )
        .into_response())
}

/// Read-only: never creates or changes anything, just reports whether the
/// Docker network/Traefik container/self-wiring the operator would
/// otherwise have to set up by hand are actually in place.
pub async fn infra_status<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    authed: Option<Extension<AuthedAs>>,
) -> ApiResult<Json<crate::InfraStatus>> {
    authorize(&authed, Capability::ManageNode, None)?;
    Ok(Json(state.cp.infra_status().await?))
}

/// Idempotent and safe to re-run: creates the Docker network/Traefik
/// container only if missing, then reports the same shape as
/// `GET /api/v1/infra/status`.
pub async fn infra_bootstrap<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    authed: Option<Extension<AuthedAs>>,
) -> ApiResult<Json<crate::InfraStatus>> {
    authorize(&authed, Capability::ManageNode, None)?;
    Ok(Json(state.cp.infra_bootstrap().await?))
}
