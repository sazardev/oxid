//! Oxid daemon binary: starts the HTTP control-plane server.
//!
//! Configuration via environment:
//! - `OXID_DATA_DIR` — data directory (default `/data`), holding `audit.sqlite`,
//!   the `git-cache/` and the generated `secret.key`.
//! - `OXID_ADDR` — bind address (default `0.0.0.0:8080`).
//! - `OXID_MASTER_KEY` — optional 64-char hex master key for secret encryption.
//!   When unset, a key is generated and persisted to `/data/secret.key`.
//! - `OXID_WEBHOOK_SECRET` — shared secret verifying GitHub webhook signatures.
//!   Webhooks are rejected while unset.
//! - `RUST_LOG` — `tracing-subscriber` `EnvFilter` syntax (default `info`),
//!   e.g. `oxid_daemon=debug,info`.
//! - `OXID_LOG_FORMAT` — `pretty` (default, human-readable/colorized) or
//!   `json` (one JSON object per line) for the structured log output; use
//!   `json` in production so a log aggregator can parse fields directly.
//! - `OXID_DOCKER_NETWORK` — docker network shared with Traefik and this
//!   daemon. When set, deployed containers join it and skip publishing a
//!   host port, reachable instead via a Traefik `Host()` subdomain
//!   (SPEC.md §3.2). Unset by default: containers publish `[routing].port`
//!   directly on a host port Docker picks itself, so a busy port never
//!   blocks a deploy — every environment's actual address is in
//!   `oxid status`/the dashboard, not a fixed, predictable one. Scale-to-zero
//!   (`OXID_GC_INTERVAL_SECS` below) only runs with this set: idle detection
//!   is driven entirely by Traefik's `forwardAuth` heartbeat touching
//!   `last_accessed_at` on real traffic, which nothing calls in direct-publish
//!   mode — the GC sweep is a deliberate no-op there instead of auto-pausing/
//!   destroying environments on data it has no way to know is accurate.
//! - `OXID_DAEMON_URL` — this daemon's own address as reachable from inside
//!   `OXID_DOCKER_NETWORK` (default `http://oxid-daemon:8080`), used to build
//!   the Traefik `forwardAuth`/`errors` middleware labels.
//! - `OXID_API_TOKEN` — bearer token required on every `/api/v1/*` control
//!   request (`Authorization: Bearer <token>`), except `/health`,
//!   `/webhooks/*` and the Traefik-facing `/wake`/`/heartbeat` endpoints.
//!   Unset by default on a loopback bind (`127.0.0.1`, `localhost`, ...):
//!   the control API stays open there, which is fine for local use.
//!   Unset on any non-loopback bind (`0.0.0.0:8080`, a LAN IP, ...), the
//!   daemon **refuses to start** — anyone who could reach it could deploy,
//!   destroy environments and read secret names. Override that refusal with
//!   `OXID_ALLOW_OPEN_API=1` (explicit opt-in to an unauthenticated API) or
//!   set `OXID_API_TOKEN`.
//! - `OXID_ALLOW_OPEN_API` — set to `1` to let a non-loopback daemon start
//!   without `OXID_API_TOKEN`. The startup warning still prints; this flag
//!   exists so an operator behind their own network controls can choose the
//!   old behavior deliberately instead of being locked out by the gate.
//! - `OXID_AUTO_TOKEN` — set to `1` for zero-config starts (`docker compose
//!   up -d` with no `.env`): any of `OXID_API_TOKEN`/`OXID_WEBHOOK_SECRET`
//!   that isn't explicitly set is generated (64 hex chars), persisted under
//!   `{OXID_DATA_DIR}/` (`api-token`, `webhook-secret`; owner-only on unix)
//!   so restarts reuse it, and printed **exactly once** to the log — that
//!   one printout is the retrieval channel from a distroless container,
//!   which has no shell to cat the file with. Explicit env values always
//!   win; without the flag behavior is unchanged (including the refusal to
//!   start unauthenticated on a non-loopback bind).
//! - `OXID_POSTGRES_URL` — admin connection string for a shared Postgres
//!   instance (SPEC.md §3.1). When set, projects declaring a `postgres`
//!   dependency get a per-branch logical database instead of failing to
//!   deploy with a "not configured" error.
//! - `OXID_REDIS_URL` — base URL for a shared Redis instance (e.g.
//!   `redis://host:6379`, no database index). Same "not configured" error
//!   otherwise for projects declaring a `redis` dependency.
//! - `OXID_REDIS_POOL_SIZE` — number of Redis logical databases available to
//!   lease from (default 16, matching Redis's own default `databases 16`).
//! - `OXID_DEFAULT_MEMORY_LIMIT_MB` — memory limit (megabytes) applied to a
//!   deployed container when its project's `oxid.toml [build]` doesn't set
//!   its own `memory_limit_mb` (default `512`). A single misbehaving
//!   preview environment can otherwise exhaust the whole host's memory —
//!   set to `0` to disable and leave containers genuinely unbounded.
//! - `OXID_DEFAULT_CPU_LIMIT_MILLICORES` — same fallback behavior as
//!   `OXID_DEFAULT_MEMORY_LIMIT_MB`, but for CPU (1000 = one full core,
//!   default `1000`). Set to `0` to disable.
//! - `OXID_RESERVED_MEMORY_MB` — memory (megabytes) reserved for the host OS
//!   and this daemon itself, subtracted from `docker info`'s reported total
//!   before deciding whether a new deploy fits (default `1024`). A deploy
//!   whose resolved memory request would push total committed usage past
//!   what's left is queued (persisted — survives a daemon restart) and
//!   retried automatically as other environments free capacity, instead of
//!   either failing outright or overcommitting the host. Set to `0` to
//!   disable admission control entirely and deploy immediately regardless of
//!   host capacity (the behavior before this existed).
//! - `OXID_TLS_CERT` / `OXID_TLS_KEY` — PEM certificate/key file paths. When
//!   both are set, the daemon serves HTTPS directly instead of plain HTTP.
//!   Unset by default: most deployments (including the shipped
//!   `docker-compose.yml`) put Traefik/another reverse proxy in front for
//!   TLS termination instead — these are for the operators who don't.
//! - `OXID_ALLOW_RESTORE` — set to `1` to accept `POST
//!   /api/v1/backup/restore` uploads at all (rejected with `403` otherwise).
//!   A restore never touches the live database in place — it stages the
//!   upload and applies it on the *next* startup (see [`apply_staged_restore`]).
//! - `OXID_RATE_LIMIT_PER_SECOND` / `OXID_RATE_LIMIT_BURST` — when **both**
//!   are set, caps the protected control-plane routes with a token bucket
//!   (sustained requests/sec, burst size) **per client IP** so one
//!   misbehaving script holding the API token can't saturate the daemon
//!   from its host while everyone else keeps working. Unset by default
//!   (unlimited). Behind a single reverse proxy all requests share the
//!   proxy's IP, so there the limit degrades to a global bucket — safe,
//!   just coarser (see `ClientIpKeyExtractor` in `api/middleware.rs` for
//!   why `X-Forwarded-For` is deliberately not trusted).
//! - `OXID_BACKUP_INTERVAL_SECS` / `OXID_BACKUP_KEEP` — when the interval
//!   is set to ≥1, snapshots the database every that many seconds into
//!   `{OXID_DATA_DIR}/backups/oxid-backup-<timestamp>.sqlite` via
//!   `VACUUM INTO` (consistent against the live pool) and rotates,
//!   keeping `OXID_BACKUP_KEEP` newest (default 7). Unset by default —
//!   restore with `oxid restore`, or run a Litestream sidecar for
//!   streaming off-site replication (see `docker-compose.yml`).
//!
//! **Resilience notes:** deployed containers carry Docker's
//! `unless-stopped` restart policy — they come back on their own after a
//! crash, an OOM-kill, or the host rebooting, without needing this daemon
//! to be up to notice. On its own startup, the daemon reconciles its
//! database against Docker's actual state before serving any request (see
//! [`apply_staged_restore`] for the restore side of this and
//! [`oxid_daemon::ControlPlane::reconcile_startup_state`] for the
//! container-drift side). It also drains in-flight requests for up to 10s
//! on `SIGTERM`/`Ctrl+C` instead of dying mid-request, and lowers its own
//! OOM score so the kernel prefers killing a disposable preview container
//! over the daemon itself under memory pressure.
//!
//! **Network topology note for `OXID_POSTGRES_URL`/`OXID_REDIS_URL`:** the
//! same URL is used both by this daemon (to run `CREATE`/`DROP DATABASE` as
//! an admin) *and* as the template for the `DATABASE_URL`/`REDIS_URL`
//! injected into deployed containers. Those containers only join
//! `OXID_DOCKER_NETWORK`, so the hostname in these URLs has to resolve from
//! *both* places — in practice that means this daemon itself needs to run
//! on `OXID_DOCKER_NETWORK` too (e.g. as its own container on that network,
//! matching SPEC.md §6's `docker run` deployment), not on the bare host with
//! Postgres/Redis published to a host port. Found the hard way: a
//! host-run daemon pointed at a container hostname fails every deploy with
//! a DNS resolution error the moment a project declares a dependency.

