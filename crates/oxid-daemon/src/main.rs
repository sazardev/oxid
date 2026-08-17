//! Oxid daemon binary: starts the HTTP control-plane server.
//!
//! Configuration via environment:
//! - `OXID_DATA_DIR` — data directory (default `/data`), holding `audit.sqlite`
//!   and the `git-cache/`.
//! - `OXID_ADDR` — bind address (default `0.0.0.0:8080`).

use std::path::PathBuf;

use oxid_daemon::ControlPlane;
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

    let db_path = PathBuf::from(&data_dir).join("audit.sqlite");
    let cache_dir = PathBuf::from(&data_dir).join("git-cache");

    let store = SqliteStore::open(db_path).await?;
    let git = GitClient::new();
    let oci = DockerClient::connect()?;
    let cp = ControlPlane::new(store, git, oci, cache_dir);

    tokio::spawn(oxid_daemon::service::scheduler::run(
        cp.clone(),
        std::time::Duration::from_secs(gc_interval_secs),
    ));

    let app = router(ApiState { cp });
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!(
        "[>] oxid daemon listening on {addr} (data: {data_dir}, gc every {gc_interval_secs}s)"
    );
    axum::serve(listener, app).await?;
    Ok(())
}
