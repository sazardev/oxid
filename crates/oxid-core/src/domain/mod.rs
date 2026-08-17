//! Domain layer: entities, value objects, state machine and pure services.

mod audit;
mod branch;
mod environment;
mod error;
pub mod ports;
mod project;
mod project_config;
mod resource_pool;
mod secret_context;
pub mod services;
mod state;
mod value_objects;

pub use audit::AuditEvent;
pub use branch::{Branch, BranchName};
pub use environment::{Environment, EnvironmentId};
pub use error::DomainError;
pub use ports::{AuditStore, EnvironmentStore, ProjectStore, RepositoryError};
pub use project::{Project, ProjectId};
pub use project_config::{BuildConfig, Dependency, ProjectConfig};
pub use resource_pool::{PoolKind, ResourcePool};
pub use secret_context::{EnvVarScope, SecretContext, SecretValue};
pub use state::{EnvironmentState, EnvironmentStateError, StateTransition, TransitionTable};
pub use value_objects::{RepoUrl, Ttl};

/// Timestamps used by domain entities.
pub use time::OffsetDateTime;
