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
use crate::api::middleware::authorize;
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
use oxid_core::services::access::{Capability, Role};
use oxid_core::{
    AuditFilter, BranchName, ContainerPort, EnvVarScope, Environment, EnvironmentId,
    EnvironmentState, GitPort, PoolError, Project, ProjectId, RepositoryError, StateTransition,
    Ttl,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use time::OffsetDateTime;
use tower_governor::GovernorLayer;

/// Body for `POST /api/v1/tokens`.
///
/// `deny_unknown_fields` is load-bearing, not tidiness: serde otherwise
/// ignores a key it doesn't recognise, so `{"projects_ids": [2]}` or any
/// other near-miss spelling quietly minted a token with *no* scope — one
/// carrying the same reach as the master credential — while the caller
/// believed they had restricted it. Rejecting the request is the only
/// answer that can't fail open.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTokenBody {
    /// Human-readable name for the operator this token identifies.
    name: String,
    /// Project ids the token is scoped to. Omitted/`null` = full access
    /// (same reach as the master credential); an empty array is rejected —
    /// a can-do-nothing token is almost certainly a client bug.
    #[serde(default)]
    projects: Option<Vec<u64>>,
    /// What this credential may do: `viewer`, `developer`, `maintainer` or
    /// `admin`.
    ///
    /// Omitting it keeps exactly the power a token had before roles existed
    /// — `maintainer` when scoped to projects, `admin` when not — because an
    /// upgrade must never quietly take away a permission somebody's CI is
    /// relying on. The same rule the `0017` migration applies to rows that
    /// predate it, so a client and a stored token cannot disagree.
    ///
    /// Least privilege is therefore something you *ask* for. `oxid token
    /// create` prints the role it granted, so the permissive default is
    /// visible rather than silent.
    #[serde(default)]
    role: Option<String>,
    /// How long the credential lasts, as a duration (`30d`, `12h`).
    /// Omitted means it never expires.
    #[serde(default)]
    expires_in: Option<String>,
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
    // Not master-only any more: an `admin` credential is the whole point of
    // having roles, and a devops who cannot delegate user management has to
    // keep handing out the master token — which is what the roles exist to
    // stop. A scoped credential still cannot reach this, whatever its role.
    authorize(&authed, Capability::ManageAccess, None)?;
    if body.name.trim().is_empty() {
        return Err(ApiError::from_validation("token name cannot be empty"));
    }
    let role = match body.role.as_deref() {
        None if body.projects.is_some() => Role::Maintainer,
        None => Role::Admin,
        Some(raw) => raw.parse::<Role>().map_err(ApiError::from_validation)?,
    };
    let expires_at = body
        .expires_in
        .as_deref()
        .map(|raw| {
            Ttl::parse(raw)
                .map(|ttl| OffsetDateTime::now_utc().unix_timestamp() + ttl.whole_seconds())
                .map_err(|e| ApiError::from_validation(e.to_string()))
        })
        .transpose()?;
    let (id, token) = state
        .cp
        .create_operator_token(body.name.trim(), body.projects.clone(), role, expires_at)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": id,
            "name": body.name,
            "token": token,
            "projects": body.projects,
            "role": role.as_str(),
            "expires_at": expires_at,
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
    authorize(&authed, Capability::ManageAccess, None)?;
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
    authorize(&authed, Capability::ManageAccess, None)?;
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

/// Body for `PATCH /api/v1/tokens/{id}` — suspending or restoring access.
#[derive(Debug, Deserialize)]
pub struct UpdateTokenBody {
    /// `true` switches the credential off, `false` back on.
    suspended: bool,
}

/// Suspends or restores a named credential without destroying it.
///
/// Revocation is permanent and forces reissuing a token everywhere it is
/// configured; suspension is the reversible one an operator actually wants
/// for somebody on leave, or a contractor between engagements.
pub async fn update_token<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
    authed: Option<Extension<AuthedAs>>,
    Json(body): Json<UpdateTokenBody>,
) -> ApiResult<Json<Value>> {
    authorize(&authed, Capability::ManageAccess, None)?;
    state.cp.set_operator_suspended(id, body.suspended).await?;
    Ok(Json(json!({ "id": id, "suspended": body.suspended })))
}
