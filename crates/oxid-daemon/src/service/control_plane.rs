//! Application service: orchestrates the domain through the ports.
//!
//! This is the thin "application" layer of the hexagonal architecture. It
//! wires [`SqliteStore`], a [`GitPort`] and a [`ContainerPort`] together to
//! expose the operations the interfaces (CLI, HTTP API) call.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use oxid_core::services::gc::{self, GcAction};
use oxid_core::services::subdomain::subdomain_for;
use oxid_core::services::var_resolution::{VarSources, set_secret};
use oxid_core::{
    AuditEvent, AuditFilter, AuditStore, Branch, BranchName, BuildSpec, CommitRef, ContainerPort,
    ContainerSpec, ContainerStatus, Dependency, DomainError, EnvVarScope, Environment,
    EnvironmentId, EnvironmentState, EnvironmentStore, GitError, GitPort, LogStream, OciError,
    OffsetDateTime, PoolError, PoolKind, Project, ProjectId, ProjectStore, RepoUrl,
    RepositoryError, SecretStore, SecretValue, StateTransition, Ttl,
};

use crate::adapter::config::{self, ConfigError};
use crate::adapter::postgres_pool::PostgresPool;
use crate::adapter::store::{ApiTokenSummary, SqliteStore};
use crate::request_context::current_request_id;

