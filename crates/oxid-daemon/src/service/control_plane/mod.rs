//! Application service: orchestrates the domain through the ports.
//!
//! This is the thin "application" layer of the hexagonal architecture. It
//! wires [`SqliteStore`], a [`GitPort`] and a [`ContainerPort`] together to
//! expose the operations the interfaces (CLI, HTTP API) call.

#![allow(unused_imports, clippy::pedantic, clippy::nursery)]

use std::path::PathBuf;
use std::sync::Arc;

use crate::adapter::store::SqliteStore;
use oxid_core::{ContainerPort, GitPort};

pub mod admission;
pub mod auth;
pub mod deploy;
pub mod error;
pub mod gc;
pub mod helpers;
pub mod infra;
pub mod lifecycle;
pub mod project;
pub mod provision;
pub mod types;

pub use error::CpError;
pub use types::{DeployOutcome, GcSummary, InfraStatus, NodeStats};

/// Orchestrates registration, deployment and lifecycle of environments.
#[derive(Clone)]
pub struct ControlPlane<G: GitPort, O: ContainerPort> {
    store: SqliteStore,
    git: G,
    oci: O,
    cache_dir: PathBuf,
    /// Docker network shared with Traefik and this daemon. `None` (the
    /// default) falls back to publishing the container's port directly on a
    /// host port Docker picks itself (see [`Self::run_and_activate`]) — safe
    /// with any number of concurrent environments per project, since no two
    /// ever fight over the same host port.
    docker_network: Option<String>,
    /// Base URL this daemon is reachable at from inside `docker_network`,
    /// used to build the Traefik `errors`/`forwardAuth` middleware labels
    /// (e.g. `http://oxid-daemon:8080`).
    daemon_url: String,
    /// Serializes every state-mutating lifecycle operation on environments —
    /// `deploy`, `pause`, `wake`, `destroy`, and each action a GC `sweep`
    /// applies — across every project. `Arc`-wrapped because `ControlPlane`
    /// is cloned per request (axum extracts a fresh clone from `State` for
    /// every handler).
    ///
    /// Originally added just around `deploy()`: without it, concurrent
    /// deploys raced on the shared git-cache checkout —
    /// `checkout_commit` force-rewrites the *same* on-disk working directory
    /// two deploys read from concurrently, so one could see files
    /// mid-rewrite (`tar_context` failing with "No such file or directory")
    /// or silently tar the wrong branch's tree. Concurrent deploys of the
    /// *same* branch had a second failure mode: each raced to create its own
    /// `Environment` row before finding out only one could win the
    /// container name, so the highest-id row — not necessarily the one
    /// whose `docker run` actually succeeded — could be the failed one,
    /// leaving `status`/`down`/`pause`/`wake` resolution pointed at a
    /// `Destroyed` row while the real container kept running. Found by
    /// firing ten concurrent `oxid up` at the same new branch.
    ///
    /// Widened to cover `pause`/`wake`/`destroy`/`sweep` too: a GC tick and
    /// a manual action on the same environment both do a read-modify-write
    /// (fetch, apply a `StateTransition`, persist) with no atomicity between
    /// the read and the write, so they could interleave and have one
    /// overwrite the other's transition with stale data.
    lifecycle_lock: Arc<tokio::sync::Mutex<()>>,
    /// Admin connection string for the shared Postgres instance
    /// (`OXID_POSTGRES_URL`). `None` means projects declaring a `postgres`
    /// dependency will fail to deploy with a clear error instead of
    /// silently skipping provisioning.
    postgres_url: Option<String>,
    /// Base URL for the shared Redis instance, without a database index
    /// (`OXID_REDIS_URL`, e.g. `redis://host:6379`). Same "clear error, not
    /// silent skip" behavior as `postgres_url` when a project needs it.
    redis_url: Option<String>,
    /// Number of logical Redis databases available to lease from
    /// (`OXID_REDIS_POOL_SIZE`, default matches Redis's own default of 16).
    redis_pool_size: u32,
    /// Memory limit (megabytes) applied to a deployed container when its
    /// project's `[build]` doesn't specify its own
    /// (`OXID_DEFAULT_MEMORY_LIMIT_MB`). `None` means genuinely unbounded —
    /// not the recommended default, but preserved for anyone who explicitly
    /// unsets it.
    default_memory_limit_mb: Option<u64>,
    /// CPU limit (millicores) applied the same way as
    /// `default_memory_limit_mb` (`OXID_DEFAULT_CPU_LIMIT_MILLICORES`).
    default_cpu_limit_millicores: Option<u32>,
    /// Memory (megabytes) reserved for the host OS + this daemon,
    /// subtracted from what `docker info` reports before deciding whether
    /// a new deploy fits (`OXID_RESERVED_MEMORY_MB`). `None` disables
    /// admission control entirely — every deploy proceeds immediately
    /// regardless of host capacity, the behavior before this existed.
    reserved_memory_mb: Option<u64>,
    /// Every branch's built-in reverse proxy in direct-publish mode — see
    /// `service/proxy.rs`. Only actually used when `docker_network` is
    /// `None`; under Traefik, Traefik itself is already the stable-address
    /// proxy in front of every container.
    proxy: crate::service::proxy::ProxyRegistry,
    /// Whether a redeploy's new instance must actually accept TCP
    /// connections before cutover (direct-publish mode only). Always `true`
    /// in production; test doubles disable it via
    /// [`Self::with_readiness_check`] since a fake [`ContainerPort`] has no
    /// real socket to connect to.
    readiness_check: bool,
}

