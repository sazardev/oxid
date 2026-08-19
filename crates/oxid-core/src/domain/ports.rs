//! Persistence and infrastructure ports (hexagonal "driven" adapters).
//!
//! These traits are the domain-facing contracts; the adapters in `oxid-daemon`
//! implement them. The domain never sees SQL, Git or Docker.
//!
//! Each trait is generated in two variants by `trait_variant`: the base trait
//! and a `Send`-suffixed variant whose futures are `Send`, safe for async
//! frameworks (`axum`, `tokio`). Adapters implement the `Send` variant.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use futures_core::Stream;

use crate::domain::audit::AuditEvent;
use crate::domain::branch::BranchName;
use crate::domain::environment::{Environment, EnvironmentId};
use crate::domain::project::{Project, ProjectId};
use crate::domain::secret_context::{EnvVarScope, SecretContext, SecretValue};
use crate::domain::state::EnvironmentState;
use crate::domain::value_objects::RepoUrl;

/// Errors surfaced by a persistence port.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RepositoryError {
    /// A record was expected but did not exist.
    #[error("record not found: {0}")]
    NotFound(String),
    /// A record already exists and uniqueness forbids the write.
    #[error("record already exists: {0}")]
    Conflict(String),
    /// The underlying storage layer failed.
    #[error("storage failure: {0}")]
    Storage(String),
}

/// Persistence contract for [`Project`] records.
#[trait_variant::make(Send)]
pub trait ProjectStore {
    /// Inserts a new project and returns its database-assigned id.
    ///
    /// # Errors
    /// [`RepositoryError::Conflict`] if a project with the same `repo_url`
    /// already exists.
    async fn create(&self, project: &Project) -> Result<ProjectId, RepositoryError>;
    /// Loads a project by id, or `None` if absent.
    ///
    /// # Errors
    /// Any storage failure.
    async fn get(&self, id: ProjectId) -> Result<Option<Project>, RepositoryError>;
    /// Lists all projects.
    ///
    /// # Errors
    /// Any storage failure.
    async fn list(&self) -> Result<Vec<Project>, RepositoryError>;
    /// Persists an updated project's config — e.g. `pause_after`/
    /// `destroy_after` changed via the dashboard/API, which `oxid.toml`
    /// only ever seeds at first registration otherwise.
    ///
    /// # Errors
    /// [`RepositoryError::NotFound`] if the project does not exist.
    async fn update(&self, project: &Project) -> Result<(), RepositoryError>;
    /// Deletes a project and its cascading records.
    ///
    /// # Errors
    /// [`RepositoryError::NotFound`] if the id does not exist.
    async fn delete(&self, id: ProjectId) -> Result<(), RepositoryError>;
}

