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
/// [`request_id_middleware`]) — generated when a client doesn't supply one,
/// echoed back on the response either way, and threaded through
/// `tracing` spans/[`oxid_core::AuditEvent::request_id`] so an operator can
/// grep structured logs for `request_id=<id>` and cross-reference `SELECT *
/// FROM audit_events WHERE request_id = '<id>'` to see one request's whole
/// story.
const REQUEST_ID_HEADER: &str = "x-request-id";

/// Shared state injected into every handler.
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
            post(register_project).get(list_projects),
        )
        .route("/api/v1/projects/{id}/environments", get(list_environments))
        .route("/api/v1/projects/{id}/deploy", post(deploy))
        .route("/api/v1/projects/{id}/rollback", post(rollback))
        .route(
            "/api/v1/projects/{id}",
            delete(delete_project).patch(update_project),
        )
        .route("/api/v1/environments/{env_id}", delete(destroy))
        .route(
            "/api/v1/secrets",
            get(list_global_secrets).post(set_global_secret),
        )
        .route("/api/v1/secrets/{name}", delete(delete_global_secret))
        .route(
            "/api/v1/projects/{id}/secrets",
            get(list_project_secrets).post(set_project_secret),
        )
        .route(
            "/api/v1/projects/{id}/secrets/{name}",
            delete(delete_project_secret),
        )
        .route("/api/v1/environments/{env_id}/pause", post(pause))
        .route("/api/v1/environments/{env_id}/wake", post(wake))
        .route("/api/v1/tokens", post(create_token).get(list_tokens))
        .route("/api/v1/tokens/{id}", delete(revoke_token))
        .route("/api/v1/rotate-key", post(rotate_key))
        .route("/api/v1/audit", get(recent_audit))
        .route("/api/v1/queue", get(list_queue))
        .route("/api/v1/stats", get(stats))
        .route("/api/v1/backup", get(backup))
        .route("/api/v1/backup/restore", post(restore))
        .route(
            "/api/v1/environments/{env_id}/audit",
            get(environment_audit),
        )
        .route("/api/v1/environments/{env_id}/logs", get(logs))
        .route(
            "/api/v1/environments/{env_id}/logs/stream",
            get(stream_logs),
        )
        .route_layer(middleware::from_fn_with_state(
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
        .route("/api/v1/wake", post(wake_by_host))
        .route(
            "/api/v1/heartbeat",
            get(heartbeat_by_host).post(heartbeat_by_host),
        )
        .route("/api/v1/webhooks/github", post(github_webhook))
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
        .layer(middleware::from_fn(request_id_middleware))
        .with_state(state)
}

/// Reads (or, if absent/blank, generates) this request's `X-Request-Id`,
/// makes it available for the rest of the request's execution via
/// [`current_request_id`] (see `request_context`'s doc comment for why a
/// `tokio::task_local!` rather than an explicit parameter threaded through
/// every `ControlPlane` method), records it into a `tracing` span covering
/// the whole request/response, and echoes it back as a response header —
/// the three places an operator correlates a single request: the response
/// itself, structured logs (`grep request_id=<id>`), and the audit trail
/// (`WHERE request_id = '<id>'`).
async fn request_id_middleware(request: Request, next: Next) -> Response {
    let request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned);

    let method = request.method().clone();
    let uri = request.uri().clone();
    let span = tracing::info_span!("http_request", request_id = %request_id, %method, %uri);

    let mut response = {
        use tracing::Instrument;
        crate::request_context::scope(request_id.clone(), next.run(request))
            .instrument(span)
            .await
    };

    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    response
}

/// Converts a panicking handler into a `500` with the same `{"error": ...}`
/// shape as [`ApiError`], instead of `tower-http`'s default plain-text body
/// (or, without this layer at all, a dropped connection the client sees as
/// a reset). Logs the panic message via `tracing::error!` tagged with this
/// request's id (if any — [`current_request_id`] reads it from the span
/// [`request_id_middleware`] set up, which wraps this layer) so it shows up
/// in the same place every other request failure does.
// `tower_http::catch_panic::ResponseForPanic`'s blanket impl requires this
// exact `Fn(Box<dyn Any + Send>) -> Response` signature (taking the box by
// value) — can't take `&Box<..>` instead as clippy's `pedantic` lint would
// otherwise suggest.
#[allow(clippy::needless_pass_by_value)]
fn handle_panic(err: Box<dyn Any + Send + 'static>) -> Response {
    let detail = if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = err.downcast_ref::<&str>() {
        (*s).to_owned()
    } else {
        "unknown panic".to_owned()
    };
    let request_id = current_request_id();
    tracing::error!(
        request_id = request_id.as_deref().unwrap_or("-"),
        panic = %detail,
        "panic in request handler"
    );
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal server error", "request_id": request_id })),
    )
        .into_response()
}

/// Which credential a request authenticated with — attached to
/// [`Request::extensions`] by [`require_bearer_token`] so handlers can
/// attribute audit events to a named operator, or restrict
/// token-management endpoints to the master credential only.
#[derive(Debug, Clone)]
enum AuthedAs {
    /// The single shared `OXID_API_TOKEN` — anonymous by design (it's
    /// meant to be one credential for the whole team/CI, not a person).
    Master,
    /// A named, database-issued token (see [`create_token`]).
    Operator(String),
}

/// Rejects requests missing a valid `Authorization: Bearer <token>` header
/// when `state.api_token` is configured; passes everything through
/// unchanged otherwise (open by default, see [`ApiState::api_token`]). A
/// valid token is either the master `OXID_API_TOKEN` or any non-revoked
/// named token issued via `POST /api/v1/tokens`.
async fn require_bearer_token<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(expected) = state.api_token.as_deref() else {
        return next.run(request).await;
    };
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let Some(token) = provided else {
        return ApiError::new(StatusCode::UNAUTHORIZED, "missing or invalid bearer token")
            .into_response();
    };
    if constant_time_eq(token.as_bytes(), expected.as_bytes()) {
        request.extensions_mut().insert(AuthedAs::Master);
        return next.run(request).await;
    }
    match state.cp.find_operator_by_token(token).await {
        Ok(Some(name)) => {
            request.extensions_mut().insert(AuthedAs::Operator(name));
            next.run(request).await
        }
        _ => ApiError::new(StatusCode::UNAUTHORIZED, "missing or invalid bearer token")
            .into_response(),
    }
}

/// The operator name to attribute an audit event to: `None` for the master
/// token (anonymous by design) or an unauthenticated (open API) request.
fn operator_name(authed: Option<&Extension<AuthedAs>>) -> Option<String> {
    match authed {
        Some(Extension(AuthedAs::Operator(name))) => Some(name.clone()),
        _ => None,
    }
}

/// Restricts an endpoint to the master `OXID_API_TOKEN` — used for token
/// management, so any named operator can't mint or revoke other operators'
/// credentials. A no-op when the API has no token configured at all
/// (matches every other endpoint's "open by default" behavior).
fn require_master<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    state: &ApiState<G, O>,
    authed: Option<&Extension<AuthedAs>>,
) -> ApiResult<()> {
    if state.api_token.is_none() {
        return Ok(());
    }
    match authed {
        Some(Extension(AuthedAs::Master)) => Ok(()),
        _ => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "token management requires the master OXID_API_TOKEN",
        )),
    }
}

/// Constant-time byte comparison — a plain `==` on the token would let an
/// attacker learn how many leading bytes matched from response timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------------------------------------------------------------------------
// request/response types
// ---------------------------------------------------------------------------

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
    fn into_filter(self) -> ApiResult<AuditFilter> {
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

/// Unified error response.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn from_validation(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
}

impl From<CpError> for ApiError {
    fn from(err: CpError) -> Self {
        match err {
            CpError::NotFound(m) | CpError::Store(RepositoryError::NotFound(m)) => {
                Self::not_found(m)
            }
            CpError::Config(_)
            | CpError::Domain(_)
            | CpError::Pool(PoolError::NotConfigured(_)) => Self::from_validation(err.to_string()),
            CpError::Store(RepositoryError::Conflict(m)) => Self::new(StatusCode::CONFLICT, m),
            CpError::Store(RepositoryError::Storage(m)) => {
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, m)
            }
            CpError::Git(_) | CpError::Oci(_) | CpError::Pool(PoolError::Failure(_)) => {
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
            }
            CpError::InsufficientCapacity(_) | CpError::DeployNotReady(_) => {
                Self::new(StatusCode::UNPROCESSABLE_ENTITY, err.to_string())
            }
            CpError::Proxy(_) => Self::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

// ---------------------------------------------------------------------------
// web dashboard (SPEC.md §5.3: "incluido dentro del mismo binario estático de
// Rust, archivos precompilados e incrustados") — a handful of static files
// embedded at compile time via `include_str!`, no build step, no bundler,
// and (besides the vendored Alpine.js) no client-side dependency at all.
// ---------------------------------------------------------------------------

async fn dashboard_index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}

async fn dashboard_style() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../web/style.css"),
    )
}