/// Errors surfaced by the control plane.
#[derive(Debug, thiserror::Error)]
pub enum CpError {
    /// Configuration file could not be read or parsed.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// A persistence operation failed.
    #[error(transparent)]
    Store(#[from] RepositoryError),
    /// A git operation failed.
    #[error(transparent)]
    Git(#[from] GitError),
    /// A docker operation failed.
    #[error(transparent)]
    Oci(#[from] OciError),
    /// A domain rule was violated.
    #[error(transparent)]
    Domain(#[from] DomainError),
    /// A resource-pool (Postgres/Redis) operation failed.
    #[error(transparent)]
    Pool(#[from] PoolError),
    /// A requested record does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The built-in zero-downtime reverse proxy (direct-publish mode) could
    /// not bind a listener.
    #[error(transparent)]
    Proxy(#[from] crate::service::proxy::ProxyError),
    /// A newly-deployed instance never started accepting connections within
    /// the readiness timeout — the redeploy is aborted and the previous
    /// instance (if any) is left running untouched.
    #[error("new instance never became ready: {0}")]
    DeployNotReady(String),
    /// A deploy's own resource request exceeds the host's total usable
    /// capacity — no amount of waiting in the queue would ever make this
    /// one fit, so it's rejected immediately instead of queued forever.
    #[error("insufficient host capacity: {0}")]
    InsufficientCapacity(String),
}

/// Outcome of one [`ControlPlane::sweep`] pass across all environments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcSummary {
    /// Environments suspended via `docker pause` (idle past `pause_after`).
    pub paused: u64,
    /// Environments stopped for deep sleep (idle past the hibernate threshold).
    pub hibernated: u64,
    /// Environments torn down (idle past `destroy_after`).
    pub destroyed: u64,
    /// Per-environment failures; the sweep continues past these.
    pub errors: Vec<(EnvironmentId, String)>,
}

/// Aggregate node-wide counts for the web dashboard's overview — see
/// [`ControlPlane::node_stats`].
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct NodeStats {
    /// Number of registered projects.
    pub projects: u64,
    /// Environments currently `Running`.
    pub environments_running: u64,
    /// Environments currently `Paused`.
    pub environments_paused: u64,
    /// Environments currently `Building`.
    pub environments_building: u64,
    /// Environments currently `Hibernating`.
    pub environments_hibernating: u64,
    /// Environments currently `Destroyed` (kept for rollback history).
    pub environments_destroyed: u64,
    /// Deploys currently waiting for host capacity.
    pub queue_length: u64,
    /// Total host memory Docker reports, in bytes.
    pub host_total_memory_bytes: u64,
    /// Host CPU count Docker reports.
    pub host_cpu_count: u32,
    /// Whether `with_traefik` was configured (`OXID_DOCKER_NETWORK` set) —
    /// when `false`, an environment's `url` (a `branch.project-base-domain`
    /// hostname, meaningful only to a Traefik `Host()` rule) isn't reachable
    /// as a URL at all; the real address is the project's own
    /// `[routing].port` published directly on whatever host is running the
    /// daemon. The dashboard uses this to decide which one to link to.
    pub traefik_enabled: bool,
}

/// Result of a capacity-aware deploy attempt (see
/// [`ControlPlane::deploy_or_queue`]).
#[derive(Debug, Clone)]
pub enum DeployOutcome {
    /// The deploy ran immediately and is now live.
    Deployed(Environment),
    /// The host doesn't currently have room; the request was persisted to
    /// `deploy_queue` (see [`SqliteStore::enqueue_deploy`]) at this 1-based
    /// position and will be retried automatically as capacity frees up.
    Queued {
        /// 1-based position in the queue at the moment of enqueueing.
        position: u64,
    },
}

/// Whether a new deploy should proceed now or wait for capacity — see
/// [`ControlPlane::check_admission`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    /// Enough host memory is free (after `reserved_memory_mb` and every
    /// other live environment's own reservation) for this request too.
    Fits,
    /// Not enough room right now, but the request could fit once other
    /// environments free memory — queue it rather than fail or overcommit.
    Queue,
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

    /// Registers the project declared by `oxid.toml` in `repo_dir`.
    ///
    /// Idempotent: if a project with the same `origin` URL already exists, the
    /// existing record is returned.
    ///
    /// # Errors
    /// Returns [`CpError`] on config, git or persistence failures.
    pub async fn register_project(&self, repo_dir: &Path) -> Result<Project, CpError> {
        let repo_url = self.git.remote_url(repo_dir).await?;

        if let Some(existing) = self.find_project_by_repo(&repo_url).await? {
            return Ok(existing);
        }

        let parsed = config::parse_project(repo_dir)?;
        let mut project = Project::new(ProjectId(0), parsed.name, repo_url.clone(), parsed.config)?;
        match ProjectStore::create(&self.store, &project).await {
            Ok(id) => {
                project.id = id;
                Ok(project)
            }
            // Lost a race with another concurrent first-time registration of
            // the same repo (e.g. two branches of a brand-new project
            // pushed at once, each triggering its own webhook-driven
            // registration): the `find_project_by_repo` check above and
            // this `create` are not atomic, so both can pass the check
            // before either commits. Found by firing ten concurrent `oxid
            // up` at a never-before-registered project. Falling back to a
            // re-read here is what actually makes this idempotent under
            // concurrency, not just on repeated sequential calls.
            Err(RepositoryError::Conflict(_)) => self
                .find_project_by_repo(&repo_url)
                .await?
                .ok_or_else(|| CpError::NotFound(format!("project for `{repo_url}`"))),
            Err(e) => Err(e.into()),
        }
    }

    /// Lists all registered projects.
    ///
    /// # Errors
    /// Returns [`CpError`] on persistence failures.
    pub async fn list_projects(&self) -> Result<Vec<Project>, CpError> {
        Ok(self.store.list().await?)
    }

    /// Lists environments of a project.
    ///
    /// # Errors
    /// Returns [`CpError::NotFound`] if the project does not exist.
    pub async fn list_environments(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<Environment>, CpError> {
        self.ensure_project(project_id).await?;
        Ok(self.store.list_by_project(project_id).await?)
    }

    /// Permanently deletes a project: destroys every environment that isn't
    /// already `Destroyed` (tearing down its container, image and any leased
    /// resource-pool slots), removes the project's git-cache clone, then
    /// deletes the project row — which cascades to its `secrets` and
    /// `environments` rows at the database level (`ON DELETE CASCADE`).
    ///
    /// Before this existed, a registered project was permanent: there was no
    /// way to remove it, so its git-cache clone and any leftover
    /// `Environment`/`secrets` rows accumulated forever.
    ///
    /// # Errors
    /// Returns [`CpError::NotFound`] if the project does not exist.
    pub async fn delete_project(&self, project_id: ProjectId) -> Result<(), CpError> {
        let project = self.ensure_project(project_id).await?;

        for env in self.store.list_by_project(project_id).await? {
            if env.state != EnvironmentState::Destroyed {
                // No need to purge secrets per-branch here: the project
                // delete below cascades and removes every secret row anyway.
                self.destroy(env.id, false).await?;
            }
        }

        let cache_path = self
            .cache_dir
            .join(crate::adapter::git::cache_dir_name(&project.repo_url));
        if let Err(e) = tokio::fs::remove_dir_all(&cache_path).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(RepositoryError::Storage(format!(
                "could not remove git cache `{}`: {e}",
                cache_path.display()
            ))
            .into());
        }

        Ok(ProjectStore::delete(&self.store, project_id).await?)
    }

    /// Updates a project's idle/lifetime policy (`pause_after`/
    /// `destroy_after`) — the two settings `oxid.toml` otherwise only ever
    /// seeds once, at first registration, with no way to change them again
    /// short of re-registering. Either can be omitted to leave it as-is;
    /// takes effect on the *next* GC sweep, no redeploy needed.
    ///
    /// # Errors
    /// Returns [`CpError::NotFound`] if the project does not exist.
    pub async fn update_project_ttls(
        &self,
        project_id: ProjectId,
        pause_after: Option<Ttl>,
        destroy_after: Option<Ttl>,
    ) -> Result<Project, CpError> {
        let mut project = self.ensure_project(project_id).await?;
        if let Some(pause_after) = pause_after {
            project.config.pause_after = pause_after;
        }
        if let Some(destroy_after) = destroy_after {
            project.config.destroy_after = destroy_after;
        }
        ProjectStore::update(&self.store, &project).await?;
        Ok(project)
    }

    /// Sets (or, with `token: None`/an empty string, clears) a project's git
    /// access token — required for the daemon to clone/fetch a *private*
    /// repository, since its own git-cache clone is independent of any
    /// credential helper the operator's own shell has configured. Never
    /// returned by any API response: it lives only in the encrypted
    /// `projects.git_token_enc` column, decrypted just-in-time by
    /// [`Self::deploy_at`] right before the git operation that needs it.
    ///
    /// # Errors
    /// Returns [`CpError::NotFound`] if the project does not exist.
    pub async fn set_project_git_token(
        &self,
        project_id: ProjectId,
        token: Option<&str>,
    ) -> Result<(), CpError> {
        self.ensure_project(project_id).await?;
        Ok(self.store.set_git_token(project_id, token).await?)
    }

    /// Deploys `branch` for a project: clone, build, run, then transition to
    /// `Running`. Always deploys immediately, ignoring admission control —
    /// see [`Self::deploy_or_queue`] for the capacity-aware entry point
    /// `oxid up`/the API actually use.
    ///
    /// # Errors
    /// Returns [`CpError`] on any pipeline step failure.
    #[tracing::instrument(skip(self), fields(%project_id, %branch))]
    pub async fn deploy(
        &self,
        project_id: ProjectId,
        branch: BranchName,
    ) -> Result<Environment, CpError> {
        match self
            .deploy_at(project_id, branch, None, None, false)
            .await?
        {
            DeployOutcome::Deployed(env) => Ok(env),
            DeployOutcome::Queued { .. } => {
                unreachable!("admission control is off, so this never queues")
            }
        }
    }

    /// Identical to [`Self::deploy`], but attributes the resulting audit
    /// events to `operator` (a named API token's owner) instead of leaving
    /// them anonymous.
    ///
    /// # Errors
    /// Returns [`CpError`] on any pipeline step failure.
    #[tracing::instrument(skip(self, operator), fields(%project_id, %branch, ?operator))]
    pub async fn deploy_with_operator(
        &self,
        project_id: ProjectId,
        branch: BranchName,
        operator: Option<String>,
    ) -> Result<Environment, CpError> {
        match self
            .deploy_at(project_id, branch, None, operator, false)
            .await?
        {
            DeployOutcome::Deployed(env) => Ok(env),
            DeployOutcome::Queued { .. } => {
                unreachable!("admission control is off, so this never queues")
            }
        }
    }

    /// The capacity-aware entry point: deploys `branch` immediately if it
    /// fits the host's currently free memory (see
    /// [`Self::with_admission_control`]), or queues it (persisted — see
    /// [`SqliteStore::enqueue_deploy`]) to be retried automatically as
    /// capacity frees up, rather than either failing outright or piling
    /// onto an already-strained host. If the request alone could *never*
    /// fit (it exceeds total usable capacity by itself), it's rejected
    /// immediately instead of queued forever.
    ///
    /// # Errors
    /// Returns [`CpError`] on any pipeline step failure, or
    /// [`CpError::InsufficientCapacity`] if the request can never fit.
    #[tracing::instrument(skip(self, operator), fields(%project_id, %branch, ?operator))]
    pub async fn deploy_or_queue(
        &self,
        project_id: ProjectId,
        branch: BranchName,
        operator: Option<String>,
    ) -> Result<DeployOutcome, CpError> {
        self.deploy_at(project_id, branch, None, operator, true)
            .await
    }

    /// Retries queued deploys (oldest first) that now fit the host's
    /// currently free capacity — meant to be driven by the scheduler
    /// alongside [`Self::sweep`], so capacity freed by a GC pause/hibernate
    /// or a manual `destroy` gets handed to whoever has been waiting
    /// longest instead of sitting idle until the next manual `oxid up`.
    ///
    /// Stops at the first entry that still doesn't fit rather than skipping
    /// ahead to a smaller one further back in the queue — preserves FIFO
    /// fairness (SPEC.md "Eficiencia Absoluta": queue and wait, don't let a
    /// small request cut in line ahead of one that's been waiting longer).
    ///
    /// Returns the queue ids that failed to redeploy once retried (e.g. the
    /// branch was deleted upstream in the meantime); the queue continues
    /// past these rather than stalling on one bad entry.
    ///
    /// # Errors
    /// Returns [`CpError`] if the queue or host capacity itself can't be
    /// read at all.
    pub async fn retry_queued_deploys(&self) -> Result<Vec<(u64, CpError)>, CpError> {
        let mut failures = Vec::new();
        for queued in self.store.list_deploy_queue().await? {
            let project = match self.ensure_project(queued.project_id).await {
                Ok(p) => p,
                Err(e) => {
                    failures.push((queued.id, e));
                    continue;
                }
            };
            match self.check_admission(&project).await {
                Ok(Admission::Fits) => {}
                Ok(Admission::Queue) => break,
                Err(e) => {
                    failures.push((queued.id, e));
                    continue;
                }
            }

            self.store.remove_from_deploy_queue(queued.id).await?;
            let branch = match BranchName::parse(&queued.branch) {
                Ok(b) => b,
                Err(e) => {
                    failures.push((queued.id, e.into()));
                    continue;
                }
            };
            match self
                .deploy_at(queued.project_id, branch, None, queued.operator, false)
                .await
            {
                Ok(DeployOutcome::Deployed(_)) => {}
                Ok(DeployOutcome::Queued { .. }) => {
                    unreachable!("check_admission is off, so this never queues")
                }
                Err(e) => failures.push((queued.id, e)),
            }
        }
        Ok(failures)
    }

    /// Deploys `branch`, pinned to `sha_override` instead of the branch's
    /// current head when given — the mechanism [`Self::rollback`] reuses to
    /// redeploy a prior commit. Otherwise identical to [`Self::deploy`].
    /// When `check_admission` is set, may return
    /// [`DeployOutcome::Queued`] instead of deploying — see
    /// [`Self::deploy_or_queue`].
    ///
    /// # Errors
    /// Returns [`CpError`] on any pipeline step failure.
    #[allow(clippy::too_many_lines)]
    #[tracing::instrument(skip(self, sha_override, operator), fields(%project_id, %branch, ?operator, check_admission))]
    async fn deploy_at(
        &self,
        project_id: ProjectId,
        branch: BranchName,
        sha_override: Option<String>,
        operator: Option<String>,
        check_admission: bool,
    ) -> Result<DeployOutcome, CpError> {
        tracing::info!(%project_id, %branch, "deploy started");
        // Serializes the whole pipeline below (see `lifecycle_lock`'s doc
        // comment for the two race conditions this closes) — admission
        // control is decided under the same lock as the deploy it gates,
        // so two concurrent requests can't both see room and both proceed.
        let _guard = self.lifecycle_lock.lock().await;

        let project = self.ensure_project(project_id).await?;

        if check_admission && let Admission::Queue = self.check_admission(&project).await? {
            let queue_id = self
                .store
                .enqueue_deploy(project_id, &branch, operator.as_deref())
                .await?;
            // `enqueue_deploy` returns the row's own id, not its rank among
            // still-pending entries (earlier ones may already have been
            // retried and removed) — look it up so `position` reports what
            // it documents.
            let position = self
                .store
                .list_deploy_queue()
                .await?
                .iter()
                .position(|q| q.id == queue_id)
                .map_or(1, |i| i as u64 + 1);
            tracing::info!(%project_id, %branch, position, "deploy queued (insufficient host capacity)");
            return Ok(DeployOutcome::Queued { position });
        }

        // 0. A redeploy of an already-live branch (a webhook firing on a new
        // push, or a second `oxid up`) used to destroy the previous
        // container *before* building/starting the new one — always a real
        // gap where the branch was unreachable, and in direct-publish mode
        // the address itself changed underneath anyone already using it.
        // The previous instance is kept fully alive here, still serving
        // traffic, until the new one is built, started and confirmed
        // healthy — see the cutover at the end of `run_and_activate`.
        let previous = self
            .find_environment_by_branch(project_id, &branch)
            .await?
            .filter(|e| e.state != EnvironmentState::Destroyed);

        // 1. Clone cache + resolve (or reuse an explicit rollback target)
        // + checkout the commit. `git_token`, when set, authenticates the
        // clone/fetch for a private repository (see
        // `Self::set_project_git_token`) — this daemon-side cache is cloned
        // independently of whatever git credential helper an operator's own
        // shell has configured, so a private repo needs its own credential.
        let git_token = self.store.get_git_token(project.id).await?;
        let repo_dir = self
            .git
            .ensure_repo(&project.repo_url, git_token.as_deref(), &self.cache_dir)
            .await?;
        let commit = match sha_override {
            Some(sha) => CommitRef {
                branch: branch.clone(),
                sha,
            },
            None => self.git.resolve_branch_head(&repo_dir, &branch).await?,
        };
        self.git.checkout_commit(&repo_dir, &commit.sha).await?;

        // 2. Build the image.
        //
        // `[build].context` (e.g. a monorepo subdirectory like `backend/`)
        // was parsed from `oxid.toml` and persisted, but never actually
        // consulted here — every build used the whole repo checkout as its
        // context regardless. Found while wiring `docker-compose.yml`
        // support, whose `build.context`/`build.dockerfile` pair only makes
        // sense if `dockerfile` really is resolved relative to `context`.
        let image = image_name(&project, &branch);
        let build = BuildSpec {
            context: repo_dir.join(&project.config.build.context),
            dockerfile: project
                .config
                .build
                .dockerfile
                .clone()
                .unwrap_or_else(|| "Dockerfile".to_owned()),
            image: image.clone(),
        };
        self.oci.build(&build).await?;

        // 3. Create the environment (Building) and persist it.
        let url = subdomain_for(&branch, &project.config.base_domain);
        let now = OffsetDateTime::now_utc();
        let mut env = Environment::new(
            EnvironmentId(0),
            project.id,
            Branch::new(commit.branch, commit.sha)?,
            EnvironmentState::Building,
            url.clone(),
            now,
        )?;
        let env_id = EnvironmentStore::create(&self.store, &env).await?;
        env.id = env_id;
        // A per-deployment-unique container name, distinct from the
        // previous (still running) instance's — the two coexist briefly
        // during the cutover below, so they can never share a name.
        env.container_name = Some(format!(
            "oxid-{}-{}-{}",
            project.name,
            sanitize_label(&branch),
            env.id.0
        ));

        // 4-7: resolve secrets, run the container, run `on_start` hooks,
        // wait for it to be ready, then cut over from `previous` (if any)
        // and activate. Everything from here on can fail (a bad secret, a
        // Docker error, a failing hook, a readiness timeout) *after* the row
        // above was already persisted as `Building` — but `previous`, if
        // any, is never touched until the new instance is confirmed ready,
        // so a failed redeploy leaves the branch exactly as reachable as it
        // was before the redeploy started. Leaving the new row stuck as
        // `Building` on error would brick the branch permanently otherwise
        // (`Building` cannot transition to `Destroy`), see regression test
        // `failed_deploy_does_not_permanently_block_branch`.
        if let Err(err) = self
            .run_and_activate(
                &project,
                &branch,
                image,
                url,
                &mut env,
                previous.as_ref(),
                operator.as_deref(),
            )
            .await
        {
            let now = OffsetDateTime::now_utc();
            if env.transition(StateTransition::BuildFailed, now).is_ok() {
                let _ = EnvironmentStore::update(&self.store, &env).await;
                let _ = self
                    .store
                    .record(
                        &AuditEvent::with_operator(
                            u64::try_from(now.unix_timestamp()).unwrap_or_default(),
                            env.id,
                            StateTransition::BuildFailed,
                            Some(err.to_string()),
                            now,
                            operator.clone(),
                        )
                        .with_request_id(current_request_id()),
                    )
                    .await;
            }
            tracing::error!(%project_id, %branch, environment_id = %env.id, error = %err, "deploy failed");
            return Err(err);
        }

        // The new instance is live (and, per the cutover inside
        // `run_and_activate`, the previous container is already gone) —
        // now retire the previous Environment row so `status`/branch
        // resolution stop pointing at it.
        if let Some(mut prev) = previous {
            let now = OffsetDateTime::now_utc();
            if prev.transition(StateTransition::Destroy, now).is_ok() {
                let _ = EnvironmentStore::update(&self.store, &prev).await;
            }
        }

        tracing::info!(%project_id, %branch, environment_id = %env.id, "deploy succeeded");
        Ok(DeployOutcome::Deployed(env))
    }

    /// Redeploys `branch` at a prior commit instead of its current head —
    /// the safety net for a bad deploy, since `oxid up` always rebuilds from
    /// HEAD with no way back otherwise. Reuses `environments`' existing
    /// per-deploy history (a new row per deploy, the prior one marked
    /// `Destroyed` once the new one cuts over — see [`Self::deploy_at`])
    /// rather than needing any new storage: every past deploy's commit is
    /// already sitting in
    /// `Environment.branch.commit_sha`.
    ///
    /// Without `to_sha`, rolls back to the commit immediately before the
    /// current live one. With `to_sha`, rolls back to that specific commit —
    /// but only if it actually appears in this branch's history, so a typo
    /// or an unrelated sha can't be deployed under the guise of "rollback".
    ///
    /// # Errors
    /// [`CpError::NotFound`] if the branch has no prior deploy to roll back
    /// to (or `to_sha` doesn't match one), plus anything [`Self::deploy`]
    /// can fail with.
    pub async fn rollback(
        &self,
        project_id: ProjectId,
        branch: BranchName,
        to_sha: Option<String>,
    ) -> Result<Environment, CpError> {
        self.rollback_with_operator(project_id, branch, to_sha, None)
            .await
    }

    /// Identical to [`Self::rollback`], attributing the resulting audit
    /// events to `operator`.
    ///
    /// # Errors
    /// Same as [`Self::rollback`].
    pub async fn rollback_with_operator(
        &self,
        project_id: ProjectId,
        branch: BranchName,
        to_sha: Option<String>,
        operator: Option<String>,
    ) -> Result<Environment, CpError> {
        let mut history = self.store.list_by_project(project_id).await?;
        history.retain(|e| e.branch.name == branch);
        history.sort_by_key(|e| std::cmp::Reverse(e.id.0));

        let target_sha = match to_sha {
            Some(sha) => history
                .iter()
                .find(|e| e.branch.commit_sha == sha)
                .map(|e| e.branch.commit_sha.clone())
                .ok_or_else(|| {
                    CpError::NotFound(format!(
                        "commit `{sha}` in branch `{branch}`'s deploy history"
                    ))
                })?,
            None => history
                .iter()
                .skip(1) // the current live deploy
                .map(|e| e.branch.commit_sha.clone())
                .next()
                .ok_or_else(|| {
                    CpError::NotFound(format!(
                        "a prior deploy of branch `{branch}` to roll back to"
                    ))
                })?,
        };

        match self
            .deploy_at(project_id, branch, Some(target_sha), operator, false)
            .await?
        {
            DeployOutcome::Deployed(env) => Ok(env),
            DeployOutcome::Queued { .. } => {
                unreachable!("admission control is off, so this never queues")
            }
        }
    }

    /// Resolves secrets, runs the new container, runs `[build].on_start`
    /// hooks, waits for it to actually accept connections, cuts traffic
    /// over from `previous` (if this is a redeploy of an already-live
    /// branch), then transitions `env` to `Running` and records the
    /// deployment. Split out of [`Self::deploy`] so its errors can be
    /// caught there and turned into a `BuildFailed` transition instead of
    /// leaving `env` stuck.
    ///
    /// `previous`, when given, is never touched until the new instance is
    /// confirmed ready — on any failure here, only the *new* container is
    /// cleaned up, and `previous` keeps running exactly as it was, so a
    /// failed redeploy never takes an already-live branch down with it.
    // The zero-downtime cutover (build new, wait ready, swap, remove old)
    // is one cohesive sequence that reads far worse split across helper
    // functions than as slightly-too-long straight-line code.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn run_and_activate(
        &self,
        project: &Project,
        branch: &BranchName,
        image: String,
        url: String,
        env: &mut Environment,
        previous: Option<&Environment>,
        operator: Option<&str>,
    ) -> Result<(), CpError> {
        // Global -> Project -> Branch secrets plus orchestrator runtime
        // variables (SPEC.md §2.1/§4.4).
        let mut sources = VarSources::default();
        let secrets = SecretStore::secrets_for(&self.store, Some(project.id), Some(branch)).await?;
        for (name, scope, value) in secrets.iter() {
            set_secret(&mut sources, name, scope, value.as_str());
        }
        // Resource pooling (SPEC.md §3.1): one shared Postgres/Redis
        // instance instead of a container per branch. Each declared
        // dependency's connection string is injected as a Runtime
        // variable, same as the other orchestrator-owned values below —
        // Runtime always wins the inheritance precedence, so a project
        // can't accidentally shadow it with a same-named secret.
        for dependency in &project.config.dependencies {
            let url = self
                .provision_dependency(project, branch, dependency)
                .await?;
            set_secret(
                &mut sources,
                &dependency.inject_url_as,
                EnvVarScope::Runtime,
                url,
            );
        }

        set_secret(
            &mut sources,
            "OXID_BRANCH",
            EnvVarScope::Runtime,
            branch.to_string(),
        );
        set_secret(
            &mut sources,
            "OXID_ENV_URL",
            EnvVarScope::Runtime,
            url.clone(),
        );
        let env_vars = sources
            .resolve()
            .into_iter()
            .map(|(k, v)| (k, v.as_str().to_owned()))
            .collect::<BTreeMap<_, _>>();

        let name = resolved_container_name(project, env);
        // Defensive: remove any leftover container under this exact
        // (per-deployment-unique) name, in case a prior crashed attempt for
        // this same environment id left one behind. `previous`'s container
        // (if any) has a *different* name and is deliberately left running
        // — it keeps serving traffic until the cutover below.
        match self.oci.remove(&name).await {
            Ok(()) | Err(OciError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }

        let mut labels = BTreeMap::from([
            ("oxid.project".to_owned(), project.name.clone()),
            ("oxid.branch".to_owned(), branch.to_string()),
            ("oxid.url".to_owned(), url.clone()),
        ]);
        labels.extend(self.traefik_labels(&name, &url, project.config.port));
        let spec = ContainerSpec {
            name: name.clone(),
            image,
            env: env_vars,
            container_port: project.config.port,
            labels,
            network: self.docker_network.clone(),
            memory_limit_mb: project
                .config
                .build
                .memory_limit_mb
                .or(self.default_memory_limit_mb),
            cpu_limit_millicores: project
                .config
                .build
                .cpu_limit_millicores
                .or(self.default_cpu_limit_millicores),
        };
        env.host_port = match self.oci.run(&spec).await {
            Ok(port) => port,
            Err(e) => {
                let _ = self.oci.remove(&name).await;
                return Err(e.into());
            }
        };

        for command in &project.config.build.on_start {
            if let Err(e) = self.oci.exec(&name, command).await {
                let _ = self.oci.remove(&name).await;
                return Err(e.into());
            }
        }

        // In direct-publish mode, wait for the new container to actually
        // accept connections before cutting traffic over to it — `on_start`
        // succeeding only proves those specific commands ran, not that the
        // app itself is up and listening.
        if self.docker_network.is_none()
            && self.readiness_check
            && let Some(port) = env.host_port
            && !crate::service::proxy::wait_until_ready(port, std::time::Duration::from_secs(20))
                .await
        {
            let _ = self.oci.remove(&name).await;
            return Err(CpError::DeployNotReady(format!(
                "container `{name}` did not accept connections on port {port} within 20s"
            )));
        }

        // Cutover: repoint the branch's stable proxy at the new container
        // before touching the previous one — the actual zero-downtime
        // moment. Anything already connected to the old target keeps
        // talking to it; every new connection goes to the new one.
        if self.docker_network.is_none()
            && let Some(port) = env.host_port
        {
            let public_port = self
                .proxy
                .ensure(env.project_id, branch, previous.and_then(|p| p.public_port))
                .await?;
            env.public_port = Some(public_port);
            self.proxy.set_target(env.project_id, branch, port).await;
        }

        // Only now remove the previous instance, if this was a redeploy —
        // it has been serving traffic this entire time, right up to the
        // cutover above.
        if let Some(prev) = previous {
            let prev_name = resolved_container_name(project, prev);
            let _ = self.oci.remove(&prev_name).await;
        }

        let now = OffsetDateTime::now_utc();
        env.transition(StateTransition::BuildSucceeded, now)
            .map_err(|e| state_err(&e))?;
        EnvironmentStore::update(&self.store, env).await?;
        self.store
            .record(
                &AuditEvent::with_operator(
                    u64::try_from(now.unix_timestamp()).unwrap_or_default(),
                    env.id,
                    StateTransition::BuildSucceeded,
                    Some(name),
                    now,
                    operator.map(str::to_owned),
                )
                .with_request_id(current_request_id()),
            )
            .await?;
        Ok(())
    }

    /// Resolves the connection string a branch should inject for
    /// `dependency` (SPEC.md §3.1), leasing a resource on first deploy and
    /// reusing the same one on every redeploy of the same branch.
    async fn provision_dependency(
        &self,
        project: &Project,
        branch: &BranchName,
        dependency: &Dependency,
    ) -> Result<String, CpError> {
        if let Some(existing) = self
            .store
            .find_resource_lease(
                project.id,
                branch,
                dependency.kind,
                &dependency.shared_instance,
            )
            .await?
        {
            return self.resource_url(dependency, &existing);
        }

        let resource_name = match dependency.kind {
            PoolKind::Postgres => {
                let db_name = format!(
                    "db_{}_{}",
                    sanitize_identifier(&project.name),
                    sanitize_identifier(branch.as_str())
                );
                let admin_url = self.postgres_url.as_deref().ok_or_else(|| {
                    PoolError::NotConfigured(format!(
                        "project `{}` declares a `postgres` dependency but OXID_POSTGRES_URL \
                         is not configured on this daemon",
                        project.name
                    ))
                })?;
                PostgresPool.ensure_database(admin_url, &db_name).await?;
                db_name
            }
            PoolKind::Redis => {
                if self.redis_url.is_none() {
                    return Err(PoolError::NotConfigured(format!(
                        "project `{}` declares a `redis` dependency but OXID_REDIS_URL is not \
                         configured on this daemon",
                        project.name
                    ))
                    .into());
                }
                let used = self
                    .store
                    .used_resource_names(PoolKind::Redis, &dependency.shared_instance)
                    .await?
                    .into_iter()
                    .filter_map(|n| n.parse::<u32>().ok())
                    .collect::<std::collections::BTreeSet<_>>();
                let index = lowest_free_index(&used, self.redis_pool_size).ok_or_else(|| {
                    PoolError::Failure(format!(
                        "redis pool `{}` is exhausted (capacity {})",
                        dependency.shared_instance, self.redis_pool_size
                    ))
                })?;
                index.to_string()
            }
        };

        self.store
            .create_resource_lease(
                project.id,
                branch,
                dependency.kind,
                &dependency.shared_instance,
                &resource_name,
            )
            .await?;
        self.resource_url(dependency, &resource_name)
    }

    /// Builds the connection string injected into the container for an
    /// already-resolved `resource_name` (a Postgres database name, or a
    /// Redis index as a string).
    fn resource_url(
        &self,
        dependency: &Dependency,
        resource_name: &str,
    ) -> Result<String, CpError> {
        match dependency.kind {
            PoolKind::Postgres => {
                // Presence already checked in `provision_dependency`'s
                // create path; on the reuse path (existing lease found) we
                // still need it to rebuild the DSN.
                let admin_url = self.postgres_url.as_deref().ok_or_else(|| {
                    PoolError::NotConfigured(
                        "OXID_POSTGRES_URL is not configured on this daemon".to_owned(),
                    )
                })?;
                Ok(crate::adapter::postgres_pool::database_url(
                    admin_url,
                    resource_name,
                )?)
            }
            PoolKind::Redis => {
                let base = self.redis_url.as_deref().ok_or_else(|| {
                    PoolError::NotConfigured(
                        "OXID_REDIS_URL is not configured on this daemon".to_owned(),
                    )
                })?;
                Ok(format!("{}/{resource_name}", base.trim_end_matches('/')))
            }
        }
    }

    /// Releases every resource this branch leased (drops the Postgres
    /// database, frees the Redis index), called when its environment is
    /// destroyed — manually or by the GC sweep.
    async fn release_dependencies(
        &self,
        project_id: ProjectId,
        branch: &BranchName,
    ) -> Result<(), CpError> {
        for (kind, resource_name) in self.store.take_resource_leases(project_id, branch).await? {
            if kind == PoolKind::Postgres
                && let Some(admin_url) = self.postgres_url.as_deref()
            {
                PostgresPool
                    .drop_database(admin_url, &resource_name)
                    .await?;
            }
        }
        Ok(())
    }

    /// Suspends an environment (scale-to-zero).
    ///
    /// # Errors
    /// Returns [`CpError`] on missing records or Docker failures.
    #[tracing::instrument(skip(self), fields(%environment_id))]
    pub async fn pause(&self, environment_id: EnvironmentId) -> Result<(), CpError> {
        let _guard = self.lifecycle_lock.lock().await;
        let mut env = self.ensure_environment(environment_id).await?;
        let project = ProjectStore::get(&self.store, env.project_id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("project `{}`", env.project_id)))?;
        self.oci
            .pause(&resolved_container_name(&project, &env))
            .await?;
        if self.docker_network.is_none() {
            self.proxy
                .mark_unavailable(env.project_id, &env.branch.name)
                .await;
        }

        let now = OffsetDateTime::now_utc();
        env.transition(StateTransition::IdleTimeout, now)
            .map_err(|e| state_err(&e))?;
        EnvironmentStore::update(&self.store, &env).await?;
        tracing::info!(%environment_id, "environment paused");
        Ok(())
    }

    /// Wakes a suspended environment.
    ///
    /// `Paused` containers are still alive in memory (`docker unpause`);
    /// `Hibernating` ones were fully `stop`ped and must be `start`ed instead.
    ///
    /// # Errors
    /// Returns [`CpError`] on missing records or Docker failures.
    #[tracing::instrument(skip(self), fields(%environment_id))]
    pub async fn wake(&self, environment_id: EnvironmentId) -> Result<(), CpError> {
        self.wake_env(environment_id).await
    }

    /// Wakes the environment routed at `url` (matched against the `Host`
    /// header Traefik forwards). Used by the wake-on-request endpoint
    /// (SPEC.md §3.2). Returns `None` silently when no environment owns
    /// `url`, since Traefik may forward hosts Oxid does not manage.
    ///
    /// # Errors
    /// Returns [`CpError`] on persistence or Docker failures.
    pub async fn wake_by_url(&self, url: &str) -> Result<Option<Environment>, CpError> {
        let Some(env) = self.store.find_by_url(url).await? else {
            return Ok(None);
        };
        self.wake_env(env.id).await?;
        Ok(EnvironmentStore::get(&self.store, env.id).await?)
    }

    /// Refreshes `last_accessed_at` for the environment routed at `url`
    /// without changing its state. Backs the heartbeat endpoint a Traefik
    /// `forwardAuth` middleware calls on every request to a `Running`
    /// environment (SPEC.md §3.2 traffic monitor). No-ops silently when no
    /// environment owns `url`.
    ///
    /// # Errors
    /// Returns [`CpError`] on persistence failures.
    pub async fn touch_by_url(&self, url: &str) -> Result<(), CpError> {
        let Some(mut env) = self.store.find_by_url(url).await? else {
            return Ok(());
        };
        let now = OffsetDateTime::now_utc();
        // Touching is best-effort bookkeeping; a Destroyed/terminal state
        // simply can't be touched and that's fine to ignore.
        let _ = env.touch(now);
        EnvironmentStore::update(&self.store, &env).await?;
        Ok(())
    }

    async fn wake_env(&self, environment_id: EnvironmentId) -> Result<(), CpError> {
        let _guard = self.lifecycle_lock.lock().await;
        let mut env = self.ensure_environment(environment_id).await?;
        let project = ProjectStore::get(&self.store, env.project_id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("project `{}`", env.project_id)))?;
        let name = resolved_container_name(&project, &env);
        match env.state {
            EnvironmentState::Hibernating => self.oci.start(&name).await?,
            _ => self.oci.unpause(&name).await?,
        }

        // Backfills `host_port` for an environment that predates dynamic
        // port assignment (deployed by an older Oxid build, so this column
        // was never populated) — otherwise it stays wrong forever, since
        // waking only starts/unpauses the *existing* container instead of
        // recreating it through `run()`, which is the only other place that
        // learns this. Best-effort: `network.is_some()` (Traefik) or a
        // lookup failure just leaves it as it was.
        if env.host_port.is_none() && self.docker_network.is_none() {
            env.host_port = self
                .oci
                .published_port(&name, project.config.port)
                .await
                .unwrap_or(None);
        }

        // Repoints the branch's stable proxy back at this container now
        // that it's alive again — without this, a woken environment stays
        // unreachable through its public address (still `mark_unavailable`d
        // from the pause that put it to sleep) even though the container
        // itself is running again.
        if self.docker_network.is_none()
            && let Some(port) = env.host_port
        {
            let public_port = self
                .proxy
                .ensure(env.project_id, &env.branch.name, env.public_port)
                .await?;
            env.public_port = Some(public_port);
            self.proxy
                .set_target(env.project_id, &env.branch.name, port)
                .await;
        }

        let now = OffsetDateTime::now_utc();
        env.transition(StateTransition::Woken, now)
            .map_err(|e| state_err(&e))?;
        EnvironmentStore::update(&self.store, &env).await?;
        tracing::info!(%environment_id, "environment woken");
        Ok(())
    }

    /// Permanently destroys an environment (`oxid down`): stops and removes
    /// its container and image, then transitions it to `Destroyed`.
    ///
    /// Branch-scoped secrets survive by default — a recurring feature
    /// branch's config (DB passwords, API keys) shouldn't vanish just
    /// because the environment idled out and got TTL-destroyed. Pass
    /// `purge_secrets = true` (`oxid down --purge-secrets`) to explicitly
    /// clear them too.
    ///
    /// # Errors
    /// Returns [`CpError`] on missing records or Docker failures.
    #[tracing::instrument(skip(self), fields(%environment_id, purge_secrets))]
    pub async fn destroy(
        &self,
        environment_id: EnvironmentId,
        purge_secrets: bool,
    ) -> Result<(), CpError> {
        self.destroy_with_operator(environment_id, purge_secrets, None)
            .await
    }

    /// Identical to [`Self::destroy`], attributing the resulting audit
    /// event to `operator`.
    ///
    /// # Errors
    /// Same as [`Self::destroy`].
    #[tracing::instrument(skip(self, operator), fields(%environment_id, purge_secrets, ?operator))]
    pub async fn destroy_with_operator(
        &self,
        environment_id: EnvironmentId,
        purge_secrets: bool,
        operator: Option<String>,
    ) -> Result<(), CpError> {
        let _guard = self.lifecycle_lock.lock().await;
        let mut env = self.ensure_environment(environment_id).await?;
        let project = ProjectStore::get(&self.store, env.project_id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("project `{}`", env.project_id)))?;
        let name = resolved_container_name(&project, &env);
        self.oci.stop(&name).await?;
        self.oci.remove(&name).await?;
        // Best-effort: an image that never finished building (a deploy that
        // failed at the `build` step) simply won't exist yet.
        match self
            .oci
            .remove_image(&image_name(&project, &env.branch.name))
            .await
        {
            Ok(()) | Err(OciError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }
        if self.docker_network.is_none() {
            self.proxy.remove(env.project_id, &env.branch.name).await;
        }

        if purge_secrets {
            self.purge_branch_secrets(env.project_id, &env.branch.name)
                .await?;
        }
        self.release_dependencies(env.project_id, &env.branch.name)
            .await?;

        let now = OffsetDateTime::now_utc();
        env.transition(StateTransition::Destroy, now)
            .map_err(|e| state_err(&e))?;
        EnvironmentStore::update(&self.store, &env).await?;
        self.store
            .record(
                &AuditEvent::with_operator(
                    u64::try_from(now.unix_timestamp()).unwrap_or_default(),
                    env.id,
                    StateTransition::Destroy,
                    None,
                    now,
                    operator,
                )
                .with_request_id(current_request_id()),
            )
            .await?;
        tracing::info!(%environment_id, "environment destroyed");
        Ok(())
    }

    /// Deletes every `branch`-scoped secret for `branch` (used by
    /// `destroy(.., purge_secrets: true)`). Global and project-scope
    /// secrets are untouched — this only clears config specific to this
    /// one branch.
    async fn purge_branch_secrets(
        &self,
        project_id: ProjectId,
        branch: &BranchName,
    ) -> Result<(), CpError> {
        let secrets =
            SecretStore::list_secrets(&self.store, Some(project_id), Some(branch)).await?;
        for (name, scope) in secrets {
            if scope == EnvVarScope::Branch {
                SecretStore::delete_secret(&self.store, Some(project_id), Some(branch), &name)
                    .await?;
            }
        }
        Ok(())
    }

    /// Finds the current environment for `branch` within a project, if any.
    /// A branch can have multiple historical rows (each `deploy` call
    /// creates a new one), so this prefers the most recent *live*
    /// (non-`Destroyed`) row over a merely higher-id one — a redeploy that
    /// zero-downtime-cuts-over successfully leaves exactly one live row as
    /// the highest id, but a *failed* redeploy leaves a higher-id
    /// `Destroyed` row sitting on top of a still-`Running` older one, which
    /// would otherwise "hide" it from callers that need to know whether the
    /// branch is actually still live (e.g. the webhook branch-deletion
    /// handler). Only falls back to the highest-id row overall (which will
    /// be `Destroyed`) when nothing is live at all.
    ///
    /// # Errors
    /// Returns [`CpError::NotFound`] if the project does not exist.
    pub async fn find_environment_by_branch(
        &self,
        project_id: ProjectId,
        branch: &BranchName,
    ) -> Result<Option<Environment>, CpError> {
        self.ensure_project(project_id).await?;
        let envs: Vec<Environment> = self
            .store
            .list_by_project(project_id)
            .await?
            .into_iter()
            .filter(|e| &e.branch.name == branch)
            .collect();
        if let Some(live) = envs
            .iter()
            .filter(|e| e.state != EnvironmentState::Destroyed)
            .max_by_key(|e| e.id.0)
        {
            return Ok(Some(live.clone()));
        }
        Ok(envs.into_iter().max_by_key(|e| e.id.0))
    }

    /// Returns the logs of an environment's container.
    ///
    /// # Errors
    /// Returns [`CpError`] on missing records or Docker failures.
    pub async fn logs(&self, environment_id: EnvironmentId) -> Result<String, CpError> {
        let env = self.ensure_environment(environment_id).await?;
        let project = ProjectStore::get(&self.store, env.project_id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("project `{}`", env.project_id)))?;
        Ok(self
            .oci
            .logs(&resolved_container_name(&project, &env))
            .await?)
    }

    /// Follows an environment's container logs live, yielding new lines as
    /// they're written (SPEC.md §5's SSE `/logs/stream` endpoint).
    ///
    /// # Errors
    /// Returns [`CpError`] on missing records or Docker failures.
    pub async fn stream_logs(&self, environment_id: EnvironmentId) -> Result<LogStream, CpError> {
        let env = self.ensure_environment(environment_id).await?;
        let project = ProjectStore::get(&self.store, env.project_id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("project `{}`", env.project_id)))?;
        Ok(self
            .oci
            .stream_logs(&resolved_container_name(&project, &env))
            .await?)
    }

