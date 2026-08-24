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
use crate::api::middleware::AuthedAs;
use crate::api::middleware::require_master;
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

/// Body for `POST /api/v1/tokens`.
#[derive(Debug, Deserialize)]
pub struct CreateTokenBody {
    /// Human-readable name for the operator this token identifies.
    name: String,
    /// Project ids the token is scoped to. Omitted/`null` = full access
    /// (same reach as the master credential); an empty array is rejected —
    /// a can-do-nothing token is almost certainly a client bug.
    #[serde(default)]
    projects: Option<Vec<u64>>,
}

/// Mints a named token, master-token-only. The raw token is only ever
/// returned in this response — only its hash is persisted.
pub async fn create_token<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    authed: Option<Extension<AuthedAs>>,
    Json(body): Json<CreateTokenBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    require_master(&state, authed.as_ref())?;
    if body.name.trim().is_empty() {
        return Err(ApiError::from_validation("token name cannot be empty"));
    }
    let (id, token) = state
        .cp
        .create_operator_token(body.name.trim(), body.projects.clone())
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": id,
            "name": body.name,
            "token": token,
            "projects": body.projects,
        })),
    ))
}

/// Lists every named token (never the raw value or its hash),
/// master-token-only.
pub async fn list_tokens<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    authed: Option<Extension<AuthedAs>>,
) -> ApiResult<Json<Vec<crate::adapter::store::ApiTokenSummary>>> {
    require_master(&state, authed.as_ref())?;
    Ok(Json(state.cp.list_operator_tokens().await?))
}

/// Revokes a named token by id, master-token-only.
pub async fn revoke_token<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
    authed: Option<Extension<AuthedAs>>,
) -> ApiResult<StatusCode> {
    require_master(&state, authed.as_ref())?;
    state.cp.revoke_operator_token(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Rotates the master encryption key: generates a fresh random key,
/// re-encrypts every secret and swaps it in with zero downtime (see
/// [`crate::ControlPlane::rotate_master_key`]), then persists it to
/// `secret.key`. Master-token-only, since a bad rotation can lock every
/// secret away.
///
/// If the daemon was started with `OXID_MASTER_KEY` set instead of a
/// `secret.key` file, that environment variable still wins on the next
/// restart — the response's `note` field calls this out so the caller
/// doesn't get a nasty surprise after a redeploy.
pub async fn rotate_key<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    authed: Option<Extension<AuthedAs>>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    require_master(&state, authed.as_ref())?;
    let mut new_key = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut new_key);
    state.cp.rotate_master_key(new_key).await?;

    let key_path = state.data_dir.join("secret.key");
    if let Err(e) = std::fs::write(&key_path, new_key) {
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "rotated every secret in the database, but failed to persist the new key to \
                 `{}`: {e}. Do NOT restart the daemon until this is fixed, or it will load the \
                 old key and be unable to decrypt any secret. The new key is `{}` — set \
                 OXID_MASTER_KEY to it explicitly if you must restart now.",
                key_path.display(),
                hex::encode(new_key)
            ),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
    }

    Ok((
        StatusCode::OK,
        Json(json!({
            "status": "rotated",
            "note": "already live; if OXID_MASTER_KEY is set on this daemon instead of relying \
                     on secret.key, update it too before the next restart",
        })),
    ))
}
