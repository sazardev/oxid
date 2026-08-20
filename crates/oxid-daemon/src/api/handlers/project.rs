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
use oxid_core::{
    AuditFilter, BranchName, ContainerPort, EnvVarScope, Environment, EnvironmentId,
    EnvironmentState, GitPort, PoolError, Project, ProjectId, RepositoryError, StateTransition,
    Ttl,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use tower_governor::GovernorLayer;

pub async fn register_project<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Json(body): Json<RegisterBody>,
) -> ApiResult<(StatusCode, Json<Project>)> {
    let project = state
        .cp
        .register_project(std::path::Path::new(&body.repo_dir))
        .await?;
    Ok((StatusCode::CREATED, Json(project)))
}

pub async fn list_projects<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
) -> ApiResult<Json<Vec<Project>>> {
    Ok(Json(state.cp.list_projects().await?))
}

pub async fn delete_project<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
) -> ApiResult<StatusCode> {
    state.cp.delete_project(ProjectId(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Body for `PATCH /api/v1/projects/{id}` — either field can be omitted to
/// leave it unchanged, so a client only sends what it's actually changing.
#[derive(Debug, Deserialize)]
pub struct UpdateProjectBody {
    /// New idle timeout before scale-to-zero pause, e.g. `"45m"`.
    pause_after: Option<String>,
    /// New max lifetime before permanent teardown, e.g. `"3d"`.
    destroy_after: Option<String>,
    /// New git access token for a private repository — an empty string
    /// clears it. Never echoed back by any response (see
    /// `ControlPlane::set_project_git_token`'s doc comment for why it isn't
    /// a field on `Project` at all).
    git_token: Option<String>,
}

/// Updates a project's `pause_after`/`destroy_after` policy and/or its git
/// access token — the settings `oxid.toml` only ever seeds once at first
/// registration, with the dashboard's project settings form as the intended
/// caller for the TTLs, and `oxid configure --git-token`/the dashboard's
/// secrets-style write-only field for the token.
pub async fn update_project<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
    Json(body): Json<UpdateProjectBody>,
) -> ApiResult<Json<Project>> {
    let pause_after = body
        .pause_after
        .map(|raw| Ttl::parse(&raw))
        .transpose()
        .map_err(|e| ApiError::from_validation(e.to_string()))?;
    let destroy_after = body
        .destroy_after
        .map(|raw| Ttl::parse(&raw))
        .transpose()
        .map_err(|e| ApiError::from_validation(e.to_string()))?;
    if let Some(git_token) = body.git_token.as_deref() {
        state
            .cp
            .set_project_git_token(ProjectId(id), Some(git_token).filter(|t| !t.is_empty()))
            .await?;
    }
    let project = state
        .cp
        .update_project_ttls(ProjectId(id), pause_after, destroy_after)
        .await?;
    Ok(Json(project))
}