    /// Mints a new named API token, returning its id and the raw token —
    /// the only time the raw value is ever available; only its `SHA-256`
    /// hash is persisted (SPEC.md's "lightweight multi-user" model: named
    /// tokens on top of the single `OXID_API_TOKEN` master credential,
    /// giving the audit trail a real operator identity instead of an
    /// anonymous shared secret).
    ///
    /// # Errors
    /// Returns [`CpError`] on storage failure.
    pub async fn create_operator_token(&self, name: &str) -> Result<(u64, String), CpError> {
        let mut raw = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut raw);
        let raw_token = hex::encode(raw);
        let id = self
            .store
            .create_api_token(name, &hash_token(&raw_token))
            .await?;
        Ok((id, raw_token))
    }

    /// Resolves a bearer token to its operator name, if it matches a live
    /// (non-revoked) named token.
    ///
    /// # Errors
    /// Returns [`CpError`] on storage failure.
    pub async fn find_operator_by_token(&self, raw_token: &str) -> Result<Option<String>, CpError> {
        Ok(self
            .store
            .find_operator_by_token_hash(&hash_token(raw_token))
            .await?)
    }

    /// Lists every named token (revoked ones included), newest first.
    ///
    /// # Errors
    /// Returns [`CpError`] on storage failure.
    pub async fn list_operator_tokens(&self) -> Result<Vec<ApiTokenSummary>, CpError> {
        Ok(self.store.list_api_tokens().await?)
    }

    /// Revokes a named token by id.
    ///
    /// # Errors
    /// [`CpError::NotFound`] if no token with that id exists.
    pub async fn revoke_operator_token(&self, id: u64) -> Result<(), CpError> {
        Ok(self.store.revoke_api_token(id).await?)
    }

    /// Re-encrypts every secret under `new_key` and swaps it in atomically
    /// (see [`SqliteStore::rotate_master_key`]) — no restart needed. The
    /// caller is still responsible for persisting `new_key` to
    /// `secret.key` (see `api.rs`'s `rotate_key` handler), since only it
    /// knows the data directory.
    ///
    /// # Errors
    /// Returns [`CpError`] on storage or crypto failure.
    pub async fn rotate_master_key(&self, new_key: [u8; 32]) -> Result<(), CpError> {
        self.store
            .rotate_master_key(crate::adapter::crypto::Cipher::from_key(new_key))
            .await
            .map_err(|e| CpError::Store(RepositoryError::Storage(e.to_string())))
    }

    /// Writes a consistent database snapshot to `dest` (see
    /// [`SqliteStore::backup_to`]). Backs `GET /api/v1/backup`.
    ///
    /// # Errors
    /// Returns [`CpError`] on storage failure.
    pub async fn backup_database(&self, dest: &std::path::Path) -> Result<(), CpError> {
        self.store
            .backup_to(dest)
            .await
            .map_err(|e| CpError::Store(RepositoryError::Storage(e.to_string())))
    }

    /// Returns the most recent audit events across every project, newest
    /// first — an operator-facing view of `AuditStore`, which until now was
    /// write-only (recorded on every deploy/pause/wake/destroy but never
    /// exposed over the API). `filter` narrows by project/branch/time
    /// range/transition kind and caps the page size — see [`AuditFilter`].
    ///
    /// # Errors
    /// Returns [`CpError`] on storage failure.
    pub async fn recent_audit_events(
        &self,
        filter: &AuditFilter,
    ) -> Result<Vec<AuditEvent>, CpError> {
        Ok(AuditStore::list_recent(&self.store, filter).await?)
    }

    /// Lists every deploy currently waiting for host capacity, oldest
    /// (highest-priority) first — see [`Self::deploy_or_queue`] and
    /// [`Self::retry_queued_deploys`].
    ///
    /// # Errors
    /// Returns [`CpError`] on persistence failures.
    pub async fn list_deploy_queue(
        &self,
    ) -> Result<Vec<crate::adapter::store::QueuedDeploy>, CpError> {
        Ok(self.store.list_deploy_queue().await?)
    }

    /// Aggregate counts + host capacity for the web dashboard's overview —
    /// one call instead of the client fetching every project's environments
    /// just to total them up.
    ///
    /// # Errors
    /// Returns [`CpError`] on storage or `docker info` failures.
    pub async fn node_stats(&self) -> Result<NodeStats, CpError> {
        let projects = self.store.list().await?;
        let mut stats = NodeStats {
            projects: projects.len() as u64,
            ..NodeStats::default()
        };
        for env in self.store.list_all_environments().await? {
            match env.state {
                EnvironmentState::Running => stats.environments_running += 1,
                EnvironmentState::Paused => stats.environments_paused += 1,
                EnvironmentState::Building => stats.environments_building += 1,
                EnvironmentState::Hibernating => stats.environments_hibernating += 1,
                EnvironmentState::Destroyed => stats.environments_destroyed += 1,
            }
        }
        stats.queue_length = self.store.list_deploy_queue().await?.len() as u64;
        let host = self.oci.host_capacity().await?;
        stats.host_total_memory_bytes = host.total_memory_bytes;
        stats.host_cpu_count = host.cpu_count;
        stats.traefik_enabled = self.docker_network.is_some();
        Ok(stats)
    }

    /// Returns an environment's full audit history, oldest first. `filter`'s
    /// `since`/`until`/`kind` narrow it further; its `project_id`/`branch`/
    /// `limit` are ignored (see [`AuditFilter`] and
    /// `AuditStore::list_by_environment`'s doc comment).
    ///
    /// # Errors
    /// Returns [`CpError::NotFound`] if the environment doesn't exist, plus
    /// anything storage can fail with.
    pub async fn audit_events_for(
        &self,
        environment_id: EnvironmentId,
        filter: &AuditFilter,
    ) -> Result<Vec<AuditEvent>, CpError> {
        self.ensure_environment(environment_id).await?;
        Ok(AuditStore::list_by_environment(&self.store, environment_id, filter).await?)
    }

    /// Stores or replaces a secret at the given scope
    /// (`Global` when `project_id` is `None`).
    ///
    /// # Errors
    /// Returns [`CpError`] on persistence or encryption failures.
    pub async fn set_secret(
        &self,
        project_id: Option<ProjectId>,
        branch: Option<&BranchName>,
        name: &str,
        scope: EnvVarScope,
        value: &str,
    ) -> Result<(), CpError> {
        Ok(SecretStore::set_secret(
            &self.store,
            project_id,
            branch,
            name,
            scope,
            &SecretValue::new(value),
        )
        .await?)
    }

    /// Lists secret names and scopes for a context (values are never exposed).
    ///
    /// # Errors
    /// Returns [`CpError`] on persistence failures.
    pub async fn list_secrets(
        &self,
        project_id: Option<ProjectId>,
        branch: Option<&BranchName>,
    ) -> Result<Vec<(String, EnvVarScope)>, CpError> {
        Ok(SecretStore::list_secrets(&self.store, project_id, branch).await?)
    }

    /// Deletes a secret from a scope.
    ///
    /// # Errors
    /// Returns [`CpError::NotFound`] if the secret does not exist.
    pub async fn delete_secret(
        &self,
        project_id: Option<ProjectId>,
        branch: Option<&BranchName>,
        name: &str,
    ) -> Result<(), CpError> {
        Ok(SecretStore::delete_secret(&self.store, project_id, branch, name).await?)
    }

    /// Runs one garbage-collection pass over every environment: evaluates
    /// each against its project's idle/TTL policy (SPEC.md §3.2) and applies
    /// the resulting pause/hibernate/destroy action.
    ///
    /// Every rule here — `pause_after`, the hibernate multiplier, and
    /// `destroy_after` — is measured from `last_accessed_at`, which only
    /// ever advances via Traefik's `forwardAuth` heartbeat calling
    /// [`Self::touch_by_url`] on real traffic. Without `OXID_DOCKER_NETWORK`
    /// (direct-publish mode), nothing ever calls that — Oxid has no way to
    /// observe traffic hitting a directly-published port at all — so
    /// `last_accessed_at` is frozen at creation time forever. Found live:
    /// a direct-mode environment got auto-paused *immediately* after being
    /// woken, because by every idle metric available it looked exactly as
    /// idle as the moment it was created, despite serving real requests the
    /// whole time. Sweeping on data Oxid knows is meaningless would
    /// eventually not just re-pause but permanently *destroy* a
    /// perfectly-live environment once enough wall-clock time passed — so
    /// the whole pass is skipped in this mode instead of acting on it
    /// anyway; `pause`/`wake`/`destroy` remain available manually (CLI/API/
    /// dashboard) either way.
    ///
    /// A failure on one environment (e.g. a stuck `Building` environment
    /// that cannot legally transition yet) is recorded in
    /// [`GcSummary::errors`] rather than aborting the whole sweep.
    ///
    /// # Errors
    /// Returns [`CpError`] only if listing environments or projects fails.
    #[tracing::instrument(skip(self, now))]
    pub async fn sweep(&self, now: OffsetDateTime) -> Result<GcSummary, CpError> {
        let mut summary = GcSummary::default();
        if self.docker_network.is_none() {
            return Ok(summary);
        }
        let mut projects: std::collections::HashMap<ProjectId, Project> =
            std::collections::HashMap::new();

        for env in self.store.list_all_environments().await? {
            let project = match projects.get(&env.project_id) {
                Some(project) => project.clone(),
                None => match ProjectStore::get(&self.store, env.project_id).await? {
                    Some(project) => {
                        projects.insert(env.project_id, project.clone());
                        project
                    }
                    // Orphaned environment (project deleted underneath it); skip.
                    None => continue,
                },
            };

            let action = gc::evaluate(&env, &project, now);
            if action == GcAction::Keep {
                continue;
            }

            match self.apply_gc_action(env.id, &project, action, now).await {
                Ok(()) => match action {
                    GcAction::Pause => summary.paused += 1,
                    GcAction::Hibernate => summary.hibernated += 1,
                    GcAction::Destroy => summary.destroyed += 1,
                    GcAction::Keep => unreachable!("Keep is filtered out above"),
                },
                Err(err) => summary.errors.push((env.id, err.to_string())),
            }
        }

        Ok(summary)
    }

    /// Reconciles the database's belief about each live environment
    /// against Docker's actual state — meant to be called once at daemon
    /// startup, since the daemon can be down for a while (crash, restart,
    /// a host reboot) during which reality drifts from whatever was last
    /// recorded:
    /// - the container is missing entirely (removed while the daemon was
    ///   down) → the environment is marked `Destroyed`, since there's
    ///   nothing left to recover.
    /// - the database says `Paused` but the container is actually
    ///   `Running` → a paused container doesn't survive a host reboot as
    ///   paused (the cgroup freezer state doesn't persist, so
    ///   `unless-stopped` brings it back fully running); it's re-paused
    ///   to honor the original intent — don't run what wasn't supposed to
    ///   be running.
    /// - the database says `Running` but the container is `Stopped` → try
    ///   to start it back up (the restart policy should normally have
    ///   already done this once Docker itself came back, but this covers
    ///   the case where it hasn't caught up yet, or gave up); if that
    ///   fails too, mark it `Destroyed` rather than leaving a permanently
    ///   wrong "Running" row behind.
    ///
    /// A failure reconciling one environment doesn't abort the pass —
    /// errors are collected and returned, matching [`Self::sweep`]'s
    /// "one bad apple doesn't block the rest" behavior.
    ///
    /// # Errors
    /// Returns [`CpError`] only if listing environments/projects fails;
    /// per-environment reconciliation failures are returned in the `Vec`.
    pub async fn reconcile_startup_state(&self) -> Result<Vec<(EnvironmentId, String)>, CpError> {
        let mut errors = Vec::new();
        let mut projects: std::collections::HashMap<ProjectId, Project> =
            std::collections::HashMap::new();

        for mut env in self.store.list_all_environments().await? {
            if matches!(
                env.state,
                EnvironmentState::Destroyed | EnvironmentState::Building
            ) {
                continue;
            }
            let project = match projects.get(&env.project_id) {
                Some(project) => project.clone(),
                None => match ProjectStore::get(&self.store, env.project_id).await? {
                    Some(project) => {
                        projects.insert(env.project_id, project.clone());
                        project
                    }
                    None => continue,
                },
            };
            let name = resolved_container_name(&project, &env);
            let status = match self.oci.container_status(&name).await {
                Ok(status) => status,
                Err(e) => {
                    errors.push((env.id, e.to_string()));
                    continue;
                }
            };

            // Same opportunistic backfill as `wake_env`: an environment
            // deployed before dynamic port assignment existed never got its
            // `host_port` recorded, and nothing else revisits it — a daemon
            // restart is a free chance to fix that without waiting for a
            // redeploy.
            if env.host_port.is_none()
                && self.docker_network.is_none()
                && status != ContainerStatus::Missing
                && let Ok(Some(port)) = self.oci.published_port(&name, project.config.port).await
            {
                env.host_port = Some(port);
                let _ = EnvironmentStore::update(&self.store, &env).await;
            }

            // The proxy registry (see `service/proxy.rs`) lives entirely in
            // memory, so a daemon restart loses every branch's stable
            // public address unless it's rebuilt here — reusing the
            // persisted `public_port` so it comes back on the exact same
            // port whenever possible instead of quietly reassigning a new
            // one out from under anyone who bookmarked it.
            if self.docker_network.is_none()
                && matches!(
                    env.state,
                    EnvironmentState::Running | EnvironmentState::Paused
                )
                && let Ok(public_port) = self
                    .proxy
                    .ensure(env.project_id, &env.branch.name, env.public_port)
                    .await
            {
                if env.public_port != Some(public_port) {
                    env.public_port = Some(public_port);
                    let _ = EnvironmentStore::update(&self.store, &env).await;
                }
                if env.state == EnvironmentState::Running
                    && let Some(port) = env.host_port
                {
                    self.proxy
                        .set_target(env.project_id, &env.branch.name, port)
                        .await;
                }
            }

            let outcome = match (env.state, status) {
                (
                    EnvironmentState::Running | EnvironmentState::Paused,
                    ContainerStatus::Missing,
                ) => self.mark_destroyed(&mut env).await,
                (EnvironmentState::Paused, ContainerStatus::Running) => {
                    self.oci.pause(&name).await.map_err(CpError::from)
                }
                (EnvironmentState::Running, ContainerStatus::Stopped) => {
                    match self.oci.start(&name).await {
                        Ok(()) => Ok(()),
                        Err(_) => self.mark_destroyed(&mut env).await,
                    }
                }
                // Already consistent, or a benign drift not worth
                // correcting (e.g. `Hibernating` found `Running` because
                // someone manually `docker start`ed it).
                _ => Ok(()),
            };
            if let Err(e) = outcome {
                errors.push((env.id, e.to_string()));
            }
        }
        Ok(errors)
    }

    /// Transitions `env` to `Destroyed` and persists it — the reconciler's
    /// fallback when a container can't be recovered.
    async fn mark_destroyed(&self, env: &mut Environment) -> Result<(), CpError> {
        let now = OffsetDateTime::now_utc();
        if env.transition(StateTransition::Destroy, now).is_ok() {
            EnvironmentStore::update(&self.store, env).await?;
        }
        Ok(())
    }

    async fn apply_gc_action(
        &self,
        env_id: EnvironmentId,
        project: &Project,
        action: GcAction,
        now: OffsetDateTime,
    ) -> Result<(), CpError> {
        // Re-fetch under the lock rather than trusting the snapshot `sweep`
        // read at the top of its loop: without this, a concurrent manual
        // pause/wake/destroy on the same environment between that snapshot
        // and this action being applied would have its change silently
        // clobbered by `store.update` writing back the GC's stale copy.
        let _guard = self.lifecycle_lock.lock().await;
        let mut env = self.ensure_environment(env_id).await?;
        let transition = action
            .transition()
            .expect("Keep is filtered out before calling apply_gc_action");
        let name = resolved_container_name(project, &env);

        match action {
            GcAction::Pause => self.oci.pause(&name).await?,
            GcAction::Hibernate | GcAction::Destroy => self.oci.stop(&name).await?,
            GcAction::Keep => unreachable!("Keep is filtered out before calling apply_gc_action"),
        }
        if self.docker_network.is_none() {
            if action == GcAction::Destroy {
                self.proxy.remove(env.project_id, &env.branch.name).await;
            } else {
                self.proxy
                    .mark_unavailable(env.project_id, &env.branch.name)
                    .await;
            }
        }
        if action == GcAction::Destroy {
            self.oci.remove(&name).await?;
            match self
                .oci
                .remove_image(&image_name(project, &env.branch.name))
                .await
            {
                Ok(()) | Err(OciError::NotFound(_)) => {}
                Err(e) => return Err(e.into()),
            }
            self.release_dependencies(env.project_id, &env.branch.name)
                .await?;
        }

        env.transition(transition, now).map_err(|e| state_err(&e))?;
        EnvironmentStore::update(&self.store, &env).await?;
        self.store
            .record(&AuditEvent::new(
                u64::try_from(now.unix_timestamp()).unwrap_or_default(),
                env.id,
                transition,
                None,
                now,
            ))
            .await?;
        Ok(())
    }

    /// Builds the Traefik labels that route `url` to `name` on
    /// `container_port`, plus the `forwardAuth` heartbeat middleware that
    /// keeps `last_accessed_at` fresh while the environment is `Running`
    /// (SPEC.md §3.2). Empty when Traefik integration is disabled
    /// (`docker_network` unset), leaving `deploy` to fall back to publishing
    /// `host_port` for direct local access.
    ///
    /// This only labels the environment's own container. Wiring the
    /// wake-on-request side (Traefik's `errors` middleware) additionally
    /// requires a globally-defined `oxid-wake` service pointing at this
    /// daemon's `/api/v1/wake` — since that's shared across every branch of
    /// every project, it belongs on the daemon's *own* container/compose
    /// entry, not here. Add these labels there:
    ///   traefik.http.services.oxid-wake.loadbalancer.server.port=8080
    ///   traefik.http.middlewares.<router>-errors.errors.status=502-504
    ///   traefik.http.middlewares.<router>-errors.errors.service=oxid-wake
    ///   traefik.http.middlewares.<router>-errors.errors.query=/api/v1/wake
    fn traefik_labels(
        &self,
        name: &str,
        url: &str,
        container_port: u16,
    ) -> BTreeMap<String, String> {
        let Some(network) = &self.docker_network else {
            return BTreeMap::new();
        };
        let heartbeat = format!("{name}-heartbeat");
        BTreeMap::from([
            ("traefik.enable".to_owned(), "true".to_owned()),
            ("traefik.docker.network".to_owned(), network.clone()),
            (
                format!("traefik.http.routers.{name}.rule"),
                format!("Host(`{url}`)"),
            ),
            (
                format!("traefik.http.routers.{name}.entrypoints"),
                "web".to_owned(),
            ),
            (
                format!("traefik.http.routers.{name}.middlewares"),
                heartbeat.clone(),
            ),
            (
                format!("traefik.http.services.{name}.loadbalancer.server.port"),
                container_port.to_string(),
            ),
            (
                format!("traefik.http.middlewares.{heartbeat}.forwardauth.address"),
                format!("{}/api/v1/heartbeat", self.daemon_url),
            ),
        ])
    }

    async fn find_project_by_repo(
        &self,
        repo_url: &RepoUrl,
    ) -> Result<Option<Project>, RepositoryError> {
        for project in self.store.list().await? {
            if &project.repo_url == repo_url {
                return Ok(Some(project));
            }
        }
        Ok(None)
    }

    async fn ensure_project(&self, project_id: ProjectId) -> Result<Project, CpError> {
        ProjectStore::get(&self.store, project_id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("project `{project_id}`")))
    }

    /// Decides whether a deploy of `project` can proceed immediately given
    /// the host's currently reported capacity (`docker info`, via
    /// [`ContainerPort::host_capacity`]) and every other environment
    /// currently holding memory (`Running` or `Paused` — a paused container
    /// still reserves its memory limit in Docker's accounting, it just isn't
    /// spending CPU). Disabled (`Admission::Fits` unconditionally) unless
    /// [`Self::with_admission_control`] set a `reserved_memory_mb`, and for
    /// any project whose resolved memory request is unbounded (no
    /// `[build].memory_limit_mb` and no daemon default) — there's nothing to
    /// gate against without a number.
    ///
    /// # Errors
    /// [`CpError::InsufficientCapacity`] if the request alone exceeds total
    /// usable capacity (queuing it would never help), plus whatever
    /// [`ContainerPort::host_capacity`] or the store can fail with.
    async fn check_admission(&self, project: &Project) -> Result<Admission, CpError> {
        let Some(reserved_mb) = self.reserved_memory_mb else {
            return Ok(Admission::Fits);
        };
        let Some(request_mb) = project
            .config
            .build
            .memory_limit_mb
            .or(self.default_memory_limit_mb)
        else {
            return Ok(Admission::Fits);
        };

        let host = self.oci.host_capacity().await?;
        let total_mb = host.total_memory_bytes / 1_048_576;
        let usable_mb = total_mb.saturating_sub(reserved_mb);

        if request_mb > usable_mb {
            return Err(CpError::InsufficientCapacity(format!(
                "project `{}` requests {request_mb}MB but the host only has \
                 {usable_mb}MB usable ({total_mb}MB total minus {reserved_mb}MB reserved)",
                project.name
            )));
        }

        let mut committed_mb: u64 = 0;
        for state in [EnvironmentState::Running, EnvironmentState::Paused] {
            for env in self.store.list_by_state(state).await? {
                let Some(env_project) = ProjectStore::get(&self.store, env.project_id).await?
                else {
                    continue;
                };
                committed_mb += env_project
                    .config
                    .build
                    .memory_limit_mb
                    .or(self.default_memory_limit_mb)
                    .unwrap_or(0);
            }
        }

        if committed_mb + request_mb > usable_mb {
            Ok(Admission::Queue)
        } else {
            Ok(Admission::Fits)
        }
    }

    async fn ensure_environment(
        &self,
        environment_id: EnvironmentId,
    ) -> Result<Environment, CpError> {
        EnvironmentStore::get(&self.store, environment_id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("environment `{environment_id}`")))
    }
}