async fn dashboard_app_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/app.js"),
    )
}

async fn dashboard_alpine_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/vendor/alpine.min.js"),
    )
}

/// Aggregate counts + host capacity backing the dashboard's stat cards —
/// see [`ControlPlane::node_stats`].
async fn stats<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
) -> ApiResult<Json<crate::NodeStats>> {
    Ok(Json(state.cp.node_stats().await?))
}

async fn register_project<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Json(body): Json<RegisterBody>,
) -> ApiResult<(StatusCode, Json<Project>)> {
    let project = state
        .cp
        .register_project(std::path::Path::new(&body.repo_dir))
        .await?;
    Ok((StatusCode::CREATED, Json(project)))
}

async fn list_projects<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
) -> ApiResult<Json<Vec<Project>>> {
    Ok(Json(state.cp.list_projects().await?))
}

async fn list_environments<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
    Query(query): Query<ListEnvironmentsQuery>,
) -> ApiResult<Json<Vec<Environment>>> {
    let envs = state.cp.list_environments(ProjectId(id)).await?;
    let envs = match query.branch {
        // The daemon keeps one row per historical deploy of a branch (audit
        // trail); only the most recent (highest id) one is "the" current
        // environment. Returning every historical row here would let CLI
        // callers such as `oxid down`/`pause`/`wake` act on a stale,
        // already-`Destroyed` deployment instead of the live one.
        Some(raw) => {
            let branch = parse_branch(&raw)?;
            envs.into_iter()
                .filter(|e| e.branch.name == branch)
                .max_by_key(|e| e.id.0)
                .into_iter()
                .collect()
        }
        None => envs,
    };
    Ok(Json(envs))
}

async fn deploy<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
    authed: Option<Extension<AuthedAs>>,
    Json(body): Json<DeployBody>,
) -> ApiResult<Response> {
    let branch = parse_branch(&body.branch)?;
    match state
        .cp
        .deploy_or_queue(ProjectId(id), branch, operator_name(authed.as_ref()))
        .await?
    {
        DeployOutcome::Deployed(env) => Ok((StatusCode::CREATED, Json(env)).into_response()),
        DeployOutcome::Queued { position } => Ok((
            StatusCode::ACCEPTED,
            Json(json!({ "status": "queued", "position": position })),
        )
            .into_response()),
    }
}

/// Body for `POST /api/v1/tokens`.
#[derive(Debug, Deserialize)]
struct CreateTokenBody {
    /// Human-readable name for the operator this token identifies.
    name: String,
}

/// Mints a named token, master-token-only. The raw token is only ever
/// returned in this response — only its hash is persisted.
async fn create_token<
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
    let (id, token) = state.cp.create_operator_token(body.name.trim()).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": id, "name": body.name, "token": token })),
    ))
}

/// Lists every named token (never the raw value or its hash),
/// master-token-only.
async fn list_tokens<
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
async fn revoke_token<
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
async fn rotate_key<
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

const DEFAULT_AUDIT_LIMIT: u64 = 50;

async fn recent_audit<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Query(query): Query<AuditQuery>,
) -> ApiResult<Json<Vec<oxid_core::AuditEvent>>> {
    let mut filter = query.into_filter()?;
    filter.limit = Some(filter.limit.unwrap_or(DEFAULT_AUDIT_LIMIT));
    Ok(Json(state.cp.recent_audit_events(&filter).await?))
}

/// Lists deploys currently waiting for host capacity (see
/// [`ControlPlane::deploy_or_queue`]), oldest first.
async fn list_queue<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
) -> ApiResult<Json<Vec<crate::adapter::store::QueuedDeploy>>> {
    Ok(Json(state.cp.list_deploy_queue().await?))
}

async fn environment_audit<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(env_id): Path<u64>,
    Query(query): Query<AuditQuery>,
) -> ApiResult<Json<Vec<oxid_core::AuditEvent>>> {
    let filter = query.into_filter()?;
    Ok(Json(
        state
            .cp
            .audit_events_for(EnvironmentId(env_id), &filter)
            .await?,
    ))
}

/// Streams a `.tar` snapshot of `/data`: a consistent point-in-time copy of
/// `audit.sqlite` (via `VACUUM INTO`, safe against the live pool) plus
/// `secret.key`. `git-cache/` is deliberately excluded — it's re-clonable
/// and can be large; restoring just re-clones on the next deploy.
async fn backup<
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
async fn restore<
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

async fn rollback<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
    authed: Option<Extension<AuthedAs>>,
    Json(body): Json<RollbackBody>,
) -> ApiResult<(StatusCode, Json<Environment>)> {
    let branch = parse_branch(&body.branch)?;
    let env = state
        .cp
        .rollback_with_operator(
            ProjectId(id),
            branch,
            body.to_sha,
            operator_name(authed.as_ref()),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(env)))
}

