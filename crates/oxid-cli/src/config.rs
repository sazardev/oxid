//! Persistent, multi-context CLI configuration (`kubectl config`-style).
//!
//! Lives at `~/.config/oxid/config.toml` and holds a set of named contexts,
//! each pointing at a daemon (`api`) with an optional bearer `token`, plus
//! which one is `current_context`. `--api`/`--token`/`OXID_API`/`OXID_TOKEN`
//! all take precedence over this file — it's only consulted as the last
//! fallback before the hardcoded default, so a single machine can juggle
//! several daemons (e.g. `staging`, `prod`, a local one) without having to
//! re-type `--api` on every invocation.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Context {
    pub api: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_context: Option<String>,
    #[serde(default)]
    pub contexts: BTreeMap<String, Context>,
}

/// Resolves `~/.config/oxid/config.toml`, honoring `$HOME`/platform data
/// dirs via `dirs::config_dir()`. Returns `None` when no home directory can
/// be determined at all (e.g. a stripped-down container) — callers treat
/// that the same as "no config file exists".
pub fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("oxid").join("config.toml"))
}

/// Loads the config file, treating a missing file as an empty config rather
/// than an error — there's nothing to migrate or initialize up front.
///
/// As a side effect, tightens lax file permissions on an existing file
/// (see [`enforce_owner_only`]) — it stores bearer tokens, and parity with
/// the daemon's own `secret.key` handling is deliberate.
pub fn load() -> Result<Config, String> {
    let Some(path) = config_path() else {
        return Ok(Config::default());
    };
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    enforce_owner_only(&path)?;
    toml::from_str(&contents).map_err(|e| format!("cannot parse {}: {e}", path.display()))
}

/// Writes the config file, creating `~/.config/oxid/` if needed. The file
/// always ends up owner-only (`0600`) — it holds bearer tokens, and a
/// world-readable token on a shared machine is as good as handing the
/// daemon to whoever can `cat` your home directory.
pub fn save(config: &Config) -> Result<(), String> {
    let path = config_path().ok_or_else(|| "cannot determine config directory".to_owned())?;
    write_config_at(&path, config)
}

fn write_config_at(path: &Path, config: &Config) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let contents =
        toml::to_string_pretty(config).map_err(|e| format!("cannot serialize config: {e}"))?;
    std::fs::write(path, contents).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    force_owner_only(path)
}

/// The active context, if any is configured and it still exists.
pub fn current(config: &Config) -> Option<&Context> {
    config
        .current_context
        .as_deref()
        .and_then(|name| config.contexts.get(name))
}

/// Masks a token for display: keeps only the last 4 characters, so
/// `oxid context list` never prints a usable secret to a terminal/log.
pub fn mask_token(token: &str) -> String {
    if token.len() <= 4 {
        "*".repeat(token.len())
    } else {
        format!(
            "{}{}",
            "*".repeat(token.len() - 4),
            &token[token.len() - 4..]
        )
    }
}

/// Sets `0600` unconditionally (used right after writing — no reason to
/// ever leave it looser).
#[cfg(unix)]
fn force_owner_only(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("cannot restrict permissions on {}: {e}", path.display()))
}

#[cfg(not(unix))]
fn force_owner_only(_path: &Path) -> Result<(), String> {
    // Windows ACLs don't map to the unix mode bits; the profile directory
    // is already user-private by default there.
    Ok(())
}

/// Warns about (and fixes) group/world-readable permissions on an existing
/// config file before its tokens are used. Auto-fix rather than warn-only:
/// the file is ours, the safe mode is unambiguous, and leaving a leaked
/// token in place after merely printing a warning the user may never see
/// would be security theater.
#[cfg(unix)]
fn enforce_owner_only(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let meta =
        std::fs::metadata(path).map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
    let mode = meta.permissions().mode();
    if mode & 0o077 != 0 {
        eprintln!(
            "[!] fixing lax permissions on {} (was {:03o}, setting 0600) — it stores bearer tokens",
            path.display(),
            mode & 0o777
        );
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("cannot tighten permissions on {}: {e}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn enforce_owner_only(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Written configs are owner-only from the start, not just after a
    /// later `load()` fixes them up.
    #[test]
    #[cfg(unix)]
    fn save_leaves_the_file_at_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config_at(
            &path,
            &Config {
                current_context: None,
                contexts: BTreeMap::from([(
                    "staging".to_owned(),
                    Context {
                        api: "http://s".to_owned(),
                        token: Some("t".to_owned()),
                    },
                )]),
            },
        )
        .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// A pre-existing world-readable file (e.g. written by an older Oxid or
    /// a careless `echo >`) gets tightened on the next `load()`.
    #[test]
    #[cfg(unix)]
    fn load_tightens_lax_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "contexts = {}\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        toml::from_str::<Config>(&contents).unwrap();
        enforce_owner_only(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
