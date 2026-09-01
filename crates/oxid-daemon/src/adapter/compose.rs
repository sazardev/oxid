//! Minimal `docker-compose.yml` parsing for the zero-config path (IDEA.md:
//! _"Si hay un Dockerfile y un docker-compose.yml, Oxid sabe qué hacer"_).
//!
//! This reads what the file says and decides nothing. What Oxid *does* with
//! each service — build it, fold it into a shared pool, or run it as
//! written — is three pure rules in
//! [`oxid_core::services::compose_plan`], so they can be tested without a
//! file and cannot drift from the YAML the adapter happens to collect.
//!
//! It used to return only the first service with a `build:` key and drop
//! the rest in silence, including the service *name*. An `api` + `worker` +
//! `db` stack deployed the api alone, with no warning, and the app failed
//! at runtime on a connection nobody had said would be missing. The name is
//! now the load-bearing part: it is the hostname siblings resolve each
//! other by.

use std::path::Path;

use oxid_core::services::compose_plan::{ComposeBuild, ComposeService};
use yaml_rust2::{Yaml, YamlLoader};

use crate::adapter::config::ConfigError;

/// Every service a compose file declares, in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeStack {
    /// In the order `services:` listed them — which is the order the plan
    /// preserves, so "the first one with a port" means what a person
    /// reading the file would expect.
    pub services: Vec<ComposeService>,
}

/// Parses `compose_path` into every service it declares.
///
/// # Errors
/// Returns [`ConfigError::Io`] if the file can't be read, or
/// [`ConfigError::Compose`] if it's not valid YAML, has no top-level
/// `services:` mapping, or no service declares a `build:` key.
pub fn parse(compose_path: &Path) -> Result<ComposeStack, ConfigError> {
    let raw = std::fs::read_to_string(compose_path).map_err(|source| ConfigError::Io {
        path: compose_path.to_owned(),
        source,
    })?;
    let docs = YamlLoader::load_from_str(&raw).map_err(|e| ConfigError::Compose {
        path: compose_path.to_owned(),
        reason: e.to_string(),
    })?;
    let doc = docs.first().ok_or_else(|| ConfigError::Compose {
        path: compose_path.to_owned(),
        reason: "file is empty".to_owned(),
    })?;

    let services = doc["services"]
        .as_hash()
        .ok_or_else(|| ConfigError::Compose {
            path: compose_path.to_owned(),
            reason: "missing a top-level `services:` mapping".to_owned(),
        })?;

    let mut parsed = Vec::with_capacity(services.len());
    for (name, service) in services {
        // A service key that is not a string is not a service. Compose
        // itself would reject the file; skipping is enough here.
        let Some(name) = name.as_str() else { continue };
        parsed.push(ComposeService {
            name: name.to_owned(),
            build: build_of(&service["build"]),
            image: service["image"].as_str().map(str::to_owned),
            port: first_port(&service["ports"]),
        });
    }

    // Still an error, and deliberately the same one: Oxid builds images, so
    // a stack with nothing to build is a stack with nothing of *this*
    // repository in it. Running someone's pinned images per branch is not
    // a preview environment, it is a different product.
    if !parsed.iter().any(|s| s.build.is_some()) {
        return Err(ConfigError::Compose {
            path: compose_path.to_owned(),
            reason: "no service declares a `build:` key (Oxid builds images, it doesn't pull \
                     pre-built ones — add `build: .` to the service you want deployed, or add an \
                     `oxid.toml` instead)"
                .to_owned(),
        });
    }

    Ok(ComposeStack { services: parsed })
}

/// A service's `build:` block, in either of Compose's notations.
fn build_of(build: &Yaml) -> Option<ComposeBuild> {
    match build {
        Yaml::String(s) => Some(ComposeBuild {
            context: s.clone(),
            dockerfile: "Dockerfile".to_owned(),
        }),
        Yaml::Hash(_) => Some(ComposeBuild {
            context: build["context"].as_str().unwrap_or(".").to_owned(),
            dockerfile: build["dockerfile"]
                .as_str()
                .unwrap_or("Dockerfile")
                .to_owned(),
        }),
        // No `build:` at all — an `image:`-only service. What happens to it
        // is the plan's decision, not this function's.
        _ => None,
    }
}

