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

/// Streams a `.tar` snapshot of `/data`: a consistent point-in-time copy of
/// `audit.sqlite` (via `VACUUM INTO`, safe against the live pool) plus
/// `secret.key`. `git-cache/` is deliberately excluded — it's re-clonable
/// and can be large; restoring just re-clones on the next deploy.
pub async fn backup<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
) -> ApiResult<Response> {
    let snapshot_path = state
        .data_dir
        .join(format!(".backup-snapshot-{}.sqlite", std::process::id()));
    // `VACUUM INTO` fails if the destination already exists — a leftover
    // from a prior crashed backup attempt would otherwise wedge every
    // future one permanently.
    let _ = std::fs::remove_file(&snapshot_path);
    state.cp.backup_database(&snapshot_path).await?;

    let secret_key_path = state.data_dir.join("secret.key");
    let tar_bytes = (|| -> std::io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut buf);
            builder.append_path_with_name(&snapshot_path, "audit.sqlite")?;
            if secret_key_path.exists() {
                builder.append_path_with_name(&secret_key_path, "secret.key")?;
            }
            builder.finish()?;
        }
        Ok(buf)
    })();
    let _ = std::fs::remove_file(&snapshot_path);
    let tar_bytes = tar_bytes.map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cannot build backup archive: {e}"),
        )
    })?;

    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/x-tar")],
        tar_bytes,
    )
        .into_response())
}

/// Accepts a `.tar` produced by [`backup`] and stages it as
/// `<data_dir>/.restore-pending.tar` for the *next* daemon restart to pick
/// up (see `main.rs`'s startup check) — swapping `audit.sqlite` out from
/// under an already-open `SqlitePool` live would be undefined behavior, so
/// this deliberately does not attempt a hot restore. Gated by
/// `OXID_ALLOW_RESTORE` (off by default) since accepting an arbitrary
/// uploaded database is a meaningfully different risk than the read-only
/// `backup` endpoint.
pub async fn restore<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if !state.allow_restore {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "restore is disabled; set OXID_ALLOW_RESTORE=1 on the daemon to enable it",
        ));
    }
    let staged_path = state.data_dir.join(".restore-pending.tar");
    std::fs::write(&staged_path, &body).map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cannot stage restore archive: {e}"),
        )
    })?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "staged",
            "message": "restore staged; restart the daemon to apply it"
        })),
    ))
}