use std::path::PathBuf;

use oxid_daemon::ControlPlane;
use oxid_daemon::adapter::crypto::Cipher;
use oxid_daemon::adapter::git::GitClient;
use oxid_daemon::adapter::oci::DockerClient;
use oxid_daemon::adapter::store::SqliteStore;
use oxid_daemon::api::{ApiState, router};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const DEFAULT_DATA_DIR: &str = "/data";
const DEFAULT_ADDR: &str = "0.0.0.0:8080";
const DEFAULT_GC_INTERVAL_SECS: u64 = 30;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let data_dir = std::env::var("OXID_DATA_DIR").unwrap_or_else(|_| DEFAULT_DATA_DIR.to_owned());
    let addr = std::env::var("OXID_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_owned());
    let gc_interval_secs = std::env::var("OXID_GC_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_GC_INTERVAL_SECS);

    let cipher = match std::env::var("OXID_MASTER_KEY") {
        Ok(raw) => {
            let bytes = hex::decode(&raw).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid OXID_MASTER_KEY: {e}"),
                )
            })?;
            let key: [u8; 32] = bytes.try_into().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "OXID_MASTER_KEY must be 64 hex characters (32 bytes)",
                )
            })?;
            Cipher::from_key(key)
        }
        Err(_) => Cipher::load_or_create(&PathBuf::from(&data_dir).join("secret.key"))?,
    };

    let data_dir_path = PathBuf::from(&data_dir);
    let db_path = data_dir_path.join("audit.sqlite");
    let cache_dir = data_dir_path.join("git-cache");

    apply_staged_restore(&data_dir_path)?;
    let store = SqliteStore::open(db_path, cipher).await?;
    let git = GitClient::new();
    let oci = DockerClient::connect()?;
    let mut cp = ControlPlane::new(store, git, oci, cache_dir);
    if let Ok(network) = std::env::var("OXID_DOCKER_NETWORK") {
        let daemon_url = std::env::var("OXID_DAEMON_URL")
            .unwrap_or_else(|_| "http://oxid-daemon:8080".to_owned());
        cp = cp.with_traefik(network, daemon_url);
    }
    let postgres_url = std::env::var("OXID_POSTGRES_URL").ok();
    let redis_url = std::env::var("OXID_REDIS_URL").ok();
    let redis_pool_size = std::env::var("OXID_REDIS_POOL_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16);
    cp = cp.with_resource_pools(postgres_url, redis_url, redis_pool_size);

    let default_memory_limit_mb = std::env::var("OXID_DEFAULT_MEMORY_LIMIT_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .or(Some(512))
        .filter(|&mb| mb > 0);
    let default_cpu_limit_millicores = std::env::var("OXID_DEFAULT_CPU_LIMIT_MILLICORES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .or(Some(1000))
        .filter(|&mc| mc > 0);
    cp = cp.with_resource_defaults(default_memory_limit_mb, default_cpu_limit_millicores);

    let reserved_memory_mb = std::env::var("OXID_RESERVED_MEMORY_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .or(Some(1024))
        .filter(|&mb| mb > 0);
    cp = cp.with_admission_control(reserved_memory_mb);

    // Reconciles the database against Docker's actual state before serving
    // any request — the daemon may have been down for a while (crash,
    // restart, a host reboot), during which containers can drift from
    // whatever was last recorded (see `ControlPlane::reconcile_startup_state`).
    match cp.reconcile_startup_state().await {
        Ok(errors) if errors.is_empty() => {}
        Ok(errors) => {
            for (env_id, message) in errors {
                tracing::warn!(environment_id = %env_id, message = %message, "startup reconciliation issue");
            }
        }
        Err(e) => tracing::error!(error = %e, "startup reconciliation failed"),
    }

    tokio::spawn(oxid_daemon::service::scheduler::run(
        cp.clone(),
        std::time::Duration::from_secs(gc_interval_secs),
    ));

    // Periodic on-disk database snapshots (DR posture) — off unless
    // OXID_BACKUP_INTERVAL_SECS is set.
    if let Some(backup_config) = oxid_daemon::service::backup::config_from_env(&data_dir_path) {
        tokio::spawn(oxid_daemon::service::backup::run(cp.clone(), backup_config));
    }

    let (webhook_secret, api_token, auto_token) = resolve_bootstrap_credentials(&data_dir_path)?;
    enforce_startup_security_posture(&addr, api_token.as_ref());
    let allow_restore = std::env::var("OXID_ALLOW_RESTORE").as_deref() == Ok("1");
    let rate_limit = rate_limit_from_env();
    let app = router(ApiState {
        cp,
        webhook_secret,
        api_token,
        data_dir: data_dir_path,
        allow_restore,
        rate_limit,
        auto_token,
    });
    lower_oom_score();
    serve(app, &addr, &data_dir, gc_interval_secs).await
}

/// Binds `addr` and serves `app` — plain HTTP, or HTTPS if
/// `OXID_TLS_CERT`/`OXID_TLS_KEY` are set — draining in-flight requests
/// for up to 10s on `SIGTERM`/`Ctrl+C` either way.
async fn serve(
    app: axum::Router,
    addr: &str,
    data_dir: &str,
    gc_interval_secs: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let tls = load_tls_config().await?;
    if let Some(config) = tls {
        let socket_addr: std::net::SocketAddr = addr
            .parse()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{e}")))?;
        tracing::info!(
            %addr,
            %data_dir,
            gc_interval_secs,
            "oxid daemon listening on https"
        );
        let handle = axum_server::Handle::new();
        let shutdown_handle = handle.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            // Give in-flight requests (a deploy, a log stream) up to 10s to
            // finish before the connection is cut regardless.
            shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(10)));
        });
        axum_server::bind_rustls(socket_addr, config)
            .handle(handle)
            // `into_make_service_with_connect_info` (not plain
            // `into_make_service`) inserts each request's peer address as a
            // `ConnectInfo` extension — the per-IP rate-limit key
            // (`ClientIpKeyExtractor`) is dead weight without it.
            .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .await?;
    } else {
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        tracing::info!(
            %addr,
            %data_dir,
            gc_interval_secs,
            "oxid daemon listening on http"
        );
        // `axum::serve(...).with_graceful_shutdown(fut)` only decides *when*
        // to stop accepting new connections — once `fut` resolves, it still
        // waits for every existing connection (an open dashboard tab's
        // keep-alive, a live log stream) to close on its own, with no cap.
        // Found live: a browser tab left open on the dashboard kept the
        // daemon from ever exiting on `SIGTERM`. `axum_server`'s TLS path
        // below already bounds this at 10s via `graceful_shutdown(Some(..))`;
        // race the same cap in manually here so the plain-HTTP path actually
        // matches this function's own doc comment.
        let (drain_tx, drain_rx) = tokio::sync::oneshot::channel::<()>();
        // `into_make_service_with_connect_info` (not plain `app`) inserts
        // each request's peer address as a `ConnectInfo` extension — the
        // per-IP rate-limit key (`ClientIpKeyExtractor`) is dead weight
        // without it. axum_server's TLS path below does the same.
        let serve_fut = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = drain_rx.await;
        })
        .into_future();
        tokio::pin!(serve_fut);
        tokio::select! {
            result = &mut serve_fut => result?,
            () = shutdown_signal() => {
                let _ = drain_tx.send(());
                if tokio::time::timeout(std::time::Duration::from_secs(10), &mut serve_fut)
                    .await
                    .is_err()
                {
                    tracing::error!("graceful shutdown timed out after 10s, forcing exit");
                }
            }
        }
    }
    Ok(())
}

