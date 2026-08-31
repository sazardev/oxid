//! `Project` entity.

use serde::{Deserialize, Serialize};

use crate::domain::error::invalid;
use crate::domain::project_config::ProjectConfig;
use crate::domain::value_objects::RepoUrl;

/// Stable identifier of a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectId(pub u64);

impl std::fmt::Display for ProjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A deployable repository tracked by Oxid.
///
/// Combines identity (`id`, `name`, `repo_url`) with the static
/// configuration declared in `oxid.toml` (`config`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// Unique identifier.
    pub id: ProjectId,
    /// Project name; used for logs and default subdomain base.
    pub name: String,
    /// Where the code lives.
    pub repo_url: RepoUrl,
    /// Configuration declared in `oxid.toml`.
    pub config: ProjectConfig,
    /// What the repository was detected to be built with, when Oxid had to
    /// work it out. `None` means the repository answered for itself — an
    /// `oxid.toml`, a Compose file or a committed `Dockerfile` — and
    /// nothing was inferred, which is both the common case and the one
    /// where a guess would be presumptuous.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detected_stack: Option<crate::services::stack::Stack>,
    /// The workspace, when the repository holds several packages.
    ///
    /// Answers a different question from `detected_stack` — that one is
    /// "what is this built with", this is "which of the several things in
    /// here can be deployed" — and a repository can have either, both or
    /// neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<crate::services::stack::Monorepo>,
}

impl Project {
    /// Validates and constructs a project.
    ///
    /// # Errors
    /// Returns [`DomainError::Invalid`] for an empty name.
    pub fn new(
        id: ProjectId,
        name: impl Into<String>,
        repo_url: RepoUrl,
        config: ProjectConfig,
    ) -> Result<Self, crate::DomainError> {
        let name = name.into();
        if name.trim().is_empty() {
            return invalid("project name cannot be empty");
        }

        Ok(Self {
            id,
            name,
            repo_url,
            config,
            detected_stack: None,
            workspace: None,
        })
    }

    /// Records what the repository was detected to be built with.
    ///
    /// Separate from [`Project::new`] because detection is not part of
    /// being a valid project: the overwhelming majority carry a Dockerfile
    /// and are never detected at all.
    #[must_use]
    pub fn with_detected_stack(mut self, stack: Option<crate::services::stack::Stack>) -> Self {
        self.detected_stack = stack;
        self
    }

    /// Records the workspace a repository turned out to be.
    #[must_use]
    pub fn with_workspace(mut self, workspace: Option<crate::services::stack::Monorepo>) -> Self {
        self.workspace = workspace;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::project_config::{BuildConfig, Dependency};
    use crate::domain::value_objects::Ttl;

    fn ttl(m: u64) -> Ttl {
        Ttl::parse(format!("{m}m")).unwrap()
    }

    fn config() -> ProjectConfig {
        ProjectConfig::new(
            "my-awesome-api.local.dev",
            ttl(30),
            ttl(7 * 24 * 60),
            8080,
            BuildConfig::default(),
            vec![Dependency {
                kind: crate::domain::PoolKind::Postgres,
                shared_instance: "local-pg-cluster".to_owned(),
                inject_url_as: "DATABASE_URL".to_owned(),
            }],
        )
        .unwrap()
    }

    #[test]
    fn builds_valid_project() {
        let project = Project::new(
            ProjectId(1),
            "my-awesome-api",
            RepoUrl::parse("https://github.com/org/my-awesome-api.git").unwrap(),
            config(),
        )
        .unwrap();

        assert_eq!(project.name, "my-awesome-api");
        assert_eq!(project.config.pause_after.whole_seconds(), 1_800);
        assert_eq!(project.config.dependencies.len(), 1);
    }

    #[test]
    fn rejects_empty_name() {
        let url = RepoUrl::parse("https://github.com/org/repo.git").unwrap();
        assert!(Project::new(ProjectId(1), "", url, config()).is_err());
    }
}