fn state_err(err: &oxid_core::EnvironmentStateError) -> CpError {
    CpError::Domain(DomainError::Invalid(err.to_string()))
}

/// Hashes a raw API token for storage/lookup — tokens are full-entropy
/// random values (not user-chosen passwords), so a plain fast hash is
/// appropriate; no salt/KDF needed since there's nothing to brute-force
/// offline once the hash alone is known.
fn hash_token(raw_token: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(raw_token.as_bytes());
    hex::encode(digest)
}

/// Legacy deterministic container name (one instance per project+branch,
/// ever) — kept only as the fallback for environments deployed before
/// `Environment::container_name` was persisted per-deployment. Every live
/// call site should go through [`resolved_container_name`] instead, so an
/// old row's actual (already-running) container is still found correctly.
fn container_name(project: &Project, branch: &BranchName) -> String {
    format!("oxid-{}-{}", project.name, sanitize_label(branch))
}

/// The container name this environment's own instance actually runs
/// under — its persisted `container_name` if set (every deployment since
/// zero-downtime redeploys shipped sets this, uniquely per environment id,
/// so a redeploy's new instance never collides with the still-running old
/// one), or the legacy project+branch name for anything deployed before.
fn resolved_container_name(project: &Project, env: &Environment) -> String {
    env.container_name
        .clone()
        .unwrap_or_else(|| container_name(project, &env.branch.name))
}