/// Waits for `SIGTERM` (what `docker stop`/a service manager sends) or
/// `Ctrl+C`, whichever comes first — without this, either signal killed
/// the process immediately mid-request (a deploy half-applied, a log
/// stream cut off) instead of letting `axum`/`axum-server` drain
/// in-flight work first.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            std::future::pending::<()>().await;
            return;
        };
        signal.recv().await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received, draining in-flight requests...");
}

/// Initializes the global `tracing` subscriber — must run before anything
/// else logs. Level is `RUST_LOG` (standard `tracing-subscriber`
/// env-filter syntax, e.g. `oxid_daemon=debug,info`), defaulting to `info`
/// when unset. Format is `OXID_LOG_FORMAT`: `pretty` (human-readable,
/// colorized, the default — meant for a terminal/`docker logs` a person is
/// watching) or `json` (one JSON object per line, with `request_id`/etc as
/// structured fields) — **use `json` in production**, so a log aggregator
/// (Loki, `CloudWatch`, whatever) can parse and index fields directly instead
/// of regexing a human-oriented format.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let json = std::env::var("OXID_LOG_FORMAT").as_deref() == Ok("json");
    let registry = tracing_subscriber::registry().with(filter);
    if json {
        registry
            .with(tracing_subscriber::fmt::layer().json())
            .init();
    } else {
        registry.with(tracing_subscriber::fmt::layer()).init();
    }
}

