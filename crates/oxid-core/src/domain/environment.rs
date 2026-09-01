//! `Environment` entity.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::domain::DomainError;
use crate::domain::branch::Branch;
use crate::domain::error::invalid;
use crate::domain::node::NodeId;
use crate::domain::project::ProjectId;
use crate::domain::state::{EnvironmentState, EnvironmentStateError, StateTransition};

/// Stable identifier of an environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EnvironmentId(pub u64);

impl std::fmt::Display for EnvironmentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A single ephemeral deployment of a branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Environment {
    /// Unique identifier.
    pub id: EnvironmentId,
    /// Owning project.
    pub project_id: ProjectId,
    /// The branch this environment serves.
    pub branch: Branch,
    /// Current lifecycle state.
    pub state: EnvironmentState,
    /// Public URL, e.g. `feature-login.my-awesome-api.local.dev`.
    pub url: String,
    /// When the environment was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When the state last changed.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// Last time traffic hit the URL (drives scale-to-zero).
    #[serde(with = "time::serde::rfc3339")]
    pub last_accessed_at: OffsetDateTime,
    /// Host port this environment's container is actually published on,
    /// when running in direct-publish mode (`OXID_DOCKER_NETWORK` unset).
    /// Docker is always asked to pick this port itself (SPEC.md "Eficiencia
    /// Absoluta" — a busy configured port should never block a deploy), so
    /// it's only known after the container actually starts. `None` when
    /// routed through Traefik instead (no host port is published at all).
    /// Changes on every redeploy — see `public_port` for the stable address.
    pub host_port: Option<u16>,
    /// The branch's stable public address in direct-publish mode: a port
    /// bound once (by Oxid's own built-in reverse proxy) and reused across
    /// every redeploy, unlike `host_port` which changes each time. A
    /// redeploy swaps this proxy's upstream target to the new container
    /// only once it's confirmed ready, so this address never has a gap.
    /// `None` under Traefik (which is already a stable-address proxy).
    pub public_port: Option<u16>,
    /// The exact Docker container name this deployment runs under. Persisted
    /// (rather than always recomputed from project+branch) so a redeploy can
    /// give its new instance a distinct name from the still-running old one —
    /// necessary for a zero-downtime cutover, where both briefly coexist.
    /// `None` for environments predating this (falls back to the legacy
    /// deterministic `oxid-{project}-{branch}` naming, which is what their
    /// real container is actually named).
    pub container_name: Option<String>,
    /// Which node in the fleet actually runs this environment's container.
    ///
    /// Not optional, and never has been in practice: every install has node
    /// 1 (`local`), seeded by migration `0020`, and rows written before that
    /// migration are backfilled to it. A `NULL` read back afterwards means
    /// "written by a binary older than the migration" and resolves to node 1
    /// too — the same answer, and one that survives rolling back.
    ///
    /// It is the environment's, not the branch's: a redeploy is allowed to
    /// *move* a branch to another node, and `LockKey::Branch` is what makes
    /// that move atomic.
    pub node_id: NodeId,
}

/// One container belonging to an environment.
///
/// An environment used to be one container, and its scalar
/// `container_name`/`host_port`/`public_port` still describe **the primary
/// service** — the one that takes the branch URL. This is the rest: the
/// workers, the sidecars, and any image a compose file asked for that has
/// no shared pool to be folded into.
///
/// Rows exist only for environments deployed after migration `0021`. None
/// means one service, which is what those environments actually are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentService {
    /// Which environment this belongs to.
    pub environment_id: EnvironmentId,
    /// The compose key — `api`, `worker`, `db`. Also the hostname siblings
    /// resolve it by inside the environment's own network, which is why it
    /// is stored rather than derived.
    pub name: String,
    /// The Docker container name.
    pub container_name: String,
    /// The image it runs.
    pub image: String,
    /// The port it listens on inside the container, if any. `None` is a
    /// worker: something that does work and answers nothing.
    pub container_port: Option<u16>,
    /// The host port Docker published for it, in direct-publish mode.
    pub host_port: Option<u16>,
    /// Whether this is the service the branch URL points at. Exactly one
    /// per environment.
    pub is_primary: bool,
}