fn image_name(project: &Project, branch: &BranchName) -> String {
    format!("oxid/{}/{}", project.name, sanitize_label(branch))
}

fn sanitize_label(branch: &BranchName) -> String {
    branch
        .to_string()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Sanitizes a project name or branch label into a valid Postgres
/// identifier fragment: lowercase `[a-z0-9_]` only.
fn sanitize_identifier(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// The lowest value in `0..capacity` not present in `used`, or `None` if
/// every slot is taken. Pure and separately tested since `ResourcePool`
/// (oxid-core) tracks *how many* slices are leased, not *which* numeric
/// slot each tenant holds — that assignment is specific to Redis indices,
/// so it lives here instead of being bent onto that more general type.
fn lowest_free_index(used: &std::collections::BTreeSet<u32>, capacity: u32) -> Option<u32> {
    (0..capacity).find(|i| !used.contains(i))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxid_core::HostCapacity;
    use std::sync::{Arc, Mutex};

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[derive(Debug, Clone, Default)]
    struct FakeGit;

    impl GitPort for FakeGit {
        async fn remote_url(&self, repo_dir: &Path) -> Result<RepoUrl, GitError> {
            let _ = repo_dir;
            RepoUrl::parse("https://github.com/org/app.git")
                .map_err(|e| GitError::Failure(e.to_string()))
        }
        async fn ensure_repo(
            &self,
            _url: &RepoUrl,
            _token: Option<&str>,
            cache_dir: &Path,
        ) -> Result<PathBuf, GitError> {
            Ok(cache_dir.join("app"))
        }
        async fn resolve_branch_head(
            &self,
            _repo_dir: &Path,
            branch: &BranchName,
        ) -> Result<oxid_core::CommitRef, GitError> {
            Ok(oxid_core::CommitRef {
                branch: branch.clone(),
                sha: SHA.to_owned(),
            })
        }
        async fn checkout_commit(&self, _repo_dir: &Path, _sha: &str) -> Result<(), GitError> {
            Ok(())
        }
    }

    #[derive(Debug, Clone, Default)]
    struct FakeOci {
        calls: Arc<Mutex<Vec<String>>>,
        /// When > 0, `run` fails and decrements this instead of succeeding.
        fail_run_times: Arc<Mutex<u32>>,
        /// Per-container overrides for `container_status`; anything not
        /// listed here defaults to `Running`.
        container_statuses: Arc<Mutex<std::collections::HashMap<String, ContainerStatus>>>,
        host_capacity: Arc<Mutex<HostCapacity>>,
    }

    impl ContainerPort for FakeOci {
        async fn build(&self, spec: &BuildSpec) -> Result<(), OciError> {
            self.calls.lock().unwrap().push(format!(
                "build:{}:context={}:dockerfile={}",
                spec.image,
                spec.context.display(),
                spec.dockerfile
            ));
            Ok(())
        }
        async fn run(&self, spec: &ContainerSpec) -> Result<Option<u16>, OciError> {
            self.calls.lock().unwrap().push(format!(
                "run:{}:env={:?}:mem={:?}:cpu={:?}",
                spec.name, spec.env, spec.memory_limit_mb, spec.cpu_limit_millicores
            ));
            let mut remaining = self.fail_run_times.lock().unwrap();
            if *remaining > 0 {
                *remaining -= 1;
                return Err(OciError::Failure("simulated transient failure".to_owned()));
            }
            Ok(spec.network.is_none().then_some(65535))
        }
        async fn published_port(
            &self,
            name: &str,
            _container_port: u16,
        ) -> Result<Option<u16>, OciError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("published_port:{name}"));
            Ok(Some(65535))
        }
        async fn start(&self, name: &str) -> Result<(), OciError> {
            self.calls.lock().unwrap().push(format!("start:{name}"));
            Ok(())
        }
        async fn pause(&self, name: &str) -> Result<(), OciError> {
            self.calls.lock().unwrap().push(format!("pause:{name}"));
            Ok(())
        }
        async fn unpause(&self, name: &str) -> Result<(), OciError> {
            self.calls.lock().unwrap().push(format!("unpause:{name}"));
            Ok(())
        }
        async fn stop(&self, name: &str) -> Result<(), OciError> {
            self.calls.lock().unwrap().push(format!("stop:{name}"));
            Ok(())
        }
        async fn remove(&self, name: &str) -> Result<(), OciError> {
            self.calls.lock().unwrap().push(format!("remove:{name}"));
            Ok(())
        }
        async fn remove_image(&self, image: &str) -> Result<(), OciError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("remove_image:{image}"));
            Ok(())
        }
        async fn logs(&self, name: &str) -> Result<String, OciError> {
            self.calls.lock().unwrap().push(format!("logs:{name}"));
            Ok("build log".to_owned())
        }
        async fn stream_logs(&self, name: &str) -> Result<LogStream, OciError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("stream_logs:{name}"));
            Ok(Box::pin(futures_util::stream::iter(vec![Ok(
                "build log".to_owned()
            )])))
        }
        async fn exec(&self, name: &str, command: &str) -> Result<(), OciError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("exec:{name}:{command}"));
            Ok(())
        }
        async fn container_status(&self, name: &str) -> Result<ContainerStatus, OciError> {
            Ok(self
                .container_statuses
                .lock()
                .unwrap()
                .get(name)
                .copied()
                .unwrap_or(ContainerStatus::Running))
        }
        async fn host_capacity(&self) -> Result<HostCapacity, OciError> {
            Ok(*self.host_capacity.lock().unwrap())
        }
    }

    async fn store() -> SqliteStore {
        SqliteStore::open_in_memory().await.unwrap()
    }

    /// A `GitPort` whose `resolve_branch_head` returns a fresh, incrementing
    /// sha on every call — unlike `FakeGit`'s fixed `SHA`, needed to tell
    /// apart successive deploys of the same branch by commit (rollback
    /// tests).
    #[derive(Clone, Default)]
    struct SequentialGit(Arc<std::sync::atomic::AtomicU32>);

    impl GitPort for SequentialGit {
        async fn remote_url(&self, repo_dir: &Path) -> Result<RepoUrl, GitError> {
            let _ = repo_dir;
            RepoUrl::parse("https://github.com/org/app.git")
                .map_err(|e| GitError::Failure(e.to_string()))
        }
        async fn ensure_repo(
            &self,
            _url: &RepoUrl,
            _token: Option<&str>,
            cache_dir: &Path,
        ) -> Result<PathBuf, GitError> {
            Ok(cache_dir.join("app"))
        }
        async fn resolve_branch_head(
            &self,
            _repo_dir: &Path,
            branch: &BranchName,
        ) -> Result<oxid_core::CommitRef, GitError> {
            let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(oxid_core::CommitRef {
                branch: branch.clone(),
                sha: format!("{n:040}"),
            })
        }
        async fn checkout_commit(&self, _repo_dir: &Path, _sha: &str) -> Result<(), GitError> {
            Ok(())
        }
    }

    fn repo_dir_with_config() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("oxid.toml"),
            r#"