/// Best-effort: makes the Linux OOM killer less likely to pick this
/// process over the ephemeral preview containers it manages. Without
/// this, the daemon (unbounded memory, unlike deployed containers which
/// now always carry a `memory_limit_mb`) has no signal telling the kernel
/// it's comparatively more important to keep alive than a disposable
/// preview environment. Silently does nothing if `/proc` isn't writable
/// (non-Linux, or lacking permission) — this is a nice-to-have, not load
/// -bearing for correctness.
fn lower_oom_score() {
    let _ = std::fs::write("/proc/self/oom_score_adj", "-500");
}

/// Reads `OXID_RATE_LIMIT_PER_SECOND`/`OXID_RATE_LIMIT_BURST`. `None`
/// (rate limiting disabled) unless both are set and parse.
fn rate_limit_from_env() -> Option<(u64, u32)> {
    let per_second = std::env::var("OXID_RATE_LIMIT_PER_SECOND")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())?;
    let burst = std::env::var("OXID_RATE_LIMIT_BURST")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())?;
    Some((per_second, burst))
}

/// Whether every address `addr` may bind to is loopback — the one situation
/// where leaving the control API unauthenticated is acceptable by default.
///
/// Handles both literal addresses (`127.0.0.1:8080`, `[::1]:8080`) and host
/// names (`localhost:8080`, resolved via `ToSocketAddrs`). Fails closed:
/// an unresolvable or wildcard (`0.0.0.0`/`[::]`) value counts as
/// not-loopback, since those reach (at least) every interface.
fn bind_is_loopback(addr: &str) -> bool {
    use std::net::ToSocketAddrs;
    if let Ok(socket_addr) = addr.parse::<std::net::SocketAddr>() {
        return socket_addr.ip().is_loopback();
    }
    match addr.to_socket_addrs() {
        Ok(mut addrs) => addrs.all(|a| a.ip().is_loopback()),
        Err(_) => false,
    }
}