/// Persistence contract for [`Environment`] records.
#[trait_variant::make(Send)]
pub trait EnvironmentStore {
    /// Inserts a new environment and returns its database-assigned id.
    ///
    /// # Errors
    /// [`RepositoryError::Conflict`] if the id already exists.
    async fn create(&self, env: &Environment) -> Result<EnvironmentId, RepositoryError>;
    /// Loads an environment by id, or `None` if absent.
    ///
    /// # Errors
    /// Any storage failure.
    async fn get(&self, id: EnvironmentId) -> Result<Option<Environment>, RepositoryError>;
    /// Lists environments of a project.
    ///
    /// # Errors
    /// Any storage failure.
    async fn list_by_project(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<Environment>, RepositoryError>;
    /// Lists environments currently in a state.
    ///
    /// # Errors
    /// Any storage failure.
    async fn list_by_state(
        &self,
        state: EnvironmentState,
    ) -> Result<Vec<Environment>, RepositoryError>;
    /// Persists an updated environment (state, timestamps, etc.).
    ///
    /// # Errors
    /// [`RepositoryError::NotFound`] if the environment does not exist.
    async fn update(&self, env: &Environment) -> Result<(), RepositoryError>;
    /// Deletes an environment.
    ///
    /// # Errors
    /// [`RepositoryError::NotFound`] if the id does not exist.
    async fn delete(&self, id: EnvironmentId) -> Result<(), RepositoryError>;
}

/// Persistence contract for the audit trail.
#[trait_variant::make(Send)]
pub trait AuditStore {
    /// Records an event.
    ///
    /// # Errors
    /// Any storage failure.
    async fn record(&self, event: &AuditEvent) -> Result<(), RepositoryError>;
    /// Lists events of one environment, oldest first.
    ///
    /// # Errors
    /// Any storage failure.
    async fn list_by_environment(
        &self,
        environment_id: EnvironmentId,
    ) -> Result<Vec<AuditEvent>, RepositoryError>;
    /// Lists the most recent `limit` events, newest first.
    ///
    /// # Errors
    /// Any storage failure.
    async fn list_recent(&self, limit: u64) -> Result<Vec<AuditEvent>, RepositoryError>;
}

// ---------------------------------------------------------------------------
// Secret port (SPEC.md §4.4 inyección de variables)
// ---------------------------------------------------------------------------

/// Persistence contract for environment variables and secrets.
///
/// A secret belongs to one of three scopes — `Global`, `Project` or `Branch` —
/// encoded positionally: `(None, None)` is global, `(Some(project), None)` is
/// project-scoped and `(Some(project), Some(branch))` is branch-scoped. Values
/// are opaque to the domain; adapters are responsible for encryption at rest.
#[trait_variant::make(Send)]
pub trait SecretStore {
    /// Stores (or replaces) a secret at the given scope.
    ///
    /// # Errors
    /// [`RepositoryError::Storage`] on persistence or encryption failure.
    async fn set_secret(
        &self,
        project_id: Option<ProjectId>,
        branch: Option<&BranchName>,
        name: &str,
        scope: EnvVarScope,
        value: &SecretValue,
    ) -> Result<(), RepositoryError>;

    /// Loads every secret relevant to a project/branch context, preserving its
    /// scope so resolution (`Global -> Project -> Branch`) can be applied.
    ///
    /// # Errors
    /// [`RepositoryError::Storage`] on persistence or decryption failure.
    async fn secrets_for(
        &self,
        project_id: Option<ProjectId>,
        branch: Option<&BranchName>,
    ) -> Result<SecretContext, RepositoryError>;

    /// Lists stored secret names and their scopes for a context (no values).
    ///
    /// # Errors
    /// [`RepositoryError::Storage`] on query failure.
    async fn list_secrets(
        &self,
        project_id: Option<ProjectId>,
        branch: Option<&BranchName>,
    ) -> Result<Vec<(String, EnvVarScope)>, RepositoryError>;

    /// Deletes a secret by name within a context.
    ///
    /// # Errors
    /// [`RepositoryError::NotFound`] if the secret does not exist.
    async fn delete_secret(
        &self,
        project_id: Option<ProjectId>,
        branch: Option<&BranchName>,
        name: &str,
    ) -> Result<(), RepositoryError>;
}

// ---------------------------------------------------------------------------
// Git port (SPEC.md §2.2 "Versionamiento")
// ---------------------------------------------------------------------------

/// Errors surfaced by the Git port.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitError {
    /// A Git operation failed.
    #[error("git failure: {0}")]
    Failure(String),
}

/// A branch pinned to a commit, used for detached checkouts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRef {
    /// The branch name.
    pub branch: BranchName,
    /// The resolved head commit.
    pub sha: String,
}

/// Versioning contract: cached clones and detached checkouts.
///
/// Git operations are blocking; adapters should offload them to a blocking
/// executor.
#[trait_variant::make(Send)]
pub trait GitPort {
    /// Reads the `origin` remote URL of the repository at `repo_dir`.
    ///
    /// # Errors
    /// [`GitError::Failure`] if the directory is not a repo or has no `origin`.
    async fn remote_url(&self, repo_dir: &Path) -> Result<RepoUrl, GitError>;
    /// Clones `url` into `cache_dir` (or reuses an existing clone), returning
    /// the repository directory.
    ///
    /// # Errors
    /// [`GitError::Failure`] on any clone/cache failure.
    async fn ensure_repo(&self, url: &RepoUrl, cache_dir: &Path) -> Result<PathBuf, GitError>;
    /// Resolves the head commit of `branch` in `repo_dir`.
    ///
    /// # Errors
    /// [`GitError::Failure`] if the branch does not exist.
    async fn resolve_branch_head(
        &self,
        repo_dir: &Path,
        branch: &BranchName,
    ) -> Result<CommitRef, GitError>;
    /// Checks out `sha` in `repo_dir` as a detached head.
    ///
    /// # Errors
    /// [`GitError::Failure`] if the commit cannot be checked out.
    async fn checkout_commit(&self, repo_dir: &Path, sha: &str) -> Result<(), GitError>;
}

