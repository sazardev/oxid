//! Application service: orchestrates the domain through the ports.
//!
//! This is the thin "application" layer of the hexagonal architecture. It
//! wires [`SqliteStore`], a [`GitPort`] and a [`ContainerPort`] together to
//! expose the operations the interfaces (CLI, HTTP API) call.

#![allow(unused_imports, clippy::pedantic, clippy::nursery)]

use std::path::PathBuf;
use std::sync::Arc;

use crate::adapter::store::SqliteStore;
use oxid_core::{ContainerPort, GitPort, NodeId, PoolKind, ProjectId};

pub mod admission;
pub mod auth;
pub mod deploy;
pub mod error;
pub mod forge;
pub mod gc;
pub mod helpers;
pub mod infra;
pub mod lifecycle;
pub mod node;
pub mod project;
pub mod provision;
pub mod types;

pub use error::CpError;
pub use node::{NodeConnector, NodeView};
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
    /// One node's capacity. Two concurrent deploys aimed at the same node
    /// must not both see the same free memory and both claim it.
    ///
    /// The lock was never the reservation, and its old comment said
    /// otherwise. The reservation is that `committed_memory_mb` counts rows
    /// in `building` as well as `running`, so a deploy that has passed
    /// admission but not yet started its container is memory already
    /// promised. All this does is stop two readers seeing the same total.
    /// There is no reservation table, and there does not need to be.
    ///
    /// Keyed by node because that count is now `AND node_id = ?`: memory
    /// committed on `eu-2` says nothing about whether a deploy fits on
    /// `eu-1`, so making the two queue behind each other would only cost
    /// throughput.
    Admission(NodeId),
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
    /// Builds a Docker client for a registered node. `None` in a build
    /// that never wired one up (the test suite), which makes registering a
    /// remote node a clear error rather than a node nothing can reach.
    node_connector: Option<crate::service::control_plane::node::NodeConnector<O>>,
    /// Every node this daemon can place a container on — see
    /// [`crate::service::fleet`]. One entry (node 1, this daemon's own
    /// Docker socket) unless an operator registered more, which is what
    /// makes an upgrade to a multi-node build behaviour-identical.
    fleet: crate::service::fleet::Fleet<O>,
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
    /// The bearer token Traefik presents when polling this daemon for the
    /// fleet's routers, and how often it polls. `None` leaves Traefik on
    /// the Docker label provider alone — which is what a daemon with no API
    /// token has to do, since it has no credential to hand out.
    fleet_routing: Option<(String, String)>,
    /// Automatic certificates for deployed environments, and the host port
    /// their `websecure` entrypoint is published on. `None` keeps every
    /// route on plain HTTP, which is what an install that never configured
    /// ACME gets.
    acme: Option<(oxid_core::AcmeConfig, u16)>,
    /// Single-flights the forge-notification drain: two passes would read
    /// the same pending row and comment twice.
    ///
    /// Still an in-process lock, unlike the deploy queue's database claim,
    /// and that is a deliberate difference of stakes rather than an
    /// oversight — the worst case here is a duplicate comment, not a
    /// duplicate deploy. If a second daemon ever becomes ordinary rather
    /// than a restart artefact, this wants the same treatment.
    forge_drain_lock: Arc<tokio::sync::Mutex<()>>,
    /// Random per-process value, so a restarted daemon never mistakes a
    /// claim it made in a previous life for one a live worker holds.
    boot_nonce: u64,
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
    /// One in-flight `git fetch` per project, shared by everyone who asked
    /// for fresh refs at the same time. A fetch brings down every branch of
    /// a repository, so a burst of pushes to one project needs one of them,
    /// not one each — see [`crate::service::refresh_coalescer`].
    git_fetches: Arc<crate::service::refresh_coalescer::RefreshCoalescer<ProjectId, PathBuf>>,
    /// How many queued deploys a single drain runs at once.
    /// `None` means "work it out from the fleet" — see
    /// [`Self::drain_width`]. `Some` is an operator who set
    /// `OXID_DEPLOY_CONCURRENCY` and meant it, so it is used verbatim
    /// however many nodes there are.
    deploy_concurrency: Option<usize>,
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
    /// How long a status query to a node may take before this daemon gives
    /// up on it for the decision in hand — see
    /// [`crate::service::fleet::STATUS_DEADLINE`], which is the default and
    /// carries the reasoning.
    ///
    /// A field rather than the constant everywhere so the test suite can
    /// shorten it, exactly as [`Self::with_readiness_check`] exists so a
    /// fake `ContainerPort` need not own a real socket. A high-latency link
    /// is the other reason it might legitimately move.
    status_deadline: std::time::Duration,
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
fn default_deploy_concurrency() -> Option<usize> {
    std::env::var("OXID_DEPLOY_CONCURRENCY")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
}

