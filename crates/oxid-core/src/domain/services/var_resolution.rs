//! Environment-variable resolution across scopes.

use std::collections::BTreeMap;

use crate::domain::secret_context::{EnvVarScope, SecretContext, SecretValue};

/// Input for variable resolution: the four scopes of the inheritance matrix.
#[derive(Debug, Clone, Default)]
pub struct VarSources {
    /// `Global` secrets for the node.
    pub global: SecretContext,
    /// `Project` secrets.
    pub project: SecretContext,
    /// `Branch` secrets.
    pub branch: SecretContext,
    /// `Runtime` secrets injected by the orchestrator.
    pub runtime: SecretContext,
}

impl VarSources {
    /// Resolves the full effective environment for a deployment.
    ///
    /// Precedence: `Global -> Project -> Branch -> Runtime` (SPEC.md §2.1).
    #[must_use]
    pub fn resolve(&self) -> BTreeMap<String, SecretValue> {
        self.global
            .clone()
            .merge([
                self.project.clone(),
                self.branch.clone(),
                self.runtime.clone(),
            ])
            .resolved_map()
    }

    /// Resolves a single key across the matrix, or `None` if undefined.
    #[must_use]
    pub fn resolve_key(&self, key: &str) -> Option<&SecretValue> {
        [&self.runtime, &self.branch, &self.project, &self.global]
            .into_iter()
            .find_map(|ctx| ctx.resolve(key))
    }
}

/// Convenience to set a secret in a source context.
pub fn set_secret(
    sources: &mut VarSources,
    key: &str,
    scope: EnvVarScope,
    value: impl Into<String>,
) {
    let value = SecretValue::new(value);
    match scope {
        EnvVarScope::Global => sources.global.set(key, scope, value),
        EnvVarScope::Project => sources.project.set(key, scope, value),
        EnvVarScope::Branch => sources.branch.set(key, scope, value),
        EnvVarScope::Runtime => sources.runtime.set(key, scope, value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources() -> VarSources {
        let mut s = VarSources::default();
        set_secret(&mut s, "DATABASE_URL", EnvVarScope::Global, "global://db");
        set_secret(&mut s, "DATABASE_URL", EnvVarScope::Project, "project://db");
        set_secret(&mut s, "DATABASE_URL", EnvVarScope::Branch, "branch://db");
        set_secret(&mut s, "DATABASE_URL", EnvVarScope::Runtime, "runtime://db");
        set_secret(&mut s, "ONLY_GLOBAL", EnvVarScope::Global, "g");
        s
    }

    #[test]
    fn runtime_wins_over_global() {
        let s = sources();
        assert_eq!(
            s.resolve_key("DATABASE_URL").unwrap().as_str(),
            "runtime://db"
        );
    }

    #[test]
    fn resolved_map_contains_all_keys() {
        let map = sources().resolve();
        assert_eq!(map.len(), 2);
        assert_eq!(map["DATABASE_URL"].as_str(), "runtime://db");
        assert_eq!(map["ONLY_GLOBAL"].as_str(), "g");
    }

    #[test]
    fn missing_key_returns_none() {
        assert_eq!(sources().resolve_key("NOPE"), None);
    }
}
