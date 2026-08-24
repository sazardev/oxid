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
use crate::api::middleware::constant_time_eq;
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

/// A push notification normalized across every supported VCS provider:
/// which repository it happened in (matched against registered projects'
/// clone URLs), which branch, and whether the branch itself was deleted.
struct PushEvent {
    repo_hint: String,
    branch: String,
    deleted: bool,
}

/// The shared tail of every webhook provider's handler: resolve the repo
/// hint to a registered project, then either deploy the pushed branch or —
/// when the push was a branch deletion — destroy its environment.
async fn handle_push<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    state: &ApiState<G, O>,
    event: PushEvent,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let project = state
        .cp
        .list_projects()
        .await?
        .into_iter()
        .find(|p| p.repo_url.as_str().contains(&event.repo_hint))
        .ok_or_else(|| {
            ApiError::not_found(format!("no project registered for `{}`", event.repo_hint))
        })?;
    let branch = parse_branch(&event.branch)?;

    if event.deleted {
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
        DeployOutcome::Deployed(env, _report) => Ok((
            StatusCode::ACCEPTED,
            Json(json!({ "status": "deployed", "environment_id": env.id.0 })),
        )),
        DeployOutcome::Queued { position } => Ok((
            StatusCode::ACCEPTED,
            Json(json!({ "status": "queued", "position": position })),
        )),
    }
}

/// The shared head of every webhook provider's handler: the daemon refuses
/// to process any webhook while no secret is configured at all (fail
/// closed — a typo'd env var must not silently open deploys to anyone who
/// can reach the port).
fn require_secret<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    state: &ApiState<G, O>,
) -> ApiResult<&str> {
    state.webhook_secret.as_deref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "webhook secret is not configured; set OXID_WEBHOOK_SECRET",
        )
    })
}

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
    let secret = require_secret(&state)?;
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "missing `X-Hub-Signature-256` header",
            )
        })?;
    verify_prefixed_hmac(secret, &body, signature)?;

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

    // A push with `"deleted": true` means the branch itself was deleted on
    // GitHub, not that new commits landed on it. Deploying it would just
    // fail with a confusing git error ("branch not found") once the cache
    // is refreshed; destroy its environment instead, if it has one.
    let deleted = payload.get("deleted").and_then(Value::as_bool) == Some(true);

    handle_push(
        &state,
        PushEvent {
            repo_hint: full_name.to_owned(),
            branch: branch.to_owned(),
            deleted,
        },
    )
    .await
}

/// GitLab push-webhook handler. Same contract as [`github_webhook`] with
/// GitLab's own conventions: authentication is the plain-text
/// `X-Gitlab-Token` secret echoed back (that's GitLab's whole verification
/// model — there is no HMAC option), the event kind lives in
/// `object_kind`, the repository path in `project.path_with_namespace`,
/// and a deleted branch arrives as a push whose `after` is the null SHA.
pub async fn gitlab_webhook<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let secret = require_secret(&state)?;
    let token = headers
        .get("x-gitlab-token")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            ApiError::new(StatusCode::UNAUTHORIZED, "missing `X-Gitlab-Token` header")
        })?;
    if !constant_time_eq(token.as_bytes(), secret.as_bytes()) {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "token mismatch"));
    }

    let payload: Value = serde_json::from_slice(&body)
        .map_err(|e| ApiError::from_validation(format!("invalid JSON payload: {e}")))?;
    match payload.get("object_kind").and_then(Value::as_str) {
        // GitLab fires many object kinds at one URL (`push`, `tag_push`,
        // `pipeline`, `note`, ...); only branch pushes are acted upon.
        Some("push") => {}
        other => {
            return Ok((
                StatusCode::OK,
                Json(json!({ "status": "ignored", "event": other })),
            ));
        }
    }
    let branch = payload
        .get("ref")
        .and_then(Value::as_str)
        .and_then(strip_refs_heads)
        .ok_or_else(|| ApiError::from_validation("webhook payload is missing `ref`"))?;
    let repo_hint = payload
        .pointer("/project/path_with_namespace")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ApiError::from_validation("webhook payload is missing `project.path_with_namespace`")
        })?;
    let deleted = payload.get("after").and_then(Value::as_str) == Some(GITLAB_BRANCH_DELETED_SHA);

    handle_push(
        &state,
        PushEvent {
            repo_hint: repo_hint.to_owned(),
            branch: branch.to_owned(),
            deleted,
        },
    )
    .await
}

/// What GitLab sends as `after` when a push event actually deletes a
/// branch — forty zeroes instead of the new commit.
const GITLAB_BRANCH_DELETED_SHA: &str = "0000000000000000000000000000000000000000";

/// Gitea push-webhook handler ([`gitea_provider_webhook`] bound to Gitea's
/// header spellings). Gitea/Gogs payloads mirror GitHub's shape (`ref`,
/// `repository.full_name`, `deleted`), but their HMAC signature is sent as
/// bare hex — no `sha256=` prefix.
pub async fn gitea_webhook<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    gitea_provider_webhook(
        &state,
        &headers,
        &body,
        ("x-gitea-signature", "x-gitea-event"),
    )
    .await
}

/// Gogs push-webhook handler — same wire format as Gitea under different
/// header names (Gogs is the project Gitea forked from).
pub async fn gogs_webhook<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    gitea_provider_webhook(
        &state,
        &headers,
        &body,
        ("x-gogs-signature", "x-gogs-event"),
    )
    .await
}

/// Gitea/Gogs family implementation: bare-hex HMAC-SHA256 over the raw
/// body, an event-kind header that must say `push`, and a GitHub-shaped
/// JSON payload.
async fn gitea_provider_webhook<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    state: &ApiState<G, O>,
    headers: &HeaderMap,
    body: &[u8],
    headers_to_read: (&str, &str),
) -> ApiResult<(StatusCode, Json<Value>)> {
    let (signature_header, event_header) = headers_to_read;
    let secret = require_secret(state)?;
    let signature = headers
        .get(signature_header)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                format!("missing `{signature_header}` header"),
            )
        })?;
    verify_bare_hmac(secret, body, signature)?;

    if let Some(event) = headers.get(event_header).and_then(|v| v.to_str().ok())
        && event != "push"
    {
        return Ok((
            StatusCode::OK,
            Json(json!({ "status": "ignored", "event": event })),
        ));
    }

    let payload: Value = serde_json::from_slice(body)
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
    let deleted = payload.get("deleted").and_then(Value::as_bool) == Some(true);

    handle_push(
        state,
        PushEvent {
            repo_hint: full_name.to_owned(),
            branch: branch.to_owned(),
            deleted,
        },
    )
    .await
}

/// Verifies `X-Hub-Signature-256` (`sha256=<hex hmac>`) against the raw body.
fn verify_prefixed_hmac(secret: &str, body: &[u8], signature: &str) -> ApiResult<()> {
    let provided = signature.strip_prefix("sha256=").ok_or_else(|| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "signature must be prefixed with `sha256=`",
        )
    })?;
    verify_bare_hmac(secret, body, provided)
}

/// Verifies a bare-hex HMAC-SHA256 (Gitea/Gogs style, no `sha256=` prefix)
/// against the raw body.
fn verify_bare_hmac(secret: &str, body: &[u8], hex_signature: &str) -> ApiResult<()> {
    let provided_bytes = hex::decode(hex_signature)
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
