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

pub async fn do_set_secret<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    state: ApiState<G, O>,
    project_id: Option<ProjectId>,
    body: SecretBody,
) -> ApiResult<StatusCode> {
    let scope = parse_scope(&body.scope)?;
    let name = validate_secret_name(&body.name)?;
    let value = body
        .value
        .clone()
        .ok_or_else(|| ApiError::from_validation("secret `value` is required"))?;

    let (pid, branch) = match (project_id, scope) {
        (None, EnvVarScope::Global) => (None, None),
        (Some(_), EnvVarScope::Global) => {
            return Err(ApiError::from_validation(
                "use the global endpoint for `global` scope",
            ));
        }
        (Some(pid), EnvVarScope::Project) => {
            if body.branch.is_some() {
                return Err(ApiError::from_validation(
                    "`branch` is only allowed with `scope = \"branch\"`",
                ));
            }
            (Some(pid), None)
        }
        (Some(pid), EnvVarScope::Branch) => {
            let raw = body.branch.as_deref().ok_or_else(|| {
                ApiError::from_validation("`branch` is required for `scope = \"branch\"`")
            })?;
            (Some(pid), Some(parse_branch(raw)?))
        }
        (None, EnvVarScope::Project | EnvVarScope::Branch) => {
            return Err(ApiError::from_validation(
                "project/branch secrets require a project id",
            ));
        }
        (_, EnvVarScope::Runtime) => {
            return Err(ApiError::from_validation(
                "`runtime` scope cannot be set by clients",
            ));
        }
    };

    state
        .cp
        .set_secret(pid, branch.as_ref(), name, scope, &value)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn do_list_secrets<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    state: ApiState<G, O>,
    project_id: Option<ProjectId>,
    query: SecretListQuery,
) -> ApiResult<Json<Value>> {
    let branch = query.branch.as_deref().map(parse_branch).transpose()?;
    let secrets = state.cp.list_secrets(project_id, branch.as_ref()).await?;
    Ok(Json(json!({
        "secrets": secrets.into_iter().map(|(name, scope)| json!({
            "name": name,
            "scope": scope.to_string(),
        })).collect::<Vec<_>>(),
    })))
}

pub async fn do_delete_secret<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    state: ApiState<G, O>,
    project_id: Option<ProjectId>,
    name: &str,
    query: SecretDeleteQuery,
) -> ApiResult<StatusCode> {
    let name = validate_secret_name(name)?;
    let branch = query.branch.as_deref().map(parse_branch).transpose()?;
    state
        .cp
        .delete_secret(project_id, branch.as_ref(), name)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn set_global_secret<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Json(body): Json<SecretBody>,
) -> ApiResult<StatusCode> {
    do_set_secret(state, None, body).await
}

pub async fn list_global_secrets<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    query: Query<SecretListQuery>,
) -> ApiResult<Json<Value>> {
    do_list_secrets(state, None, query.0).await
}

pub async fn delete_global_secret<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(name): Path<String>,
    query: Query<SecretDeleteQuery>,
) -> ApiResult<StatusCode> {
    do_delete_secret(state, None, &name, query.0).await
}

pub async fn set_project_secret<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
    Json(body): Json<SecretBody>,
) -> ApiResult<StatusCode> {
    do_set_secret(state, Some(ProjectId(id)), body).await
}

pub async fn list_project_secrets<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
    query: Query<SecretListQuery>,
) -> ApiResult<Json<Value>> {
    do_list_secrets(state, Some(ProjectId(id)), query.0).await
}

pub async fn delete_project_secret<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path((id, name)): Path<(u64, String)>,
    query: Query<SecretDeleteQuery>,
) -> ApiResult<StatusCode> {
    do_delete_secret(state, Some(ProjectId(id)), &name, query.0).await
}

fn parse_scope(raw: &str) -> ApiResult<EnvVarScope> {
    match raw {
        "global" => Ok(EnvVarScope::Global),
        "project" => Ok(EnvVarScope::Project),
        "branch" => Ok(EnvVarScope::Branch),
        _ => Err(ApiError::from_validation(format!(
            "invalid scope `{raw}`; expected `global`, `project` or `branch`"
        ))),
    }
}

fn validate_secret_name(name: &str) -> ApiResult<&str> {
    if name.trim().is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(ApiError::from_validation(format!(
            "invalid secret name `{name}`; use alphanumeric characters and underscores"
        )));
    }
    Ok(name)
}

// ---------------------------------------------------------------------------
// webhook handler
// ---------------------------------------------------------------------------
