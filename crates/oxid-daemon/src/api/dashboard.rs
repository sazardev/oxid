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

pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

// ---------------------------------------------------------------------------
// web dashboard (SPEC.md §5.3: "incluido dentro del mismo binario estático de
// Rust, archivos precompilados e incrustados") — a handful of static files
// embedded at compile time via `include_str!`, no build step, no bundler,
// and (besides the vendored Alpine.js) no client-side dependency at all.
// ---------------------------------------------------------------------------

pub async fn dashboard_index() -> Html<&'static str> {
    Html(include_str!("../../web/index.html"))
}

pub async fn dashboard_style() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../../web/style.css"),
    )
}

pub async fn dashboard_app_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../../web/app.js"),
    )
}

/// The translation catalog. Its own asset rather than part of `app.js`
/// because it is the file a translator edits, and separating it means adding
/// a language never means reading application code.
pub async fn dashboard_i18n_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../../web/i18n.js"),
    )
}

/// The PWA manifest. Makes the panel installable — a devops on call gets
/// it as an app on the home screen rather than a bookmark behind a browser
/// chrome that wastes a fifth of a phone screen.
pub async fn dashboard_manifest() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/manifest+json; charset=utf-8",
        )],
        include_str!("../../web/manifest.webmanifest"),
    )
}

/// The service worker.
///
/// Served with `Cache-Control: no-cache` deliberately: a stale worker keeps
/// serving a stale shell, and it is the one file whose own caching would
/// defeat the mechanism it implements.
pub async fn dashboard_service_worker() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../../web/sw.js"),
    )
}

/// App icon, and the maskable variant a launcher crops to its own shape.
/// SVG rather than a set of PNGs: one file, a few hundred bytes, sharp at
/// every density, and nothing to generate at build time.
pub async fn dashboard_icon() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/svg+xml")],
        include_str!("../../web/icon.svg"),
    )
}

pub async fn dashboard_icon_maskable() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/svg+xml")],
        include_str!("../../web/icon-maskable.svg"),
    )
}

pub async fn dashboard_alpine_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../../web/vendor/alpine.min.js"),
    )
}