// ---------------------------------------------------------------------------
// OCI/container port (SPEC.md §2.2 "Orquestación OCI")
// ---------------------------------------------------------------------------

/// Errors surfaced by the OCI port.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OciError {
    /// The referenced container or image does not exist.
    #[error("container `{0}` not found")]
    NotFound(String),
    /// A Docker operation failed.
    #[error("docker failure: {0}")]
    Failure(String),
}

/// A boxed, `Send` stream of container log lines, yielded as they're
/// written rather than gathered into a single bounded snapshot.
pub type LogStream = Pin<Box<dyn Stream<Item = Result<String, OciError>> + Send>>;

/// A container's actual state, as Docker reports it — independent of
/// whatever `EnvironmentState` the database believes. Backs startup
/// reconciliation: the daemon can be down for a while (crash, restart, a
/// host reboot) during which a container's real state can drift from the
/// last thing the database recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerStatus {
    /// Exists and is actively running (not paused).
    Running,
    /// Exists, running, but frozen (`docker pause`).
    Paused,
    /// Exists but is not running (`docker stop`, a crash, or an
    /// exhausted restart policy).
    Stopped,
    /// No container by that name exists at all.
    Missing,
}

/// Total host resources Docker itself reports (`docker info`) — the
/// admission-control source of truth for whether a new deploy actually
/// fits, since Docker already knows the real host capacity whether this
/// daemon runs on bare metal or inside its own container on the same host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HostCapacity {
    /// Total host memory, in bytes.
    pub total_memory_bytes: u64,
    /// Number of host CPUs.
    pub cpu_count: u32,
}

/// Inputs for a container image build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildSpec {
    /// Build context directory.
    pub context: PathBuf,
    /// Dockerfile path, relative to `context`.
    pub dockerfile: String,
    /// Image tag to produce.
    pub image: String,
}

/// Inputs for running an environment container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerSpec {
    /// Container name.
    pub name: String,
    /// Image to run.
    pub image: String,
    /// Environment variables to inject.
    pub env: BTreeMap<String, String>,
    /// Port the container listens on internally.
    pub container_port: u16,
    /// Labels used for routing/inventory (e.g. `oxid.project`), including
    /// the Traefik router/service/middleware labels when `network` is set.
    pub labels: BTreeMap<String, String>,
    /// Docker network shared with Traefik and the daemon
    /// (`OXID_DOCKER_NETWORK`). When set, the container is attached to it
    /// and no host port is published (SPEC.md §3.2). When `None`,
    /// `container_port` is published on a host port Docker picks itself
    /// (SPEC.md "Eficiencia Absoluta" — a busy port should never block a
    /// deploy) — the fallback used when no Traefik instance is configured;
    /// [`ContainerPort::run`] reports back which port was actually bound.
    pub network: Option<String>,
    /// Memory limit in megabytes, already resolved from the project's
    /// `[build]` config or the daemon's `OXID_DEFAULT_MEMORY_LIMIT_MB`.
    /// `None` means genuinely unbounded (both were unset).
    pub memory_limit_mb: Option<u64>,
    /// CPU limit in millicores (1000 = one full core), same resolution
    /// order as `memory_limit_mb`.
    pub cpu_limit_millicores: Option<u32>,
}

