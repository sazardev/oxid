//! Parsing of `oxid.toml` into domain configuration.
//!
//! The schema mirrors the blocks described in IDEA.md: `[project]`, `[build]`,
//! `[routing]` and `[dependencies.<name>]`. Only declared fields are mapped;
//! `ProjectId` and `RepoUrl` are supplied by the caller since they come from
//! the Git repository, not the config file.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use oxid_core::{BuildConfig, Dependency, DomainError, PoolKind, ProjectConfig, Ttl};

/// A parsed `oxid.toml`, ready for [`oxid_core::Project::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedProject {
    /// Project name (`[project] name`).
    pub name: String,
    /// Domain configuration validated through [`ProjectConfig`].
    pub config: ProjectConfig,
}

/// Errors raised while reading or interpreting `oxid.toml`.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("failed to read `{path}`: {source}")]
    Io {
        /// Path that could not be read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// The file is not valid TOML.
    #[error("invalid oxid.toml: {0}")]
    Parse(#[from] toml::de::Error),
    /// The file is valid TOML but violates domain rules or is incomplete.
    #[error("{0}")]
    Validation(#[from] DomainError),
}

/// Raw mirror of the `oxid.toml` schema. Unknown keys are ignored so future
/// versions stay forward-compatible.
#[derive(Debug, Default, Deserialize)]
struct Config {
    #[serde(default)]
    project: ProjectBlock,
    #[serde(default)]
    build: BuildBlock,
    #[serde(default)]
    routing: RoutingBlock,
    #[serde(default)]
    dependencies: BTreeMap<String, DependencyBlock>,
}

#[derive(Debug, Default, Deserialize)]
struct ProjectBlock {
    name: Option<String>,
    pause_after: Option<String>,
    destroy_after: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct BuildBlock {
    dockerfile: Option<String>,
    context: Option<String>,
    #[serde(default)]
    on_start: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RoutingBlock {
    base_domain: Option<String>,
    port: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct DependencyBlock {
    #[serde(rename = "type")]
    kind: String,
    shared_instance: String,
    inject_url_as: String,
}

/// Defaults applied when a block or field is absent (IDEA.md).
const DEFAULT_PAUSE_AFTER: &str = "30m";
const DEFAULT_DESTROY_AFTER: &str = "7d";
const DEFAULT_CONTEXT: &str = ".";

impl Config {
    fn into_domain(self) -> Result<ParsedProject, ConfigError> {
        let name = required(self.project.name, "[project] name")?;
        let base_domain = required(self.routing.base_domain, "[routing] base_domain")?;
        let port = required(self.routing.port, "[routing] port")?;

        let pause_after = match self.project.pause_after {
            Some(raw) => Ttl::parse(raw)?,
            None => Ttl::parse(DEFAULT_PAUSE_AFTER).expect("static default is valid"),
        };
        let destroy_after = match self.project.destroy_after {
            Some(raw) => Ttl::parse(raw)?,
            None => Ttl::parse(DEFAULT_DESTROY_AFTER).expect("static default is valid"),
        };

        let build = BuildConfig {
            dockerfile: self.build.dockerfile,
            context: self
                .build
                .context
                .unwrap_or_else(|| DEFAULT_CONTEXT.to_owned()),
            on_start: self.build.on_start,
        };

        let mut dependencies = Vec::with_capacity(self.dependencies.len());
        for (name, block) in self.dependencies {
            let kind: PoolKind = block.kind.parse()?;
            dependencies.push(Dependency {
                kind,
                shared_instance: block.shared_instance,
                inject_url_as: block.inject_url_as,
            });
            let _ = name; // friendly name is informative but not stored
        }

        let config = ProjectConfig::new(
            base_domain,
            pause_after,
            destroy_after,
            port,
            build,
            dependencies,
        )?;

        Ok(ParsedProject { name, config })
    }
}

fn required<T>(value: Option<T>, what: &str) -> Result<T, ConfigError> {
    value.ok_or_else(|| {
        ConfigError::Validation(DomainError::Invalid(format!(
            "oxid.toml is missing `{what}`"
        )))
    })
}

/// Parses `oxid.toml` from a string.
///
/// # Errors
/// Returns [`ConfigError::Parse`] for invalid TOML or
/// [`ConfigError::Validation`] when required fields are missing or values are
/// invalid.
pub fn parse_str(input: &str) -> Result<ParsedProject, ConfigError> {
    let config: Config = toml::from_str(input)?;
    config.into_domain()
}

/// Reads and parses an `oxid.toml` file.
///
/// # Errors
/// Returns [`ConfigError::Io`] when the file cannot be read, plus the
/// [`ConfigError::Parse`] / [`ConfigError::Validation`] cases of
/// [`parse_str`].
pub fn parse_file(path: impl AsRef<Path>) -> Result<ParsedProject, ConfigError> {
    let path = path.as_ref();
    let content = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_owned(),
        source,
    })?;
    parse_str(&content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxid_core::PoolKind;

    const FULL: &str = r#"
[project]
name = "my-awesome-api"
pause_after = "30m"
destroy_after = "7d"

[build]
dockerfile = "deploy/Dockerfile.dev"
context = "deploy"
on_start = ["npm run db:migrate", "npm run db:seed"]

[routing]
base_domain = "my-awesome-api.local.dev"
port = 8080

[dependencies.database]
type = "postgres"
shared_instance = "local-pg-cluster"
inject_url_as = "DATABASE_URL"

[dependencies.cache]
type = "redis"
shared_instance = "local-redis-cluster"
inject_url_as = "REDIS_URL"
"#;

    #[test]
    fn parses_full_config() {
        let parsed = parse_str(FULL).unwrap();

        assert_eq!(parsed.name, "my-awesome-api");
        assert_eq!(parsed.config.base_domain, "my-awesome-api.local.dev");
        assert_eq!(parsed.config.pause_after.whole_seconds(), 1_800);
        assert_eq!(parsed.config.destroy_after.whole_seconds(), 604_800);
        assert_eq!(parsed.config.port, 8080);
        assert_eq!(
            parsed.config.build.dockerfile.as_deref(),
            Some("deploy/Dockerfile.dev")
        );
        assert_eq!(parsed.config.build.context, "deploy");
        assert_eq!(parsed.config.build.on_start.len(), 2);

        assert_eq!(parsed.config.dependencies.len(), 2);
        let db = parsed
            .config
            .dependencies
            .iter()
            .find(|d| d.shared_instance == "local-pg-cluster")
            .expect("postgres dependency present");
        assert_eq!(db.kind, PoolKind::Postgres);
        assert_eq!(db.shared_instance, "local-pg-cluster");
        assert_eq!(db.inject_url_as, "DATABASE_URL");
        let cache = parsed
            .config
            .dependencies
            .iter()
            .find(|d| d.shared_instance == "local-redis-cluster")
            .expect("redis dependency present");
        assert_eq!(cache.kind, PoolKind::Redis);
    }

    #[test]
    fn applies_defaults_for_omitted_blocks() {
        let toml = r#"
[project]
name = "minimal"

[routing]
base_domain = "minimal.local.dev"
port = 3000
"#;
        let parsed = parse_str(toml).unwrap();

        assert_eq!(parsed.name, "minimal");
        assert_eq!(parsed.config.pause_after.whole_seconds(), 1_800); // 30m
        assert_eq!(parsed.config.destroy_after.whole_seconds(), 604_800); // 7d
        assert_eq!(parsed.config.build.context, ".");
        assert!(parsed.config.build.on_start.is_empty());
        assert!(parsed.config.build.dockerfile.is_none());
        assert!(parsed.config.dependencies.is_empty());
    }

    #[test]
    fn rejects_missing_required_fields() {
        assert!(parse_str("[project]\nname = \"x\"\n").is_err());
        assert!(parse_str("[routing]\nbase_domain = \"x\"\nport = 1\n").is_err());
        assert!(parse_str("[project]\nname = \"x\"\n[routing]\nport = 1\n").is_err());
    }

    #[test]
    fn rejects_invalid_duration() {
        let toml = r#"
[project]
name = "x"
pause_after = "nope"

[routing]
base_domain = "x.local.dev"
port = 1
"#;
        let err = parse_str(toml).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)), "{err:?}");
    }

    #[test]
    fn rejects_unknown_pool_kind() {
        let toml = r#"
[project]
name = "x"

[routing]
base_domain = "x.local.dev"
port = 1

[dependencies.database]
type = "mysql"
shared_instance = "s"
inject_url_as = "URL"
"#;
        let err = parse_str(toml).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)), "{err:?}");
    }

    #[test]
    fn rejects_invalid_toml() {
        let err = parse_str("this is [ not : toml ]").unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)), "{err:?}");
    }

    #[test]
    fn parses_from_file() {
        let path = std::env::temp_dir().join(format!("oxid-test-{}.toml", std::process::id()));
        std::fs::write(&path, FULL).unwrap();
        let parsed = parse_file(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(parsed.name, "my-awesome-api");
    }

    #[test]
    fn reports_missing_file() {
        let err = parse_file("does-not-exist.toml").unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }), "{err:?}");
    }
}
