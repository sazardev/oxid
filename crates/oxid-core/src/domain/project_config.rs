//! Static configuration of a project, mirroring `oxid.toml`
//! (IDEA.md `[project]`, `[build]`, `[routing]`, `[dependencies]` blocks).

use serde::{Deserialize, Serialize};

use crate::domain::error::invalid;
use crate::domain::resource_pool::PoolKind;
use crate::domain::services::branch_filter::DeployConfig;
use crate::domain::value_objects::Ttl;

/// Build instructions (`[build]` block).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildConfig {
    /// Dockerfile path override, relative to `context`.
    pub dockerfile: Option<String>,
    /// Build context directory.
    pub context: String,
    /// Commands to run once the container starts (ephemeral data seeding).
    pub on_start: Vec<String>,
    /// Memory limit for the deployed container, in megabytes. When unset,
    /// the daemon's own `OXID_DEFAULT_MEMORY_LIMIT_MB` applies instead
    /// (SPEC.md "Eficiencia Absoluta" — no environment should be able to
    /// exhaust the host by default).
    pub memory_limit_mb: Option<u64>,
    /// CPU limit for the deployed container, in millicores (1000 = one full
    /// core). When unset, the daemon's own `OXID_DEFAULT_CPU_LIMIT_MILLICORES`
    /// applies instead. An integer (rather than a fractional core count) to
    /// keep `BuildConfig` exactly comparable (`Eq`) without float pitfalls.
    pub cpu_limit_millicores: Option<u32>,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            dockerfile: None,
            context: ".".to_owned(),
            on_start: Vec::new(),
            memory_limit_mb: None,
            cpu_limit_millicores: None,
        }
    }
}

/// Reference to a shared dependency multiplexed across branches
/// (`[dependencies.<name>]` block).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    /// Kind of the shared instance.
    pub kind: PoolKind,
    /// Name of the shared `ResourcePool` this project draws from.
    pub shared_instance: String,
    /// Environment variable receiving the per-branch connection info.
    pub inject_url_as: String,
}

/// The immutable, configuration-driven part of a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// Base domain for routing, e.g. `my-awesome-api.local.dev`.
    pub base_domain: String,
    /// Idle time before an environment is paused (`pause_after`).
    pub pause_after: Ttl,
    /// Max lifetime before an environment is destroyed (`destroy_after`).
    pub destroy_after: Ttl,
    /// Internal port the proxy routes traffic to (`[routing].port`).
    pub port: u16,
    /// Build instructions.
    pub build: BuildConfig,
    /// Shared dependencies to multiplex.
    pub dependencies: Vec<Dependency>,
    /// Which branches a push may deploy, and how many environments this
    /// project may hold (`[deploy]` block). Default is unrestricted, so a
    /// project that never configures it keeps deploying every branch.
    ///
    /// Set through [`ProjectConfig::with_deploy`] rather than `new`, whose
    /// positional signature is used across the workspace and in every
    /// fixture — a project without a `[deploy]` block is the common case and
    /// should not have to name it.
    #[serde(default)]
    pub deploy: DeployConfig,
}

impl ProjectConfig {
    /// Validates and constructs a project configuration.
    ///
    /// Defaults (missing `[build]` fields) must be applied by the caller
    /// before calling this; the domain only validates.
    ///
    /// # Errors
    /// Returns [`DomainError::Invalid`] for an empty base domain, a port of
    /// zero or an empty `shared_instance`/`inject_url_as` on a dependency.
    pub fn new(
        base_domain: impl Into<String>,
        pause_after: Ttl,
        destroy_after: Ttl,
        port: u16,
        build: BuildConfig,
        dependencies: Vec<Dependency>,
    ) -> Result<Self, crate::DomainError> {
        let base_domain = base_domain.into();

        if base_domain.trim().is_empty() {
            return invalid("base domain cannot be empty");
        }
        if port == 0 {
            return invalid("port cannot be zero");
        }
        for dep in &dependencies {
            if dep.shared_instance.trim().is_empty() {
                return invalid("dependency `shared_instance` cannot be empty");
            }
            if dep.inject_url_as.trim().is_empty() {
                return invalid("dependency `inject_url_as` cannot be empty");
            }
        }

        Ok(Self {
            base_domain,
            pause_after,
            destroy_after,
            port,
            build,
            dependencies,
            deploy: DeployConfig::default(),
        })
    }

    /// Returns this configuration with its `[deploy]` rules replaced.
    #[must_use]
    pub fn with_deploy(mut self, deploy: DeployConfig) -> Self {
        self.deploy = deploy;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ttl(m: u64) -> Ttl {
        Ttl::parse(format!("{m}m")).unwrap()
    }

    #[test]
    fn builds_valid_config() {
        let config = ProjectConfig::new(
            "my-awesome-api.local.dev",
            ttl(30),
            ttl(7 * 24 * 60),
            8080,
            BuildConfig::default(),
            vec![Dependency {
                kind: PoolKind::Postgres,
                shared_instance: "local-pg-cluster".to_owned(),
                inject_url_as: "DATABASE_URL".to_owned(),
            }],
        )
        .unwrap();

        assert_eq!(config.base_domain, "my-awesome-api.local.dev");
        assert_eq!(config.dependencies.len(), 1);
    }

    #[test]
    fn rejects_invalid_input() {
        let build = BuildConfig::default();
        assert!(ProjectConfig::new("", ttl(1), ttl(2), 8080, build.clone(), vec![]).is_err());
        assert!(ProjectConfig::new("dom.local", ttl(1), ttl(2), 0, build.clone(), vec![]).is_err());
        assert!(
            ProjectConfig::new(
                "dom.local",
                ttl(1),
                ttl(2),
                8080,
                build,
                vec![Dependency {
                    kind: PoolKind::Redis,
                    shared_instance: String::new(),
                    inject_url_as: "REDIS_URL".to_owned(),
                }],
            )
            .is_err()
        );
    }
}
