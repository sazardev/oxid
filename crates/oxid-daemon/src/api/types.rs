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

use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
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
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::GlobalKeyExtractor;
use tower_http::catch_panic::CatchPanicLayer;

use crate::request_context::current_request_id;
use crate::{ControlPlane, CpError, DeployOutcome};

/// Header carrying the per-request correlation id (see
use super::error::{ApiError, ApiResult};

/// Body for `POST /api/v1/projects`.
#[derive(Debug, Deserialize)]
pub struct RegisterBody {
    /// Path to the repository containing `oxid.toml`.
    pub repo_dir: String,
}

/// Body for `POST /api/v1/projects/{id}/deploy`.
#[derive(Debug, Deserialize)]
pub struct DeployBody {
    /// Branch to deploy.
    pub branch: String,
}

/// Query for `GET /api/v1/audit` and `GET /api/v1/environments/{id}/audit`.
///
/// **Exact query-param names — `oxid-cli`'s `oxid audit
/// --project/--branch/--since/--until/--kind` flags map onto these
/// verbatim, so don't rename a field without updating the CLI side too:**
/// - `project_id` — numeric project id. Only applies to `GET /api/v1/audit`
///   (an environment's audit history is already scoped to one project).
/// - `branch` — exact branch name (e.g. `main`, `feature-x`). Same
///   `/api/v1/audit`-only scope as `project_id`.
/// - `since` / `until` — RFC3339 timestamps (e.g.
///   `2026-08-01T00:00:00Z`), inclusive on both ends. Apply to both
///   endpoints.
/// - `kind` — a [`StateTransition`] variant in `snake_case` (`build_succeeded`,
///   `build_failed`, `idle_timeout`, `woken`, `deep_sleep`, `ttl_expired`,
///   `destroy`). Apply to both endpoints.
/// - `limit` — max rows, default 50. Only applies to `GET /api/v1/audit`
///   (the environment endpoint has always returned the full history).
#[derive(Debug, Default, Deserialize)]
pub struct AuditQuery {
    /// See the field-by-field breakdown above.
    pub project_id: Option<u64>,
    /// See the field-by-field breakdown above.
    pub branch: Option<String>,
    /// See the field-by-field breakdown above.
    pub since: Option<String>,
    /// See the field-by-field breakdown above.
    pub until: Option<String>,
    /// See the field-by-field breakdown above.
    pub kind: Option<String>,
    /// See the field-by-field breakdown above.
    pub limit: Option<u64>,
}

impl AuditQuery {
    /// Parses the query-string values into a strongly-typed [`AuditFilter`],
    /// rejecting an unparseable `since`/`until`/`kind` with `400` rather
    /// than silently ignoring it.
    pub(crate) fn into_filter(self) -> ApiResult<AuditFilter> {
        let since = self
            .since
            .as_deref()
            .map(parse_rfc3339)
            .transpose()
            .map_err(|e| ApiError::from_validation(format!("invalid `since`: {e}")))?;
        let until = self
            .until
            .as_deref()
            .map(parse_rfc3339)
            .transpose()
            .map_err(|e| ApiError::from_validation(format!("invalid `until`: {e}")))?;
        let kind = self
            .kind
            .as_deref()
            .map(|k| {
                k.parse::<StateTransition>()
                    .map_err(|e| ApiError::from_validation(format!("invalid `kind`: {e}")))
            })
            .transpose()?;
        Ok(AuditFilter {
            project_id: self.project_id.map(ProjectId),
            branch: self.branch,
            since,
            until,
            kind,
            limit: self.limit,
        })
    }
}

/// Parses an RFC3339 timestamp (`since`/`until` query params).
fn parse_rfc3339(raw: &str) -> Result<time::OffsetDateTime, time::error::Parse> {
    time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
}

/// Body for `POST /api/v1/projects/{id}/rollback`.
#[derive(Debug, Deserialize)]
pub struct RollbackBody {
    /// Branch to roll back.
    pub branch: String,
    /// Specific commit to roll back to. When omitted, rolls back to the
    /// deploy immediately before the current live one.
    pub to_sha: Option<String>,
}

/// Query for `GET /api/v1/projects/{id}/environments`.
#[derive(Debug, Default, Deserialize)]
pub struct ListEnvironmentsQuery {
    /// When set, only the most recent environment for this branch is
    /// returned (0 or 1 elements), not its full deploy history.
    pub branch: Option<String>,
}

/// Body for `POST /api/v1/secrets` and `POST /api/v1/projects/{id}/secrets`.
#[derive(Debug, Deserialize)]
pub struct SecretBody {
    /// Secret name, e.g. `DB_PASSWORD`.
    pub name: String,
    /// Scope of the secret (`global`, `project` or `branch`).
    pub scope: String,
    /// Value to store. Optional on delete.
    pub value: Option<String>,
    /// Branch scope (only meaningful for `scope = "branch"`).
    pub branch: Option<String>,
}

/// Body for `DELETE` secret endpoints (optional branch scope).
#[derive(Debug, Deserialize)]
pub struct SecretDeleteQuery {
    /// Branch scope to delete from.
    pub branch: Option<String>,
}

/// Query for listing secrets (optional branch scope).
#[derive(Debug, Deserialize)]
pub struct SecretListQuery {
    /// Branch scope to list.
    pub branch: Option<String>,
}