/// Resolves the two control-plane credentials, honoring explicit env values
/// first: `(webhook_secret, api_token, auto_token)`.
///
/// With `OXID_AUTO_TOKEN=1` any credential that wasn't supplied is generated
/// and persisted (see [`credential`]) — the zero-config path the shipped
/// `docker-compose.yml` takes. Without the flag both fall back to plain env
/// reads and behavior is exactly as before.
fn resolve_bootstrap_credentials(
    data_dir: &std::path::Path,
) -> std::io::Result<(Option<String>, Option<String>, bool)> {
    let auto = std::env::var("OXID_AUTO_TOKEN").as_deref() == Ok("1");
    let api_token = credential(
        std::env::var("OXID_API_TOKEN").ok(),
        auto,
        &data_dir.join("api-token"),
        "API token",
    )?;
    let webhook_secret = credential(
        std::env::var("OXID_WEBHOOK_SECRET").ok(),
        auto,
        &data_dir.join("webhook-secret"),
        "webhook secret",
    )?;
    Ok((webhook_secret, api_token, auto))
}

/// Resolves one credential: an explicitly-set value always wins; otherwise
/// with `auto` it is loaded from `path` or, on a first run, generated
/// (64 hex chars), persisted there owner-only, and printed to the log.
///
/// The one-time printout is a deliberate, documented exception to the
/// "secrets never in logs" rule: it happens **only at generation** (a reused
/// file logs nothing but its path), and it exists because the retrieval
/// channel for a distroless container — no shell, no `cat` — is precisely
/// `docker compose logs`. Anyone who can read those logs can already
/// `docker cp` the same 0600 file off the volume, so printing once at
/// bootstrap grants nothing the socket mount didn't already.
fn credential(
    explicit: Option<String>,
    auto: bool,
    path: &std::path::Path,
    label: &str,
) -> std::io::Result<Option<String>> {
    if let Some(value) = explicit.filter(|v| !v.trim().is_empty()) {
        return Ok(Some(value));
    }
    if !auto {
        return Ok(None);
    }
    if let Ok(existing) = std::fs::read_to_string(path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            tracing::info!(path = %path.display(), "reusing existing {label}");
            return Ok(Some(trimmed.to_owned()));
        }
    }
    let value = generate_hex_secret();
    std::fs::create_dir_all(path.parent().unwrap_or_else(|| std::path::Path::new(".")))?;
    std::fs::write(path, &value)?;
    restrict_to_owner(path)?;
    eprintln!(
        "[oxid] Generated {label} (persisted at {}, owner-only):\n\n    {value}\n\n\
         Printed once — retrieve later from that file, e.g.:\n  \
         docker compose cp oxid-daemon:{path_display} -",
        path.display(),
        path_display = path.display(),
    );
    Ok(Some(value))
}

