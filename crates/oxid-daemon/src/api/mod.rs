#![allow(
    clippy::pedantic,
    clippy::nursery,
    clippy::too_many_lines,
    clippy::empty_line_after_doc_comments,
    clippy::duplicate_mod
)]

//! `HTTP` API and webhook surface (SPEC.md §5: the shared internal API).
//!
//! The CLI, TUI, dashboard and desktop app all consume this router.

use std::path::PathBuf;
use std::time::Duration;

use axum::Router;
use axum::middleware::{self as axum_middleware};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use oxid_core::{BranchName, ContainerPort, GitPort};
use tower_governor::GovernorLayer;
use tower_governor::governor::GovernorConfigBuilder;
use tower_http::catch_panic::CatchPanicLayer;

use crate::ControlPlane;

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
    dashboard_alpine_js, dashboard_app_js, dashboard_i18n_js, dashboard_icon,
    dashboard_icon_maskable, dashboard_index, dashboard_manifest, dashboard_service_worker,
    dashboard_style, health,
};
pub use error::{ApiError, ApiResult};
use middleware::{ClientIpKeyExtractor, handle_panic, request_id_middleware, require_bearer_token};
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
    /// Shared secret verifying webhook authenticity for every supported
    /// provider (`OXID_WEBHOOK_SECRET`): GitHub/Gitea/Gogs HMAC-SHA256
    /// signatures and GitLab's plain token echo. Webhooks are rejected
    /// while unset.
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
    /// `OXID_RATE_LIMIT_BURST`). Keyed per client IP
    /// ([`ClientIpKeyExtractor`]) — each peer gets its own token bucket.
    /// Behind a single reverse proxy every request shares the proxy's IP,
    /// so there it degrades back to one shared bucket; see the extractor's
    /// doc comment for why `X-Forwarded-For` is deliberately not trusted.
    pub rate_limit: Option<(u64, u32)>,
    /// Whether this daemon was started with `OXID_AUTO_TOKEN=1` — reported
    /// by the public `GET /api/v1/setup/status` so the onboarding wizard can
    /// point at the exact retrieval command (`docker compose logs …`)
    /// instead of generic "ask your operator" advice.
    pub auto_token: bool,
    /// Who `GET /api/v1/setup/token` will hand the auto-generated master
    /// token to. See [`BootstrapAccess`].
    pub bootstrap_access: BootstrapAccess,
}

/// Who may retrieve the auto-generated master token pre-auth.
///
/// The endpoint exists so a fresh install can self-serve instead of digging
/// through `docker compose logs`, and it is safe exactly as long as only
/// the operator can reach it. Whether that holds is a property of *how the
/// port was published*, which the daemon cannot see: inside a container it
/// is always bound to `0.0.0.0` and a connection forwarded in from anywhere
/// arrives from the bridge gateway, indistinguishable from one the operator
/// made on the host. So the answer is the operator's to give, and the
/// default withholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BootstrapAccess {
    /// Only callers connecting from the daemon's own host. Correct when the
    /// daemon runs as a plain process; a containerized daemon will refuse
    /// even the operator, because Docker's port forwarding rewrites the
    /// peer address. The default: it withholds a credential rather than
    /// assume a topology.
    #[default]
    Loopback,
    /// Any caller that can reach the port. For a containerized daemon whose
    /// port the operator has published privately — as the shipped
    /// `docker-compose.yml` does, on `127.0.0.1` — where "can reach the
    /// port" already means "is on this host".
    Any,
    /// Nobody. The token is retrieved from the daemon's logs or data
    /// directory instead.
    Off,
}

impl BootstrapAccess {
    /// Reads `OXID_BOOTSTRAP_TOKEN_ACCESS`; anything unrecognized (or
    /// unset) is the withholding default.
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var("OXID_BOOTSTRAP_TOKEN_ACCESS")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "any" => Self::Any,
            "off" => Self::Off,
            _ => Self::Loopback,
        }
    }

    /// Whether a caller connecting from `peer` may be served.
    #[must_use]
    pub fn permits(self, peer: Option<std::net::IpAddr>) -> bool {
        match self {
            Self::Off => false,
            Self::Any => true,
            // A missing peer address means nothing looked at where the
            // request came from; the only safe reading is "not local".
            Self::Loopback => peer.is_some_and(|ip| ip.is_loopback()),
        }
    }
}