[project]
name = "app"

[routing]
base_domain = "app.local.dev"
port = 8080
"#,
        )
        .unwrap();
        dir
    }

    async fn cp(oci: FakeOci) -> ControlPlane<FakeGit, FakeOci> {
        let cache = tempfile::tempdir().unwrap();
        // FakeOci doesn't simulate a real listening socket, so the
        // zero-downtime readiness gate (which does a real TCP connect)
        // would otherwise time out on every deploy — see
        // `ControlPlane::with_readiness_check`'s doc comment.
        ControlPlane::new(store().await, FakeGit, oci, cache.path().to_owned())
            .with_readiness_check(false)
    }

    /// A project declaring one `redis` dependency — deliberately no
    /// `postgres` one, so these tests never need a real Postgres instance;
    /// that path is covered separately by `postgres_pool.rs`'s `#[ignore]`d
    /// integration test.
    fn repo_dir_with_redis_dependency() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("oxid.toml"),
            r#"
[project]
name = "app"

[routing]
base_domain = "app.local.dev"
port = 8080

[dependencies.cache]
type = "redis"
shared_instance = "local-redis"
inject_url_as = "REDIS_URL"
"#,
        )
        .unwrap();
        dir
    }

    #[tokio::test]
    async fn register_and_deploy_happy_path() {
        let repo = repo_dir_with_config();
        let cp = cp(FakeOci::default()).await;

        let project = cp.register_project(repo.path()).await.unwrap();
        assert_eq!(project.name, "app");
        assert_eq!(project.repo_url.as_str(), "https://github.com/org/app.git");

        // Idempotent registration.
        let again = cp.register_project(repo.path()).await.unwrap();
        assert_eq!(again.id, project.id);
        assert_eq!(cp.list_projects().await.unwrap().len(), 1);

        let env = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();
        assert_eq!(env.state, EnvironmentState::Running);
        assert_eq!(env.url, "feature-a.app.local.dev");
        assert_eq!(cp.list_environments(project.id).await.unwrap().len(), 1);
    }

    /// Regression test for a real race found by firing ten concurrent `oxid
    /// up` at a project that had never been registered before: the
    /// check-then-act between `find_project_by_repo` and `ProjectStore::
    /// create` isn't atomic, so every concurrent first-time caller could
    /// pass the "does it exist?" check before any of them committed,
    /// leaving all but one to blow up with a raw `UNIQUE constraint failed`
    /// instead of the idempotent behavior `register_project` documents.
    #[tokio::test]
    async fn concurrent_first_registration_is_idempotent() {
        let repo = repo_dir_with_config();
        let cp = cp(FakeOci::default()).await;

        let handles: Vec<_> = (0..10)
            .map(|_| {
                let cp = cp.clone();
                let path = repo.path().to_owned();
                tokio::spawn(async move { cp.register_project(&path).await })
            })
            .collect();

        let mut ids = std::collections::HashSet::new();
        for handle in handles {
            ids.insert(handle.await.unwrap().unwrap().id);
        }
        assert_eq!(ids.len(), 1, "every call must resolve to the same project");
        assert_eq!(cp.list_projects().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deploy_records_oci_operations() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        let cp = cp(oci.clone()).await;
        let project = cp.register_project(repo.path()).await.unwrap();

        cp.deploy(project.id, BranchName::parse("feature-b").unwrap())
            .await
            .unwrap();

        let calls = oci.calls.lock().unwrap();
        assert!(calls.iter().any(|c| c.starts_with("build:")), "{calls:?}");
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with("run:oxid-app-feature-b")),
            "{calls:?}"
        );
    }

    /// Regression test: `[build].context` was parsed from `oxid.toml` and
    /// persisted, but never actually consulted when building the image —
    /// every build used the whole repo checkout regardless, silently
    /// ignoring a monorepo-style subdirectory context. Found while wiring
    /// `docker-compose.yml` support, whose `build.context`/`build.dockerfile`
    /// pair only makes sense if `dockerfile` is resolved relative to
    /// `context`, not the repo root.
    #[tokio::test]
    async fn deploy_honors_a_non_default_build_context() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("oxid.toml"),
            r#"
[project]
name = "app"

[build]
context = "backend"
dockerfile = "Dockerfile.prod"

[routing]
base_domain = "app.local.dev"
port = 8080
"#,
        )
        .unwrap();
        let oci = FakeOci::default();
        let cp = cp(oci.clone()).await;
        let project = cp.register_project(dir.path()).await.unwrap();

        cp.deploy(project.id, BranchName::parse("main").unwrap())
            .await
            .unwrap();

        let calls = oci.calls.lock().unwrap();
        let build_call = calls
            .iter()
            .find(|c| c.starts_with("build:"))
            .expect("a build call was made");
        // `FakeGit::ensure_repo` always resolves to `<cache_dir>/app`; the
        // context must be that path joined with the configured `backend`
        // subdirectory, and the dockerfile must be resolved relative to it.
        assert!(
            build_call.ends_with("/app/backend:dockerfile=Dockerfile.prod"),
            "{build_call}"
        );
    }

    #[tokio::test]
    async fn deploy_fails_clearly_when_dependency_is_unconfigured() {
        let repo = repo_dir_with_redis_dependency();
        let cp = cp(FakeOci::default()).await; // no `with_resource_pools` call
        let project = cp.register_project(repo.path()).await.unwrap();

        let err = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, CpError::Pool(PoolError::NotConfigured(m)) if m.contains("OXID_REDIS_URL")),
            "{err:?}"
        );
    }

    async fn redis_lease_for(
        cp: &ControlPlane<FakeGit, FakeOci>,
        project_id: ProjectId,
        branch: &str,
    ) -> Option<String> {
        cp.store
            .find_resource_lease(
                project_id,
                &BranchName::parse(branch).unwrap(),
                PoolKind::Redis,
                "local-redis",
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn deploy_injects_a_distinct_redis_index_per_branch_and_reuses_on_redeploy() {
        let repo = repo_dir_with_redis_dependency();
        let cache = tempfile::tempdir().unwrap();
        let cp = ControlPlane::new(
            store().await,
            FakeGit,
            FakeOci::default(),
            cache.path().to_owned(),
        )
        .with_resource_pools(None, Some("redis://cache:6379".to_owned()), 16)
        .with_readiness_check(false);
        let project = cp.register_project(repo.path()).await.unwrap();

        let env_a = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();
        let env_b = cp
            .deploy(project.id, BranchName::parse("feature-b").unwrap())
            .await
            .unwrap();
        assert_ne!(env_a.id, env_b.id);

        let index_a = redis_lease_for(&cp, project.id, "feature-a").await.unwrap();
        let index_b = redis_lease_for(&cp, project.id, "feature-b").await.unwrap();
        assert_ne!(index_a, index_b, "each branch must get its own index");

        // Redeploying feature-a must reuse the same index, not lease a new
        // one (which would eventually exhaust the pool for no reason).
        cp.deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();
        assert_eq!(
            redis_lease_for(&cp, project.id, "feature-a").await,
            Some(index_a)
        );
    }

    #[tokio::test]
    async fn destroy_releases_the_redis_index_for_reuse() {
        let repo = repo_dir_with_redis_dependency();
        let cache = tempfile::tempdir().unwrap();
        let cp = ControlPlane::new(
            store().await,
            FakeGit,
            FakeOci::default(),
            cache.path().to_owned(),
        )
        .with_resource_pools(None, Some("redis://cache:6379".to_owned()), 1)
        .with_readiness_check(false);
        let project = cp.register_project(repo.path()).await.unwrap();

        let env_a = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();

        // Pool capacity is 1: a second branch must fail while feature-a
        // holds the only slot.
        let err = cp
            .deploy(project.id, BranchName::parse("feature-b").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(err, CpError::Pool(PoolError::Failure(_))),
            "{err:?}"
        );

        cp.destroy(env_a.id, false).await.unwrap();

        // Now that feature-a released its slot, feature-b can have it.
        cp.deploy(project.id, BranchName::parse("feature-b").unwrap())
            .await
            .unwrap();
    }

    /// Regression test: redeploying a branch that's already live (e.g. a
    /// webhook firing on a second push) must tear down the previous
    /// container first instead of leaving Docker to reject a duplicate
    /// container name, and must mark the old row Destroyed rather than
    /// leaving two "live-looking" rows around.
    #[tokio::test]
    async fn redeploying_a_live_branch_replaces_the_previous_environment() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        let cp = cp(oci.clone()).await;
        let project = cp.register_project(repo.path()).await.unwrap();

        let first = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();

        let second = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(second.state, EnvironmentState::Running);

        let old = EnvironmentStore::get(&cp.store, first.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(old.state, EnvironmentState::Destroyed);

        {
            // Zero-downtime cutover: the new instance (`-2`) is built and
            // started fully before the previous one (`-1`) is ever removed
            // — the reverse of the old "destroy first, build second" order,
            // which always had a gap where the branch was unreachable.
            let calls = oci.calls.lock().unwrap();
            let run_new = calls
                .iter()
                .position(|c| c == "run:oxid-app-feature-a-2:env={\"OXID_BRANCH\": \"feature-a\", \"OXID_ENV_URL\": \"feature-a.app.local.dev\"}:mem=None:cpu=None")
                .expect("new instance must have been run");
            // The *last* removal of `-1` is the cutover teardown — its
            // *first* occurrence is just the defensive pre-run cleanup its
            // own deploy already did for itself.
            let remove_old = calls
                .iter()
                .rposition(|c| c == "remove:oxid-app-feature-a-1")
                .expect("previous container must eventually be removed");
            assert!(
                run_new < remove_old,
                "previous container must not be removed until the new one is up: {calls:?}"
            );
        }
        // Exactly one live environment remains for the branch.
        let envs = cp.list_environments(project.id).await.unwrap();
        assert_eq!(
            envs.iter()
                .filter(|e| e.state != EnvironmentState::Destroyed)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn rollback_without_to_sha_redeploys_the_immediately_prior_commit() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        let cache = tempfile::tempdir().unwrap();
        let cp = ControlPlane::new(
            store().await,
            SequentialGit::default(),
            oci,
            cache.path().to_owned(),
        )
        .with_readiness_check(false);
        let project = cp.register_project(repo.path()).await.unwrap();
        let branch = BranchName::parse("main").unwrap();

        let first = cp.deploy(project.id, branch.clone()).await.unwrap();
        let second = cp.deploy(project.id, branch.clone()).await.unwrap();
        assert_ne!(first.branch.commit_sha, second.branch.commit_sha);

        let rolled_back = cp.rollback(project.id, branch, None).await.unwrap();
        assert_eq!(rolled_back.branch.commit_sha, first.branch.commit_sha);
        assert_eq!(rolled_back.state, EnvironmentState::Running);
    }

    #[tokio::test]
    async fn rollback_with_explicit_to_sha_uses_that_commit() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        let cache = tempfile::tempdir().unwrap();
        let cp = ControlPlane::new(
            store().await,
            SequentialGit::default(),
            oci,
            cache.path().to_owned(),
        )
        .with_readiness_check(false);
        let project = cp.register_project(repo.path()).await.unwrap();
        let branch = BranchName::parse("main").unwrap();

        let first = cp.deploy(project.id, branch.clone()).await.unwrap();
        cp.deploy(project.id, branch.clone()).await.unwrap();
        cp.deploy(project.id, branch.clone()).await.unwrap();

        let rolled_back = cp
            .rollback(project.id, branch, Some(first.branch.commit_sha.clone()))
            .await
            .unwrap();
        assert_eq!(rolled_back.branch.commit_sha, first.branch.commit_sha);
    }

    #[tokio::test]
    async fn rollback_rejects_a_sha_not_in_the_branchs_history() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        let cache = tempfile::tempdir().unwrap();
        let cp = ControlPlane::new(
            store().await,
            SequentialGit::default(),
            oci,
            cache.path().to_owned(),
        )
        .with_readiness_check(false);
        let project = cp.register_project(repo.path()).await.unwrap();
        let branch = BranchName::parse("main").unwrap();
        cp.deploy(project.id, branch.clone()).await.unwrap();

        let err = cp
            .rollback(project.id, branch, Some("not-a-real-sha".to_owned()))
            .await
            .unwrap_err();
        assert!(matches!(err, CpError::NotFound(_)));
    }

    #[tokio::test]
    async fn rollback_with_no_prior_deploy_is_not_found() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        let cache = tempfile::tempdir().unwrap();
        let cp = ControlPlane::new(
            store().await,
            SequentialGit::default(),
            oci,
            cache.path().to_owned(),
        )
        .with_readiness_check(false);
        let project = cp.register_project(repo.path()).await.unwrap();
        let branch = BranchName::parse("main").unwrap();
        cp.deploy(project.id, branch.clone()).await.unwrap();

        let err = cp.rollback(project.id, branch, None).await.unwrap_err();
        assert!(matches!(err, CpError::NotFound(_)));
    }

    /// Regression test for a real bricking bug: a transient failure in
    /// `run()` (Docker error, bad secret, failing `on_start` hook) happening
    /// *after* the `Environment` row was persisted as `Building` used to
    /// leave it there forever, since `Building` cannot transition to
    /// `Destroy` — every subsequent `oxid up` of that branch failed with
    /// "transition `Destroy` is not allowed from `Building`" instead of
    /// retrying. Found by deploying, having a container-name conflict fail
    /// the `run` step, then deploying again.
    #[tokio::test]
    async fn failed_deploy_does_not_permanently_block_branch() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        *oci.fail_run_times.lock().unwrap() = 1;
        let cp = cp(oci.clone()).await;
        let project = cp.register_project(repo.path()).await.unwrap();

        let err = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap_err();
        assert!(matches!(err, CpError::Oci(_)), "{err:?}");

        let envs = cp.list_environments(project.id).await.unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].state, EnvironmentState::Destroyed);

        // The audit trail must carry the *real* error (e.g. "port already
        // allocated"), not a blank `detail` — found live when a real
        // deploy failure showed up in the dashboard with no way to tell
        // what actually went wrong.
        let events = cp
            .audit_events_for(envs[0].id, &AuditFilter::default())
            .await
            .unwrap();
        let failed = events
            .iter()
            .find(|e| e.kind == StateTransition::BuildFailed)
            .expect("a BuildFailed audit event");
        assert_eq!(failed.detail.as_deref(), Some(err.to_string().as_str()));

        // The retry must succeed instead of hitting "Destroy not allowed
        // from Building".
        let env = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();
        assert_eq!(env.state, EnvironmentState::Running);
    }

    /// The whole point of building new-before-old for a redeploy: if the
    /// new instance never comes up, the previous one — which has been
    /// serving traffic this entire time — must be left running exactly as
    /// it was, not torn down. Explicit user requirement ("siempre
    /// levantando algo para no tener fallas"): a bad push must never take
    /// an already-live branch down with it.
    #[tokio::test]
    async fn failed_redeploy_leaves_the_previous_instance_untouched() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        let cp = cp(oci.clone()).await;
        let project = cp.register_project(repo.path()).await.unwrap();

        let first = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();
        assert_eq!(first.state, EnvironmentState::Running);

        *oci.fail_run_times.lock().unwrap() = 1;
        let err = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap_err();
        assert!(matches!(err, CpError::Oci(_)), "{err:?}");

        // The previous environment must still be exactly as it was.
        let still_live = EnvironmentStore::get(&cp.store, first.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still_live.state, EnvironmentState::Running);

        // Its container must have been removed exactly once — the
        // defensive pre-run cleanup its *own* first deploy already did for
        // itself — and never a second time because of the failed redeploy.
        {
            let calls = oci.calls.lock().unwrap();
            let removes_of_previous = calls
                .iter()
                .filter(|c| *c == "remove:oxid-app-feature-a-1")
                .count();
            assert_eq!(
                removes_of_previous, 1,
                "previous container must survive a failed redeploy untouched: {calls:?}"
            );
        }

        // A subsequent, successful redeploy must still work normally.
        let second = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();
        assert_eq!(second.state, EnvironmentState::Running);
        let old = EnvironmentStore::get(&cp.store, first.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(old.state, EnvironmentState::Destroyed);
    }

    /// Regression test for a real race found by firing ten concurrent `oxid
    /// up` calls at the same brand-new branch: without serializing
    /// `deploy()`, they raced to create their own `Environment` row before
    /// any of them found out only one could win the container name, so the
    /// row left standing (highest id) was not necessarily the one whose
    /// container actually ended up running. `deploy_lock` forces them into a
    /// sequence instead, so exactly one row should end up `Running` — and it
    /// should be the *last* deploy to actually run, not an arbitrary loser.
    #[tokio::test]
    async fn concurrent_deploys_of_the_same_branch_leave_a_consistent_state() {
        let repo = repo_dir_with_config();
        let cp = cp(FakeOci::default()).await;
        let project = cp.register_project(repo.path()).await.unwrap();

        let handles: Vec<_> = (0..10)
            .map(|_| {
                let cp = cp.clone();
                let project_id = project.id;
                tokio::spawn(async move {
                    cp.deploy(project_id, BranchName::parse("feature-a").unwrap())
                        .await
                })
            })
            .collect();

        let mut successes = 0;
        for handle in handles {
            if handle.await.unwrap().is_ok() {
                successes += 1;
            }
        }
        assert_eq!(
            successes, 10,
            "the lock should let every deploy succeed in turn"
        );

        let envs = cp.list_environments(project.id).await.unwrap();
        let running: Vec<_> = envs
            .iter()
            .filter(|e| e.state == EnvironmentState::Running)
            .collect();
        assert_eq!(
            running.len(),
            1,
            "exactly one environment must be left Running: {envs:?}"
        );
        // It must be the most recent row — not a stale one left standing by
        // a race — since each deploy tears down the previous live one.
        let max_id = envs.iter().map(|e| e.id.0).max().unwrap();
        assert_eq!(running[0].id.0, max_id);
    }

    #[tokio::test]
    async fn pause_wake_and_logs() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        let cp = cp(oci.clone()).await;
        let project = cp.register_project(repo.path()).await.unwrap();
        let env = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();

        cp.pause(env.id).await.unwrap();
        let paused = EnvironmentStore::get(&cp.store, env.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(paused.state, EnvironmentState::Paused);

        cp.wake(env.id).await.unwrap();
        let woken = EnvironmentStore::get(&cp.store, env.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(woken.state, EnvironmentState::Running);

        let logs = cp.logs(env.id).await.unwrap();
        assert_eq!(logs, "build log");

        let mut stream = cp.stream_logs(env.id).await.unwrap();
        let first = futures_util::StreamExt::next(&mut stream).await;
        assert_eq!(first, Some(Ok("build log".to_owned())));
    }

    #[tokio::test]
    async fn deploy_applies_daemon_default_resource_limits_when_project_sets_none() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        let cp = cp(oci.clone())
            .await
            .with_resource_defaults(Some(512), Some(1000));
        let project = cp.register_project(repo.path()).await.unwrap();
        cp.deploy(project.id, BranchName::parse("main").unwrap())
            .await
            .unwrap();

        let calls = oci.calls.lock().unwrap();
        assert!(
            calls.iter().any(|c| c.starts_with("run:")
                && c.contains("mem=Some(512)")
                && c.contains("cpu=Some(1000)")),
            "{calls:?}"
        );
    }

    #[tokio::test]
    async fn deploy_lets_a_projects_own_resource_limits_win_over_the_daemon_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("oxid.toml"),
            r#"
