use oxid_core::{BranchName, DomainError, Environment, Project};

use super::error::CpError;

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

pub(crate) fn image_name(project: &Project, branch: &BranchName) -> String {
    format!("oxid/{}/{}", project.name, sanitize_label(branch))
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
