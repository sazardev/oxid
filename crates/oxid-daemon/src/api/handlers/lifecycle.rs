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
use crate::api::middleware::authorize;
use crate::api::middleware::{AuthedAs, operator_name};
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
use oxid_core::services::access::Capability;
use oxid_core::{
    AuditFilter, BranchName, ContainerPort, EnvVarScope, Environment, EnvironmentId,
    EnvironmentState, GitPort, PoolError, Project, ProjectId, RepositoryError, StateTransition,
    Ttl,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use tower_governor::GovernorLayer;

pub async fn pause<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(env_id): Path<u64>,
    authed: Option<Extension<AuthedAs>>,
) -> ApiResult<StatusCode> {
    // Environment-addressed routes are authorized by the environment's
    // *project* (404 for out-of-scope ids, before any state changes).
    let project_id = state
        .cp
        .environment_project_id(EnvironmentId(env_id))
        .await?;
    authorize(&authed, Capability::Operate, Some(project_id.0))?;
    state
        .cp
        .pause_with_operator(EnvironmentId(env_id), operator_name(authed.as_ref()))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn wake<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(env_id): Path<u64>,
    authed: Option<Extension<AuthedAs>>,
) -> ApiResult<StatusCode> {
    let project_id = state
        .cp
        .environment_project_id(EnvironmentId(env_id))
        .await?;
    authorize(&authed, Capability::Operate, Some(project_id.0))?;
    state
        .cp
        .wake_with_operator(EnvironmentId(env_id), operator_name(authed.as_ref()))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn logs<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(env_id): Path<u64>,
    authed: Option<Extension<AuthedAs>>,
) -> ApiResult<Json<Value>> {
    let project_id = state
        .cp
        .environment_project_id(EnvironmentId(env_id))
        .await?;
    authorize(&authed, Capability::Read, Some(project_id.0))?;
    let logs = state.cp.logs(EnvironmentId(env_id)).await?;
    Ok(Json(json!({ "logs": logs })))
}

/// Follows an environment's container logs live over SSE, one `data:` event
/// per line, instead of the bounded snapshot [`logs`] returns.
pub async fn stream_logs<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(env_id): Path<u64>,
    authed: Option<Extension<AuthedAs>>,
) -> ApiResult<Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>>> {
    let project_id = state
        .cp
        .environment_project_id(EnvironmentId(env_id))
        .await?;
    authorize(&authed, Capability::Read, Some(project_id.0))?;
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

pub async fn destroy<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(env_id): Path<u64>,
    Query(query): Query<DestroyQuery>,
    authed: Option<Extension<AuthedAs>>,
) -> ApiResult<StatusCode> {
    let project_id = state
        .cp
        .environment_project_id(EnvironmentId(env_id))
        .await?;
    authorize(&authed, Capability::Operate, Some(project_id.0))?;
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
pub async fn wake_by_host<
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
        // Marks this response as the interstitial rather than the app's own
        // output, so the page's poll below can tell the two apart on the
        // same URL without parsing any HTML.
        [(WAKING_HEADER, HeaderValue::from_static("1"))],
        axum::response::Html(wake_page_html(&branch)),
    )
        .into_response())
}

/// Present only on the wake interstitial. Its absence is what tells the page
/// the environment is serving again.
const WAKING_HEADER: &str = "x-oxid-waking";

/// Heartbeat endpoint (SPEC.md §3.2 traffic monitor): wired as a Traefik
/// `forwardAuth` middleware on every router, it refreshes `last_accessed_at`
/// on each request to a live environment. Always `200 OK` so it never blocks
/// traffic, even for hosts Oxid does not manage.
pub async fn heartbeat_by_host<
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
<html><head><meta charset="utf-8"><meta http-equiv="refresh" content="2">
<style>
body {{ background:#121212; color:#F4F4F5; font-family:monospace;
       display:flex; height:100vh; align-items:center; justify-content:center; }}
strong {{ color:#DE5236; }}
</style></head>
<body><p>[~] Waking up <strong>{branch}</strong>&hellip;</p>
<script>
// A stopped container is usually back and routable in well under a second,
// so a fixed reload timer was most of the visible wake: the page sat out its
// full interval and, if the proxy had not caught up yet, sat out another
// one. This polls the same URL instead and reloads the moment the response
// stops being this interstitial. The <meta refresh> above stays as the
// no-JavaScript fallback.
(function () {{
  var tries = 0;
  function poll() {{
    if (++tries > 120) return; // ~30s, then leave it to the meta refresh
    fetch(location.href, {{ cache: "no-store" }})
      .then(function (r) {{
        if (!r.headers.get("x-oxid-waking")) {{ location.reload(); return; }}
        setTimeout(poll, 250);
      }})
      .catch(function () {{ setTimeout(poll, 250); }});
  }}
  setTimeout(poll, 250);
}})();
</script>
</body></html>"#
    )
}

// ---------------------------------------------------------------------------
// secret handlers
// ---------------------------------------------------------------------------
