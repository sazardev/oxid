//! `ResourcePool` entity — the basis of dependency multiplexing
//! (SPEC.md §3.1).

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::domain::error::invalid;

/// Errors surfaced while provisioning a shared Postgres database
/// (SPEC.md §3.1). Redis needs no equivalent: assigning an index is pure
/// `SQLite` bookkeeping (see `control_plane.rs`), not a live call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PoolError {
    /// The daemon has no admin connection configured for this dependency
    /// kind (`OXID_POSTGRES_URL` unset).
    #[error("{0}")]
    NotConfigured(String),
    /// The Postgres operation itself failed.
    #[error("resource pool failure: {0}")]
    Failure(String),
}

/// Kind of shared dependency multiplexed across branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PoolKind {
    /// Relational database shared via per-branch logical databases/schemas.
    Postgres,
    /// Shared cache shared via per-branch database index or key prefix.
    Redis,
}

impl fmt::Display for PoolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Postgres => "postgres",
            Self::Redis => "redis",
        };
        f.write_str(s)
    }
}

impl FromStr for PoolKind {
    type Err = crate::domain::DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "postgres" => Ok(Self::Postgres),
            "redis" => Ok(Self::Redis),
            _ => invalid(format!("unknown pool kind `{s}`")),
        }
    }
}

/// A single shared instance of a dependency.
///
/// The control plane keeps one pool per kind and carves per-branch slices out
/// of it instead of booting one container per environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePool {
    /// Unique identifier.
    pub id: u64,
    /// What kind of resource this pool multiplexes.
    pub kind: PoolKind,
    /// Number of concurrent slices (logical DBs / indexes) supported.
    pub capacity: u32,
    /// Slices currently handed out (e.g. branch names).
    pub leased: BTreeSet<String>,
}

impl ResourcePool {
    /// Validates and constructs a pool.
    ///
    /// # Errors
    /// Returns [`DomainError::Invalid`] when `capacity` is zero.
    pub fn new(id: u64, kind: PoolKind, capacity: u32) -> Result<Self, crate::DomainError> {
        if capacity == 0 {
            return invalid("resource pool capacity cannot be zero");
        }
        Ok(Self {
            id,
            kind,
            capacity,
            leased: BTreeSet::new(),
        })
    }

    /// Number of slices still available.
    #[must_use]
    pub fn available(&self) -> u32 {
        self.capacity
            .saturating_sub(u32::try_from(self.leased.len()).unwrap_or(u32::MAX))
    }

    /// Whether a slice can still be handed out.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.available() == 0
    }

    /// Leases a slice for `tenant`.
    ///
    /// # Errors
    /// Returns [`DomainError::Invalid`] if the pool is exhausted or the tenant
    /// already holds a slice.
    pub fn lease(&mut self, tenant: &str) -> Result<(), crate::DomainError> {
        if self.leased.contains(tenant) {
            return invalid(format!("tenant `{tenant}` already holds a slice"));
        }
        if self.is_exhausted() {
            return invalid(format!(
                "pool `{}` is exhausted (capacity {})",
                self.id, self.capacity
            ));
        }
        self.leased.insert(tenant.to_owned());
        Ok(())
    }

    /// Releases a previously leased slice.
    ///
    /// # Errors
    /// Returns [`DomainError::Invalid`] if `tenant` holds no slice.
    pub fn release(&mut self, tenant: &str) -> Result<(), crate::DomainError> {
        if self.leased.remove(tenant) {
            Ok(())
        } else {
            invalid(format!(
                "tenant `{tenant}` holds no slice in pool `{}`",
                self.id
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_tracking() {
        let mut pool = ResourcePool::new(1, PoolKind::Postgres, 2).unwrap();
        assert_eq!(pool.available(), 2);

        pool.lease("feature-a").unwrap();
        pool.lease("feature-b").unwrap();
        assert!(pool.is_exhausted());
        assert!(pool.lease("feature-c").is_err());
    }

    #[test]
    fn release_frees_slices() {
        let mut pool = ResourcePool::new(1, PoolKind::Redis, 1).unwrap();
        pool.lease("feature-a").unwrap();
        pool.release("feature-a").unwrap();
        assert!(!pool.is_exhausted());
    }

    #[test]
    fn double_lease_rejected() {
        let mut pool = ResourcePool::new(1, PoolKind::Postgres, 5).unwrap();
        pool.lease("feature-a").unwrap();
        assert!(pool.lease("feature-a").is_err());
    }

    #[test]
    fn release_unknown_tenant_rejected() {
        let mut pool = ResourcePool::new(1, PoolKind::Postgres, 5).unwrap();
        assert!(pool.release("nobody").is_err());
    }

    #[test]
    fn zero_capacity_rejected() {
        assert!(ResourcePool::new(1, PoolKind::Postgres, 0).is_err());
    }
}
