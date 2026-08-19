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
//! - `OXID_DOCKER_NETWORK` — docker network shared with Traefik and this
//!   daemon. When set, deployed containers join it and skip publishing a
//!   host port (SPEC.md §3.2). Unset by default: containers publish
//!   `[routing].port` directly, which only supports one live environment per
//!   project at a time.
//! - `OXID_DAEMON_URL` — this daemon's own address as reachable from inside
//!   `OXID_DOCKER_NETWORK` (default `http://oxid-daemon:8080`), used to build
//!   the Traefik `forwardAuth`/`errors` middleware labels.
//! - `OXID_API_TOKEN` — bearer token required on every `/api/v1/*` control
//!   request (`Authorization: Bearer <token>`), except `/health`,
//!   `/webhooks/*` and the Traefik-facing `/wake`/`/heartbeat` endpoints.
//!   Unset by default: the control API is open, which is fine on localhost
//!   or a private network but not otherwise — a startup warning is printed
//!   when it's unset.
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
//! - `OXID_TLS_CERT` / `OXID_TLS_KEY` — PEM certificate/key file paths. When
//!   both are set, the daemon serves HTTPS directly instead of plain HTTP.
//!   Unset by default: most deployments (including the shipped
//!   `docker-compose.yml`) put Traefik/another reverse proxy in front for
//!   TLS termination instead — these are for the operators who don't.
//! - `OXID_ALLOW_RESTORE` — set to `1` to accept `POST
//!   /api/v1/backup/restore` uploads at all (rejected with `403` otherwise).
//!   A restore never touches the live database in place — it stages the
//!   upload and applies it on the *next* startup (see [`apply_staged_restore`]).
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

const DEFAULT_DATA_DIR: &str = "/data";
const DEFAULT_ADDR: &str = "0.0.0.0:8080";
const DEFAULT_GC_INTERVAL_SECS: u64 = 30;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    tokio::spawn(oxid_daemon::service::scheduler::run(
        cp.clone(),
        std::time::Duration::from_secs(gc_interval_secs),
    ));

    let webhook_secret = std::env::var("OXID_WEBHOOK_SECRET").ok();
    let api_token = std::env::var("OXID_API_TOKEN").ok();
    if api_token.is_none() {
        println!(
            "[~] OXID_API_TOKEN is not set: the control API is open to anyone who can reach it"
        );
    }
    let allow_restore = std::env::var("OXID_ALLOW_RESTORE").as_deref() == Ok("1");
    let app = router(ApiState {
        cp,
        webhook_secret,
        api_token,
        data_dir: data_dir_path,
        allow_restore,
    });
    let tls = load_tls_config().await?;
    if let Some(config) = tls {
        let socket_addr: std::net::SocketAddr = addr
            .parse()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{e}")))?;
        println!(
            "[>] oxid daemon listening on https://{addr} (data: {data_dir}, gc every {gc_interval_secs}s)"
        );
        axum_server::bind_rustls(socket_addr, config)
            .serve(app.into_make_service())
            .await?;
    } else {
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        println!(
            "[>] oxid daemon listening on {addr} (data: {data_dir}, gc every {gc_interval_secs}s)"
        );
        axum::serve(listener, app).await?;
    }
    Ok(())
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
    println!("[>] applying staged restore from {}", staged_path.display());
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
    println!("[+] restore applied; starting normally");
    Ok(())
}