/// Container orchestration contract backed by the Docker socket.
#[trait_variant::make(Send)]
pub trait ContainerPort {
    /// Builds an image from a context.
    ///
    /// # Errors
    /// [`OciError::Failure`] on build failure.
    async fn build(&self, spec: &BuildSpec) -> Result<(), OciError>;
    /// Creates and starts a container. Returns the host port actually bound
    /// when `spec.network` is `None` (Docker picks it — see
    /// [`ContainerSpec::network`]), or `None` when attached to a Traefik
    /// network instead (no host port is published at all).
    ///
    /// # Errors
    /// [`OciError::Failure`] on failure.
    async fn run(&self, spec: &ContainerSpec) -> Result<Option<u16>, OciError>;
    /// Looks up the host port already published for `container_port` on an
    /// existing container — same information [`ContainerPort::run`] reports
    /// back at creation time, but for a container that was created before
    /// this daemon ever tracked it (e.g. one deployed by an older Oxid
    /// version, or whose row otherwise never recorded it). Used to backfill
    /// [`crate::Environment::host_port`] opportunistically on wake/
    /// reconciliation instead of leaving it wrong forever. `None` if the
    /// container publishes no host port at all (Traefik network mode) or
    /// isn't running.
    ///
    /// # Errors
    /// [`OciError::NotFound`] if the container does not exist.
    async fn published_port(
        &self,
        name: &str,
        container_port: u16,
    ) -> Result<Option<u16>, OciError>;
    /// Starts an existing, stopped container (`docker start`). Distinct from
    /// [`ContainerPort::run`], which creates a new container from a spec;
    /// this is used to wake a `Hibernating` environment whose container was
    /// `stop`ped (not `pause`d) and therefore still exists.
    ///
    /// # Errors
    /// [`OciError::NotFound`] if the container does not exist.
    async fn start(&self, name: &str) -> Result<(), OciError>;
    /// Suspends a running container (`docker pause`).
    ///
    /// # Errors
    /// [`OciError::NotFound`] if the container does not exist.
    async fn pause(&self, name: &str) -> Result<(), OciError>;
    /// Resumes a paused container (`docker unpause`).
    ///
    /// # Errors
    /// [`OciError::NotFound`] if the container does not exist.
    async fn unpause(&self, name: &str) -> Result<(), OciError>;
    /// Stops a container.
    ///
    /// # Errors
    /// [`OciError::NotFound`] if the container does not exist.
    async fn stop(&self, name: &str) -> Result<(), OciError>;
    /// Removes a container (forcefully).
    ///
    /// # Errors
    /// [`OciError::NotFound`] if the container does not exist.
    async fn remove(&self, name: &str) -> Result<(), OciError>;
    /// Removes a built image. Used when an environment is destroyed, so
    /// branch-specific images don't accumulate on disk forever.
    ///
    /// # Errors
    /// [`OciError::NotFound`] if the image does not exist.
    async fn remove_image(&self, image: &str) -> Result<(), OciError>;
    /// Streams the tail of a container's logs.
    ///
    /// # Errors
    /// [`OciError::NotFound`] if the container does not exist.
    async fn logs(&self, name: &str) -> Result<String, OciError>;
    /// Follows a container's logs live (`docker logs -f`), yielding new
    /// lines as they're written instead of a bounded snapshot. Backs the
    /// SSE `/logs/stream` endpoint (SPEC.md §5).
    ///
    /// # Errors
    /// [`OciError::NotFound`] if the container does not exist.
    async fn stream_logs(&self, name: &str) -> Result<LogStream, OciError>;
    /// Runs a one-off command inside a running container (`docker exec`).
    ///
    /// Used to execute `[build].on_start` hooks after a deployment.
    ///
    /// # Errors
    /// [`OciError::NotFound`] if the container does not exist,
    /// [`OciError::Failure`] if the command exits non-zero.
    async fn exec(&self, name: &str, command: &str) -> Result<(), OciError>;
    /// Reports a container's actual current state — used at daemon startup
    /// to reconcile the database against reality after a restart, rather
    /// than trusting a potentially stale `EnvironmentState`.
    ///
    /// # Errors
    /// [`OciError::Failure`] on a Docker communication failure (distinct
    /// from the container simply not existing, which is
    /// [`ContainerStatus::Missing`], not an error).
    async fn container_status(&self, name: &str) -> Result<ContainerStatus, OciError>;
    /// Total host memory/CPU Docker reports — the source of truth for
    /// admission control before a new deploy.
    ///
    /// # Errors
    /// [`OciError::Failure`] on a Docker communication failure.
    async fn host_capacity(&self) -> Result<HostCapacity, OciError>;
}
