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
use crate::api::middleware::authorize;
use crate::api::middleware::{AuthedAs, operator_scopes};
use crate::api::types::{RegisterBody, RegistrationSource};
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

pub async fn register_project<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    authed: Option<Extension<AuthedAs>>,
    Json(body): Json<RegisterBody>,
) -> ApiResult<(StatusCode, Json<Project>)> {
    let source = body.into_source()?;
    // `oxid up` registers first on every run (idempotent by repo URL), so a
    // scoped token must still be able to *resolve* its own project here —
    // it just may never CREATE one. In scope: return the project. Out of
    // scope: 404, indistinguishable from "doesn't exist". Brand-new repo:
    // 403 with the reason. The URL form resolves by exact `repo_url` match
    // — deliberately before any clone, so a scoped token can't even make
    // the daemon fetch an arbitrary remote.
    if let Some(scopes) = operator_scopes(authed.as_ref()) {
        let existing = match &source {
            RegistrationSource::Dir { dir, .. } => {
                state.cp.project_for_repo(std::path::Path::new(dir)).await?
            }
            RegistrationSource::Url { url, .. } => state.cp.project_for_repo_url(url).await?,
        };
        match existing {
            Some(project) if scopes.contains(&project.id.0) => {
                return Ok((StatusCode::CREATED, Json(project)));
            }
            Some(_) => return Err(ApiError::not_found("project")),
            None => {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "registering new projects requires an unscoped credential \
                     (the master OXID_API_TOKEN or an unscoped named token)",
                ));
            }
        }
    }
    let project = match source {
        RegistrationSource::Dir {
            dir,
            git_token,
            context,
        } => {
            let project = state
                .cp
                .register_project(std::path::Path::new(&dir), context.as_deref())
                .await?;
            apply_git_token(&state, project.id, git_token).await;
            project
        }
        // The URL form persists its token inside `register_project_by_url`
        // itself — atomically with the create.
        RegistrationSource::Url {
            url,
            git_token,
            context,
        } => {
            state
                .cp
                .register_project_by_url(&url, git_token.as_deref(), context.as_deref())
                .await?
        }
    };
    Ok((StatusCode::CREATED, Json(project)))
}

/// Applies a post-registration `git_token` for the `repo_dir` form (the URL
/// form persists it inside `register_project_by_url` itself, atomically with
/// the create). A no-op unless a non-empty token came along.
async fn apply_git_token<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    state: &ApiState<G, O>,
    project_id: ProjectId,
    git_token: Option<String>,
) {
    if let Some(token) = git_token.as_deref().filter(|t| !t.is_empty()) {
        // Best-effort: the project row exists either way; surface the
        // failure in the response rather than silently dropping it.
        let _ = state
            .cp
            .set_project_git_token(project_id, Some(token))
            .await;
    }
}

pub async fn list_projects<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    authed: Option<Extension<AuthedAs>>,
) -> ApiResult<Json<Vec<Project>>> {
    let projects = state.cp.list_projects().await?;
    // A scoped operator sees only its own projects — listing others would
    // leak their existence (names, repo URLs) even if every per-project
    // endpoint 404s.
    let projects = match operator_scopes(authed.as_ref()) {
        None => projects,
        Some(scopes) => projects
            .into_iter()
            .filter(|p| scopes.contains(&p.id.0))
            .collect(),
    };
    Ok(Json(projects))
}

pub async fn delete_project<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
    authed: Option<Extension<AuthedAs>>,
) -> ApiResult<StatusCode> {
    authorize(&authed, Capability::ManageProject, Some(id))?;
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
    /// Glob patterns a pushed branch must match to deploy. An empty array
    /// clears the allowlist, which means every branch again.
    branches: Option<Vec<String>>,
    /// Glob patterns that refuse a pushed branch outright.
    ignore: Option<Vec<String>>,
    /// Most environments this project may hold. Nested so that omitting the
    /// field and clearing the cap stay distinguishable: absent leaves it
    /// alone, `null` removes it.
    #[serde(default, deserialize_with = "present_or_absent")]
    max_environments: Option<Option<u32>>,
    /// Write-scoped token for this project's git host, used only to post
    /// the preview URL back to a pull request. Empty clears it.
    ///
    /// Separate from `git_token`, which clones: that one may legitimately
    /// be read-only, and quietly requiring it to gain write access to
    /// somebody's issues would be a security regression nobody asked for.
    /// Never echoed back by any response.
    forge_token: Option<String>,
}

/// Distinguishes "field absent" from "field set to null" for an optional
/// field whose value is itself optional.
///
/// Serde collapses both to `None` on a plain `Option`, which would make
/// clearing the cap impossible to express: the request to remove it and the
/// request to leave it alone would arrive identical. The outer `Option` is
/// the presence of the field, the inner one its value.
fn present_or_absent<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(de).map(Some)
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
    authed: Option<Extension<AuthedAs>>,
    Json(body): Json<UpdateProjectBody>,
) -> ApiResult<Json<Project>> {
    authorize(&authed, Capability::ManageProject, Some(id))?;
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
    if let Some(forge_token) = body.forge_token.as_deref() {
        state
            .cp
            .set_project_forge_token(ProjectId(id), Some(forge_token).filter(|t| !t.is_empty()))
            .await?;
    }
    if body.branches.is_some() || body.ignore.is_some() || body.max_environments.is_some() {
        state
            .cp
            .update_project_deploy(
                ProjectId(id),
                body.branches,
                body.ignore,
                body.max_environments,
            )
            .await?;
    }
    let project = state
        .cp
        .update_project_ttls(ProjectId(id), pause_after, destroy_after)
        .await?;
    Ok(Json(project))
}