/// Queued deploys run per node, per drain wave, when the operator has not
/// said otherwise. Four is what a single host was measured to want: enough
/// to hide the wait on Docker, low enough that a backlog cannot ask one
/// machine to build everything at once.
const DEPLOYS_PER_NODE: usize = 4;

/// Ceiling on a drain wave however large the fleet gets.
///
/// Not because more nodes could not absorb more, but because every entry in
/// a wave is a claim, a git checkout on *this* machine and a build context
/// tarred here — the control plane is the shared resource, and a fleet of
/// fifty would otherwise ask it to hold fifty contexts at once.
const MAX_DRAIN_WIDTH: usize = 32;
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
            node_connector: None,
            fleet: crate::service::fleet::Fleet::single(oci),
            cache_dir,
            docker_network: None,
            daemon_url: DEFAULT_DAEMON_URL.to_owned(),
            traefik_http_port: DEFAULT_TRAEFIK_HTTP_PORT,
            acme: None,
            fleet_routing: None,
            forge_drain_lock: Arc::new(tokio::sync::Mutex::new(())),
            boot_nonce: rand::RngCore::next_u64(&mut rand::rngs::OsRng),
            lifecycle_lock: Arc::new(crate::service::keyed_lock::KeyedLocks::default()),
            deploy_concurrency: default_deploy_concurrency(),
            git_fetches: Arc::new(crate::service::refresh_coalescer::RefreshCoalescer::default()),
            postgres_url: None,
            redis_url: None,
            redis_pool_size: DEFAULT_REDIS_POOL_SIZE,
            default_memory_limit_mb: None,
            default_cpu_limit_millicores: None,
            reserved_memory_mb: None,
            proxy: crate::service::proxy::ProxyRegistry::default(),
            status_deadline: crate::service::fleet::STATUS_DEADLINE,
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

    /// Shortens (or lengthens) the deadline on a status query to a node.
    ///
    /// Production keeps the default. Tests set it to milliseconds so that
    /// "a silent node is skipped rather than waited on" can be asserted
    /// without the test itself waiting.
    #[must_use]
    pub const fn with_status_deadline(mut self, deadline: std::time::Duration) -> Self {
        self.status_deadline = deadline;
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

    /// Points Traefik's HTTP provider at this daemon, so routers come from
    /// the database as well as from container labels.
    ///
    /// **Both providers stay on.** The label provider keeps routing exactly
    /// what it routes today; the HTTP one adds the two classes of
    /// environment labels structurally cannot describe — those on another
    /// node, and those whose container is stopped. An upgrade that removed
    /// the first would be a migration silently taking behaviour away.
    #[must_use]
    pub fn with_fleet_routing(
        mut self,
        api_token: impl Into<String>,
        poll_interval: impl Into<String>,
    ) -> Self {
        self.fleet_routing = Some((api_token.into(), poll_interval.into()));
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

    /// An environment's address as a person should type it, scheme
    /// included — or `None` when this daemon cannot know it.
    ///
    /// In Traefik mode the address is the routed hostname and the scheme
    /// follows from whether certificates are configured, so both are known.
    ///
    /// In direct-publish mode they are not. The environment is reachable at
    /// a port on *the host running Oxid*, and the daemon has no idea what
    /// name or address that host answers to from wherever the reader is
    /// sitting — behind NAT, on a VPN, through a bastion. An earlier
    /// version filled that gap with a literal `<host>` placeholder, which
    /// was fine in a terminal where the reader knows the machine and
    /// useless in a pull-request comment, where it was published as a link.
    /// Returning `None` is the honest answer, and callers say what they can
    /// instead of inventing a hostname.
    #[must_use]
    pub fn public_url(&self, env: &oxid_core::Environment) -> Option<String> {
        self.docker_network
            .is_some()
            .then(|| format!("{}://{}/", self.routing_scheme(), env.url))
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

    /// Teaches this control plane how to reach a registered node.
    ///
    /// Separate from [`Self::new`] because constructing a `ContainerPort` is
    /// the adapter's knowledge, not the application layer's — see
    /// [`crate::service::control_plane::node::NodeConnector`]. Without it a
    /// daemon runs perfectly well on its own node and refuses to register
    /// others, which is exactly right for a build that has no way to talk
    /// to them.
    #[must_use]
    pub fn with_node_connector(
        mut self,
        connect: crate::service::control_plane::node::NodeConnector<O>,
    ) -> Self {
        self.node_connector = Some(connect);
        self
    }

    /// How wide a drain wave should be right now.
    ///
    /// Builds are mostly spent waiting on Docker, so overlapping them is
    /// nearly free — but "how many" was a constant, and a constant is wrong
    /// in both directions once there is a fleet. Four deploys across five
    /// nodes leaves four fifths of the hardware idle; four across a single
    /// full node is three wasted admission round trips and three requeues.
    ///
    /// So it scales with the nodes that can actually take work: `draining`
    /// and `down` ones are excluded, because a wave sized for hardware that
    /// refuses placements is a wave that mostly comes back unplaced.
    ///
    /// `OXID_DEPLOY_CONCURRENCY` overrides it entirely. An operator who set
    /// a number meant that number, and inferring a different one from the
    /// node count would quietly ignore them.
    pub(crate) fn drain_width(&self) -> usize {
        if let Some(explicit) = self.deploy_concurrency {
            return explicit;
        }
        let placeable = self
            .fleet
            .handles()
            .iter()
            .filter(|handle| handle.node.state.accepts_placements())
            .count()
            // A fleet whose every node is draining still drains its queue,
            // one wave at a time: those deploys will not be placed, and
            // finding that out is what puts them back with a delay instead
            // of leaving the queue frozen.
            .max(1);
        (placeable * DEPLOYS_PER_NODE).min(MAX_DRAIN_WIDTH)
    }

    /// The fleet, for the API and the scheduler's health probe.
    #[must_use]
    pub const fn fleet(&self) -> &crate::service::fleet::Fleet<O> {
        &self.fleet
    }

    /// The node `id`, or an error naming it.
    ///
    /// An error, never a silent fallback to the local node: dispatching a
    /// `remove` meant for `eu-1` at this daemon's own Docker would delete a
    /// container that happens to share the name, and dispatching a `stop`
    /// would find nothing and report success for a container still running
    /// somewhere else. Both are worse than a failed operation an operator
    /// can see.
    pub(crate) fn node(
        &self,
        id: NodeId,
    ) -> Result<std::sync::Arc<crate::service::fleet::NodeHandle<O>>, CpError> {
        self.fleet.get(id).ok_or_else(|| {
            CpError::UnknownNode(format!(
                "node `{id}` is not registered with this daemon — its \
                 environments are left exactly as they are"
            ))
        })
    }

    /// The Docker client for node `id`.
    pub(crate) fn oci_for(&self, id: NodeId) -> Result<std::sync::Arc<O>, CpError> {
        Ok(std::sync::Arc::clone(&self.node(id)?.oci))
    }

    /// Where the branch proxy should send traffic for an environment.
    ///
    /// The node's address, or loopback when it named none — which is both
    /// correct for the local node and literally what this dialled before
    /// nodes existed, so a single-node install is unchanged.
    ///
    /// An error rather than a loopback fallback for an unknown node: sending
    /// a remote branch's traffic at this machine's port would not fail, it
    /// would connect to whatever else happens to be listening there.
    pub(crate) fn proxy_target(
        &self,
        env: &oxid_core::Environment,
        port: u16,
    ) -> Result<crate::service::proxy::Target, CpError> {
        Ok(crate::service::proxy::Target {
            host: self.node(env.node_id)?.proxy_host().to_owned(),
            port,
        })
    }

    /// This daemon's own Docker client.
    ///
    /// For infrastructure that belongs to the *control plane* rather than to
    /// a node: the Traefik in front of every environment, the shared Docker
    /// network it and the daemon sit on, the ACME volume. Those live here
    /// and nowhere else, so they are addressed here and not through
    /// [`Self::oci_for`].
    pub(crate) fn local_oci(&self) -> std::sync::Arc<O> {
        std::sync::Arc::clone(&self.fleet.local().oci)
    }
}

#[cfg(test)]
mod tests;