impl Environment {
    /// Validates and constructs an environment.
    ///
    /// # Errors
    /// Returns [`DomainError::Invalid`] for an empty URL.
    pub fn new(
        id: EnvironmentId,
        project_id: ProjectId,
        branch: Branch,
        state: EnvironmentState,
        url: impl Into<String>,
        now: OffsetDateTime,
    ) -> Result<Self, DomainError> {
        let url = url.into();
        if url.trim().is_empty() {
            return invalid("environment URL cannot be empty");
        }

        Ok(Self {
            id,
            project_id,
            branch,
            state,
            url,
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            host_port: None,
            public_port: None,
            container_name: None,
            // Every caller that does not care — including the whole
            // existing test suite — means "this node", and that is what
            // node 1 is. Defaulting here rather than making the parameter
            // explicit is what keeps an upgrade behaviour-identical.
            node_id: NodeId::LOCAL,
        })
    }

    /// Applies a state transition following the domain state machine.
    ///
    /// On success the environment is returned with `state` and `updated_at`
    /// updated.
    ///
    /// # Errors
    /// - [`EnvironmentStateError::Forbidden`] when the move is not allowed.
    /// - [`EnvironmentStateError::Noop`] when the move would not change state.
    pub fn transition(
        &mut self,
        transition: StateTransition,
        now: OffsetDateTime,
    ) -> Result<(), EnvironmentStateError> {
        let next = crate::domain::state::TransitionTable::apply(self.state, transition)?;
        self.state = next;
        self.updated_at = now;
        Ok(())
    }

    /// Registers traffic, refreshing `last_accessed_at`.
    ///
    /// # Errors
    /// Returns [`EnvironmentStateError::Noop`] if the environment is already
    /// destroyed (terminal state).
    pub fn touch(&mut self, now: OffsetDateTime) -> Result<(), EnvironmentStateError> {
        if self.state == EnvironmentState::Destroyed {
            return Err(EnvironmentStateError::Noop(EnvironmentState::Destroyed));
        }
        self.last_accessed_at = now;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::branch::BranchName;
    use crate::domain::state::TransitionTable;

    fn branch() -> Branch {
        Branch::new(
            BranchName::parse("feature-login").unwrap(),
            "0123456789abcdef0123456789abcdef01234567",
        )
        .unwrap()
    }

    fn env(now: OffsetDateTime) -> Environment {
        Environment::new(
            EnvironmentId(1),
            ProjectId(1),
            branch(),
            EnvironmentState::Running,
            "feature-login.my-awesome-api.local.dev",
            now,
        )
        .unwrap()
    }

    #[test]
    fn transitions_update_state_and_timestamp() {
        let now = OffsetDateTime::from_unix_timestamp(1_000).unwrap();
        let mut env = env(now);

        env.transition(
            StateTransition::IdleTimeout,
            now + time::Duration::minutes(1),
        )
        .unwrap();
        assert_eq!(env.state, EnvironmentState::Paused);
        assert_eq!(env.updated_at, now + time::Duration::minutes(1));
    }

    #[test]
    fn forbidden_transition_keeps_state() {
        let now = OffsetDateTime::from_unix_timestamp(1_000).unwrap();
        let mut env = env(now);

        // Running -> DeepSleep is not allowed.
        let err = env
            .transition(StateTransition::DeepSleep, now + time::Duration::minutes(1))
            .unwrap_err();
        assert_eq!(
            err,
            TransitionTable::apply(EnvironmentState::Running, StateTransition::DeepSleep)
                .unwrap_err()
        );
        assert_eq!(env.state, EnvironmentState::Running);
        assert_eq!(env.updated_at, now);
    }

    #[test]
    fn touch_refreshes_last_access() {
        let now = OffsetDateTime::from_unix_timestamp(1_000).unwrap();
        let mut env = env(now);

        let later = now + time::Duration::hours(1);
        env.touch(later).unwrap();
        assert_eq!(env.last_accessed_at, later);
    }

    #[test]
    fn destroyed_environment_cannot_be_touched() {
        let now = OffsetDateTime::from_unix_timestamp(1_000).unwrap();
        let mut env = env(now);
        env.transition(StateTransition::TtlExpired, now).unwrap();
        assert_eq!(env.state, EnvironmentState::Destroyed);

        assert!(env.touch(now).is_err());
    }
}
