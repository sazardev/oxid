//! Persistence ports (hexagonal "driven" adapters).
//!
//! These traits are the domain-facing contracts; the `SQLite` adapter in
//! `oxid-daemon` implements them. The domain never sees SQL.
//!
//! Native `async fn` in traits is used on purpose: the traits are consumed
//! through concrete adapter types (not `dyn`), so `Send` auto-trait bounds are
//! not required at the port boundary.

#![allow(async_fn_in_trait)]

use crate::domain::audit::AuditEvent;
use crate::domain::environment::{Environment, EnvironmentId};
use crate::domain::project::{Project, ProjectId};
use crate::domain::state::EnvironmentState;

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
