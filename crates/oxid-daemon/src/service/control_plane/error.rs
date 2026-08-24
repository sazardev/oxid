use oxid_core::{DomainError, GitError, OciError, PoolError, RepositoryError};

use crate::adapter::config::ConfigError;

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
    /// A request was structurally valid JSON but semantically wrong
    /// (e.g. an empty project-scope list on a new API token).
    #[error("{0}")]
    Validation(String),
}
