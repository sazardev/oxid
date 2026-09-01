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

use crate::api::parse_branch;
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

/// `GET /api/v1/environments/{env_id}` — one environment by id.
///
/// The route previously only accepted `DELETE`, so fetching a single
/// environment meant listing its whole project and filtering client-side,
/// and callers who guessed the obvious URL got a bare `405` with nothing
/// naming the alternative.
pub async fn get_environment<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(env_id): Path<u64>,
    authed: Option<Extension<AuthedAs>>,
) -> ApiResult<Json<Environment>> {
    let project_id = state
        .cp
        .environment_project_id(EnvironmentId(env_id))
        .await?;
    authorize(&authed, Capability::Read, Some(project_id.0))?;
    let env = state
        .cp
        .find_environment(EnvironmentId(env_id))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("environment `{env_id}`")))?;
    Ok(Json(env))
}

pub async fn list_environments<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
    authed: Option<Extension<AuthedAs>>,
    Query(query): Query<ListEnvironmentsQuery>,
) -> ApiResult<Json<Vec<Environment>>> {
    authorize(&authed, Capability::Read, Some(id))?;
    let envs = state.cp.list_environments(ProjectId(id)).await?;
    let envs = match query.branch {
        // The daemon keeps one row per historical deploy of a branch (audit
        // trail); only the most recent (highest id) one is "the" current
        // environment. Returning every historical row here would let CLI
        // callers such as `oxid down`/`pause`/`wake` act on a stale,
        // already-`Destroyed` deployment instead of the live one.
        Some(raw) => {
            let branch = parse_branch(&raw)?;
            envs.into_iter()
                .filter(|e| e.branch.name == branch)
                .max_by_key(|e| e.id.0)
                .into_iter()
                .collect()
        }
        None => envs,
    };
    Ok(Json(envs))
}

/// The services one environment runs.
///
/// Backs the dashboard's log selector and `oxid logs --service`. Scoped to
/// the environment's own project, like every other read about it — this is
/// not fleet-wide information.
pub async fn list_services<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(env_id): Path<u64>,
    authed: Option<Extension<AuthedAs>>,
) -> ApiResult<Json<Vec<oxid_core::EnvironmentService>>> {
    let project_id = state
        .cp
        .environment_project_id(EnvironmentId(env_id))
        .await?;
    authorize(&authed, Capability::Read, Some(project_id.0))?;
    Ok(Json(
        state.cp.environment_services(EnvironmentId(env_id)).await?,
    ))
}