/// Reads the container-side port from the first `ports:` entry, in any of
/// Compose's notations: short string form (`"8080:8080"` or bare `"8080"`),
/// bare integer, or the long form (`{ target: 8080 }`).
fn first_port(ports: &Yaml) -> Option<u16> {
    let entry = ports.as_vec()?.first()?;
    match entry {
        Yaml::String(s) => s.rsplit(':').next()?.split('/').next()?.parse().ok(),
        Yaml::Integer(n) => u16::try_from(*n).ok(),
        Yaml::Hash(_) => entry["target"].as_i64().and_then(|n| u16::try_from(n).ok()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &tempfile::TempDir, content: &str) -> std::path::PathBuf {
        let path = dir.path().join("docker-compose.yml");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn parses_short_build_and_short_port() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            r#"
services:
  app:
    build: .
    ports:
      - "8080:8080"
"#,
        );
        let stack = parse(&path).unwrap();
        let svc = &stack.services[0];
        assert_eq!(svc.name, "app");
        let build = svc.build.as_ref().unwrap();
        assert_eq!(build.context, ".");
        assert_eq!(build.dockerfile, "Dockerfile");
        assert_eq!(svc.port, Some(8080));
    }

    #[test]
    fn parses_long_build_and_long_port() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            r"
services:
  app:
    build:
      context: ./backend
      dockerfile: Dockerfile.prod
    ports:
      - target: 9090
        published: 9090
",
        );
        let stack = parse(&path).unwrap();
        let svc = &stack.services[0];
        let build = svc.build.as_ref().unwrap();
        assert_eq!(build.context, "./backend");
        assert_eq!(build.dockerfile, "Dockerfile.prod");
        assert_eq!(svc.port, Some(9090));
    }

    /// Renamed from `skips_image_only_services_and_picks_the_buildable_one`,
    /// and the change of name is the change of behaviour: nothing is
    /// skipped any more. An `image:`-only service is *reported*, and what
    /// becomes of it — a shared-pool lease, or a container of its own — is
    /// `compose_plan`'s decision, not this parser's.
    #[test]
    fn image_only_services_are_reported_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            r#"
services:
  db:
    image: postgres:16
  app:
    build: .
    ports:
      - "3000:3000"
"#,
        );
        let stack = parse(&path).unwrap();
        assert_eq!(
            stack.services.len(),
            2,
            "both services must survive parsing"
        );
        let db = &stack.services[0];
        assert_eq!(db.name, "db");
        assert!(db.build.is_none());
        assert_eq!(db.image.as_deref(), Some("postgres:16"));
        let app = &stack.services[1];
        assert_eq!(app.name, "app");
        assert_eq!(app.port, Some(3000));
    }

    /// The name is the load-bearing field the old parser threw away: it is
    /// the hostname siblings resolve each other by inside the environment.
    #[test]
    fn every_service_keeps_its_name_and_file_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            r#"
services:
  api:
    build: .
    ports:
      - "3000:3000"
  worker:
    build:
      context: .
      dockerfile: Dockerfile.worker
  cache:
    image: redis:7
"#,
        );
        let stack = parse(&path).unwrap();
        assert_eq!(
            stack
                .services
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["api", "worker", "cache"]
        );
        assert_eq!(
            stack.services[1].build.as_ref().unwrap().dockerfile,
            "Dockerfile.worker"
        );
    }

    #[test]
    fn no_port_declared_is_fine_caller_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            r"
services:
  app:
    build: .
",
        );
        let stack = parse(&path).unwrap();
        assert_eq!(stack.services[0].port, None);
    }

    #[test]
    fn no_buildable_service_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            r"
services:
  db:
    image: postgres:16
",
        );
        assert!(matches!(parse(&path), Err(ConfigError::Compose { .. })));
    }

    #[test]
    fn invalid_yaml_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "not: [valid: yaml: at: all");
        assert!(matches!(parse(&path), Err(ConfigError::Compose { .. })));
    }
}
