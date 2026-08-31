//! Application service: orchestrates the domain through the ports.
//!
//! This is the thin "application" layer of the hexagonal architecture. It
//! wires [`SqliteStore`], a [`GitPort`] and a [`ContainerPort`] together to
//! expose the operations the interfaces (CLI, HTTP API) call.

#![allow(unused_imports, clippy::pedantic, clippy::nursery)]

use std::path::PathBuf;
use std::sync::Arc;

use crate::adapter::store::SqliteStore;
use oxid_core::{ContainerPort, GitPort, PoolKind, ProjectId};

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
pub use types::{DeployOutcome, DeployReport, GcSummary, InfraStatus, NodeStats};

/// What a lifecycle operation is exclusive *against*.
///
/// One process-wide mutex used to serialize all of them. That closed real
/// races — see `lifecycle_lock` — but two branches of two different projects
/// share no checkout, no container name and no environment row, so making
/// them queue only cost throughput. These variants name the things that
/// genuinely cannot overlap; everything else is free to run at once.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum LockKey {
    /// One branch of one project: its environment rows, its container name,
    /// its cutover. Held for a whole deploy, pause, wake or destroy.
    Branch(ProjectId, String),
    /// One project's git cache. `checkout_commit` force-rewrites a single
    /// on-disk working directory that every branch of that project shares,
    /// so two of them checking out at once would have one build the other's
    /// tree. Held only until the build context has been captured, never
    /// across the build itself — that is the part worth overlapping.
    GitCache(ProjectId),
    /// Host capacity. One query, serialized node-wide so two concurrent
    /// deploys cannot both see the same free memory and both claim it.
    Admission,
    /// One shared resource pool, by kind and instance name. Slots are handed
    /// out by reading which are taken and picking the lowest free one, so
    /// two branches provisioning at once would otherwise both read the same
    /// set and both take the same slot — two branches sharing one Redis
    /// database, which no uniqueness constraint would catch, since a lease
    /// is unique per branch rather than per slot.
    ResourcePool(PoolKind, String),
}

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
    /// Host port the built-in Traefik (`oxid infra setup`) publishes its
    /// `web` entrypoint on. 80 by default; configurable because an operator
    /// whose 80 is already taken by another proxy could otherwise never run
    /// the bootstrap at all.
    traefik_http_port: u16,
    /// Automatic certificates for deployed environments, and the host port
    /// their `websecure` entrypoint is published on. `None` keeps every
    /// route on plain HTTP, which is what an install that never configured
    /// ACME gets.
    acme: Option<(oxid_core::AcmeConfig, u16)>,
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
    ///
    /// Keyed rather than global: see [`LockKey`] for what each key protects.
    /// A single mutex gave the same guarantees but also made every deploy on
    /// the node wait for every other one.
    lifecycle_lock: Arc<crate::service::keyed_lock::KeyedLocks<LockKey>>,
    /// Ensures only one deploy-queue drain runs at a time. The scheduler
    /// drains on every tick and each accepted webhook kicks off a drain of
    /// its own, so without this two drains could read the same pending row
    /// before either removed it and deploy the same push twice.
    /// `try_lock`-ed, never awaited: if a drain is already in flight it will
    /// pick up the row that was just enqueued anyway.
    deploy_drain_lock: Arc<tokio::sync::Mutex<()>>,
    /// One in-flight `git fetch` per project, shared by everyone who asked
    /// for fresh refs at the same time. A fetch brings down every branch of
    /// a repository, so a burst of pushes to one project needs one of them,
    /// not one each — see [`crate::service::refresh_coalescer`].
    git_fetches: Arc<crate::service::refresh_coalescer::RefreshCoalescer<ProjectId, PathBuf>>,
    /// How many queued deploys a single drain runs at once. Builds are
    /// mostly waiting on Docker, so overlapping them is nearly free; the cap
    /// exists so a large backlog cannot ask the host to build everything
    /// simultaneously.
    deploy_concurrency: usize,
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
/// Host port the built-in Traefik publishes on unless told otherwise — the
/// one every wildcard-DNS setup expects to reach a branch on without a
/// port suffix.
const DEFAULT_TRAEFIK_HTTP_PORT: u16 = 80;

/// Queued deploys run per drain, from `OXID_DEPLOY_CONCURRENCY`. Four by
/// default: enough to hide the wait on Docker, low enough that a backlog
/// cannot ask one host to build everything at once.
fn default_deploy_concurrency() -> usize {
    std::env::var("OXID_DEPLOY_CONCURRENCY")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(4)
}
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
            traefik_http_port: DEFAULT_TRAEFIK_HTTP_PORT,
            acme: None,
            lifecycle_lock: Arc::new(crate::service::keyed_lock::KeyedLocks::default()),
            deploy_drain_lock: Arc::new(tokio::sync::Mutex::new(())),
            deploy_concurrency: default_deploy_concurrency(),
            git_fetches: Arc::new(crate::service::refresh_coalescer::RefreshCoalescer::default()),
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

    /// Enables automatic certificates for deployed environments.
    #[must_use]
    pub fn with_acme(mut self, acme: oxid_core::AcmeConfig, https_port: u16) -> Self {
        self.acme = Some((acme, https_port));
        self
    }

    /// The scheme deployed environments are reachable on, for anything that
    /// renders a URL. Derived from whether certificates are configured, so
    /// the CLI and the dashboard cannot disagree with the proxy.
    #[must_use]
    pub const fn routing_scheme(&self) -> &'static str {
        if self.acme.is_some() { "https" } else { "http" }
    }

    /// An environment's address as a person should type it, scheme included.
    ///
    /// The CLI used to print the bare hostname in Traefik mode, because the
    /// scheme is a property of the proxy and only the daemon knows it. Now
    /// that certificates can be on, guessing `http://` would be wrong half
    /// the time and copying the bare host is wrong every time.
    #[must_use]
    pub fn public_url(&self, env: &oxid_core::Environment) -> String {
        if self.docker_network.is_some() {
            format!("{}://{}/", self.routing_scheme(), env.url)
        } else {
            // Direct-publish: the container's own port, never behind the
            // proxy that terminates TLS, so it genuinely is plain HTTP.
            env.public_port
                .or(env.host_port)
                .map_or_else(|| env.url.clone(), |p| format!("http://<host>:{p}/"))
        }
    }

    /// Sets the host port the built-in Traefik publishes on
    /// (`OXID_TRAEFIK_HTTP_PORT`). Defaults to 80.
    #[must_use]
    pub const fn with_traefik_http_port(mut self, port: u16) -> Self {
        self.traefik_http_port = port;
        self
    }

    /// The host port [`Self::infra_bootstrap`] will publish Traefik on.
    #[must_use]
    pub const fn traefik_http_port(&self) -> u16 {
        self.traefik_http_port
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
