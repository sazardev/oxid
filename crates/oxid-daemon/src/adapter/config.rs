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

use oxid_core::services::branch_filter::DeployConfig;
use oxid_core::services::compose_plan;
use oxid_core::services::stack::{Monorepo, RepoManifest, Stack, detect, detect_monorepo};
use oxid_core::{BuildConfig, Dependency, DomainError, PoolKind, ProjectConfig, Ttl};

/// A parsed `oxid.toml`, ready for [`oxid_core::Project::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedProject {
    /// Project name (`[project] name`).
    pub name: String,
    /// Domain configuration validated through [`ProjectConfig`].
    pub config: ProjectConfig,
    /// What the repository was detected to be built with, when Oxid had to
    /// work it out rather than being told. `None` means the repository
    /// answered for itself — an `oxid.toml`, a Compose file or a
    /// `Dockerfile` — and nothing was inferred.
    pub stack: Option<Stack>,
    /// The workspace, when the repository holds several packages.
    ///
    /// Detected even when the repository has its own `oxid.toml` or
    /// `Dockerfile`: which services live in a monorepo is worth telling an
    /// operator regardless of who wrote the build, and it is what makes
    /// `[build].context` a choice rather than a guess.
    pub monorepo: Option<Monorepo>,
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
    /// `docker-compose.yml`/`compose.yml` exists but couldn't be parsed
    /// enough to find a buildable service.
    #[error("could not read `{path}` as a Compose file: {reason}")]
    Compose {
        /// The compose file that failed to parse.
        path: PathBuf,
        /// What went wrong.
        reason: String,
    },
    /// Nothing Oxid knows how to build was found in the repo.
    #[error(
        "no `oxid.toml`, `docker-compose.yml` or `Dockerfile` found in `{repo_dir}`; Oxid needs \
         at least a `Dockerfile` to build your app. Did you mean to run this from your \
         repository's root?"
    )]
    NoConfigFound {
        /// The directory that was searched.
        repo_dir: PathBuf,
    },
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
    #[serde(default)]
    deploy: DeployBlock,
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
    memory_limit_mb: Option<u64>,
    cpu_limit_millicores: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
struct RoutingBlock {
    base_domain: Option<String>,
    port: Option<u16>,
}

/// `[deploy]` — which branches a push may deploy, and how many environments
/// this project may hold. Every field is optional and an absent block means
/// "every branch, no cap", which is what a project had before this existed.
#[derive(Debug, Default, Deserialize)]
struct DeployBlock {
    #[serde(default)]
    branches: Vec<String>,
    #[serde(default)]
    ignore: Vec<String>,
    max_environments: Option<u32>,
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
        let name = required(
            self.project.name,
            "[project] name",
            r#"add it under `[project]`, e.g. `name = "my-app"`"#,
        )?;
        let base_domain = required(
            self.routing.base_domain,
            "[routing] base_domain",
            r#"add it under `[routing]`, e.g. `base_domain = "preview.mycompany.dev"`"#,
        )?;
        let port = required(
            self.routing.port,
            "[routing] port",
            "add it under `[routing]`, e.g. `port = 8080` — the container port your app listens on",
        )?;

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
            memory_limit_mb: self.build.memory_limit_mb,
            cpu_limit_millicores: self.build.cpu_limit_millicores,
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
        )?
        .with_deploy(DeployConfig {
            branches: self.deploy.branches,
            ignore: self.deploy.ignore,
            max_environments: self.deploy.max_environments,
        });

        Ok(ParsedProject {
            name,
            config,
            stack: None,
            monorepo: None,
        })
    }
}

