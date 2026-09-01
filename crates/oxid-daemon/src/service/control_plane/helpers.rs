use oxid_core::{BranchName, DomainError, Environment, Project};

use super::error::CpError;

/// The service name a single-service environment uses.
///
/// A repository with a plain `Dockerfile` has no compose file and therefore
/// no service *names*, but it still needs one to be addressed by — so it
/// gets this. It is also what `oxid logs` defaults to, which is why it
/// reads as a word rather than as a placeholder.
pub(crate) const PRIMARY_SERVICE: &str = "app";

/// Converts a domain state error into a control-plane error.
pub(crate) fn state_err(err: &oxid_core::EnvironmentStateError) -> CpError {
    CpError::Domain(DomainError::Invalid(err.to_string()))
}

/// Hashes a raw API token for storage/lookup — tokens are full-entropy
/// random values (not user-chosen passwords), so a plain fast hash is
/// appropriate; no salt/KDF needed since there's nothing to brute-force
/// offline once the hash alone is known.
pub(crate) fn hash_token(raw_token: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(raw_token.as_bytes());
    hex::encode(digest)
}

/// Legacy deterministic container name (one instance per project+branch,
/// ever) — kept only as the fallback for environments deployed before
/// `Environment::container_name` was persisted per-deployment. Every live
/// call site should go through [`resolved_container_name`] instead, so an
/// old row's actual (already-running) container is still found correctly.
pub(crate) fn container_name(project: &Project, branch: &BranchName) -> String {
    format!("oxid-{}-{}", project.name, sanitize_label(branch))
}

/// The container name this environment's own instance actually runs
/// under — its persisted `container_name` if set (every deployment since
/// zero-downtime redeploys shipped sets this, uniquely per environment id,
/// so a redeploy's new instance never collides with the still-running old
/// one), or the legacy project+branch name for anything deployed before.
pub(crate) fn resolved_container_name(project: &Project, env: &Environment) -> String {
    env.container_name
        .clone()
        .unwrap_or_else(|| container_name(project, &env.branch.name))
}

/// The image tag a branch's build produces.
///
/// Lowercased in full, because Docker refuses any reference that is not:
/// `oxid/appB/JIRA-123` comes back as `invalid reference format: repository
/// name must be lowercase` and the deploy fails outright. Ticket-prefixed
/// branches (`JIRA-123`, `ABC-456-fix`) are among the most common naming
/// schemes there is, so this was every deploy of those branches failing on
/// a message that says nothing about branch names.
///
/// Two branches differing only in case therefore share an image tag. That
/// is not avoidable — Docker has no case-sensitive alternative — and costs
/// nothing in practice: every deploy rebuilds its image, and a running
/// container holds its image by id, so a retag never disturbs it.
pub(crate) fn image_name(project: &Project, branch: &BranchName) -> String {
    format!("oxid/{}/{}", project.name, sanitize_label(branch)).to_ascii_lowercase()
}

/// The same rule as [`sanitize_label`], for a name that is not a branch —
/// a compose service, say. Docker's container names accept
/// `[A-Za-z0-9][A-Za-z0-9_.-]*`, so anything else becomes a dash.
pub(crate) fn sanitize_label_str(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

pub(crate) fn sanitize_label(branch: &BranchName) -> String {
    branch
        .to_string()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Sanitizes a project name or branch label into a valid Postgres
/// identifier fragment: lowercase `[a-z0-9_]` only.
pub(crate) fn sanitize_identifier(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// The lowest value in `0..capacity` not present in `used`, or `None` if
/// every slot is taken. Pure and separately tested since `ResourcePool`
/// (oxid-core) tracks *how many* slices are leased, not *which* numeric
/// slot each tenant holds — that assignment is specific to Redis indices,
/// so it lives here instead of being bent onto that more general type.
pub(crate) fn lowest_free_index(
    used: &std::collections::BTreeSet<u32>,
    capacity: u32,
) -> Option<u32> {
    (0..capacity).find(|i| !used.contains(i))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxid_core::{BuildConfig, ProjectConfig, ProjectId, RepoUrl, Ttl};

    fn project(name: &str) -> Project {
        let config = ProjectConfig::new(
            "example.dev",
            Ttl::parse("30m").unwrap(),
            Ttl::parse("7d").unwrap(),
            8080,
            BuildConfig::default(),
            Vec::new(),
        )
        .unwrap();
        Project::new(
            ProjectId(1),
            name.to_owned(),
            RepoUrl::parse("https://example.com/org/repo.git").unwrap(),
            config,
        )
        .unwrap()
    }

    /// Docker rejects any reference that is not lowercase, so a
    /// ticket-prefixed branch used to fail every deploy with
    /// `invalid reference format` — a message that never mentions branches.
    #[test]
    fn image_names_are_lowercase_whatever_the_branch_is_called() {
        let branch = BranchName::parse("JIRA-123").unwrap();
        let name = image_name(&project("appB"), &branch);
        assert_eq!(name, "oxid/appb/jira-123");
        assert!(!name.chars().any(char::is_uppercase), "{name}");
    }

    #[test]
    fn an_uppercase_project_name_is_lowercased_too() {
        let branch = BranchName::parse("main").unwrap();
        assert_eq!(image_name(&project("MyApp"), &branch), "oxid/myapp/main");
    }
}