async fn delete_project<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
) -> ApiResult<StatusCode> {
    state.cp.delete_project(ProjectId(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Body for `PATCH /api/v1/projects/{id}` — either field can be omitted to
/// leave it unchanged, so a client only sends what it's actually changing.
#[derive(Debug, Deserialize)]
struct UpdateProjectBody {
    /// New idle timeout before scale-to-zero pause, e.g. `"45m"`.
    pause_after: Option<String>,
    /// New max lifetime before permanent teardown, e.g. `"3d"`.
    destroy_after: Option<String>,
    /// New git access token for a private repository — an empty string
    /// clears it. Never echoed back by any response (see
    /// `ControlPlane::set_project_git_token`'s doc comment for why it isn't
    /// a field on `Project` at all).
    git_token: Option<String>,
}

/// Updates a project's `pause_after`/`destroy_after` policy and/or its git
/// access token — the settings `oxid.toml` only ever seeds once at first
/// registration, with the dashboard's project settings form as the intended
/// caller for the TTLs, and `oxid configure --git-token`/the dashboard's
/// secrets-style write-only field for the token.
async fn update_project<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
    Json(body): Json<UpdateProjectBody>,
) -> ApiResult<Json<Project>> {
    let pause_after = body
        .pause_after
        .map(|raw| Ttl::parse(&raw))
        .transpose()
        .map_err(|e| ApiError::from_validation(e.to_string()))?;
    let destroy_after = body
        .destroy_after
        .map(|raw| Ttl::parse(&raw))
        .transpose()
        .map_err(|e| ApiError::from_validation(e.to_string()))?;
    if let Some(git_token) = body.git_token.as_deref() {
        state
            .cp
            .set_project_git_token(ProjectId(id), Some(git_token).filter(|t| !t.is_empty()))
            .await?;
    }
    let project = state
        .cp
        .update_project_ttls(ProjectId(id), pause_after, destroy_after)
        .await?;
    Ok(Json(project))
}

async fn pause<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(env_id): Path<u64>,
) -> ApiResult<StatusCode> {
    state.cp.pause(EnvironmentId(env_id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn wake<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(env_id): Path<u64>,
) -> ApiResult<StatusCode> {
    state.cp.wake(EnvironmentId(env_id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn logs<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(env_id): Path<u64>,
) -> ApiResult<Json<Value>> {
    let logs = state.cp.logs(EnvironmentId(env_id)).await?;
    Ok(Json(json!({ "logs": logs })))
}

/// Follows an environment's container logs live over SSE, one `data:` event
/// per line, instead of the bounded snapshot [`logs`] returns.
async fn stream_logs<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(env_id): Path<u64>,
) -> ApiResult<Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>>> {
    let log_stream = state.cp.stream_logs(EnvironmentId(env_id)).await?;
    let events = log_stream.map(|item| {
        Ok(match item {
            Ok(line) => Event::default().data(line),
            Err(err) => Event::default().event("error").data(err.to_string()),
        })
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::default()))
}

/// Query for `DELETE /api/v1/environments/{env_id}`.
#[derive(Debug, Default, Deserialize)]
pub struct DestroyQuery {
    /// When `true`, also deletes this branch's `branch`-scoped secrets.
    /// Left `false` by default so a recurring feature branch's config
    /// survives a routine TTL-based destroy/redeploy cycle.
    #[serde(default)]
    pub purge_secrets: bool,
}

async fn destroy<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(env_id): Path<u64>,
    Query(query): Query<DestroyQuery>,
    authed: Option<Extension<AuthedAs>>,
) -> ApiResult<StatusCode> {
    state
        .cp
        .destroy_with_operator(
            EnvironmentId(env_id),
            query.purge_secrets,
            operator_name(authed.as_ref()),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Extracts the routed host from a request, preferring `X-Forwarded-Host`
/// (set by Traefik) and falling back to `Host`, stripping any port suffix.
fn routed_host(headers: &HeaderMap) -> Option<String> {
    let raw = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))
        .and_then(|v| v.to_str().ok())?;
    Some(raw.split(':').next().unwrap_or(raw).to_owned())
}

/// Wake-on-request endpoint (SPEC.md §3.2): Traefik's `errors` middleware
/// forwards the original request here (preserving `Host`) when the target
/// container is paused/hibernating and the proxy gets a connection error.
/// Unpauses/starts the environment and returns a small page that reloads.
async fn wake_by_host<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let Some(host) = routed_host(&headers) else {
        return Ok((StatusCode::BAD_REQUEST, "missing Host header").into_response());
    };
    let env = state.cp.wake_by_url(&host).await?;
    let branch = env
        .as_ref()
        .map_or_else(|| host.clone(), |e| e.branch.name.to_string());
    Ok((
        StatusCode::OK,
        axum::response::Html(wake_page_html(&branch)),
    )
        .into_response())
}

/// Heartbeat endpoint (SPEC.md §3.2 traffic monitor): wired as a Traefik
/// `forwardAuth` middleware on every router, it refreshes `last_accessed_at`
/// on each request to a live environment. Always `200 OK` so it never blocks
/// traffic, even for hosts Oxid does not manage.
async fn heartbeat_by_host<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    headers: HeaderMap,
) -> ApiResult<StatusCode> {
    if let Some(host) = routed_host(&headers) {
        state.cp.touch_by_url(&host).await?;
    }
    Ok(StatusCode::OK)
}

/// Small dark-themed page shown while an environment wakes up
/// (DESIGN.md §1 palette, §3.1 "Paused" state).
fn wake_page_html(branch: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html><head><meta http-equiv="refresh" content="1">
<style>
body {{ background:#121212; color:#F4F4F5; font-family:monospace;
       display:flex; height:100vh; align-items:center; justify-content:center; }}
strong {{ color:#DE5236; }}
</style></head>
<body><p>[~] Waking up <strong>{branch}</strong>&hellip;</p></body></html>"#
    )
}

// ---------------------------------------------------------------------------
// secret handlers
// ---------------------------------------------------------------------------

async fn do_set_secret<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    state: ApiState<G, O>,
    project_id: Option<ProjectId>,
    body: SecretBody,
) -> ApiResult<StatusCode> {
    let scope = parse_scope(&body.scope)?;
    let name = validate_secret_name(&body.name)?;
    let value = body
        .value
        .clone()
        .ok_or_else(|| ApiError::from_validation("secret `value` is required"))?;

    let (pid, branch) = match (project_id, scope) {
        (None, EnvVarScope::Global) => (None, None),
        (Some(_), EnvVarScope::Global) => {
            return Err(ApiError::from_validation(
                "use the global endpoint for `global` scope",
            ));
        }
        (Some(pid), EnvVarScope::Project) => {
            if body.branch.is_some() {
                return Err(ApiError::from_validation(
                    "`branch` is only allowed with `scope = \"branch\"`",
                ));
            }
            (Some(pid), None)
        }
        (Some(pid), EnvVarScope::Branch) => {
            let raw = body.branch.as_deref().ok_or_else(|| {
                ApiError::from_validation("`branch` is required for `scope = \"branch\"`")
            })?;
            (Some(pid), Some(parse_branch(raw)?))
        }
        (None, EnvVarScope::Project | EnvVarScope::Branch) => {
            return Err(ApiError::from_validation(
                "project/branch secrets require a project id",
            ));
        }
        (_, EnvVarScope::Runtime) => {
            return Err(ApiError::from_validation(
                "`runtime` scope cannot be set by clients",
            ));
        }
    };

    state
        .cp
        .set_secret(pid, branch.as_ref(), name, scope, &value)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn do_list_secrets<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    state: ApiState<G, O>,
    project_id: Option<ProjectId>,
    query: SecretListQuery,
) -> ApiResult<Json<Value>> {
    let branch = query.branch.as_deref().map(parse_branch).transpose()?;
    let secrets = state.cp.list_secrets(project_id, branch.as_ref()).await?;
    Ok(Json(json!({
        "secrets": secrets.into_iter().map(|(name, scope)| json!({
            "name": name,
            "scope": scope.to_string(),
        })).collect::<Vec<_>>(),
    })))
}

async fn do_delete_secret<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    state: ApiState<G, O>,
    project_id: Option<ProjectId>,
    name: &str,
    query: SecretDeleteQuery,
) -> ApiResult<StatusCode> {
    let name = validate_secret_name(name)?;
    let branch = query.branch.as_deref().map(parse_branch).transpose()?;
    state
        .cp
        .delete_secret(project_id, branch.as_ref(), name)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_global_secret<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Json(body): Json<SecretBody>,
) -> ApiResult<StatusCode> {
    do_set_secret(state, None, body).await
}

async fn list_global_secrets<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    query: Query<SecretListQuery>,
) -> ApiResult<Json<Value>> {
    do_list_secrets(state, None, query.0).await
}

async fn delete_global_secret<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(name): Path<String>,
    query: Query<SecretDeleteQuery>,
) -> ApiResult<StatusCode> {
    do_delete_secret(state, None, &name, query.0).await
}

async fn set_project_secret<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
    Json(body): Json<SecretBody>,
) -> ApiResult<StatusCode> {
    do_set_secret(state, Some(ProjectId(id)), body).await
}

async fn list_project_secrets<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
    query: Query<SecretListQuery>,
) -> ApiResult<Json<Value>> {
    do_list_secrets(state, Some(ProjectId(id)), query.0).await
}

async fn delete_project_secret<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path((id, name)): Path<(u64, String)>,
    query: Query<SecretDeleteQuery>,
) -> ApiResult<StatusCode> {
    do_delete_secret(state, Some(ProjectId(id)), &name, query.0).await
}

fn parse_scope(raw: &str) -> ApiResult<EnvVarScope> {
    match raw {
        "global" => Ok(EnvVarScope::Global),
        "project" => Ok(EnvVarScope::Project),
        "branch" => Ok(EnvVarScope::Branch),
        _ => Err(ApiError::from_validation(format!(
            "invalid scope `{raw}`; expected `global`, `project` or `branch`"
        ))),
    }
}

fn validate_secret_name(name: &str) -> ApiResult<&str> {
    if name.trim().is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(ApiError::from_validation(format!(
            "invalid secret name `{name}`; use alphanumeric characters and underscores"
        )));
    }
    Ok(name)
}

// ---------------------------------------------------------------------------
// webhook handler
// ---------------------------------------------------------------------------

/// GitHub push-webhook handler with HMAC-SHA256 signature verification
/// (SPEC.md §4.1). The signature covers the exact raw request body, so the
/// payload is read as bytes and parsed after verification.
async fn github_webhook<
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