/// 64 hex chars of OS entropy — same strength class as the secrets
/// `install.sh` generates.
fn generate_hex_secret() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(unix)]
fn restrict_to_owner(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

/// The startup security posture, enforced before anything binds:
///
/// 1. **Open-API gate** — an open control API is fine on loopback, but a
///    daemon reachable beyond it must either authenticate every request or
///    have the operator explicitly accept the risk (`OXID_ALLOW_OPEN_API=1`);
///    otherwise the process refuses to start with actionable output.
/// 2. **Topology warning** — without `OXID_DOCKER_NETWORK` the daemon runs
///    in direct-publish mode, where scale-to-zero is disabled by design
///    (nothing refreshes `last_accessed_at`, so the sweep no-ops); operators
///    get told once, loudly, instead of discovering it via a full disk of
///    never-destroyed environments.
fn enforce_startup_security_posture(addr: &str, api_token: Option<&String>) {
    if api_token.is_none() {
        if !bind_is_loopback(addr) && std::env::var("OXID_ALLOW_OPEN_API").as_deref() != Ok("1") {
            eprintln!(
                "refusing to start: OXID_ADDR ({addr}) is not a loopback address and \
                 OXID_API_TOKEN is not set, so anyone who can reach this daemon could deploy,\n\
                 destroy environments and read secret names.\n\n\
                 Fix one of three ways:\n  \
                 1. set OXID_API_TOKEN to a long random value (recommended; pass it to the CLI \
                 as --token/OXID_TOKEN),\n  \
                 2. bind OXID_ADDR to 127.0.0.1 (or localhost) and put your own proxy in front,\n  \
                 3. set OXID_ALLOW_OPEN_API=1 to explicitly run an unauthenticated API anyway."
            );
            std::process::exit(1);
        }
        tracing::warn!(
            "OXID_API_TOKEN is not set: the control API is open to anyone who can reach it"
        );
    }
    let traefik_configured =
        std::env::var("OXID_DOCKER_NETWORK").is_ok_and(|v| !v.trim().is_empty());
    if !traefik_configured {
        tracing::warn!(
            "OXID_DOCKER_NETWORK is not set: running in direct-publish mode — scale-to-zero is \
             DISABLED (no idle auto-pause/GC destroy; environments run until manually paused or \
             destroyed). Traefik mode is the supported production topology: see `oxid infra setup`."
        );
    }
}

/// Loads `OXID_TLS_CERT`/`OXID_TLS_KEY` into a rustls server config when
/// both are set, installing the `ring` crypto provider on first use.
/// Returns `None` (serve plain HTTP) when either is unset.
async fn load_tls_config()
-> Result<Option<axum_server::tls_rustls::RustlsConfig>, Box<dyn std::error::Error>> {
    let (Some(cert), Some(key)) = (
        std::env::var("OXID_TLS_CERT").ok(),
        std::env::var("OXID_TLS_KEY").ok(),
    ) else {
        return Ok(None);
    };
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
        .await
        .map_err(|e| {
            std::io::Error::other(format!("cannot load TLS cert/key ({cert}, {key}): {e}"))
        })?;
    Ok(Some(config))
}

/// If `<data_dir>/.restore-pending.tar` exists (staged by a prior
/// `POST /api/v1/backup/restore`), extracts `audit.sqlite`/`secret.key`
/// from it over the real files and removes the marker — applied here,
/// before `SqliteStore::open` runs, so the restore is never attempted
/// against an already-open pool.
fn apply_staged_restore(data_dir: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let staged_path = data_dir.join(".restore-pending.tar");
    if !staged_path.exists() {
        return Ok(());
    }
    tracing::info!(path = %staged_path.display(), "applying staged restore");
    let file = std::fs::File::open(&staged_path)?;
    let mut archive = tar::Archive::new(file);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().into_owned();
        if name == "audit.sqlite" || name == "secret.key" {
            entry.unpack(data_dir.join(&name))?;
        }
    }
    std::fs::remove_file(&staged_path)?;
    tracing::info!("restore applied; starting normally");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{bind_is_loopback, credential, generate_hex_secret};

    #[test]
    fn loopback_literals_are_loopback() {
        assert!(bind_is_loopback("127.0.0.1:8080"));
        assert!(bind_is_loopback("127.9.9.9:1"));
        assert!(bind_is_loopback("[::1]:8080"));
    }

    #[test]
    fn wildcard_and_remote_addresses_are_not_loopback() {
        // The default bind reaches every interface — the gate must treat it
        // as public even though 0.0.0.0 is technically "unspecified".
        assert!(!bind_is_loopback("0.0.0.0:8080"));
        assert!(!bind_is_loopback("[::]:8080"));
        assert!(!bind_is_loopback("192.168.2.73:8080"));
    }

    #[test]
    fn localhost_resolves_to_loopback_and_bogus_names_fail_closed() {
        assert!(bind_is_loopback("localhost:8080"));
        // An unresolvable host must count as not-loopback (fail closed) —
        // the daemon would otherwise start open on an address nobody can
        // pin down.
        assert!(!bind_is_loopback("no-such-host.invalid:8080"));
    }

    #[test]
    fn explicit_value_always_wins_over_auto_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-token");
        let resolved = credential(Some("explicit".to_owned()), true, &path, "API token")
            .unwrap()
            .unwrap();
        assert_eq!(resolved, "explicit");
        assert!(!path.exists(), "nothing should be written when env is set");
    }

    #[test]
    fn auto_generates_persists_and_reuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-token");

        let first = credential(None, true, &path, "API token").unwrap().unwrap();
        assert_eq!(first.len(), 64, "64 hex chars");
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "generated credential must be owner-only");
        }

        // A second run (restart) must reuse the persisted value — never
        // rotate it behind the operator's back.
        let second = credential(None, true, &path, "API token").unwrap().unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn without_auto_flag_unset_env_resolves_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-token");
        assert!(
            credential(None, false, &path, "API token")
                .unwrap()
                .is_none()
        );
        assert!(!path.exists());
    }

    #[test]
    fn generated_secrets_are_unique() {
        assert_ne!(generate_hex_secret(), generate_hex_secret());
    }

    #[test]
    fn blank_explicit_value_falls_through_to_auto() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-token");
        let resolved = credential(Some("   ".to_owned()), true, &path, "API token")
            .unwrap()
            .expect("auto mode must still resolve");
        assert_eq!(resolved.len(), 64);
    }

    #[test]
    fn reuse_reads_trimmed_file_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-token");
        std::fs::write(&path, "abc123\n").unwrap();
        let resolved = credential(None, true, &path, "API token").unwrap().unwrap();
        assert_eq!(resolved, "abc123");
    }
}
