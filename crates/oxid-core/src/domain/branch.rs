//! `Branch` entity.

use serde::{Deserialize, Serialize};

use crate::domain::error::invalid;

/// A validated branch name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BranchName(String);

impl BranchName {
    /// Validates a branch name.
    ///
    /// Rejects empty names, whitespace and invalid path separators.
    ///
    /// # Errors
    /// Returns [`DomainError::Invalid`] when the name is not usable.
    pub fn parse(value: impl Into<String>) -> Result<Self, crate::DomainError> {
        let raw = value.into();
        let trimmed = raw.trim();

        if trimmed.is_empty() {
            return invalid("branch name cannot be empty");
        }
        if trimmed.chars().any(char::is_whitespace) {
            return invalid(format!("branch name `{trimmed}` cannot contain whitespace"));
        }
        if trimmed.contains("..") || trimmed.contains("@{") {
            return invalid(format!("branch name `{trimmed}` is not a valid ref"));
        }

        Ok(Self(trimmed.to_owned()))
    }

    /// Returns the raw branch name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BranchName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A branch under test, pinned to a specific commit for detached checkouts
/// (SPEC.md §2.2: "checkouts sin cabeza").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    /// Branch name.
    pub name: BranchName,
    /// The commit the environment is pinned to.
    pub commit_sha: String,
}

impl Branch {
    /// Creates a new branch.
    ///
    /// # Errors
    /// Returns [`DomainError::Invalid`] when the name is invalid or the commit
    /// SHA is not a valid 40-hex-char string.
    pub fn new(
        name: BranchName,
        commit_sha: impl Into<String>,
    ) -> Result<Self, crate::DomainError> {
        let sha = commit_sha.into();
        let trimmed = sha.trim();

        if trimmed.is_empty() {
            return invalid("commit SHA cannot be empty");
        }
        if !(trimmed.len() == 40 && trimmed.chars().all(|c| c.is_ascii_hexdigit())) {
            return invalid(format!(
                "commit SHA `{trimmed}` must be a 40-character hex string"
            ));
        }

        Ok(Self {
            name,
            commit_sha: trimmed.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_branch_names() {
        for name in ["feature-login", "fix/checkout", "release-1.0.0"] {
            assert!(BranchName::parse(name).is_ok());
        }
    }

    #[test]
    fn rejects_invalid_branch_names() {
        for name in ["", "   ", "feat ure", "a..b", "feature@{}"] {
            assert!(BranchName::parse(name).is_err(), "should reject `{name}`");
        }
    }

    #[test]
    fn creates_branch_with_valid_sha() {
        let branch = Branch::new(
            BranchName::parse("feature-login").unwrap(),
            "0123456789abcdef0123456789abcdef01234567",
        )
        .unwrap();
        assert_eq!(branch.name.as_str(), "feature-login");
        assert_eq!(branch.commit_sha.len(), 40);
    }

    #[test]
    fn rejects_bad_sha() {
        assert!(Branch::new(BranchName::parse("x").unwrap(), "").is_err());
        assert!(Branch::new(BranchName::parse("x").unwrap(), "abc").is_err());
        assert!(Branch::new(BranchName::parse("x").unwrap(), "g".repeat(40)).is_err());
    }
}
