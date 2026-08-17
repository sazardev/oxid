//! Subdomain derivation.
//!
//! Spec (IDEA.md): the proxy routes `feature-login.my-awesome-api.local.dev`
//! by prepending the branch name to the project's base domain.

use crate::domain::branch::BranchName;

/// Renders a branch name into a DNS-safe subdomain label.
///
/// Lowercases, collapses `_`/`.` to `-` and strips disallowed characters so
/// the result is safe for wildcard DNS.
#[must_use]
pub fn sanitize_branch(branch: &BranchName) -> String {
    let mut out = String::with_capacity(branch.as_str().len());
    let mut prev_dash = false;

    for c in branch.as_str().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }

    out.trim_matches('-').to_owned()
}

/// Builds the full subdomain for a branch under a base domain.
#[must_use]
pub fn subdomain_for(branch: &BranchName, base_domain: &str) -> String {
    let label = sanitize_branch(branch);
    if label.is_empty() {
        return base_domain.to_owned();
    }
    format!("{label}.{base_domain}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branch(name: &str) -> BranchName {
        BranchName::parse(name).unwrap()
    }

    #[test]
    fn sanitizes_slashes_underscores_and_case() {
        assert_eq!(sanitize_branch(&branch("feature-login")), "feature-login");
        assert_eq!(
            sanitize_branch(&branch("feature/carrito")),
            "feature-carrito"
        );
        assert_eq!(sanitize_branch(&branch("FEATURE_Login")), "feature-login");
        assert_eq!(sanitize_branch(&branch("v1.0.0")), "v1-0-0");
    }

    #[test]
    fn builds_full_subdomain() {
        let b = branch("feature-login");
        assert_eq!(
            subdomain_for(&b, "my-awesome-api.local.dev"),
            "feature-login.my-awesome-api.local.dev"
        );
    }

    #[test]
    fn empty_label_falls_back_to_base() {
        let b = branch("---");
        assert_eq!(subdomain_for(&b, "dom.local"), "dom.local");
    }
}