const DEFAULT_DAEMON_URL: &str = "http://oxid-daemon:8080";
const DEFAULT_REDIS_POOL_SIZE: u32 = 16;

impl<G: GitPort, O: ContainerPort> ControlPlane<G, O> {
    /// Creates a control plane bound to a store, git client and docker
    /// client. Traefik integration and resource pooling are disabled until
    /// [`ControlPlane::with_traefik`] / [`ControlPlane::with_resource_pools`]
    /// are called.
    #[must_use]
    pub fn new(store: SqliteStore, git: G, oci: O, cache_dir: PathBuf) -> Self {
        Self {
            store,
            git,
            oci,
            cache_dir,
            docker_network: None,
            daemon_url: DEFAULT_DAEMON_URL.to_owned(),
            lifecycle_lock: Arc::new(tokio::sync::Mutex::new(())),
            postgres_url: None,
            redis_url: None,
            redis_pool_size: DEFAULT_REDIS_POOL_SIZE,
            default_memory_limit_mb: None,
            default_cpu_limit_millicores: None,
            reserved_memory_mb: None,
            proxy: crate::service::proxy::ProxyRegistry::default(),
            readiness_check: true,
        }
    }

    /// Enables or disables the zero-downtime readiness gate (direct-publish
    /// mode: wait for a redeploy's new container to actually accept TCP
    /// connections before cutting traffic over to it). Defaults to `true`;
    /// test doubles that don't simulate a real listening socket must
    /// disable it or every deploy will fail waiting for a connection that
    /// can never succeed.
    #[must_use]
    pub fn with_readiness_check(mut self, enabled: bool) -> Self {
        self.readiness_check = enabled;
        self
    }

    /// Enables Traefik routing: deployed containers join `network` (no host
    /// port is published) and carry labels pointing Traefik's `errors` and
    /// `forwardAuth` middlewares at `daemon_url` (SPEC.md §3.2).
    #[must_use]
    pub fn with_traefik(
        mut self,
        network: impl Into<String>,
        daemon_url: impl Into<String>,
    ) -> Self {
        self.docker_network = Some(network.into());
        self.daemon_url = daemon_url.into();
        self
    }

    /// Sets the daemon-wide fallback memory/CPU limits applied to a
    /// deployed container when its project's `[build]` doesn't specify its
    /// own (SPEC.md "Eficiencia Absoluta" — an environment should never be
    /// able to exhaust the host by default, only by explicit configuration).
    #[must_use]
    pub fn with_resource_defaults(
        mut self,
        default_memory_limit_mb: Option<u64>,
        default_cpu_limit_millicores: Option<u32>,
    ) -> Self {
        self.default_memory_limit_mb = default_memory_limit_mb;
        self.default_cpu_limit_millicores = default_cpu_limit_millicores;
        self
    }

    /// Enables admission control: a deploy whose resolved memory request,
    /// added to every other currently `Running`/`Paused` environment's,
    /// would exceed `docker info`'s reported host memory (minus
    /// `reserved_memory_mb`) is queued instead of deployed immediately —
    /// see [`Self::deploy_or_queue`]. `None` (the default) leaves every
    /// deploy going through immediately regardless of host capacity.
    #[must_use]
    pub fn with_admission_control(mut self, reserved_memory_mb: Option<u64>) -> Self {
        self.reserved_memory_mb = reserved_memory_mb;
        self
    }

    /// Enables resource pooling (SPEC.md §3.1): projects declaring
    /// `[dependencies.*]` of kind `postgres`/`redis` get a per-branch
    /// logical database / Redis index carved out of these shared instances
    /// instead of failing to deploy.
    #[must_use]
    pub fn with_resource_pools(
        mut self,
        postgres_url: Option<String>,
        redis_url: Option<String>,
        redis_pool_size: u32,
    ) -> Self {
        self.postgres_url = postgres_url;
        self.redis_url = redis_url;
        self.redis_pool_size = redis_pool_size;
        self
    }
}

#[cfg(test)]
mod tests;
