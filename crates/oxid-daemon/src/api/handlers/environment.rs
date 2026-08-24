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
use crate::api::middleware::{AuthedAs, authorize_project};
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

pub async fn list_environments<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
    authed: Option<Extension<AuthedAs>>,
    Query(query): Query<ListEnvironmentsQuery>,
) -> ApiResult<Json<Vec<Environment>>> {
    authorize_project(&authed, ProjectId(id))?;
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
