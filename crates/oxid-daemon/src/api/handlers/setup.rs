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
use crate::api::middleware::{AuthedAs, require_master};
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

/// Public, pre-auth onboarding probe backing the dashboard's setup wizard:
/// reachable *before* any token exists (that's the point — it tells a
/// fresh browser tab what it needs to obtain). Non-sensitive booleans and
/// the version only; everything node-specific (`infra/status`, `stats`)
/// stays behind auth. `auth_required` distinguishes "paste your token" from
/// "you're on an open loopback daemon"; `webhook_secret_configured` gates
/// the wizard's webhook step; `auto_token` selects the exact retrieval hint
/// (`docker compose logs …`) shown for where the generated token lives.
pub async fn setup_status<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "auth_required": state.api_token.is_some(),
        "auto_token": state.auto_token,
        "webhook_secret_configured": state.webhook_secret.is_some(),
        "allow_restore": state.allow_restore,
    })))
}

/// Reveals the auto-generated master API token, pre-auth. Deliberately
/// mirrors the trust model already documented on [`crate::credential`]: the
/// zero-config path (`OXID_AUTO_TOKEN=1`) writes this same value to a
/// `0600` file in `OXID_DATA_DIR` and prints it once to the container log —
/// anyone who can reach this HTTP endpoint at all can already retrieve it by
/// those channels (`docker compose cp`/`docker compose logs`), so serving it
/// here too adds no new exposure, only removes the friction of finding a
/// shell/volume-access path to fetch it. This is *why* the shipped
/// `docker-compose.yml` publishes the control API on `127.0.0.1` only —
/// widening that to `0.0.0.0` hands this endpoint (and the token file) to
/// anyone on the network, which is a operator-made decision this endpoint
/// can't detect or veto.
///
/// Only ever serves the *auto-generated* value (`state.auto_token`): an
/// operator who explicitly set `OXID_API_TOKEN` already knows it, and this
/// deliberately never echoes an explicitly-configured secret back over
/// HTTP. 404s when there's nothing to hand over (open API, or an explicit
/// token) so the CLI/wizard can fall back to "paste your own".
pub async fn bootstrap_token<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
) -> ApiResult<Json<Value>> {
    if state.auto_token
        && let Some(token) = state.api_token.clone()
    {
        return Ok(Json(json!({ "token": token })));
    }
    Err(ApiError::not_found(
        "no auto-generated token to hand over — either the API is open (no token configured) \
         or OXID_API_TOKEN was set explicitly by the operator; retrieve that value from your \
         own deployment config",
    ))
}

/// Reveals the webhook-signing secret to the **master** credential only —
/// the same trust level as `GET /api/v1/backup`, which already ships the
/// AES master key itself. Named operators (scoped or not) get 403: knowing
/// the webhook secret lets anyone forge pushes that deploy arbitrary
/// branches. Returns 404 when nothing is configured so the wizard can say
/// "set `OXID_WEBHOOK_SECRET` (or enable `OXID_AUTO_TOKEN`)" instead of
/// showing an empty box.
pub async fn webhook_secret<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    authed: Option<Extension<AuthedAs>>,
) -> ApiResult<Json<Value>> {
    require_master(&state, authed.as_ref())?;
    match state.webhook_secret.clone() {
        Some(secret) => Ok(Json(json!({ "webhook_secret": secret }))),
        None => Err(ApiError::not_found(
            "no webhook secret is configured — set OXID_WEBHOOK_SECRET \
             (or start with OXID_AUTO_TOKEN=1 to generate one)",
        )),
    }
}
