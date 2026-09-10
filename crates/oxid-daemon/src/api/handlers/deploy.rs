#![allow(
    clippy::pedantic,
    clippy::nursery,
    clippy::too_many_lines,
    clippy::empty_line_after_doc_comments,
    clippy::duplicate_mod
)]

use crate::api::parse_branch;

use crate::DeployOutcome;
use crate::api::ApiState;
use crate::api::error::ApiResult;
use crate::api::middleware::AuthedAs;
use crate::api::middleware::authorize;
use crate::api::middleware::operator_name;
use crate::api::types::{DeployBody, RollbackBody};
use axum::Json;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use oxid_core::services::access::Capability;
use oxid_core::{ContainerPort, Environment, GitPort, ProjectId};
use serde_json::{Value, json};

pub async fn deploy<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
    authed: Option<Extension<AuthedAs>>,
    Json(body): Json<DeployBody>,
) -> ApiResult<Response> {
    authorize(&authed, Capability::Operate, Some(id))?;
    let branch = parse_branch(&body.branch)?;
    match state
        .cp
        .deploy_or_queue(ProjectId(id), branch, operator_name(authed.as_ref()))
        .await?
    {
        DeployOutcome::Deployed(env, report) => Ok((
            StatusCode::CREATED,
            Json({
                let mut body = environment_with_report(&env, &report);
                // The scheme is a property of the proxy, and only the daemon
                // knows whether certificates are configured. Sending it means
                // the CLI prints a URL a person can paste rather than a bare
                // hostname it had to guess at.
                body["url_scheme"] = json!(state.cp.routing_scheme());
                body
            }),
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
    authorize(&authed, Capability::Operate, Some(id))?;
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
