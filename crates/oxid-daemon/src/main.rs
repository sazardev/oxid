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

    let db_path = PathBuf::from(&data_dir).join("audit.sqlite");
    let cache_dir = PathBuf::from(&data_dir).join("git-cache");

    let store = SqliteStore::open(db_path, cipher).await?;
    let git = GitClient::new();
    let oci = DockerClient::connect()?;
    let mut cp = ControlPlane::new(store, git, oci, cache_dir);
    if let Ok(network) = std::env::var("OXID_DOCKER_NETWORK") {
        let daemon_url = std::env::var("OXID_DAEMON_URL")
            .unwrap_or_else(|_| "http://oxid-daemon:8080".to_owned());
        cp = cp.with_traefik(network, daemon_url);
    }

    tokio::spawn(oxid_daemon::service::scheduler::run(
        cp.clone(),
        std::time::Duration::from_secs(gc_interval_secs),
    ));

    let webhook_secret = std::env::var("OXID_WEBHOOK_SECRET").ok();
    let app = router(ApiState { cp, webhook_secret });
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!(
        "[>] oxid daemon listening on {addr} (data: {data_dir}, gc every {gc_interval_secs}s)"
    );
    axum::serve(listener, app).await?;
    Ok(())
}
