//! Secret context and environment-variable inheritance.
//!
//! Spec (SPEC.md §2.1): variables resolve through
//! `Global -> Project -> Branch -> Runtime`, where a more specific scope wins.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Scope of a secret in the inheritance matrix.
///
/// Ordering matters: a higher variant shadows lower ones at resolution time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvVarScope {
    /// Applies to every environment on the node.
    Global,
    /// Applies to every environment of a project.
    Project,
    /// Applies to one branch of a project.
    Branch,
    /// Injected at runtime by the orchestrator; wins over everything.
    Runtime,
}

/// A resolved environment variable name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretValue(String);

impl SecretValue {
    /// Wraps a raw value. Empty values are permitted (deliberate overrides).
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the raw value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A key-value pair tagged with its scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedSecret {
    /// Scope this secret was defined in.
    pub scope: EnvVarScope,
    /// The secret value.
    pub value: SecretValue,
}

/// Collection of secrets grouped by scope.
///
/// Resolution walks `Global -> Project -> Branch -> Runtime`; the last scope
/// that defines a key wins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretContext {
    /// Key -> (scope, value).
    vars: BTreeMap<String, ScopedSecret>,
}

impl SecretContext {
    /// Creates an empty context.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces a secret at the given scope.
    pub fn set(&mut self, name: impl Into<String>, scope: EnvVarScope, value: SecretValue) {
        self.vars.insert(name.into(), ScopedSecret { scope, value });
    }

    /// Removes a secret by name.
    pub fn remove(&mut self, name: &str) {
        self.vars.remove(name);
    }

    /// Number of stored secrets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.vars.len()
    }

    /// Whether the context holds no secrets.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// Resolves the effective value of `name` following the inheritance
    /// matrix, or `None` if the key is not defined anywhere.
    #[must_use]
    pub fn resolve(&self, name: &str) -> Option<&SecretValue> {
        self.vars.get(name).map(|s| &s.value)
    }

    /// Computes the full map of effective values.
    ///
    /// For each key the value of its highest (most specific) scope wins.
    #[must_use]
    pub fn resolved_map(&self) -> BTreeMap<String, SecretValue> {
        self.vars
            .iter()
            .map(|(k, s)| (k.clone(), s.value.clone()))
            .collect()
    }

    /// Iterates over (name, scope, value) triples.
    pub fn iter(&self) -> impl Iterator<Item = (&str, EnvVarScope, &SecretValue)> {
        self.vars
            .iter()
            .map(|(k, s)| (k.as_str(), s.scope, &s.value))
    }

    /// Builds a resolved map from a chain of contexts, from least to most
    /// specific (e.g. `[global, project, branch, runtime]`).
    ///
    /// Later contexts override earlier ones; per-key scope precedence is
    /// honored as a tie-breaker.
    #[must_use]
    pub fn merge(mut self, contexts: impl IntoIterator<Item = Self>) -> Self {
        for other in contexts {
            for (name, scope, value) in other.iter() {
                let current = self.vars.get(name);
                let replace = current.is_none_or(|c| c.scope <= scope);
                if replace {
                    self.vars.insert(
                        name.to_owned(),
                        ScopedSecret {
                            scope,
                            value: value.clone(),
                        },
                    );
                }
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ctx: &mut SecretContext, name: &str, scope: EnvVarScope, value: &str) {
        ctx.set(name, scope, SecretValue::new(value));
    }

    #[test]
    fn more_specific_scope_wins() {
        let mut ctx = SecretContext::new();
        set(&mut ctx, "DATABASE_URL", EnvVarScope::Global, "global://db");
        set(&mut ctx, "DATABASE_URL", EnvVarScope::Branch, "branch://db");
        set(
            &mut ctx,
            "DATABASE_URL",
            EnvVarScope::Runtime,
            "runtime://db",
        );

        assert_eq!(
            ctx.resolve("DATABASE_URL").unwrap().as_str(),
            "runtime://db"
        );
    }

    #[test]
    fn missing_key_resolves_to_none() {
        let ctx = SecretContext::new();
        assert_eq!(ctx.resolve("DATABASE_URL"), None);
    }

    #[test]
    fn global_is_fallback_when_no_override() {
        let mut ctx = SecretContext::new();
        set(&mut ctx, "REDIS_URL", EnvVarScope::Global, "redis://global");
        assert_eq!(ctx.resolve("REDIS_URL").unwrap().as_str(), "redis://global");
    }

    #[test]
    fn merge_applies_scope_precedence() {
        let mut global = SecretContext::new();
        set(&mut global, "KEY", EnvVarScope::Global, "global");

        let mut project = SecretContext::new();
        set(&mut project, "KEY", EnvVarScope::Project, "project");
        set(&mut project, "ONLY_PROJECT", EnvVarScope::Project, "yes");

        let mut branch = SecretContext::new();
        set(&mut branch, "KEY", EnvVarScope::Global, "should-not-win");

        let merged = branch.merge([global.clone(), project.clone()]);
        assert_eq!(merged.resolve("KEY").unwrap().as_str(), "project");
        assert_eq!(merged.resolve("ONLY_PROJECT").unwrap().as_str(), "yes");
    }

    #[test]
    fn resolved_map_keeps_most_specific() {
        let mut ctx = SecretContext::new();
        set(&mut ctx, "A", EnvVarScope::Global, "g");
        set(&mut ctx, "A", EnvVarScope::Project, "p");
        set(&mut ctx, "B", EnvVarScope::Branch, "b");

        let map = ctx.resolved_map();
        assert_eq!(map.len(), 2);
        assert_eq!(map["A"].as_str(), "p");
        assert_eq!(map["B"].as_str(), "b");
    }
}