fn parse_branch(raw: &str) -> ApiResult<BranchName> {
    BranchName::parse(raw).map_err(|e| ApiError::from_validation(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::http::Request;
    use oxid_core::{BuildSpec, ContainerSpec, GitError, OciError, RepoUrl};
    use tower::ServiceExt;

    use crate::adapter::crypto::Cipher;
    use crate::adapter::store::SqliteStore;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[derive(Debug, Clone, Default)]
    struct FakeGit;

    impl GitPort for FakeGit {
        async fn remote_url(&self, repo_dir: &std::path::Path) -> Result<RepoUrl, GitError> {
            let _ = repo_dir;
            RepoUrl::parse("https://github.com/org/app.git")
                .map_err(|e| GitError::Failure(e.to_string()))
        }
        async fn ensure_repo(
            &self,
            _url: &RepoUrl,
            _token: Option<&str>,
            cache_dir: &std::path::Path,
        ) -> Result<std::path::PathBuf, GitError> {
            Ok(cache_dir.join("app"))
        }
        async fn resolve_branch_head(
            &self,
            _repo_dir: &std::path::Path,
            branch: &BranchName,
        ) -> Result<oxid_core::CommitRef, GitError> {
            Ok(oxid_core::CommitRef {
                branch: branch.clone(),
                sha: SHA.to_owned(),
            })
        }
        async fn checkout_commit(
            &self,
            _repo_dir: &std::path::Path,
            _sha: &str,
        ) -> Result<(), GitError> {
            Ok(())
        }
    }

    #[derive(Debug, Clone, Default)]
    struct FakeOci {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl ContainerPort for FakeOci {
        async fn build(&self, spec: &BuildSpec) -> Result<(), OciError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("build:{}", spec.image));
            Ok(())
        }
        async fn run(&self, spec: &ContainerSpec) -> Result<Option<u16>, OciError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("run:{}:env={:?}", spec.name, spec.env));
            Ok(spec.network.is_none().then_some(65535))
        }
        async fn published_port(
            &self,
            _name: &str,
            _container_port: u16,
        ) -> Result<Option<u16>, OciError> {
            Ok(Some(65535))
        }
        async fn start(&self, name: &str) -> Result<(), OciError> {
            self.calls.lock().unwrap().push(format!("start:{name}"));
            Ok(())
        }
        async fn pause(&self, name: &str) -> Result<(), OciError> {
            self.calls.lock().unwrap().push(format!("pause:{name}"));
            Ok(())
        }
        async fn unpause(&self, name: &str) -> Result<(), OciError> {
            self.calls.lock().unwrap().push(format!("unpause:{name}"));
            Ok(())
        }
        async fn stop(&self, name: &str) -> Result<(), OciError> {
            self.calls.lock().unwrap().push(format!("stop:{name}"));
            Ok(())
        }
        async fn remove(&self, name: &str) -> Result<(), OciError> {
            self.calls.lock().unwrap().push(format!("remove:{name}"));
            Ok(())
        }
        async fn remove_image(&self, image: &str) -> Result<(), OciError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("remove_image:{image}"));
            Ok(())
        }
        async fn logs(&self, name: &str) -> Result<String, OciError> {
            self.calls.lock().unwrap().push(format!("logs:{name}"));
            Ok("build log".to_owned())
        }
        async fn stream_logs(&self, name: &str) -> Result<oxid_core::LogStream, OciError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("stream_logs:{name}"));
            Ok(Box::pin(futures_util::stream::iter(vec![Ok(
                "build log".to_owned()
            )])))
        }
        async fn exec(&self, name: &str, command: &str) -> Result<(), OciError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("exec:{name}:{command}"));
            Ok(())
        }
        async fn container_status(
            &self,
            _name: &str,
        ) -> Result<oxid_core::ContainerStatus, OciError> {
            Ok(oxid_core::ContainerStatus::Running)
        }
        async fn host_capacity(&self) -> Result<oxid_core::HostCapacity, OciError> {
            Ok(oxid_core::HostCapacity {
                total_memory_bytes: 8 * 1_073_741_824,
                cpu_count: 4,
            })
        }
    }

    /// A data dir with a dummy `secret.key`, for backup-endpoint tests.
    /// `.keep()` leaks it (no auto-delete on drop) since it must outlive
    /// the individual request(s) a test makes against the returned router.
    fn test_data_dir() -> PathBuf {
        let dir = tempfile::tempdir().unwrap().keep();
        std::fs::write(dir.join("secret.key"), b"test-key-material").unwrap();
        dir
    }

    async fn test_app() -> (Router, FakeOci) {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let cache = tempfile::tempdir().unwrap();
        let oci = FakeOci::default();
        let cp = ControlPlane::new(store, FakeGit, oci.clone(), cache.path().to_owned())
            .with_readiness_check(false);
        (
            router(ApiState {
                cp,
                webhook_secret: Some("test-secret".to_owned()),
                api_token: None,
                data_dir: test_data_dir(),
                allow_restore: true,
                rate_limit: None,
            }),
            oci,
        )
    }

    async fn test_app_with_token(token: &str) -> Router {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let cache = tempfile::tempdir().unwrap();
        let cp = ControlPlane::new(store, FakeGit, FakeOci::default(), cache.path().to_owned())
            .with_readiness_check(false);
        router(ApiState {
            cp,
            webhook_secret: Some("test-secret".to_owned()),
            api_token: Some(token.to_owned()),
            data_dir: test_data_dir(),
            allow_restore: true,
            rate_limit: None,
        })
    }

    /// A router whose `ControlPlane` has admission control enabled with an
    /// 8GB host (`FakeOci::host_capacity`'s fixed value) minus
    /// `reserved_mb`, and `default_mem_mb` as the daemon-default memory
    /// request for any project that doesn't set its own — for exercising
    /// `/api/v1/projects/{id}/deploy`'s queued-response path end to end.
    async fn test_app_with_admission_control(
        reserved_mb: u64,
        default_mem_mb: u64,
    ) -> (Router, FakeOci) {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let cache = tempfile::tempdir().unwrap();
        let oci = FakeOci::default();
        let cp = ControlPlane::new(store, FakeGit, oci.clone(), cache.path().to_owned())
            .with_resource_defaults(Some(default_mem_mb), None)
            .with_admission_control(Some(reserved_mb))
            .with_readiness_check(false);
        (
            router(ApiState {
                cp,
                webhook_secret: Some("test-secret".to_owned()),
                api_token: None,
                data_dir: test_data_dir(),
                allow_restore: true,
                rate_limit: None,
            }),
            oci,
        )
    }

    fn repo_dir_with_config() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("oxid.toml"),
            r#"
[project]
name = "app"

