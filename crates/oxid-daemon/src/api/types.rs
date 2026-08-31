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
    EnvironmentState, GitPort, PoolError, Project, ProjectId, RepoUrl, RepositoryError,
    StateTransition, Ttl,
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
///
/// Exactly one of:
/// - `repo_dir` — a checkout the daemon can read (how `oxid up` registers
///   its cwd against a locally-run daemon), or
/// - `repo_url` — any remote the daemon clones itself into its git cache
///   (how the dashboard's onboarding wizard registers against a
///   containerized daemon; private repos pass `git_token`).
///
/// must be present. Sending both or neither is a 400.
#[derive(Debug, Deserialize)]
pub struct RegisterBody {
    /// Path to the repository containing `oxid.toml`.
    pub repo_dir: Option<String>,
    /// Remote repository URL (`https://…`, `ssh://…`, or scp-style
    /// `git@host:org/repo.git`, which is normalized server-side).
    pub repo_url: Option<String>,
    /// Write-only access token for a *private* repository — encrypted at
    /// rest alongside the project row, never echoed back by any response.
    pub git_token: Option<String>,
    /// Which part of the repository this project builds, for a monorepo —
    /// `apps/api`, `services/worker`. Omitted, the detected workspace's
    /// first deployable service is used, or the whole repository when it
    /// holds one thing.
    ///
    /// This is what lets one repository be registered several times: a
    /// turborepo's API and web app are two projects that deploy, scale and
    /// fail independently, and `(repo_url, context)` is what the schema
    /// makes unique.
    pub context: Option<String>,
}

/// The validated form of a [`RegisterBody`].
#[derive(Debug)]
pub(crate) enum RegistrationSource {
    Dir {
        dir: String,
        git_token: Option<String>,
        context: Option<String>,
    },
    Url {
        url: RepoUrl,
        git_token: Option<String>,
        context: Option<String>,
    },
}

impl RegisterBody {
    /// Validates exactly-one-of and normalizes scp-style remotes.
    ///
    /// # Errors
    /// [`ApiError`] 400 when both/neither source is given or the URL fails
    /// `RepoUrl::parse`.
    pub(crate) fn into_source(self) -> ApiResult<RegistrationSource> {
        let dir = self
            .repo_dir
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty());
        let url = self
            .repo_url
            .map(|v| normalize_git_url(&v))
            .filter(|v| !v.is_empty());
        let context = self
            .context
            .as_deref()
            .map(|v| v.trim().trim_matches('/').to_owned())
            .filter(|v| !v.is_empty());
        match (dir, url) {
            (Some(dir), None) => Ok(RegistrationSource::Dir {
                dir,
                git_token: self.git_token,
                context: context.clone(),
            }),
            (None, Some(url)) => {
                let url = RepoUrl::parse(url.clone())
                    .map_err(|e| ApiError::from_validation(format!("invalid `repo_url`: {e}")))?;
                Ok(RegistrationSource::Url {
                    url,
                    git_token: self.git_token,
                    context: context.clone(),
                })
            }
            (Some(_), Some(_)) => Err(ApiError::from_validation(
                "pass either `repo_dir` or `repo_url`, not both",
            )),
            (None, None) => Err(ApiError::from_validation(
                "missing `repo_url` (remote to clone) or `repo_dir` (checkout path)",
            )),
        }
    }
}

/// Normalizes scp-style git remotes into the explicit scheme form
/// `RepoUrl::parse` requires: `git@github.com:org/repo.git` becomes
/// `ssh://git@github.com/org/repo.git`. Everything else passes through
/// unchanged (absolute paths included — those belong in `repo_dir`
/// anyway; the conservative `@` requirement keeps odd inputs from being
/// silently mangled).
fn normalize_git_url(raw: &str) -> String {
    let raw = raw.trim();
    if raw.contains("://") || raw.starts_with('/') {
        return raw.to_owned();
    }
    match raw.split_once(':') {
        Some((user_host, path))
            if !path.is_empty()
                && !user_host.is_empty()
                && !user_host.contains('/')
                && user_host.contains('@') =>
        {
            format!("ssh://{user_host}/{path}")
        }
        _ => raw.to_owned(),
    }
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

#[cfg(test)]
mod tests {
    use super::{RegisterBody, RegistrationSource, normalize_git_url};

    fn source_of(json: serde_json::Value) -> Result<RegistrationSource, ()> {
        let body: RegisterBody = serde_json::from_value(json).unwrap();
        body.into_source().map_err(|_| ())
    }

    #[test]
    fn scp_style_urls_are_normalized_to_ssh() {
        assert_eq!(
            normalize_git_url("git@github.com:org/repo.git"),
            "ssh://git@github.com/org/repo.git"
        );
        assert_eq!(
            normalize_git_url("  git@github.com:org/repo.git  "),
            "ssh://git@github.com/org/repo.git"
        );
    }

    #[test]
    fn other_url_shapes_pass_through_unchanged() {
        assert_eq!(
            normalize_git_url("https://github.com/org/repo.git"),
            "https://github.com/org/repo.git"
        );
        assert_eq!(
            normalize_git_url("ssh://git@host/x/y.git"),
            "ssh://git@host/x/y.git"
        );
    }

    #[test]
    fn colon_forms_without_a_user_are_not_mangled() {
        // A bare `host:path` or an absolute path must not be guessed into
        // an scp remote — the `@` requirement keeps the transform tight.
        assert_eq!(
            normalize_git_url("github.com:org/repo.git"),
            "github.com:org/repo.git"
        );
        assert_eq!(normalize_git_url("/srv/git/repo.git"), "/srv/git/repo.git");
    }

    #[test]
    fn exactly_one_source_is_accepted() {
        assert!(matches!(
            source_of(serde_json::json!({ "repo_dir": "/repos/app" })),
            Ok(RegistrationSource::Dir { .. })
        ));
        assert!(matches!(
            source_of(serde_json::json!({ "repo_url": "https://x.org/a.git" })),
            Ok(RegistrationSource::Url { .. })
        ));
        assert!(source_of(serde_json::json!({})).is_err());
        assert!(
            source_of(serde_json::json!({ "repo_dir": "/a", "repo_url": "https://x.org/a.git" }))
                .is_err()
        );
        // Blank strings count as absent:
        assert!(source_of(serde_json::json!({ "repo_dir": "   " })).is_err());
    }

    #[test]
    fn invalid_repo_url_is_a_validation_error() {
        assert!(source_of(serde_json::json!({ "repo_url": "not-a-url" })).is_err());
    }
}
