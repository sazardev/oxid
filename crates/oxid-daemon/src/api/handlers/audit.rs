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
use crate::api::DEFAULT_AUDIT_LIMIT;
use crate::api::error::{ApiError, ApiResult};
use crate::api::middleware::{AuthedAs, authorize_project, operator_scopes};
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

pub async fn recent_audit<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    authed: Option<Extension<AuthedAs>>,
    Query(query): Query<AuditQuery>,
) -> ApiResult<Json<Vec<oxid_core::AuditEvent>>> {
    let limit = query.limit.unwrap_or(DEFAULT_AUDIT_LIMIT);
    let mut filter = query.into_filter()?;
    filter.limit = Some(filter.limit.unwrap_or(DEFAULT_AUDIT_LIMIT));
    // A scoped operator either names one of its own projects explicitly, or
    // gets the merged trail of exactly its projects — never anyone else's.
    match operator_scopes(authed.as_ref()) {
        None => Ok(Json(state.cp.recent_audit_events(&filter).await?)),
        Some(scopes) => {
            if let Some(project_id) = filter.project_id {
                authorize_project(&authed, project_id)?;
                return Ok(Json(state.cp.recent_audit_events(&filter).await?));
            }
            let mut events = Vec::new();
            for id in scopes {
                filter.project_id = Some(ProjectId(*id));
                events.extend(state.cp.recent_audit_events(&filter).await?);
            }
            events.sort_by_key(|event| std::cmp::Reverse(event.occurred_at));
            events.truncate(limit as usize);
            Ok(Json(events))
        }
    }
}

/// Lists deploys currently waiting for host capacity (see
/// [`ControlPlane::deploy_or_queue`]), oldest first. Scoped operators only
/// see their own projects' entries.
pub async fn list_queue<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    authed: Option<Extension<AuthedAs>>,
) -> ApiResult<Json<Vec<crate::adapter::store::QueuedDeploy>>> {
    let queue = state.cp.list_deploy_queue().await?;
    let queue = match operator_scopes(authed.as_ref()) {
        None => queue,
        Some(scopes) => queue
            .into_iter()
            .filter(|entry| scopes.contains(&entry.project_id.0))
            .collect(),
    };
    Ok(Json(queue))
}

pub async fn environment_audit<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(env_id): Path<u64>,
    authed: Option<Extension<AuthedAs>>,
    Query(query): Query<AuditQuery>,
) -> ApiResult<Json<Vec<oxid_core::AuditEvent>>> {
    let project_id = state
        .cp
        .environment_project_id(EnvironmentId(env_id))
        .await?;
    authorize_project(&authed, project_id)?;
    let filter = query.into_filter()?;
    Ok(Json(
        state
            .cp
            .audit_events_for(EnvironmentId(env_id), &filter)
            .await?,
    ))
}
