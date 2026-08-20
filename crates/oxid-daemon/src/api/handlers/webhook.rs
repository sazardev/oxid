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

/// GitHub push-webhook handler with HMAC-SHA256 signature verification
/// (SPEC.md §4.1). The signature covers the exact raw request body, so the
/// payload is read as bytes and parsed after verification.
pub async fn github_webhook<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let secret = state.webhook_secret.as_deref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "webhook secret is not configured; set OXID_WEBHOOK_SECRET",
        )
    })?;
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "missing `X-Hub-Signature-256` header",
            )
        })?;
    verify_hmac(secret, &body, signature)?;

    // GitHub sends more than `push` to a webhook URL once it's configured —
    // most commonly a `ping` event on setup, which has no `ref` at all and
    // used to fail here with a confusing "missing `ref`" error instead of
    // just being acknowledged. Only `push` (or an unset header, for callers
    // that don't set it, e.g. the existing tests) is actually processed.
    if let Some(event) = headers.get("x-github-event").and_then(|v| v.to_str().ok())
        && event != "push"
    {
        return Ok((
            StatusCode::OK,
            Json(json!({ "status": "ignored", "event": event })),
        ));
    }

    let payload: Value = serde_json::from_slice(&body)
        .map_err(|e| ApiError::from_validation(format!("invalid JSON payload: {e}")))?;
    let branch = payload
        .get("ref")
        .and_then(Value::as_str)
        .and_then(strip_refs_heads)
        .ok_or_else(|| ApiError::from_validation("webhook payload is missing `ref`"))?;
    let full_name = payload
        .get("repository")
        .and_then(|r| r.get("full_name"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ApiError::from_validation("webhook payload is missing `repository.full_name`")
        })?;

    let project = state
        .cp
        .list_projects()
        .await?
        .into_iter()
        .find(|p| p.repo_url.as_str().contains(full_name))
        .ok_or_else(|| ApiError::not_found(format!("no project registered for `{full_name}`")))?;
    let branch = parse_branch(branch)?;

    // A push with `"deleted": true` means the branch itself was deleted on
    // GitHub, not that new commits landed on it. Deploying it would just
    // fail with a confusing git error ("branch not found") once the cache
    // is refreshed; destroy its environment instead, if it has one.
    if payload.get("deleted").and_then(Value::as_bool) == Some(true) {
        return match state
            .cp
            .find_environment_by_branch(project.id, &branch)
            .await?
        {
            Some(env) if env.state != EnvironmentState::Destroyed => {
                state.cp.destroy(env.id, false).await?;
                Ok((
                    StatusCode::OK,
                    Json(json!({ "status": "destroyed", "environment_id": env.id.0 })),
                ))
            }
            _ => Ok((StatusCode::OK, Json(json!({ "status": "ignored" })))),
        };
    }

    match state.cp.deploy_or_queue(project.id, branch, None).await? {
        DeployOutcome::Deployed(env) => Ok((
            StatusCode::ACCEPTED,
            Json(json!({ "status": "deployed", "environment_id": env.id.0 })),
        )),
        DeployOutcome::Queued { position } => Ok((
            StatusCode::ACCEPTED,
            Json(json!({ "status": "queued", "position": position })),
        )),
    }
}

/// Verifies `X-Hub-Signature-256` (`sha256=<hex hmac>`) against the raw body.
fn verify_hmac(secret: &str, body: &[u8], signature: &str) -> ApiResult<()> {
    let provided = signature.strip_prefix("sha256=").ok_or_else(|| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "signature must be prefixed with `sha256=`",
        )
    })?;
    let provided_bytes = hex::decode(provided)
        .map_err(|_| ApiError::new(StatusCode::UNAUTHORIZED, "signature is not valid hex"))?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "invalid secret"))?;
    mac.update(body);
    mac.verify_slice(&provided_bytes)
        .map_err(|_| ApiError::new(StatusCode::UNAUTHORIZED, "signature mismatch"))?;
    Ok(())
}

fn strip_refs_heads(reference: &str) -> Option<&str> {
    reference.strip_prefix("refs/heads/")
}