fn required<T>(value: Option<T>, what: &str, hint: &str) -> Result<T, ConfigError> {
    value.ok_or_else(|| {
        ConfigError::Validation(DomainError::Invalid(format!(
            "oxid.toml is missing `{what}` — {hint}"
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

/// Port assumed when nothing in the repo says otherwise (no `EXPOSE`, no
/// Compose `ports:`). 8080 is the single most common convention across the
/// example stacks Oxid targets (Node, Python, Go).
const DEFAULT_ZERO_CONFIG_PORT: u16 = 8080;

/// Resolves the project declared for `repo_dir`: an explicit `oxid.toml` if
/// present, otherwise a zero-config guess (IDEA.md's "fricción cero": _"Si
/// hay un Dockerfile... Oxid sabe qué hacer"_) from, in order,
/// `docker-compose.yml`/`compose.yml` or a bare `Dockerfile`.
///
/// The zero-config path infers: `name` from the repository directory name,
/// `base_domain` as `<name>.local.dev`, and `port` from the Dockerfile's
/// `EXPOSE` directive (or the first Compose `ports:` entry), falling back to
/// `8080`. `pause_after`/`destroy_after` use the same defaults `oxid.toml`
/// would. Placing an `oxid.toml` in the repo always overrides this — this
/// is a starting point, not a replacement for it.
///
/// # Errors
/// Returns [`ConfigError::NoConfigFound`] if none of the three exist, plus
/// whatever [`parse_file`]/Compose parsing would return for a malformed one.
pub fn parse_project(repo_dir: &Path) -> Result<ParsedProject, ConfigError> {
    // Read once, used by every branch below: which services a monorepo
    // holds is worth reporting whether or not Oxid had to guess the build.
    let manifest = read_repo_manifest(repo_dir);
    let monorepo = detect_monorepo(&manifest);

    let toml_path = repo_dir.join("oxid.toml");
    if toml_path.exists() {
        return parse_file(toml_path).map(|mut parsed| {
            parsed.monorepo = monorepo;
            parsed
        });
    }

    let name = derive_project_name(repo_dir);

    for candidate in [
        "docker-compose.yml",
        "docker-compose.yaml",
        "compose.yml",
        "compose.yaml",
    ] {
        let compose_path = repo_dir.join(candidate);
        if compose_path.exists() {
            let stack = crate::adapter::compose::parse(&compose_path)?;
            let plan = compose_plan::plan(&stack.services, None);
            // The primary is the service that takes the branch URL, and the
            // one this project's single `[build]`/`[routing].port` describe.
            // `compose::parse` refuses a stack with nothing to build, and
            // `plan` only ever nominates a buildable service, so a primary
            // exists here by construction.
            let primary = plan.primary().ok_or_else(|| ConfigError::Compose {
                path: compose_path.clone(),
                reason: "no service could be deployed".to_owned(),
            })?;
            let compose_plan::Disposition::Build(build) = &primary.disposition else {
                return Err(ConfigError::Compose {
                    path: compose_path.clone(),
                    reason: "the primary service does not build".to_owned(),
                });
            };
            let port = primary
                .port
                .or_else(|| {
                    read_exposed_port(&repo_dir.join(&build.context).join(&build.dockerfile))
                })
                .unwrap_or(DEFAULT_ZERO_CONFIG_PORT);
            return zero_config_project(
                name,
                Some(build.dockerfile.clone()),
                build.context.clone(),
                port,
            )
            .map(|mut parsed| {
                parsed.monorepo = monorepo;
                parsed
            });
        }
    }

    let dockerfile_path = repo_dir.join("Dockerfile");
    if dockerfile_path.exists() {
        let port = read_exposed_port(&dockerfile_path).unwrap_or(DEFAULT_ZERO_CONFIG_PORT);
        return zero_config_project(name, None, DEFAULT_CONTEXT.to_owned(), port).map(
            |mut parsed| {
                parsed.monorepo = monorepo;
                parsed
            },
        );
    }

    // Last: work out what this is. Deliberately after every explicit
    // answer, so detection only ever fills a gap — a committed Dockerfile
    // is a decision someone made and is never second-guessed.
    // A monorepo root usually builds nothing itself. Pointing the default
    // context at the first deployable service is a far better starting
    // guess than the root, which typically has no app in it at all — and
    // the operator sees which one was chosen and can name another.
    if let Some(mono) = &monorepo
        && let Some(first) = mono.deployable.first()
    {
        let mut parsed = zero_config_project(name, None, first.path.clone(), first.port)?;
        parsed.monorepo = monorepo;
        return Ok(parsed);
    }

    if let Some(stack) = detect(&manifest) {
        let mut parsed = zero_config_project(name, None, DEFAULT_CONTEXT.to_owned(), stack.port)?;
        parsed.stack = Some(stack);
        parsed.monorepo = monorepo;
        return Ok(parsed);
    }

    Err(ConfigError::NoConfigFound {
        repo_dir: repo_dir.to_owned(),
    })
}

/// Describes a repository for [`detect`] and [`detect_monorepo`].
///
/// Bounded on purpose. The root is listed, plus one level inside each
/// conventional workspace directory — enough to find `apps/api/package.json`
/// without walking a repository that may hold a hundred thousand files. The
/// authoritative member list comes from those `package.json` files
/// themselves, so nothing is missed that Oxid could act on.
#[must_use]
pub fn read_repo_manifest(root: &Path) -> RepoManifest {
    let mut manifest = RepoManifest::default();
    let Ok(entries) = std::fs::read_dir(root) else {
        return manifest;
    };
    for entry in entries.flatten() {
        manifest
            .entries
            .push(entry.file_name().to_string_lossy().into_owned());
    }
    for name in RepoManifest::files_worth_reading() {
        if manifest.entries.iter().any(|e| e == name)
            && let Ok(body) = std::fs::read_to_string(root.join(name))
        {
            manifest.files.insert((*name).to_owned(), body);
        }
    }

    // One level into `apps/`, `packages/` and friends, for the member
    // manifests a monorepo is described by.
    for group in RepoManifest::workspace_roots() {
        if !manifest.entries.iter().any(|e| e == group) {
            continue;
        }
        let Ok(members) = std::fs::read_dir(root.join(group)) else {
            continue;
        };
        for member in members.flatten() {
            let dir = member.file_name().to_string_lossy().into_owned();
            let package = format!("{group}/{dir}/package.json");
            if let Ok(body) = std::fs::read_to_string(root.join(&package)) {
                manifest.entries.push(package.clone());
                manifest.files.insert(package, body);
            }
        }
    }
    manifest
}

fn zero_config_project(
    name: String,
    dockerfile: Option<String>,
    context: String,
    port: u16,
) -> Result<ParsedProject, ConfigError> {
    let base_domain = format!("{name}.local.dev");
    let pause_after = Ttl::parse(DEFAULT_PAUSE_AFTER).expect("static default is valid");
    let destroy_after = Ttl::parse(DEFAULT_DESTROY_AFTER).expect("static default is valid");
    let build = BuildConfig {
        dockerfile,
        context,
        on_start: Vec::new(),
        memory_limit_mb: None,
        cpu_limit_millicores: None,
    };
    let config = ProjectConfig::new(
        base_domain,
        pause_after,
        destroy_after,
        port,
        build,
        Vec::new(),
    )?;
    Ok(ParsedProject {
        name,
        config,
        stack: None,
        monorepo: None,
    })
}

/// Derives a safe project name from `repo_dir`'s own directory name
/// (lowercase `[a-z0-9-]`, collapsing anything else to `-`), falling back to
/// `"app"` if the path has no usable last component (e.g. `/`).
fn derive_project_name(repo_dir: &Path) -> String {
    let raw = repo_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "app".to_owned());
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    if sanitized.trim_matches('-').is_empty() {
        "app".to_owned()
    } else {
        sanitized
    }
}

/// Reads the port from a Dockerfile's first `EXPOSE` directive (e.g.
/// `EXPOSE 8080` or `EXPOSE 8080/tcp`), if any. Best-effort: a missing or
/// unparsable file just means "no hint found", not an error — the caller
/// falls back to [`DEFAULT_ZERO_CONFIG_PORT`].
fn read_exposed_port(dockerfile: &Path) -> Option<u16> {
    let content = std::fs::read_to_string(dockerfile).ok()?;
    content.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("EXPOSE")?;
        let port_str = rest.split_whitespace().next()?;
        let port_str = port_str.split('/').next()?;
        port_str.parse().ok()
    })
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
memory_limit_mb = 256
cpu_limit_millicores = 500

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
        assert_eq!(parsed.config.build.memory_limit_mb, Some(256));
        assert_eq!(parsed.config.build.cpu_limit_millicores, Some(500));

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
        assert!(parsed.config.build.memory_limit_mb.is_none());
        assert!(parsed.config.build.cpu_limit_millicores.is_none());
        assert!(parsed.config.dependencies.is_empty());
    }

    #[test]
    fn rejects_missing_required_fields() {
        assert!(parse_str("[project]\nname = \"x\"\n").is_err());
        assert!(parse_str("[routing]\nbase_domain = \"x\"\nport = 1\n").is_err());
        assert!(parse_str("[project]\nname = \"x\"\n[routing]\nport = 1\n").is_err());
    }

    /// Missing-field errors must be rustc-style (DESIGN.md §5): say what's
    /// missing AND how to fix it, not just that something is wrong.
    #[test]
    fn missing_field_errors_carry_an_actionable_example() {
        let err = parse_str("[project]\nname = \"x\"\n[routing]\nport = 1\n").unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("`[routing] base_domain`") && message.contains("e.g."),
            "{message}"
        );
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

    #[test]
    fn parse_project_prefers_oxid_toml_when_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("oxid.toml"), FULL).unwrap();
        // Also drop a Dockerfile with a *different* port, to prove
        // `oxid.toml` wins over the zero-config guess, not the other way
        // around.
        std::fs::write(dir.path().join("Dockerfile"), "FROM alpine\nEXPOSE 1234\n").unwrap();

        let parsed = parse_project(dir.path()).unwrap();
        assert_eq!(parsed.name, "my-awesome-api");
        assert_eq!(parsed.config.port, 8080);
    }

    #[test]
    fn a_repository_with_no_dockerfile_is_detected_rather_than_refused() {
        // The demand this removes: a team adding preview environments to an
        // existing NestJS service had to become Docker authors first.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"dependencies":{"@nestjs/core":"^10.0.0"}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("pnpm-lock.yaml"), "lockfileVersion: 9\n").unwrap();

        let parsed = parse_project(dir.path()).unwrap();
        let stack = parsed.stack.expect("nothing was detected");
        assert_eq!(stack.framework.as_str(), "nestjs");
        assert_eq!(stack.package_manager.unwrap().as_str(), "pnpm");
        // The port comes from the framework, not from Oxid's default.
        assert_eq!(parsed.config.port, 3000);
    }

    #[test]
    fn a_committed_dockerfile_is_never_second_guessed() {
        // Detection fills a gap; it does not overrule a decision someone
        // made. A Nest repo whose Dockerfile exposes 9000 is a repo that
        // serves on 9000, whatever Nest's default is.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"dependencies":{"@nestjs/core":"^10.0.0"}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM node:22\nEXPOSE 9000\n").unwrap();

        let parsed = parse_project(dir.path()).unwrap();
        assert_eq!(parsed.config.port, 9000);
        assert!(
            parsed.stack.is_none(),
            "detection ran even though the repository answered for itself"
        );
    }

    #[test]
    fn an_unrecognisable_repository_still_asks_for_a_dockerfile() {
        // Better than a generated build that dies halfway through for
        // reasons the developer has to reverse-engineer.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "# notes").unwrap();
        assert!(matches!(
            parse_project(dir.path()),
            Err(ConfigError::NoConfigFound { .. })
        ));
    }

    #[test]
    fn parse_project_zero_config_from_bare_dockerfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Dockerfile"),
            "FROM node:20-alpine\nEXPOSE 3000\nCMD [\"node\", \"server.js\"]\n",
        )
        .unwrap();

        let parsed = parse_project(dir.path()).unwrap();
        assert_eq!(parsed.config.port, 3000);
        assert!(parsed.config.base_domain.ends_with(".local.dev"));
        assert!(parsed.config.dependencies.is_empty());
    }

    #[test]
    fn parse_project_zero_config_falls_back_to_default_port_without_expose() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Dockerfile"),
            "FROM alpine\nCMD [\"true\"]\n",
        )
        .unwrap();

        let parsed = parse_project(dir.path()).unwrap();
        assert_eq!(parsed.config.port, DEFAULT_ZERO_CONFIG_PORT);
    }

    #[test]
    fn parse_project_derives_name_from_directory_and_sanitizes_it() {
        let dir = tempfile::tempdir().unwrap();
        let weird = dir.path().join("My Cool App!!");
        std::fs::create_dir(&weird).unwrap();
        std::fs::write(weird.join("Dockerfile"), "FROM alpine\n").unwrap();

        let parsed = parse_project(&weird).unwrap();
        assert_eq!(parsed.name, "my-cool-app--");
        assert_eq!(parsed.config.base_domain, "my-cool-app--.local.dev");
    }

    #[test]
    fn parse_project_prefers_compose_over_bare_dockerfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM alpine\nEXPOSE 1111\n").unwrap();
        std::fs::write(
            dir.path().join("docker-compose.yml"),
            "services:\n  app:\n    build: .\n    ports:\n      - \"5555:5555\"\n",
        )
        .unwrap();

        let parsed = parse_project(dir.path()).unwrap();
        assert_eq!(parsed.config.port, 5555);
    }

    #[test]
    fn parse_project_compose_context_and_dockerfile_are_relative_to_repo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("backend")).unwrap();
        std::fs::write(
            dir.path().join("backend").join("Dockerfile.prod"),
            "FROM alpine\nEXPOSE 4242\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("docker-compose.yml"),
            "services:\n  app:\n    build:\n      context: ./backend\n      dockerfile: \
             Dockerfile.prod\n",
        )
        .unwrap();

        let parsed = parse_project(dir.path()).unwrap();
        // No `ports:` in the compose file, so the port hint must come from
        // the Dockerfile the compose service actually points at.
        assert_eq!(parsed.config.port, 4242);
        assert_eq!(parsed.config.build.context, "./backend");
        assert_eq!(
            parsed.config.build.dockerfile.as_deref(),
            Some("Dockerfile.prod")
        );
    }

    #[test]
    fn parse_project_errors_helpfully_when_nothing_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = parse_project(dir.path()).unwrap_err();
        assert!(matches!(err, ConfigError::NoConfigFound { .. }), "{err:?}");
        assert!(err.to_string().contains("Dockerfile"), "{err}");
    }
}
