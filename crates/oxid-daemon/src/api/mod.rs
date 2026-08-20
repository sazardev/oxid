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
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::GlobalKeyExtractor;
use tower_http::catch_panic::CatchPanicLayer;

use crate::request_context::current_request_id;
use crate::{ControlPlane, CpError, DeployOutcome};

/// Header carrying the per-request correlation id (see
/// [`request_id_middleware`]) — generated when a client doesn't supply one,
/// echoed back on the response either way, and threaded through
/// `tracing` spans/[`oxid_core::AuditEvent::request_id`] so an operator can
/// grep structured logs for `request_id=<id>` and cross-reference `SELECT *
/// FROM audit_events WHERE request_id = '<id>'` to see one request's whole
/// story.
pub(crate) const REQUEST_ID_HEADER: &str = "x-request-id";

/// Shared state injected into every handler.
pub mod dashboard;
pub mod error;
pub mod handlers;
pub mod middleware;
pub mod types;
pub(crate) const DEFAULT_AUDIT_LIMIT: u64 = 50;

pub use dashboard::{
    dashboard_alpine_js, dashboard_app_js, dashboard_index, dashboard_style, health,
};
pub use error::{ApiError, ApiResult};
use middleware::AuthedAs;
use middleware::{
    handle_panic, operator_name, request_id_middleware, require_bearer_token, require_master,
};
pub use types::{
    AuditQuery, DeployBody, ListEnvironmentsQuery, RegisterBody, RollbackBody, SecretBody,
    SecretDeleteQuery, SecretListQuery,
};
#[derive(Clone)]
pub struct ApiState<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
> {
    /// The application service backing all endpoints.
    pub cp: ControlPlane<G, O>,
    /// Shared secret verifying GitHub webhook signatures (`OXID_WEBHOOK_SECRET`).
    /// Webhooks are rejected while unset.
    pub webhook_secret: Option<String>,
    /// Bearer token every `/api/v1/*` control-plane request must present
    /// (`Authorization: Bearer <token>`), except `/health`, `/webhooks/*`
    /// and the Traefik-facing `/wake`/`/heartbeat` endpoints. `None` (the
    /// default) leaves the control API open — appropriate for a daemon only
    /// reachable on localhost/a private network, not for one exposed
    /// beyond that.
    pub api_token: Option<String>,
    /// Directory holding `audit.sqlite`/`secret.key` — needed by
    /// `GET /api/v1/backup`/`POST /api/v1/backup/restore` since neither is
    /// reachable through `ControlPlane`'s narrower port abstractions.
    pub data_dir: PathBuf,
    /// Gates `POST /api/v1/backup/restore` (`OXID_ALLOW_RESTORE`). Off by
    /// default: restoring stages a file for the *next* daemon restart (see
    /// the handler's doc comment) rather than hot-swapping a live database,
    /// but accepting the upload at all is still an operator opt-in.
    pub allow_restore: bool,
    /// Rate limit for the protected control-plane routes: `(requests per
    /// second sustained, burst size)`. `None` disables it entirely — the
    /// default, since it only matters once an API token is handed to more
    /// than one trusted party (`OXID_RATE_LIMIT_PER_SECOND`/
    /// `OXID_RATE_LIMIT_BURST`). Deliberately a single global bucket, not
    /// per-client: distinguishing clients would need `ConnectInfo` wired
    /// through both the plain-HTTP and TLS serve paths in `main.rs` for
    /// comparatively little benefit on what's meant to be a small
    /// team/CI's shared credential, not a public API.
    pub rate_limit: Option<(u64, u32)>,
}

