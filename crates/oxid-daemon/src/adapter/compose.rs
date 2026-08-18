//! Minimal `docker-compose.yml` parsing for the zero-config path (IDEA.md:
//! _"Si hay un Dockerfile y un docker-compose.yml, Oxid sabe qué hacer"_).
//!
//! Deliberately narrow: Oxid runs one container per branch and multiplexes
//! shared dependencies instead of orchestrating a whole Compose stack per
//! environment (SPEC.md §3.1 — the entire point is *not* booting a
//! database-per-branch container). So this only extracts the first service
//! that declares a `build:` key; an `image:`-only service listed alongside
//! it (a local dev Postgres, say) is intentionally ignored, not deployed.

use std::path::Path;

use yaml_rust2::{Yaml, YamlLoader};

use crate::adapter::config::ConfigError;

/// The one buildable service extracted from a compose file.
pub struct ComposeService {
    /// Dockerfile path, relative to `context`.
    pub dockerfile: String,
    /// Build context directory, relative to the compose file's directory.
    pub context: String,
    /// Container-side port from the first `ports:` entry, if any.
    pub port: Option<u16>,
}

/// Parses `compose_path` looking for the first service with a `build:` key.
///
/// # Errors
/// Returns [`ConfigError::Io`] if the file can't be read, or
/// [`ConfigError::Compose`] if it's not valid YAML, has no top-level
/// `services:` mapping, or no service declares a `build:` key.
pub fn parse(compose_path: &Path) -> Result<ComposeService, ConfigError> {
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

    for (_name, service) in services {
        let build = &service["build"];
        let (context, dockerfile) = match build {
            Yaml::String(s) => (s.clone(), "Dockerfile".to_owned()),
            Yaml::Hash(_) => {
                let context = build["context"].as_str().unwrap_or(".").to_owned();
                let dockerfile = build["dockerfile"]
                    .as_str()
                    .unwrap_or("Dockerfile")
                    .to_owned();
                (context, dockerfile)
            }
            // No `build:` key at all (an `image:`-only service, e.g. a
            // database) — not what we're looking for, keep scanning.
            _ => continue,
        };
        let port = first_port(&service["ports"]);
        return Ok(ComposeService {
            dockerfile,
            context,
            port,
        });
    }

    Err(ConfigError::Compose {
        path: compose_path.to_owned(),
        reason: "no service declares a `build:` key (Oxid builds images, it doesn't pull \
                 pre-built ones — add `build: .` to the service you want deployed, or add an \
                 `oxid.toml` instead)"
            .to_owned(),
    })
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
        let svc = parse(&path).unwrap();
        assert_eq!(svc.context, ".");
        assert_eq!(svc.dockerfile, "Dockerfile");
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
        let svc = parse(&path).unwrap();
        assert_eq!(svc.context, "./backend");
        assert_eq!(svc.dockerfile, "Dockerfile.prod");
        assert_eq!(svc.port, Some(9090));
    }

    #[test]
    fn skips_image_only_services_and_picks_the_buildable_one() {
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
        let svc = parse(&path).unwrap();
        assert_eq!(svc.port, Some(3000));
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
        let svc = parse(&path).unwrap();
        assert_eq!(svc.port, None);
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