[routing]
base_domain = "app.local.dev"
port = 8080
"#,
        )
        .unwrap();
        dir
    }

    async fn json_request(
        app: &Router,
        method: &str,
        uri: &str,
        body: Value,
    ) -> (StatusCode, Vec<u8>) {
        json_request_with_auth(app, method, uri, body, None).await
    }

    async fn json_request_with_auth(
        app: &Router,
        method: &str,
        uri: &str,
        body: Value,
        bearer: Option<&str>,
    ) -> (StatusCode, Vec<u8>) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(token) = bearer {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let response = app
            .clone()
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        (status, bytes.to_vec())
    }

    /// Like `json_request`, but for endpoints that consume/produce raw
    /// bytes instead of JSON (`/backup`, `/backup/restore`).
    async fn raw_request(
        app: &Router,
        method: &str,
        uri: &str,
        body: Vec<u8>,
    ) -> (StatusCode, Vec<u8>) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
            .await
            .unwrap();
        (status, bytes.to_vec())
    }

    /// Sends a GitHub webhook with a valid `X-Hub-Signature-256` header.
    async fn signed_webhook(app: &Router, payload: Value) -> (StatusCode, Vec<u8>) {
        signed_webhook_with_event(app, payload, None).await
    }

    async fn signed_webhook_with_event(
        app: &Router,
        payload: Value,
        event: Option<&str>,
    ) -> (StatusCode, Vec<u8>) {
        let raw = payload.to_string();
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"test-secret").unwrap();
        mac.update(raw.as_bytes());
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

        let mut builder = Request::builder()
            .method("POST")
            .uri("/api/v1/webhooks/github")
            .header("content-type", "application/json")
            .header("x-hub-signature-256", signature);
        if let Some(event) = event {
            builder = builder.header("x-github-event", event);
        }
        let response = app
            .clone()
            .oneshot(builder.body(Body::from(raw)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        (status, bytes.to_vec())
    }

    #[tokio::test]
    async fn health_ok() {
        let (app, _) = test_app().await;
        let (status, _) = json_request(&app, "GET", "/api/v1/health", json!({})).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn dashboard_static_assets_are_served_without_a_token() {
        let app = test_app_with_token("s3cr3t").await;
        for (path, marker) in [
            ("/", "OXID"),
            ("/index.html", "OXID"),
            ("/style.css", "--oxid-orange"),
            ("/app.js", "function dashboard()"),
            ("/vendor/alpine.min.js", "Alpine"),
        ] {
            let (status, body) = json_request(&app, "GET", path, json!({})).await;
            assert_eq!(status, StatusCode::OK, "{path}");
            let text = String::from_utf8(body).unwrap();
            assert!(text.contains(marker), "{path}: {text:.200}");
        }
    }

    #[tokio::test]
    async fn spa_deep_links_fall_back_to_the_dashboard_shell() {
        let app = test_app_with_token("s3cr3t").await;
        // The client-side router owns everything under `/ui/...` — a hard
        // refresh or a shared link on any of these has to return the same
        // `index.html` shell, not a 404, so the JS router can take over and
        // render the right page from `location.pathname`.
        for path in [
            "/ui/environments",
            "/ui/projects/1",
            "/ui/projects/1/secrets",
            "/ui/environments/1?tab=logs",
            "/ui/audit",
            "/ui/admin",
            "/this/route/does/not/exist/at/all",
        ] {
            let (status, body) = json_request(&app, "GET", path, json!({})).await;
            assert_eq!(status, StatusCode::OK, "{path}");
            let text = String::from_utf8(body).unwrap();
            assert!(text.contains("OXID"), "{path}: {text:.200}");
        }
    }

    #[tokio::test]
    async fn stats_endpoint_reports_aggregate_counts() {
        let (app, _) = test_app().await;
        let repo = repo_dir_with_config();
        let (_, body) = json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;
        let project: Project = serde_json::from_slice(&body).unwrap();
        json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
            json!({ "branch": "feature-login" }),
        )
        .await;

        let (status, body) = json_request(&app, "GET", "/api/v1/stats", json!({})).await;
        assert_eq!(status, StatusCode::OK);
        let node_stats: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(node_stats["projects"], 1);
        assert_eq!(node_stats["environments_running"], 1);
        assert!(node_stats["host_total_memory_bytes"].as_u64().unwrap() > 0);
        // No `with_traefik(...)` call in `test_app()` — the dashboard relies
        // on this to know an environment's `url` isn't a reachable link
        // without Traefik fronting it (SPEC.md's direct-port-publish mode).
        assert_eq!(node_stats["traefik_enabled"], false);
    }

    #[tokio::test]
    async fn api_is_open_when_no_token_is_configured() {
        let (app, _) = test_app().await;
        let (status, _) = json_request(&app, "GET", "/api/v1/projects", json!({})).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn protected_routes_reject_missing_or_wrong_token() {
        let app = test_app_with_token("s3cr3t").await;

        let (status, _) = json_request(&app, "GET", "/api/v1/projects", json!({})).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _) = json_request_with_auth(
            &app,
            "GET",
            "/api/v1/projects",
            json!({}),
            Some("wrong-token"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn protected_routes_accept_the_correct_token() {
        let app = test_app_with_token("s3cr3t").await;
        let (status, _) =
            json_request_with_auth(&app, "GET", "/api/v1/projects", json!({}), Some("s3cr3t"))
                .await;
        assert_eq!(status, StatusCode::OK);
    }

    /// `/health` and the Traefik-facing `/wake`/`/heartbeat` endpoints must
    /// stay reachable without a token even when one is configured — Traefik
    /// has no way to attach it, and health checks shouldn't need auth.
    #[tokio::test]
    async fn public_routes_stay_open_even_with_a_token_configured() {
        let app = test_app_with_token("s3cr3t").await;
        let (status, _) = json_request(&app, "GET", "/api/v1/health", json!({})).await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = request_with_host(&app, "GET", "/api/v1/heartbeat", "nobody").await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = request_with_host(&app, "POST", "/api/v1/wake", "nobody").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn creating_a_token_requires_the_master_credential() {
        let app = test_app_with_token("master-secret").await;

        // A request with no token at all isn't even authenticated.
        let (status, _) =
            json_request(&app, "POST", "/api/v1/tokens", json!({ "name": "alice" })).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // The master token can mint one.
        let (status, body) = json_request_with_auth(
            &app,
            "POST",
            "/api/v1/tokens",
            json!({ "name": "alice" }),
            Some("master-secret"),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let created: Value = serde_json::from_slice(&body).unwrap();
        let alice_token = created["token"].as_str().unwrap().to_owned();
        assert_eq!(alice_token.len(), 64, "expected a 32-byte hex token");

        // Alice's own (non-master) token cannot mint tokens for others.
        let (status, _) = json_request_with_auth(
            &app,
            "POST",
            "/api/v1/tokens",
            json!({ "name": "bob" }),
            Some(&alice_token),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_named_token_authenticates_and_attributes_audit_events() {
        let repo = repo_dir_with_config();
        let app = test_app_with_token("master-secret").await;

        let (_, body) = json_request_with_auth(
            &app,
            "POST",
            "/api/v1/tokens",
            json!({ "name": "alice" }),
            Some("master-secret"),
        )
        .await;
        let alice_token = serde_json::from_slice::<Value>(&body).unwrap()["token"]
            .as_str()
            .unwrap()
            .to_owned();

        let (status, body) = json_request_with_auth(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
            Some(&alice_token),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let project: Project = serde_json::from_slice(&body).unwrap();

        let (status, body) = json_request_with_auth(
            &app,
            "POST",
            format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
            json!({ "branch": "feature-login" }),
            Some(&alice_token),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let env: Environment = serde_json::from_slice(&body).unwrap();

        let (status, body) = json_request_with_auth(
            &app,
            "GET",
            format!("/api/v1/environments/{}/audit", env.id.0).as_str(),
            json!({}),
            Some("master-secret"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let events: Vec<Value> = serde_json::from_slice(&body).unwrap();
        assert!(
            events.iter().any(|e| e["operator"] == "alice"),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn revoked_tokens_stop_authenticating() {
        let app = test_app_with_token("master-secret").await;
        let (_, body) = json_request_with_auth(
            &app,
            "POST",
            "/api/v1/tokens",
            json!({ "name": "alice" }),
            Some("master-secret"),
        )
        .await;
        let created: Value = serde_json::from_slice(&body).unwrap();
        let alice_token = created["token"].as_str().unwrap().to_owned();
        let id = created["id"].as_u64().unwrap();

        let (status, _) = json_request_with_auth(
            &app,
            "GET",
            "/api/v1/projects",
            json!({}),
            Some(&alice_token),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = json_request_with_auth(
            &app,
            "DELETE",
            format!("/api/v1/tokens/{id}").as_str(),
            json!({}),
            Some("master-secret"),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (status, _) = json_request_with_auth(
            &app,
            "GET",
            "/api/v1/projects",
            json!({}),
            Some(&alice_token),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn rotate_key_requires_master_and_keeps_secrets_readable() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let cache = tempfile::tempdir().unwrap();
        let cp = ControlPlane::new(store, FakeGit, FakeOci::default(), cache.path().to_owned())
            .with_readiness_check(false);
        let data_dir = test_data_dir();
        let app = router(ApiState {
            cp,
            webhook_secret: Some("test-secret".to_owned()),
            api_token: Some("master-secret".to_owned()),
            data_dir: data_dir.clone(),
            allow_restore: true,
            rate_limit: None,
        });

        json_request_with_auth(
            &app,
            "POST",
            "/api/v1/secrets",
            json!({ "name": "DB_PASSWORD", "scope": "global", "value": "hunter2" }),
            Some("master-secret"),
        )
        .await;

        let old_key = std::fs::read(data_dir.join("secret.key")).unwrap();

        // A named (non-master) token can't rotate the key.
        let (_, body) = json_request_with_auth(
            &app,
            "POST",
            "/api/v1/tokens",
            json!({ "name": "alice" }),
            Some("master-secret"),
        )
        .await;
        let alice_token = serde_json::from_slice::<Value>(&body).unwrap()["token"]
            .as_str()
            .unwrap()
            .to_owned();
        let (status, _) = json_request_with_auth(
            &app,
            "POST",
            "/api/v1/rotate-key",
            json!({}),
            Some(&alice_token),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // The master token can, and the secret.key file actually changes.
        let (status, _) = json_request_with_auth(
            &app,
            "POST",
            "/api/v1/rotate-key",
            json!({}),
            Some("master-secret"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let new_key = std::fs::read(data_dir.join("secret.key")).unwrap();
        assert_ne!(old_key, new_key);

        // Secrets set before rotation are still readable after it — hot
        // rotation, not "wipe and start over".
        let (status, body) = json_request_with_auth(
            &app,
            "GET",
            "/api/v1/secrets",
            json!({}),
            Some("master-secret"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["secrets"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rate_limit_blocks_a_burst_past_its_configured_size() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let cache = tempfile::tempdir().unwrap();
        let cp = ControlPlane::new(store, FakeGit, FakeOci::default(), cache.path().to_owned())
            .with_readiness_check(false);
        let app = router(ApiState {
            cp,
            webhook_secret: Some("test-secret".to_owned()),
            api_token: None,
            data_dir: test_data_dir(),
            allow_restore: true,
            // 1 request/sec sustained, burst of 2 — the 3rd immediate
            // request must be rejected.
            rate_limit: Some((1, 2)),
        });

        let mut statuses = Vec::new();
        for _ in 0..3 {
            let (status, _) = json_request(&app, "GET", "/api/v1/projects", json!({})).await;
            statuses.push(status);
        }
        assert_eq!(statuses[0], StatusCode::OK);
        assert_eq!(statuses[1], StatusCode::OK);
        assert_eq!(statuses[2], StatusCode::TOO_MANY_REQUESTS, "{statuses:?}");

        // Public routes (no auth gate) are never rate-limited by this —
        // Traefik's forwardAuth heartbeat hits `/heartbeat` on every single
        // request to a live app and must never be throttled.
        for _ in 0..5 {
            let (status, _) = request_with_host(&app, "POST", "/api/v1/wake", "nobody").await;
            assert_eq!(status, StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn register_and_deploy_flow() {
        let repo = repo_dir_with_config();
        let (app, oci) = test_app().await;

        let (status, body) = json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let project: Project = serde_json::from_slice(&body).unwrap();
        assert_eq!(project.name, "app");

        let (status, body) = json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
            json!({ "branch": "feature-login" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let env: Environment = serde_json::from_slice(&body).unwrap();
        assert_eq!(env.state.to_string(), "running");
        assert_eq!(env.url, "feature-login.app.local.dev");

        let (status, body) = json_request(
            &app,
            "GET",
            format!("/api/v1/projects/{}/environments", project.id.0).as_str(),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let envs: Vec<Environment> = serde_json::from_slice(&body).unwrap();
        assert_eq!(envs.len(), 1);

        let calls = oci.calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with("run:oxid-app-feature-login")),
            "{calls:?}"
        );
    }

    #[tokio::test]
    async fn backup_produces_a_tar_with_a_valid_sqlite_snapshot_and_the_secret_key() {
        // `VACUUM INTO` (what `backup_to` uses) doesn't work against a
        // `:memory:` source — a real file-backed store is needed here,
        // matching how the daemon always runs in production.
        let data_dir = test_data_dir();
        let store = SqliteStore::open(data_dir.join("audit.sqlite"), Cipher::from_key([1u8; 32]))
            .await
            .unwrap();
        let cache = tempfile::tempdir().unwrap();
        let cp = ControlPlane::new(store, FakeGit, FakeOci::default(), cache.path().to_owned())
            .with_readiness_check(false);
        let app = router(ApiState {
            cp,
            webhook_secret: Some("test-secret".to_owned()),
            api_token: None,
            data_dir: data_dir.clone(),
            allow_restore: true,
            rate_limit: None,
        });
        // Give the backup something real to capture.
        json_request(
            &app,
            "POST",
            "/api/v1/secrets",
            json!({ "name": "GLOBAL_X", "scope": "global", "value": "v" }),
        )
        .await;

        let (status, body) = raw_request(&app, "GET", "/api/v1/backup", Vec::new()).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

        let dir = tempfile::tempdir().unwrap();
        let mut archive = tar::Archive::new(body.as_slice());
        archive.unpack(dir.path()).unwrap();
        assert!(dir.path().join("secret.key").exists());
        let snapshot = dir.path().join("audit.sqlite");
        assert!(snapshot.exists());

        let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&snapshot);
        let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM secrets WHERE name = 'GLOBAL_X'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.0, 1);
    }

    #[tokio::test]
    async fn restore_is_rejected_when_not_allowed() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let cache = tempfile::tempdir().unwrap();
        let cp = ControlPlane::new(store, FakeGit, FakeOci::default(), cache.path().to_owned())
            .with_readiness_check(false);
        let app = router(ApiState {
            cp,
            webhook_secret: Some("test-secret".to_owned()),
            api_token: None,
            data_dir: test_data_dir(),
            allow_restore: false,
            rate_limit: None,
        });

        let (status, _) = raw_request(&app, "POST", "/api/v1/backup/restore", vec![1, 2, 3]).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn restore_stages_the_upload_without_touching_the_live_database() {
        let data_dir = test_data_dir();
        let store = SqliteStore::open(data_dir.join("audit.sqlite"), Cipher::from_key([1u8; 32]))
            .await
            .unwrap();
        let cache = tempfile::tempdir().unwrap();
        let cp = ControlPlane::new(store, FakeGit, FakeOci::default(), cache.path().to_owned())
            .with_readiness_check(false);
        let app = router(ApiState {
            cp,
            webhook_secret: Some("test-secret".to_owned()),
            api_token: None,
            data_dir: data_dir.clone(),
            allow_restore: true,
            rate_limit: None,
        });

        let (status, body) = raw_request(&app, "GET", "/api/v1/backup", Vec::new()).await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = raw_request(&app, "POST", "/api/v1/backup/restore", body).await;
        assert_eq!(status, StatusCode::ACCEPTED);

        // Staged for the next restart to pick up, not applied in place —
        // the running daemon's own database is left untouched.
        assert!(data_dir.join(".restore-pending.tar").exists());
        let (status, _) = json_request(&app, "GET", "/api/v1/health", json!({})).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn audit_endpoints_expose_the_previously_write_only_trail() {
        let repo = repo_dir_with_config();
        let (app, _) = test_app().await;
        let (_, body) = json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;
        let project: Project = serde_json::from_slice(&body).unwrap();
        let (_, body) = json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
            json!({ "branch": "feature-login" }),
        )
        .await;
        let env: Environment = serde_json::from_slice(&body).unwrap();

        let (status, body) = json_request(
            &app,
            "GET",
            format!("/api/v1/environments/{}/audit", env.id.0).as_str(),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let events: Vec<Value> = serde_json::from_slice(&body).unwrap();
        assert!(!events.is_empty(), "expected at least the Deploy event");
        assert_eq!(events[0]["environment_id"], env.id.0);

        let (status, body) = json_request(&app, "GET", "/api/v1/audit", json!({})).await;
        assert_eq!(status, StatusCode::OK);
        let recent: Vec<Value> = serde_json::from_slice(&body).unwrap();
        assert!(
            recent.iter().any(|e| e["environment_id"] == env.id.0),
            "{recent:?}"
        );

        // `kind`/`project_id` narrow `/api/v1/audit` to a subset.
        let (status, body) =
            json_request(&app, "GET", "/api/v1/audit?kind=build_succeeded", json!({})).await;
        assert_eq!(status, StatusCode::OK);
        let filtered: Vec<Value> = serde_json::from_slice(&body).unwrap();
        assert!(
            filtered
                .iter()
                .all(|e| e["kind"] == "build_succeeded" && e["environment_id"] == env.id.0),
            "{filtered:?}"
        );

        let (status, body) = json_request(
            &app,
            "GET",
            format!("/api/v1/audit?project_id={}", project.id.0 + 1).as_str(),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let none: Vec<Value> = serde_json::from_slice(&body).unwrap();
        assert!(none.is_empty(), "{none:?}");

        // An unparseable `kind`/`since` is a 400, not a silently-ignored filter.
        let (status, _) =
            json_request(&app, "GET", "/api/v1/audit?kind=not_a_kind", json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) =
            json_request(&app, "GET", "/api/v1/audit?since=not-a-date", json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn deploy_request_id_is_correlated_into_the_audit_trail() {
        let repo = repo_dir_with_config();
        let (app, _) = test_app().await;
        let (_, body) = json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;
        let project: Project = serde_json::from_slice(&body).unwrap();

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/projects/{}/deploy", project.id.0))
                    .header("content-type", "application/json")
                    .header("x-request-id", "trace-abc-123")
                    .body(Body::from(json!({ "branch": "feature-login" }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers().get("x-request-id").unwrap(),
            "trace-abc-123"
        );
        let body = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        let env: Environment = serde_json::from_slice(&body).unwrap();

        let (_, body) = json_request(
            &app,
            "GET",
            format!("/api/v1/environments/{}/audit", env.id.0).as_str(),
            json!({}),
        )
        .await;
        let events: Vec<Value> = serde_json::from_slice(&body).unwrap();
        assert!(
            events
                .iter()
                .any(|e| e["request_id"] == "trace-abc-123" && e["kind"] == "build_succeeded"),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn every_response_carries_a_request_id_header_and_echoes_a_provided_one() {
        let (app, _) = test_app().await;

        // No `X-Request-Id` sent: one is generated.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let generated = response
            .headers()
            .get("x-request-id")
            .expect("x-request-id header present")
            .to_str()
            .unwrap()
            .to_owned();
        assert!(!generated.is_empty());

        // A caller-supplied id is echoed back unchanged.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/health")
                    .header("x-request-id", "my-trace-id-123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers().get("x-request-id").unwrap(),
            "my-trace-id-123"
        );
    }

    #[tokio::test]
    async fn deploy_queues_past_capacity_and_the_queue_endpoint_reports_it() {
        // 8GB host (FakeOci's fixed `host_capacity`) minus 8000MB reserved
        // leaves 192MB usable; two 100MB deploys can't both fit.
        let (app, _) = test_app_with_admission_control(8000, 100).await;
        let repo = repo_dir_with_config();
        let (_, body) = json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;
        let project: Project = serde_json::from_slice(&body).unwrap();

        let (status, _) = json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
            json!({ "branch": "main" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let (status, body) = json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
            json!({ "branch": "other" }),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let queued: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(queued["status"], "queued");
        assert_eq!(queued["position"], 1);

        let (status, body) = json_request(&app, "GET", "/api/v1/queue", json!({})).await;
        assert_eq!(status, StatusCode::OK);
        let entries: Vec<Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["branch"], "other");
    }

    #[tokio::test]
    async fn rollback_endpoint_is_wired_and_errors_clearly_with_no_prior_deploy() {
        let repo = repo_dir_with_config();
        let (app, _) = test_app().await;
        let (_, body) = json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;
        let project: Project = serde_json::from_slice(&body).unwrap();

        // No deploy yet: rollback has nothing to roll back to.
        let (status, _) = json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/rollback", project.id.0).as_str(),
            json!({ "branch": "feature-login" }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // One deploy: still nothing *prior* to roll back to.
        json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
            json!({ "branch": "feature-login" }),
        )
        .await;
        let (status, _) = json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/rollback", project.id.0).as_str(),
            json!({ "branch": "feature-login" }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // A second deploy gives rollback something to redeploy.
        json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
            json!({ "branch": "feature-login" }),
        )
        .await;
        let (status, body) = json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/rollback", project.id.0).as_str(),
            json!({ "branch": "feature-login" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let env: Environment = serde_json::from_slice(&body).unwrap();
        assert_eq!(env.state.to_string(), "running");
    }

    #[tokio::test]
    async fn logs_stream_endpoint_emits_sse_data_events() {
        let repo = repo_dir_with_config();
        let (app, _) = test_app().await;

        let (_, body) = json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;
        let project: Project = serde_json::from_slice(&body).unwrap();

        let (_, body) = json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
            json!({ "branch": "feature-login" }),
        )
        .await;
        let env: Environment = serde_json::from_slice(&body).unwrap();

        let (status, body) = json_request(
            &app,
            "GET",
            format!("/api/v1/environments/{}/logs/stream", env.id.0).as_str(),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let text = String::from_utf8(body).unwrap();
        assert!(text.contains("data: build log"), "{text:?}");
    }

    #[tokio::test]
    async fn webhook_deploys_branch_when_signed() {
        let repo = repo_dir_with_config();
        let (app, _) = test_app().await;

        json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;

        let (status, body) = signed_webhook(
            &app,
            json!({
                "ref": "refs/heads/feature-hook",
                "repository": { "full_name": "org/app" }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["status"], "deployed");
    }

    /// Regression test: GitHub sends a `ping` event (no `ref` at all) the
    /// moment a webhook is configured. This used to fail with "webhook
    /// payload is missing `ref`" instead of just being acknowledged.
    #[tokio::test]
    async fn webhook_ignores_non_push_events() {
        let (app, _) = test_app().await;
        let (status, body) = signed_webhook_with_event(
            &app,
            json!({ "zen": "Anything added dilutes everything else." }),
            Some("ping"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["status"], "ignored");
    }

    /// Regression test: a push with `"deleted": true` (branch deletion on
    /// GitHub) used to be treated like a normal push and attempt to deploy
    /// a branch that no longer exists in the remote, failing with a
    /// confusing git error. It should destroy the branch's environment
    /// instead.
    #[tokio::test]
    async fn webhook_destroys_environment_on_branch_deletion() {
        let repo = repo_dir_with_config();
        let (app, oci) = test_app().await;
        json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;
        signed_webhook(
            &app,
            json!({
                "ref": "refs/heads/feature-hook",
                "repository": { "full_name": "org/app" }
            }),
        )
        .await;

        let (status, body) = signed_webhook(
            &app,
            json!({
                "ref": "refs/heads/feature-hook",
                "deleted": true,
                "repository": { "full_name": "org/app" }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["status"], "destroyed");
        assert!(
            oci.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.starts_with("remove:")),
            "{:?}",
            oci.calls
        );
    }

    /// A deletion push for a branch Oxid never deployed must be a no-op,
    /// not an error.
    #[tokio::test]
    async fn webhook_branch_deletion_for_unknown_branch_is_a_noop() {
        let repo = repo_dir_with_config();
        let (app, _) = test_app().await;
        json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;

        let (status, body) = signed_webhook(
            &app,
            json!({
                "ref": "refs/heads/never-deployed",
                "deleted": true,
                "repository": { "full_name": "org/app" }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["status"], "ignored");
    }

    #[tokio::test]
    async fn webhook_rejects_bad_signature() {
        let repo = repo_dir_with_config();
        let (app, _) = test_app().await;

        json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;

        let (status, _) = json_request(
            &app,
            "POST",
            "/api/v1/webhooks/github",
            json!({
                "ref": "refs/heads/feature-hook",
                "repository": { "full_name": "org/app" }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn webhook_rejects_wrong_secret() {
        let repo = repo_dir_with_config();
        let (app, _) = test_app().await;
        json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;

        let raw = serde_json::to_string(&json!({
            "ref": "refs/heads/feature-hook",
            "repository": { "full_name": "org/app" }
        }))
        .unwrap();
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"wrong-secret").unwrap();
        mac.update(raw.as_bytes());
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/webhooks/github")
                    .header("content-type", "application/json")
                    .header("x-hub-signature-256", signature)
                    .body(Body::from(raw))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn secrets_crud_and_injection() {
        let repo = repo_dir_with_config();
        let (app, oci) = test_app().await;

        let (status, body) = json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let project: Project = serde_json::from_slice(&body).unwrap();

        let (status, _) = json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/secrets", project.id.0).as_str(),
            json!({ "name": "DB_PASSWORD", "scope": "project", "value": "hunter2" }),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (status, _) = json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/secrets", project.id.0).as_str(),
            json!({
                "name": "API_TOKEN",
                "scope": "branch",
                "branch": "feature-login",
                "value": "tok-123"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // Without a `branch` filter, only global+project-scope secrets are
        // visible — a `branch`-scoped secret is meaningless without a branch
        // context and must not leak into this listing.
        let (status, body) = json_request(
            &app,
            "GET",
            format!("/api/v1/projects/{}/secrets", project.id.0).as_str(),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["secrets"].as_array().unwrap().len(), 1);

        // With `?branch=feature-login`, its branch-scoped secret joins the
        // project-scope one.
        let (status, body) = json_request(
            &app,
            "GET",
            format!(
                "/api/v1/projects/{}/secrets?branch=feature-login",
                project.id.0
            )
            .as_str(),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["secrets"].as_array().unwrap().len(), 2);

        let (status, _) = json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
            json!({ "branch": "feature-login" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let calls = oci.calls.lock().unwrap();
        let run = calls
            .iter()
            .find(|c| c.starts_with("run:"))
            .expect("container was started");
        assert!(run.starts_with("run:oxid-app-feature-login"), "{run}");
        assert!(run.contains("\"DB_PASSWORD\": \"hunter2\""), "{run}");
        assert!(run.contains("\"API_TOKEN\": \"tok-123\""), "{run}");
        assert!(run.contains("\"OXID_BRANCH\": \"feature-login\""), "{run}");
    }

    /// Regression test for a real secret-leakage bug found by deploying two
    /// real branches with same-named branch-scoped secrets: the SQL filter
    /// resolving "secrets visible to this deploy" matched every row for the
    /// project regardless of branch, so branch A's value could shadow branch
    /// B's when both defined the same key.
    #[tokio::test]
    async fn branch_secrets_do_not_cross_over_on_deploy() {
        let repo = repo_dir_with_config();
        let (app, oci) = test_app().await;
        let (_, body) = json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;
        let project: Project = serde_json::from_slice(&body).unwrap();

        json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/secrets", project.id.0).as_str(),
            json!({
                "name": "DB_PASSWORD", "scope": "branch",
                "branch": "feature-a", "value": "secret-a"
            }),
        )
        .await;
        json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/secrets", project.id.0).as_str(),
            json!({
                "name": "DB_PASSWORD", "scope": "branch",
                "branch": "feature-b", "value": "secret-b"
            }),
        )
        .await;

        json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
            json!({ "branch": "feature-a" }),
        )
        .await;
        json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
            json!({ "branch": "feature-b" }),
        )
        .await;

        let calls = oci.calls.lock().unwrap();
        let run_a = calls
            .iter()
            .find(|c| c.starts_with("run:oxid-app-feature-a-"))
            .expect("feature-a container was started");
        let run_b = calls
            .iter()
            .find(|c| c.starts_with("run:oxid-app-feature-b-"))
            .expect("feature-b container was started");
        assert!(run_a.contains("\"DB_PASSWORD\": \"secret-a\""), "{run_a}");
        assert!(run_b.contains("\"DB_PASSWORD\": \"secret-b\""), "{run_b}");
    }

    #[tokio::test]
    async fn global_secret_endpoint() {
        let (app, _) = test_app().await;
        let (status, _) = json_request(
            &app,
            "POST",
            "/api/v1/secrets",
            json!({ "name": "GLOBAL_FLAG", "scope": "global", "value": "1" }),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (status, body) = json_request(&app, "GET", "/api/v1/secrets", json!({})).await;
        assert_eq!(status, StatusCode::OK);
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["secrets"].as_array().unwrap().len(), 1);

        let (status, _) =
            json_request(&app, "DELETE", "/api/v1/secrets/GLOBAL_FLAG", json!({})).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn rejects_invalid_scope() {
        let (app, _) = test_app().await;
        let (status, _) = json_request(
            &app,
            "POST",
            "/api/v1/secrets",
            json!({ "name": "X", "scope": "runtime", "value": "1" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn missing_project_is_404() {
        let (app, _) = test_app().await;
        let (status, _) = json_request(
            &app,
            "POST",
            "/api/v1/projects/999/deploy",
            json!({ "branch": "main" }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    async fn request_with_host(
        app: &Router,
        method: &str,
        uri: &str,
        host: &str,
    ) -> (StatusCode, Vec<u8>) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("host", host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        (status, bytes.to_vec())
    }

    #[tokio::test]
    async fn destroy_removes_environment() {
        let repo = repo_dir_with_config();
        let (app, oci) = test_app().await;
        let (_, body) = json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;
        let project: Project = serde_json::from_slice(&body).unwrap();
        let (_, body) = json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
            json!({ "branch": "feature-login" }),
        )
        .await;
        let env: Environment = serde_json::from_slice(&body).unwrap();

        let (status, _) = json_request(
            &app,
            "DELETE",
            format!("/api/v1/environments/{}", env.id.0).as_str(),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(
            oci.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.starts_with("remove:")),
            "{:?}",
            oci.calls
        );
    }

    #[tokio::test]
    async fn destroy_with_purge_secrets_query_param_deletes_branch_secrets() {
        let repo = repo_dir_with_config();
        let (app, _) = test_app().await;
        let (_, body) = json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;
        let project: Project = serde_json::from_slice(&body).unwrap();
        json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/secrets", project.id.0).as_str(),
            json!({
                "name": "API_KEY", "scope": "branch",
                "branch": "feature-login", "value": "x"
            }),
        )
        .await;
        let (_, body) = json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
            json!({ "branch": "feature-login" }),
        )
        .await;
        let env: Environment = serde_json::from_slice(&body).unwrap();

        let (status, _) = json_request(
            &app,
            "DELETE",
            format!("/api/v1/environments/{}?purge_secrets=true", env.id.0).as_str(),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (_, body) = json_request(
            &app,
            "GET",
            format!(
                "/api/v1/projects/{}/secrets?branch=feature-login",
                project.id.0
            )
            .as_str(),
            json!({}),
        )
        .await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            value["secrets"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["name"] != "API_KEY"),
            "{value}"
        );
    }

    #[tokio::test]
    async fn delete_project_endpoint_removes_project_and_environments() {
        let repo = repo_dir_with_config();
        let (app, oci) = test_app().await;
        let (_, body) = json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;
        let project: Project = serde_json::from_slice(&body).unwrap();
        json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
            json!({ "branch": "feature-login" }),
        )
        .await;

        let (status, _) = json_request(
            &app,
            "DELETE",
            format!("/api/v1/projects/{}", project.id.0).as_str(),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(
            oci.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.starts_with("remove_image:")),
            "{:?}",
            oci.calls
        );

        let (status, _) = json_request(
            &app,
            "GET",
            format!("/api/v1/projects/{}/environments", project.id.0).as_str(),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn update_project_changes_ttls_and_rejects_bad_durations() {
        let repo = repo_dir_with_config();
        let (app, _) = test_app().await;
        let (_, body) = json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;
        let project: Project = serde_json::from_slice(&body).unwrap();

        let (status, body) = json_request(
            &app,
            "PATCH",
            format!("/api/v1/projects/{}", project.id.0).as_str(),
            json!({ "pause_after": "45m" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let updated: Project = serde_json::from_slice(&body).unwrap();
        assert_eq!(updated.config.pause_after.to_string(), "2700s");
        // Omitted field stays whatever it already was.
        assert_eq!(updated.config.destroy_after, project.config.destroy_after);

        let (status, body) = json_request(
            &app,
            "PATCH",
            format!("/api/v1/projects/{}", project.id.0).as_str(),
            json!({ "destroy_after": "not-a-duration" }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{}",
            String::from_utf8_lossy(&body)
        );
    }

    #[tokio::test]
    async fn list_environments_filters_by_branch() {
        let repo = repo_dir_with_config();
        let (app, _) = test_app().await;
        let (_, body) = json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;
        let project: Project = serde_json::from_slice(&body).unwrap();
        json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
            json!({ "branch": "feature-login" }),
        )
        .await;

        let (status, body) = json_request(
            &app,
            "GET",
            format!(
                "/api/v1/projects/{}/environments?branch=feature-login",
                project.id.0
            )
            .as_str(),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let envs: Vec<Environment> = serde_json::from_slice(&body).unwrap();
        assert_eq!(envs.len(), 1);

        let (status, body) = json_request(
            &app,
            "GET",
            format!("/api/v1/projects/{}/environments?branch=nope", project.id.0).as_str(),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let envs: Vec<Environment> = serde_json::from_slice(&body).unwrap();
        assert!(envs.is_empty());
    }

    #[tokio::test]
    async fn wake_by_host_wakes_matching_environment() {
        let repo = repo_dir_with_config();
        let (app, oci) = test_app().await;
        let (_, body) = json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;
        let project: Project = serde_json::from_slice(&body).unwrap();
        let (_, body) = json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
            json!({ "branch": "feature-login" }),
        )
        .await;
        let env: Environment = serde_json::from_slice(&body).unwrap();
        json_request(
            &app,
            "POST",
            format!("/api/v1/environments/{}/pause", env.id.0).as_str(),
            json!({}),
        )
        .await;

        let (status, body) = request_with_host(&app, "POST", "/api/v1/wake", &env.url).await;
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8_lossy(&body).contains("feature-login"));
        assert!(
            oci.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.starts_with("unpause:")),
            "{:?}",
            oci.calls
        );
    }

    #[tokio::test]
    async fn wake_by_host_unknown_host_is_ok_noop() {
        let (app, _) = test_app().await;
        let (status, _) = request_with_host(&app, "POST", "/api/v1/wake", "nobody.local.dev").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn heartbeat_always_ok() {
        let repo = repo_dir_with_config();
        let (app, _) = test_app().await;
        let (_, body) = json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;
        let project: Project = serde_json::from_slice(&body).unwrap();
        let (_, body) = json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
            json!({ "branch": "feature-login" }),
        )
        .await;
        let env: Environment = serde_json::from_slice(&body).unwrap();

        let (status, _) = request_with_host(&app, "GET", "/api/v1/heartbeat", &env.url).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) =
            request_with_host(&app, "GET", "/api/v1/heartbeat", "nobody.local.dev").await;
        assert_eq!(status, StatusCode::OK);
    }
}