/// Builds the API router. Every control-plane route under `/api/v1` requires
/// `state.api_token` when it's configured (see [`require_bearer_token`]),
/// with a small public surface kept reachable on purpose: `/health`, the
/// four webhook endpoints (each verifying its own HMAC/token), the
/// Traefik-facing `/wake` and `/heartbeat`, and the pre-auth onboarding pair
/// `/setup/status` + `/setup/token` (itself gated by [`BootstrapAccess`]).
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
            get(handlers::environment::get_environment).delete(handlers::lifecycle::destroy),
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
            "/api/v1/environments/{env_id}/services",
            get(handlers::environment::list_services),
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
            delete(handlers::tokens::revoke_token).patch(handlers::tokens::update_token),
        )
        .route("/api/v1/me", get(handlers::tokens::whoami))
        .route("/api/v1/rotate-key", post(handlers::tokens::rotate_key))
        .route("/api/v1/audit", get(handlers::audit::recent_audit))
        .route("/api/v1/queue", get(handlers::audit::list_queue))
        .route("/api/v1/queue/{id}", delete(handlers::audit::cancel_queued))
        // Master-only: reveals the (env-provided or auto-generated) webhook
        // secret so the wizard can hand it over for the Git host config —
        // same trust level as `GET /api/v1/backup`, which ships `secret.key`.
        .route(
            "/api/v1/setup/webhook-secret",
            get(handlers::setup::webhook_secret),
        )
        .route(
            "/api/v1/nodes",
            get(handlers::node::list_nodes).post(handlers::node::add_node),
        )
        .route(
            "/api/v1/nodes/{id}",
            patch(handlers::node::update_node).delete(handlers::node::remove_node),
        )
        .route(
            "/api/v1/environments",
            get(handlers::node::list_fleet_environments),
        )
        .route("/api/v1/stats", get(handlers::infra::stats))
        .route("/api/v1/infra/status", get(handlers::infra::infra_status))
        .route(
            "/api/v1/traefik/config",
            get(handlers::infra::traefik_config),
        )
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

    // The two routes an unauthenticated caller may reach on purpose, kept
    // together so the rate limiter below can cover them as a unit.
    //
    // `setup/status` is a non-sensitive onboarding probe (booleans only —
    // see `handlers::setup::setup_status`), called before any token exists
    // so the wizard knows what to ask for. `setup/token` hands over the
    // auto-generated master token so the CLI (`oxid token generate`) and
    // the wizard can self-serve instead of hunting through `docker compose
    // logs`; it never reveals an explicitly-configured one, and it now
    // answers only callers on the daemon's own host.
    let mut public_setup = Router::new()
        .route("/api/v1/setup/status", get(handlers::setup::setup_status))
        .route("/api/v1/setup/token", get(handlers::setup::bootstrap_token))
        .with_state(state.clone());

    // The four Git-host webhook endpoints, grouped so a configured rate
    // limit can cover them: they are unauthenticated by design (the HMAC
    // *is* the auth) and a flood of bad signatures is otherwise free CPU.
    // `/wake` and `/heartbeat` deliberately stay outside every bucket — see
    // `rate_limit_blocks_a_burst_past_its_configured_size`: Traefik's
    // `errors` middleware hits them on every request to a live app, so
    // throttling them would throttle real users behind the proxy's IP.
    let mut webhooks = Router::new()
        .route(
            "/api/v1/webhooks/github",
            post(handlers::webhook::github_webhook),
        )
        .route(
            "/api/v1/webhooks/gitlab",
            post(handlers::webhook::gitlab_webhook),
        )
        .route(
            "/api/v1/webhooks/gitea",
            post(handlers::webhook::gitea_webhook),
        )
        .route(
            "/api/v1/webhooks/gogs",
            post(handlers::webhook::gogs_webhook),
        );

    if let Some((per_second, burst)) = rate_limit {
        // `tower_governor`'s `per_second(n)` is a false friend: despite the
        // name, it sets the replenish *period* to `n` seconds per token
        // (i.e. a rate of `1/n` per second), not "n tokens per second" —
        // confirmed live: `OXID_RATE_LIMIT_PER_SECOND=10` was silently
        // configuring one token every 10s (a sustained ~0.1 req/s), so any
        // burst took minutes to recover instead of ~2s, wedging `oxid
        // doctor` and the dashboard in 429s well after traffic stopped.
        // `.period()` takes the actual per-token interval, so invert here.
        let per_second = u32::try_from(per_second.max(1)).unwrap_or(u32::MAX);
        let bucket = || {
            GovernorConfigBuilder::default()
                .period(Duration::from_secs(1) / per_second)
                .burst_size(burst.max(1))
                .key_extractor(ClientIpKeyExtractor)
                .finish()
                .expect("valid rate limit config")
        };
        protected = protected.layer(GovernorLayer::new(bucket()));
        // Webhooks share the same configured limit: same trust level (none
        // by construction) and the same flood cost.
        webhooks = webhooks.layer(GovernorLayer::new(bucket()));
        // The pre-auth onboarding routes get a bucket of their own. They
        // used to sit outside the limiter entirely, which left the two
        // endpoints an unauthenticated caller can reach *by design* — one
        // of which hands over a credential — as the only ones on the daemon
        // nothing throttled.
        public_setup = public_setup.layer(GovernorLayer::new(bucket()));
    }

    Router::new()
        .route("/api/v1/health", get(health))
        .merge(public_setup)
        .route("/", get(dashboard_index))
        .route("/index.html", get(dashboard_index))
        .route("/style.css", get(dashboard_style))
        .route("/app.js", get(dashboard_app_js))
        .route("/i18n.js", get(dashboard_i18n_js))
        .route("/vendor/alpine.min.js", get(dashboard_alpine_js))
        // Installable-app assets. Public like the rest of the shell: they
        // carry no data, and a login page that cannot load its own
        // stylesheet is not a login page.
        .route("/manifest.webmanifest", get(dashboard_manifest))
        .route("/sw.js", get(dashboard_service_worker))
        .route("/icon.svg", get(dashboard_icon))
        .route("/icon-maskable.svg", get(dashboard_icon_maskable))
        // Traefik's `errors` middleware always issues a GET when it
        // substitutes a custom error page (confirmed live: the original
        // POST/GET distinction is not preserved), so this must answer GET
        // too, not just the POST a manual/API caller would use.
        .route(
            "/api/v1/wake",
            get(handlers::lifecycle::wake_by_host).post(handlers::lifecycle::wake_by_host),
        )
        .route(
            "/api/v1/heartbeat",
            get(handlers::lifecycle::heartbeat_by_host)
                .post(handlers::lifecycle::heartbeat_by_host),
        )
        .merge(webhooks)
        .merge(protected)
        // Any GET that doesn't match an API route or a static asset above is
        // a client-side dashboard route (`/ui/projects/1`,
        // `/ui/environments/5?tab=logs`, ...) — the SPA shell handles
        // routing itself from `location.pathname`/`location.search`, so a
        // hard refresh or a shared deep link on any of those paths still
        // has to return `index.html`, not a 404. Paths under `/api/` are not
        // client routes, though: a typo there is a client mistake and gets
        // the same JSON 404 shape as every other API failure.
        .fallback(dashboard_fallback)
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

