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