[project]
name = "app"

[routing]
base_domain = "app.local.dev"
port = 8080

[build]
memory_limit_mb = 128
cpu_limit_millicores = 250
"#,
        )
        .unwrap();
        let oci = FakeOci::default();
        let cp = cp(oci.clone())
            .await
            .with_resource_defaults(Some(512), Some(1000));
        let project = cp.register_project(dir.path()).await.unwrap();
        cp.deploy(project.id, BranchName::parse("main").unwrap())
            .await
            .unwrap();

        let calls = oci.calls.lock().unwrap();
        assert!(
            calls.iter().any(|c| c.starts_with("run:")
                && c.contains("mem=Some(128)")
                && c.contains("cpu=Some(250)")),
            "{calls:?}"
        );
    }

    #[tokio::test]
    async fn destroy_stops_removes_and_transitions() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        let cp = cp(oci.clone()).await;
        let project = cp.register_project(repo.path()).await.unwrap();
        let env = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();

        cp.destroy(env.id, false).await.unwrap();
        let destroyed = EnvironmentStore::get(&cp.store, env.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(destroyed.state, EnvironmentState::Destroyed);

        let calls = oci.calls.lock().unwrap();
        assert!(calls.iter().any(|c| c.starts_with("stop:")), "{calls:?}");
        assert!(calls.iter().any(|c| c.starts_with("remove:")), "{calls:?}");
        assert!(
            calls.iter().any(|c| c.starts_with("remove_image:")),
            "destroy must also remove the branch's image, not just its container: {calls:?}"
        );
    }

    #[tokio::test]
    async fn destroy_keeps_branch_secrets_by_default() {
        let repo = repo_dir_with_config();
        let cp = cp(FakeOci::default()).await;
        let project = cp.register_project(repo.path()).await.unwrap();
        let branch = BranchName::parse("feature-a").unwrap();
        let env = cp.deploy(project.id, branch.clone()).await.unwrap();

        cp.set_secret(
            Some(project.id),
            Some(&branch),
            "DB_PASS",
            EnvVarScope::Branch,
            "keep-me",
        )
        .await
        .unwrap();

        cp.destroy(env.id, false).await.unwrap();

        let secrets = cp
            .list_secrets(Some(project.id), Some(&branch))
            .await
            .unwrap();
        assert!(
            secrets
                .iter()
                .any(|(n, s)| n == "DB_PASS" && *s == EnvVarScope::Branch),
            "a plain `down` must not delete branch secrets: {secrets:?}"
        );
    }

    #[tokio::test]
    async fn destroy_with_purge_secrets_deletes_only_that_branchs_branch_scope_secrets() {
        let repo = repo_dir_with_config();
        let cp = cp(FakeOci::default()).await;
        let project = cp.register_project(repo.path()).await.unwrap();
        let branch_a = BranchName::parse("feature-a").unwrap();
        let branch_b = BranchName::parse("feature-b").unwrap();
        let env_a = cp.deploy(project.id, branch_a.clone()).await.unwrap();

        cp.set_secret(
            Some(project.id),
            None,
            "SHARED",
            EnvVarScope::Project,
            "project-level",
        )
        .await
        .unwrap();
        cp.set_secret(
            Some(project.id),
            Some(&branch_a),
            "ONLY_A",
            EnvVarScope::Branch,
            "a-secret",
        )
        .await
        .unwrap();
        cp.set_secret(
            Some(project.id),
            Some(&branch_b),
            "ONLY_B",
            EnvVarScope::Branch,
            "b-secret",
        )
        .await
        .unwrap();

        cp.destroy(env_a.id, true).await.unwrap();

        let for_a = cp
            .list_secrets(Some(project.id), Some(&branch_a))
            .await
            .unwrap();
        assert!(
            for_a.iter().all(|(n, _)| n != "ONLY_A"),
            "purge_secrets must delete branch A's own secret: {for_a:?}"
        );
        assert!(
            for_a.iter().any(|(n, _)| n == "SHARED"),
            "purge_secrets must not touch project-scope secrets: {for_a:?}"
        );
        let for_b = cp
            .list_secrets(Some(project.id), Some(&branch_b))
            .await
            .unwrap();
        assert!(
            for_b.iter().any(|(n, _)| n == "ONLY_B"),
            "purge_secrets on branch A must not delete branch B's secret: {for_b:?}"
        );
    }

    #[tokio::test]
    async fn delete_project_destroys_environments_removes_cache_and_row() {
        let repo = repo_dir_with_config();
        let cache = tempfile::tempdir().unwrap();
        let oci = FakeOci::default();
        let cp = ControlPlane::new(store().await, FakeGit, oci.clone(), cache.path().to_owned())
            .with_readiness_check(false);
        let project = cp.register_project(repo.path()).await.unwrap();
        let env = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();
        // Populate the cache dir the way `ensure_repo` would, so deletion has
        // something real to remove.
        let cache_path = cache
            .path()
            .join(crate::adapter::git::cache_dir_name(&project.repo_url));
        std::fs::create_dir_all(&cache_path).unwrap();
        std::fs::write(cache_path.join("marker"), "x").unwrap();

        cp.delete_project(project.id).await.unwrap();

        assert!(!cache_path.exists(), "git cache must be removed");
        assert!(
            ProjectStore::get(&cp.store, project.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            EnvironmentStore::get(&cp.store, env.id)
                .await
                .unwrap()
                .is_none(),
            "cascade must remove the environment row too"
        );
        let calls = oci.calls.lock().unwrap();
        assert!(
            calls.iter().any(|c| c.starts_with("remove:")),
            "project delete must tear down live containers: {calls:?}"
        );
    }

    #[tokio::test]
    async fn delete_project_unknown_fails() {
        let cp = cp(FakeOci::default()).await;
        let err = cp.delete_project(ProjectId(999)).await.unwrap_err();
        assert!(matches!(err, CpError::NotFound(_)));
    }

    #[tokio::test]
    async fn gc_destroy_also_removes_the_image() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        // `sweep` no-ops entirely without Traefik configured (idle detection
        // needs its heartbeat) — exercise that real path here.
        let cp = cp(oci.clone()).await.with_traefik("net", "http://daemon");
        let project = cp.register_project(repo.path()).await.unwrap();
        let env = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();

        let now = OffsetDateTime::now_utc();
        touch_env(&cp, env.clone(), now - time::Duration::days(8)).await;
        let summary = cp.sweep(now).await.unwrap();
        assert_eq!(summary.destroyed, 1, "{:?}", summary.errors);

        let calls = oci.calls.lock().unwrap();
        assert!(
            calls.iter().any(|c| c.starts_with("remove_image:")),
            "{calls:?}"
        );
    }

    #[tokio::test]
    async fn find_environment_by_branch_matches_and_misses() {
        let repo = repo_dir_with_config();
        let cp = cp(FakeOci::default()).await;
        let project = cp.register_project(repo.path()).await.unwrap();
        let env = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();

        let found = cp
            .find_environment_by_branch(project.id, &BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();
        assert_eq!(found.unwrap().id, env.id);

        let missing = cp
            .find_environment_by_branch(project.id, &BranchName::parse("feature-b").unwrap())
            .await
            .unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn wake_by_url_unpauses_paused_and_starts_hibernating() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        let cp = cp(oci.clone()).await;
        let project = cp.register_project(repo.path()).await.unwrap();
        let env = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();

        cp.pause(env.id).await.unwrap();
        let woken = cp.wake_by_url(&env.url).await.unwrap().unwrap();
        assert_eq!(woken.state, EnvironmentState::Running);
        assert!(
            oci.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.starts_with("unpause:")),
            "{:?}",
            oci.calls
        );

        // Force it to Hibernating directly (bypassing the multi-hour sweep
        // needed to get there naturally) to test the `start` branch of wake.
        let mut hibernating = EnvironmentStore::get(&cp.store, env.id)
            .await
            .unwrap()
            .unwrap();
        hibernating
            .transition(StateTransition::IdleTimeout, OffsetDateTime::now_utc())
            .unwrap();
        hibernating
            .transition(StateTransition::DeepSleep, OffsetDateTime::now_utc())
            .unwrap();
        EnvironmentStore::update(&cp.store, &hibernating)
            .await
            .unwrap();

        let woken = cp.wake_by_url(&env.url).await.unwrap().unwrap();
        assert_eq!(woken.state, EnvironmentState::Running);
        assert!(
            oci.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.starts_with("start:")),
            "{:?}",
            oci.calls
        );
    }

    /// Regression test for a real bug found live: an environment deployed
    /// before dynamic host-port assignment existed has `host_port: None`
    /// forever, since nothing but `run()` (which only fires on a *new*
    /// container) ever learns it — waking it just unpauses the existing
    /// container without recreating it. `wake` must opportunistically
    /// backfill `host_port` in that case instead of leaving the dashboard
    /// showing a dead Traefik-style URL forever after a wake.
    #[tokio::test]
    async fn wake_backfills_host_port_for_environments_predating_dynamic_ports() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        let cp = cp(oci.clone()).await;
        let project = cp.register_project(repo.path()).await.unwrap();
        let env = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();
        assert_eq!(env.host_port, Some(65535));

        // Simulate a row from before this column existed.
        let mut stale = EnvironmentStore::get(&cp.store, env.id)
            .await
            .unwrap()
            .unwrap();
        stale.host_port = None;
        EnvironmentStore::update(&cp.store, &stale).await.unwrap();

        cp.pause(env.id).await.unwrap();
        cp.wake(env.id).await.unwrap();

        let refreshed = EnvironmentStore::get(&cp.store, env.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(refreshed.host_port, Some(65535));
    }

    #[tokio::test]
    async fn wake_by_url_unknown_host_is_none() {
        let cp = cp(FakeOci::default()).await;
        assert!(cp.wake_by_url("nobody.local.dev").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn touch_by_url_refreshes_last_access_and_ignores_unknown() {
        let repo = repo_dir_with_config();
        let cp = cp(FakeOci::default()).await;
        let project = cp.register_project(repo.path()).await.unwrap();
        let env = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();

        cp.touch_by_url("nobody.local.dev").await.unwrap();

        let before = env.last_accessed_at;
        touch_env(&cp, env.clone(), before - time::Duration::hours(1)).await;
        cp.touch_by_url(&env.url).await.unwrap();
        let touched = EnvironmentStore::get(&cp.store, env.id)
            .await
            .unwrap()
            .unwrap();
        assert!(touched.last_accessed_at > before - time::Duration::hours(1));
    }

    #[tokio::test]
    async fn deploy_unknown_project_fails() {
        let cp = cp(FakeOci::default()).await;
        let err = cp
            .deploy(ProjectId(999), BranchName::parse("main").unwrap())
            .await
            .unwrap_err();
        assert!(matches!(err, CpError::NotFound(_)));
    }

    async fn touch_env(
        cp: &ControlPlane<FakeGit, FakeOci>,
        mut env: Environment,
        at: OffsetDateTime,
    ) {
        env.touch(at).unwrap();
        EnvironmentStore::update(&cp.store, &env).await.unwrap();
    }

    /// Regression test for a real bug found live: without Traefik, nothing
    /// ever calls [`ControlPlane::touch_by_url`], so `last_accessed_at`
    /// stays frozen at creation time forever regardless of real traffic —
    /// a woken environment looked exactly as idle as the moment it was
    /// created and got auto-paused again on the very next sweep. `sweep`
    /// must be a complete no-op in this mode instead of acting on data it
    /// knows is meaningless.
    #[tokio::test]
    async fn sweep_does_nothing_without_traefik_even_when_wildly_idle() {
        let repo = repo_dir_with_config();
        let cp = cp(FakeOci::default()).await; // no `with_traefik(...)` call
        let project = cp.register_project(repo.path()).await.unwrap();
        let env = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();

        // Idle well past every threshold (pause/hibernate/destroy).
        let now = OffsetDateTime::now_utc();
        touch_env(&cp, env.clone(), now - time::Duration::days(30)).await;

        let summary = cp.sweep(now).await.unwrap();
        assert_eq!(summary, GcSummary::default());

        let loaded = EnvironmentStore::get(&cp.store, env.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.state, EnvironmentState::Running);
    }

    #[tokio::test]
    async fn sweep_keeps_recently_active_environment() {
        let repo = repo_dir_with_config();
        // `sweep` no-ops entirely without Traefik configured (idle detection
        // needs its heartbeat) — exercise that real path here.
        let cp = cp(FakeOci::default())
            .await
            .with_traefik("net", "http://daemon");
        let project = cp.register_project(repo.path()).await.unwrap();
        let env = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();

        let now = OffsetDateTime::now_utc();
        touch_env(&cp, env.clone(), now - time::Duration::seconds(60)).await;

        let summary = cp.sweep(now).await.unwrap();
        assert_eq!(summary, GcSummary::default());
        let loaded = EnvironmentStore::get(&cp.store, env.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.state, EnvironmentState::Running);
    }

    #[tokio::test]
    async fn sweep_pauses_idle_environment() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        // `sweep` no-ops entirely without Traefik configured (idle detection
        // needs its heartbeat) — exercise that real path here.
        let cp = cp(oci.clone()).await.with_traefik("net", "http://daemon");
        let project = cp.register_project(repo.path()).await.unwrap();
        let env = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();

        // pause_after defaults to 30m.
        let now = OffsetDateTime::now_utc();
        touch_env(&cp, env.clone(), now - time::Duration::minutes(31)).await;

        let summary = cp.sweep(now).await.unwrap();
        assert_eq!(summary.paused, 1);
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);

        let loaded = EnvironmentStore::get(&cp.store, env.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.state, EnvironmentState::Paused);
        assert!(
            oci.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.starts_with("pause:")),
            "{:?}",
            oci.calls
        );
    }

    /// Regression test for a real race: a GC `sweep` tick and a manual
    /// action on the *same* environment both do read-modify-write (fetch,
    /// apply a `StateTransition`, persist) with no atomicity between the
    /// read and the write. Without `lifecycle_lock` covering both,
    /// interleaving them could have one silently overwrite the other's
    /// transition with a stale copy. This doesn't assert a specific winner
    /// (either legitimately can win) — it asserts the lock actually
    /// serializes them: no panic, and the persisted state is always a
    /// state genuinely reachable by one of the two actions, never a
    /// corrupted/impossible one.
    #[tokio::test]
    async fn concurrent_sweep_and_manual_destroy_do_not_corrupt_state() {
        let repo = repo_dir_with_config();
        // `sweep` no-ops entirely without Traefik configured (idle detection
        // needs its heartbeat) — exercise that real path here.
        let cp = cp(FakeOci::default())
            .await
            .with_traefik("net", "http://daemon");
        let project = cp.register_project(repo.path()).await.unwrap();
        let env = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();

        // Idle well past every GC threshold, so `sweep` will try to act on
        // it at the same time a manual `destroy` races in.
        let now = OffsetDateTime::now_utc();
        touch_env(&cp, env.clone(), now - time::Duration::days(8)).await;

        let cp_a = cp.clone();
        let cp_b = cp.clone();
        let env_id = env.id;
        let (sweep_result, destroy_result) = tokio::join!(
            tokio::spawn(async move { cp_a.sweep(now).await }),
            tokio::spawn(async move { cp_b.destroy(env_id, false).await }),
        );
        sweep_result.unwrap().unwrap();
        // Exactly one of the two "destroy" paths can win the state machine;
        // the loser gets a clean `Forbidden`/`Noop` domain error, not a
        // panic or a corrupted row — both are acceptable outcomes here.
        let _ = destroy_result.unwrap();

        let loaded = EnvironmentStore::get(&cp.store, env.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.state, EnvironmentState::Destroyed);
    }

    #[tokio::test]
    async fn sweep_hibernates_deeply_idle_paused_environment() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        // `sweep` no-ops entirely without Traefik configured (idle detection
        // needs its heartbeat) — exercise that real path here.
        let cp = cp(oci.clone()).await.with_traefik("net", "http://daemon");
        let project = cp.register_project(repo.path()).await.unwrap();
        let env = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();

        // First pass: 31m idle suspends it (Running -> Paused).
        let t1 = OffsetDateTime::now_utc();
        touch_env(&cp, env.clone(), t1 - time::Duration::minutes(31)).await;
        cp.sweep(t1).await.unwrap();

        // Second pass: 3h idle (> 4 * pause_after) hibernates it from Paused.
        let t2 = t1 + time::Duration::hours(3);
        let summary = cp.sweep(t2).await.unwrap();
        assert_eq!(summary.hibernated, 1, "{:?}", summary.errors);

        let loaded = EnvironmentStore::get(&cp.store, env.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.state, EnvironmentState::Hibernating);
    }

    #[tokio::test]
    async fn sweep_destroys_expired_environment() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        // `sweep` no-ops entirely without Traefik configured (idle detection
        // needs its heartbeat) — exercise that real path here.
        let cp = cp(oci.clone()).await.with_traefik("net", "http://daemon");
        let project = cp.register_project(repo.path()).await.unwrap();
        let env = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();

        // destroy_after defaults to 7d.
        let now = OffsetDateTime::now_utc();
        touch_env(&cp, env.clone(), now - time::Duration::days(8)).await;

        let summary = cp.sweep(now).await.unwrap();
        assert_eq!(summary.destroyed, 1, "{:?}", summary.errors);

        let loaded = EnvironmentStore::get(&cp.store, env.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.state, EnvironmentState::Destroyed);
        let calls = oci.calls.lock().unwrap();
        assert!(calls.iter().any(|c| c.starts_with("stop:")), "{calls:?}");
        assert!(calls.iter().any(|c| c.starts_with("remove:")), "{calls:?}");
    }

    #[tokio::test]
    async fn reconcile_marks_a_missing_container_destroyed() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        let cp = cp(oci.clone()).await;
        let project = cp.register_project(repo.path()).await.unwrap();
        let env = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();

        oci.container_statuses.lock().unwrap().insert(
            format!("oxid-app-feature-a-{}", env.id.0),
            ContainerStatus::Missing,
        );

        let errors = cp.reconcile_startup_state().await.unwrap();
        assert!(errors.is_empty(), "{errors:?}");

        let loaded = EnvironmentStore::get(&cp.store, env.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.state, EnvironmentState::Destroyed);
    }

    #[tokio::test]
    async fn reconcile_re_pauses_a_container_a_reboot_brought_back_running() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        let cp = cp(oci.clone()).await;
        let project = cp.register_project(repo.path()).await.unwrap();
        let env = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();
        cp.pause(env.id).await.unwrap();

        // A reboot doesn't preserve the cgroup-freezer "paused" state —
        // `unless-stopped` brings the container back fully running.
        oci.container_statuses.lock().unwrap().insert(
            format!("oxid-app-feature-a-{}", env.id.0),
            ContainerStatus::Running,
        );

        let errors = cp.reconcile_startup_state().await.unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert!(
            oci.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| *c == format!("pause:oxid-app-feature-a-{}", env.id.0)),
            "{:?}",
            oci.calls.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn reconcile_restarts_a_running_environment_whose_container_stopped() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        let cp = cp(oci.clone()).await;
        let project = cp.register_project(repo.path()).await.unwrap();
        let env = cp
            .deploy(project.id, BranchName::parse("feature-a").unwrap())
            .await
            .unwrap();

        oci.container_statuses.lock().unwrap().insert(
            format!("oxid-app-feature-a-{}", env.id.0),
            ContainerStatus::Stopped,
        );

        let errors = cp.reconcile_startup_state().await.unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert!(
            oci.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| *c == format!("start:oxid-app-feature-a-{}", env.id.0)),
            "{:?}",
            oci.calls.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn deploy_or_queue_deploys_immediately_when_capacity_is_available() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        *oci.host_capacity.lock().unwrap() = HostCapacity {
            total_memory_bytes: 1024 * 1_048_576,
            cpu_count: 4,
        };
        let cp = cp(oci)
            .await
            .with_resource_defaults(Some(200), None)
            .with_admission_control(Some(100));
        let project = cp.register_project(repo.path()).await.unwrap();

        let outcome = cp
            .deploy_or_queue(project.id, BranchName::parse("main").unwrap(), None)
            .await
            .unwrap();
        assert!(matches!(outcome, DeployOutcome::Deployed(_)), "{outcome:?}");
    }

    #[tokio::test]
    async fn deploy_or_queue_queues_when_the_host_is_already_committed() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        *oci.host_capacity.lock().unwrap() = HostCapacity {
            total_memory_bytes: 300 * 1_048_576,
            cpu_count: 4,
        };
        let cp = cp(oci)
            .await
            .with_resource_defaults(Some(200), None)
            .with_admission_control(Some(50));
        let project = cp.register_project(repo.path()).await.unwrap();

        // First deploy fits alone (200MB request <= 250MB usable) and stays
        // Running, committing its 200MB.
        cp.deploy_or_queue(project.id, BranchName::parse("main").unwrap(), None)
            .await
            .unwrap();

        // A second branch's 200MB request would push committed usage to
        // 400MB against only 250MB usable — must queue, not overcommit.
        let outcome = cp
            .deploy_or_queue(project.id, BranchName::parse("other").unwrap(), None)
            .await
            .unwrap();
        let DeployOutcome::Queued { position } = outcome else {
            panic!("expected Queued, got {outcome:?}");
        };
        assert_eq!(position, 1);

        let queued = cp.store.list_deploy_queue().await.unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].branch, "other");
    }

    #[tokio::test]
    async fn deploy_or_queue_rejects_a_request_that_could_never_fit() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        *oci.host_capacity.lock().unwrap() = HostCapacity {
            total_memory_bytes: 1024 * 1_048_576,
            cpu_count: 4,
        };
        let cp = cp(oci)
            .await
            .with_resource_defaults(Some(2000), None)
            .with_admission_control(Some(1000));
        let project = cp.register_project(repo.path()).await.unwrap();

        let err = cp
            .deploy_or_queue(project.id, BranchName::parse("main").unwrap(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, CpError::InsufficientCapacity(_)), "{err:?}");
    }

    #[tokio::test]
    async fn deploy_or_queue_always_deploys_when_admission_control_is_disabled() {
        let repo = repo_dir_with_config();
        // Zero capacity by default — if admission control were mistakenly
        // active this would queue or reject, not deploy.
        let cp = cp(FakeOci::default()).await;
        let project = cp.register_project(repo.path()).await.unwrap();

        let outcome = cp
            .deploy_or_queue(project.id, BranchName::parse("main").unwrap(), None)
            .await
            .unwrap();
        assert!(matches!(outcome, DeployOutcome::Deployed(_)), "{outcome:?}");
    }

    #[tokio::test]
    async fn retry_queued_deploys_leaves_the_queue_untouched_when_nothing_fits_yet() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        *oci.host_capacity.lock().unwrap() = HostCapacity {
            total_memory_bytes: 300 * 1_048_576,
            cpu_count: 4,
        };
        let cp = cp(oci)
            .await
            .with_resource_defaults(Some(200), None)
            .with_admission_control(Some(50));
        let project = cp.register_project(repo.path()).await.unwrap();

        cp.deploy_or_queue(project.id, BranchName::parse("main").unwrap(), None)
            .await
            .unwrap();
        cp.deploy_or_queue(project.id, BranchName::parse("other").unwrap(), None)
            .await
            .unwrap();

        let failures = cp.retry_queued_deploys().await.unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        let queued = cp.store.list_deploy_queue().await.unwrap();
        assert_eq!(queued.len(), 1, "{queued:?}");
        assert_eq!(queued[0].branch, "other");
    }

    #[tokio::test]
    async fn retry_queued_deploys_deploys_once_capacity_frees_up() {
        let repo = repo_dir_with_config();
        let oci = FakeOci::default();
        *oci.host_capacity.lock().unwrap() = HostCapacity {
            total_memory_bytes: 300 * 1_048_576,
            cpu_count: 4,
        };
        let cp = cp(oci.clone())
            .await
            .with_resource_defaults(Some(200), None)
            .with_admission_control(Some(50));
        let project = cp.register_project(repo.path()).await.unwrap();

        let main_env = match cp
            .deploy_or_queue(project.id, BranchName::parse("main").unwrap(), None)
            .await
            .unwrap()
        {
            DeployOutcome::Deployed(env) => env,
            other @ DeployOutcome::Queued { .. } => panic!("expected Deployed, got {other:?}"),
        };
        cp.deploy_or_queue(project.id, BranchName::parse("other").unwrap(), None)
            .await
            .unwrap();

        // Freeing `main`'s 200MB should let `other`'s queued 200MB request
        // through on the next retry pass.
        cp.destroy(main_env.id, false).await.unwrap();

        let failures = cp.retry_queued_deploys().await.unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        assert!(cp.store.list_deploy_queue().await.unwrap().is_empty());
        assert!(
            oci.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.starts_with("run:oxid-app-other")),
            "{:?}",
            oci.calls.lock().unwrap()
        );
    }
}
