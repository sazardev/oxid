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

use crate::DeployOutcome;
use crate::api::ApiState;
use crate::api::error::{ApiError, ApiResult};
use crate::api::middleware::AuthedAs;
use crate::api::middleware::operator_name;
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

pub async fn deploy<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
    authed: Option<Extension<AuthedAs>>,
    Json(body): Json<DeployBody>,
) -> ApiResult<Response> {
    let branch = parse_branch(&body.branch)?;
    match state
        .cp
        .deploy_or_queue(ProjectId(id), branch, operator_name(authed.as_ref()))
        .await?
    {
        DeployOutcome::Deployed(env, report) => Ok((
            StatusCode::CREATED,
            Json(environment_with_report(&env, &report)),
        )
            .into_response()),
        DeployOutcome::Queued { position } => Ok((
            StatusCode::ACCEPTED,
            Json(json!({ "status": "queued", "position": position })),
        )
            .into_response()),
    }
}

/// Serializes an `Environment` and merges the [`DeployReport`] into the
/// same JSON object as sibling keys — `"build"` (duration/cache stats) and
/// `"dependencies"` (human-readable provisioning lines). Consumers that
/// deserialize strictly into `Environment` keep working (serde ignores
/// unknown fields); the CLI reads the extras for DESIGN.md §3.3's
/// "Cache hit: N%" / created-database output.
fn environment_with_report(env: &Environment, report: &crate::DeployReport) -> Value {
    let mut body = match serde_json::to_value(env) {
        Ok(value) => value,
        // An `Environment` is plain data; this arm is unreachable in
        // practice but a failed serialize must not 500 with no detail.
        Err(e) => {
            return json!({ "error": format!("cannot serialize environment: {e}") });
        }
    };
    body["build"] = json!({
        "duration_ms": report.build.duration_ms,
        "steps_total": report.build.steps_total,
        "steps_cached": report.build.steps_cached,
        "cache_hit_percent": report.build.cache_hit_percent(),
    });
    body["dependencies"] = json!(report.dependencies);
    body
}

pub async fn rollback<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
    authed: Option<Extension<AuthedAs>>,
    Json(body): Json<RollbackBody>,
) -> ApiResult<Response> {
    let branch = parse_branch(&body.branch)?;
    let (env, report) = state
        .cp
        .rollback_with_operator(
            ProjectId(id),
            branch,
            body.to_sha,
            operator_name(authed.as_ref()),
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(environment_with_report(&env, &report)),
    )
        .into_response())
}