/// Builds the API router. Every route except `/health`, `/webhooks/github`,
/// `/wake` and `/heartbeat` requires `state.api_token` when it's configured
/// (see [`require_bearer_token`]).
///
/// # Panics
/// Panics if `state.rate_limit` is `Some` with a rate-limit configuration
/// `tower_governor` rejects — can't happen here since both values are
/// clamped to at least `1` first.
pub fn router<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    state: ApiState<G, O>,
) -> Router {
    let rate_limit = state.rate_limit;
    let mut protected = Router::new()
        .route(
            "/api/v1/projects",
            post(handlers::project::register_project).get(handlers::project::list_projects),
        )
        .route(
            "/api/v1/projects/{id}/environments",
            get(handlers::environment::list_environments),
        )
        .route(
            "/api/v1/projects/{id}/deploy",
            post(handlers::deploy::deploy),
        )
        .route(
            "/api/v1/projects/{id}/rollback",
            post(handlers::deploy::rollback),
        )
        .route(
            "/api/v1/projects/{id}",
            delete(handlers::project::delete_project).patch(handlers::project::update_project),
        )
        .route(
            "/api/v1/environments/{env_id}",
            delete(handlers::lifecycle::destroy),
        )
        .route(
            "/api/v1/secrets",
            get(handlers::secrets::list_global_secrets).post(handlers::secrets::set_global_secret),
        )
        .route(
            "/api/v1/secrets/{name}",
            delete(handlers::secrets::delete_global_secret),
        )
        .route(
            "/api/v1/projects/{id}/secrets",
            get(handlers::secrets::list_project_secrets)
                .post(handlers::secrets::set_project_secret),
        )
        .route(
            "/api/v1/projects/{id}/secrets/{name}",
            delete(handlers::secrets::delete_project_secret),
        )
        .route(
            "/api/v1/environments/{env_id}/pause",
            post(handlers::lifecycle::pause),
        )
        .route(
            "/api/v1/environments/{env_id}/wake",
            post(handlers::lifecycle::wake),
        )
        .route(
            "/api/v1/tokens",
            post(handlers::tokens::create_token).get(handlers::tokens::list_tokens),
        )
        .route(
            "/api/v1/tokens/{id}",
            delete(handlers::tokens::revoke_token),
        )
        .route("/api/v1/rotate-key", post(handlers::tokens::rotate_key))
        .route("/api/v1/audit", get(handlers::audit::recent_audit))
        .route("/api/v1/queue", get(handlers::audit::list_queue))
        .route("/api/v1/stats", get(handlers::infra::stats))
        .route("/api/v1/infra/status", get(handlers::infra::infra_status))
        // Idempotent and safe to re-run: creates the Docker network/Traefik
        // container only if missing, otherwise a no-op that just reports
        // current status (see `ControlPlane::infra_bootstrap`).
        .route(
            "/api/v1/infra/bootstrap",
            post(handlers::infra::infra_bootstrap),
        )
        .route("/api/v1/backup", get(handlers::backup::backup))
        .route("/api/v1/backup/restore", post(handlers::backup::restore))
        .route(
            "/api/v1/environments/{env_id}/audit",
            get(handlers::audit::environment_audit),
        )
        .route(
            "/api/v1/environments/{env_id}/logs",
            get(handlers::lifecycle::logs),
        )
        .route(
            "/api/v1/environments/{env_id}/logs/stream",
            get(handlers::lifecycle::stream_logs),
        )
        .route_layer(axum_middleware::from_fn_with_state(
            state.clone(),
            require_bearer_token::<G, O>,
        ));

    if let Some((per_second, burst)) = rate_limit {
        let governor_conf = GovernorConfigBuilder::default()
            .per_second(per_second.max(1))
            .burst_size(burst.max(1))
            .key_extractor(GlobalKeyExtractor)
            .finish()
            .expect("valid rate limit config");
        protected = protected.layer(GovernorLayer::new(governor_conf));
    }

    Router::new()
        .route("/api/v1/health", get(health))
        .route("/", get(dashboard_index))
        .route("/index.html", get(dashboard_index))
        .route("/style.css", get(dashboard_style))
        .route("/app.js", get(dashboard_app_js))
        .route("/vendor/alpine.min.js", get(dashboard_alpine_js))
        .route("/api/v1/wake", post(handlers::lifecycle::wake_by_host))
        .route(
            "/api/v1/heartbeat",
            get(handlers::lifecycle::heartbeat_by_host)
                .post(handlers::lifecycle::heartbeat_by_host),
        )
        .route(
            "/api/v1/webhooks/github",
            post(handlers::webhook::github_webhook),
        )
        .merge(protected)
        // Any GET that doesn't match an API route or a static asset above is
        // a client-side dashboard route (`/ui/projects/1`,
        // `/ui/environments/5?tab=logs`, ...) — the SPA shell handles
        // routing itself from `location.pathname`/`location.search`, so a
        // hard refresh or a shared deep link on any of those paths still
        // has to return `index.html`, not a 404.
        .fallback(get(dashboard_index))
        // `CatchPanicLayer` first (innermost — applied to every route,
        // including the fallback) so a handler panic never becomes a raw
        // dropped connection; `request_id_middleware` wraps *that*
        // (outermost) so the id it generates/reads is available both to
        // `handle_panic`'s log line and to every normal response, panic or
        // not. Layer application order in axum is "last `.layer()` call is
        // outermost" — see this function's own ordering below.
        .layer(CatchPanicLayer::custom(handle_panic))
        .layer(axum_middleware::from_fn(request_id_middleware))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

pub(crate) fn parse_branch(raw: &str) -> ApiResult<BranchName> {
    BranchName::parse(raw).map_err(|e| ApiError::from_validation(e.to_string()))
}

#[cfg(test)]
mod tests;