/// Fallback for any path no route matched: the SPA shell for dashboard deep
/// links, a JSON 404 for `/api/` ones (an API client must be able to tell a
/// typo from a success; a `200 text/html` does the opposite).
async fn dashboard_fallback(uri: axum::http::Uri) -> Response {
    if uri.path().starts_with("/api/") {
        ApiError::not_found(format!("route `{}`", uri.path())).into_response()
    } else {
        dashboard_index().await.into_response()
    }
}

#[cfg(test)]
mod bootstrap_access_tests {
    use super::BootstrapAccess;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    /// The pre-auth token endpoint's gate. The case worth pinning is the
    /// fail-closed one: an unknown peer address must never read as "local",
    /// or a containerized daemon would hand out the master token to anyone
    /// who can reach the port.
    #[test]
    fn bootstrap_access_permits_exactly_what_it_says() {
        let loopback_v4 = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let loopback_v6 = Some(IpAddr::V6(Ipv6Addr::LOCALHOST));
        let remote = Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)));

        assert!(!BootstrapAccess::Off.permits(loopback_v4));
        assert!(!BootstrapAccess::Off.permits(remote));

        assert!(BootstrapAccess::Any.permits(loopback_v4));
        assert!(BootstrapAccess::Any.permits(remote));
        assert!(BootstrapAccess::Any.permits(None));

        assert!(BootstrapAccess::Loopback.permits(loopback_v4));
        assert!(BootstrapAccess::Loopback.permits(loopback_v6));
        assert!(!BootstrapAccess::Loopback.permits(remote));
        assert!(
            !BootstrapAccess::Loopback.permits(None),
            "an unknown peer must not be treated as local"
        );
    }
}

#[cfg(test)]
mod tests;
