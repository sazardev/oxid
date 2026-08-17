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

use crate::domain::audit::AuditEvent;
use crate::domain::branch::BranchName;
use crate::domain::environment::{Environment, EnvironmentId};
use crate::domain::project::{Project, ProjectId};
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
    /// Inserts a new project.
    ///
    /// # Errors
    /// [`RepositoryError::Conflict`] if the id already exists.
    async fn create(&self, project: &Project) -> Result<(), RepositoryError>;
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
    /// Deletes a project and its cascading records.
    ///
    /// # Errors
    /// [`RepositoryError::NotFound`] if the id does not exist.
    async fn delete(&self, id: ProjectId) -> Result<(), RepositoryError>;
}

/// Persistence contract for [`Environment`] records.
#[trait_variant::make(Send)]
pub trait EnvironmentStore {
    /// Inserts a new environment.
    ///
    /// # Errors
    /// [`RepositoryError::Conflict`] if the id already exists.
    async fn create(&self, env: &Environment) -> Result<(), RepositoryError>;
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
    /// Port to publish on the host.
    pub host_port: u16,
    /// Labels used for routing/inventory (e.g. `oxid.project`).
    pub labels: BTreeMap<String, String>,
}

/// Container orchestration contract backed by the Docker socket.
#[trait_variant::make(Send)]
pub trait ContainerPort {
    /// Builds an image from a context.
    ///
    /// # Errors
    /// [`OciError::Failure`] on build failure.
    async fn build(&self, spec: &BuildSpec) -> Result<(), OciError>;
    /// Creates and starts a container.
    ///
    /// # Errors
    /// [`OciError::Failure`] on failure.
    async fn run(&self, spec: &ContainerSpec) -> Result<(), OciError>;
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
    /// Streams the tail of a container's logs.
    ///
    /// # Errors
    /// [`OciError::NotFound`] if the container does not exist.
    async fn logs(&self, name: &str) -> Result<String, OciError>;
}
