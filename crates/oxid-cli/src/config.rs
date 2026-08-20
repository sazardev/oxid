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
pub fn load() -> Result<Config, String> {
    let Some(path) = config_path() else {
        return Ok(Config::default());
    };
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            toml::from_str(&contents).map_err(|e| format!("cannot parse {}: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Writes the config file, creating `~/.config/oxid/` if needed.
pub fn save(config: &Config) -> Result<(), String> {
    let path = config_path().ok_or_else(|| "cannot determine config directory".to_owned())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let contents =
        toml::to_string_pretty(config).map_err(|e| format!("cannot serialize config: {e}"))?;
    std::fs::write(&path, contents).map_err(|e| format!("cannot write {}: {e}", path.display()))
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
