//! Environment lifecycle state machine.
//!
//! Spec (SPEC.md): *"Un `Environment` solo puede estar en un estado a la vez"*.
//! This module encodes the allowed transitions; the domain rejects any move
//! that is not in the table.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Lifecycle states of an ephemeral environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvironmentState {
    /// Image is being built and the container started.
    Building,
    /// Container is live and receiving traffic.
    Running,
    /// Container suspended via `docker pause` (idle > `pause_after`).
    Paused,
    /// Deeper sleep; only the URL mapping remains.
    Hibernating,
    /// Container and ephemeral volumes torn down (TTL exceeded). Terminal.
    Destroyed,
}

impl EnvironmentState {
    /// Returns the directed transitions allowed from this state.
    #[must_use]
    pub fn allowed_transitions(self) -> &'static [StateTransition] {
        match self {
            Self::Building => &[
                StateTransition::BuildSucceeded,
                StateTransition::BuildFailed,
            ],
            Self::Running => &[
                StateTransition::IdleTimeout,
                StateTransition::Woken,
                StateTransition::TtlExpired,
                StateTransition::Destroy,
            ],
            Self::Paused => &[
                StateTransition::Woken,
                StateTransition::DeepSleep,
                StateTransition::TtlExpired,
                StateTransition::Destroy,
            ],
            Self::Hibernating => &[
                StateTransition::Woken,
                StateTransition::TtlExpired,
                StateTransition::Destroy,
            ],
            Self::Destroyed => &[],
        }
    }
}

/// Reasons an environment changes state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateTransition {
    /// Build finished successfully.
    BuildSucceeded,
    /// Build failed.
    BuildFailed,
    /// No traffic for `pause_after`.
    IdleTimeout,
    /// Traffic arrived and woke the environment.
    Woken,
    /// Idle for longer than the intermediate threshold.
    DeepSleep,
    /// TTL (`destroy_after`) exceeded.
    TtlExpired,
    /// User explicitly destroyed the environment (`oxid down`).
    Destroy,
}

impl StateTransition {
    /// The state reached after applying this transition.
    #[must_use]
    pub fn target(self) -> EnvironmentState {
        match self {
            Self::BuildFailed | Self::TtlExpired | Self::Destroy => EnvironmentState::Destroyed,
            Self::IdleTimeout => EnvironmentState::Paused,
            Self::BuildSucceeded | Self::Woken => EnvironmentState::Running,
            Self::DeepSleep => EnvironmentState::Hibernating,
        }
    }
}

/// Errors specific to applying a state transition.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvironmentStateError {
    /// The transition is not allowed from the current state.
    #[error("transition `{0:?}` is not allowed from `{1:?}`")]
    Forbidden(StateTransition, EnvironmentState),
    /// The transition would not change the state.
    #[error("environment is already `{0:?}`")]
    Noop(EnvironmentState),
}

/// Lookup table mapping a source state + transition to the resulting state.
///
/// This is the single source of truth for the state machine; entities and
/// services both consult it.
#[derive(Debug, Clone, Copy)]
pub struct TransitionTable;

impl TransitionTable {
    /// Applies `transition` to `current`, returning the resulting state or an
    /// error if the move is forbidden.
    ///
    /// # Errors
    /// - [`EnvironmentStateError::Forbidden`] if the transition is not in the
    ///   allowed list for `current`.
    /// - [`EnvironmentStateError::Noop`] if the target equals `current`.
    pub fn apply(
        current: EnvironmentState,
        transition: StateTransition,
    ) -> Result<EnvironmentState, EnvironmentStateError> {
        let target = transition.target();

        if current == target {
            return Err(EnvironmentStateError::Noop(target));
        }

        if current.allowed_transitions().contains(&transition) {
            Ok(target)
        } else {
            Err(EnvironmentStateError::Forbidden(transition, current))
        }
    }
}

impl fmt::Display for EnvironmentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Building => "building",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Hibernating => "hibernating",
            Self::Destroyed => "destroyed",
        };
        f.write_str(s)
    }
}

impl FromStr for EnvironmentState {
    type Err = crate::domain::DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "building" => Ok(Self::Building),
            "running" => Ok(Self::Running),
            "paused" => Ok(Self::Paused),
            "hibernating" => Ok(Self::Hibernating),
            "destroyed" => Ok(Self::Destroyed),
            _ => crate::domain::error::invalid(format!("unknown environment state `{s}`")),
        }
    }
}

impl fmt::Display for StateTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::BuildSucceeded => "build_succeeded",
            Self::BuildFailed => "build_failed",
            Self::IdleTimeout => "idle_timeout",
            Self::Woken => "woken",
            Self::DeepSleep => "deep_sleep",
            Self::TtlExpired => "ttl_expired",
            Self::Destroy => "destroy",
        };
        f.write_str(s)
    }
}

impl FromStr for StateTransition {
    type Err = crate::domain::DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "build_succeeded" => Ok(Self::BuildSucceeded),
            "build_failed" => Ok(Self::BuildFailed),
            "idle_timeout" => Ok(Self::IdleTimeout),
            "woken" => Ok(Self::Woken),
            "deep_sleep" => Ok(Self::DeepSleep),
            "ttl_expired" => Ok(Self::TtlExpired),
            "destroy" => Ok(Self::Destroy),
            _ => crate::domain::error::invalid(format!("unknown state transition `{s}`")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_lifecycle() {
        let mut state = EnvironmentState::Building;

        state = TransitionTable::apply(state, StateTransition::BuildSucceeded).unwrap();
        assert_eq!(state, EnvironmentState::Running);

        state = TransitionTable::apply(state, StateTransition::IdleTimeout).unwrap();
        assert_eq!(state, EnvironmentState::Paused);

        state = TransitionTable::apply(state, StateTransition::DeepSleep).unwrap();
        assert_eq!(state, EnvironmentState::Hibernating);

        state = TransitionTable::apply(state, StateTransition::Woken).unwrap();
        assert_eq!(state, EnvironmentState::Running);

        state = TransitionTable::apply(state, StateTransition::TtlExpired).unwrap();
        assert_eq!(state, EnvironmentState::Destroyed);
    }

    #[test]
    fn build_failure_destroys() {
        let state =
            TransitionTable::apply(EnvironmentState::Building, StateTransition::BuildFailed)
                .unwrap();
        assert_eq!(state, EnvironmentState::Destroyed);
    }

    #[test]
    fn destroyed_is_terminal() {
        for transition in [
            StateTransition::BuildSucceeded,
            StateTransition::IdleTimeout,
            StateTransition::Woken,
            StateTransition::DeepSleep,
            StateTransition::TtlExpired,
            StateTransition::BuildFailed,
            StateTransition::Destroy,
        ] {
            let err = TransitionTable::apply(EnvironmentState::Destroyed, transition).unwrap_err();
            assert!(
                matches!(
                    err,
                    EnvironmentStateError::Forbidden(_, _)
                        | EnvironmentStateError::Noop(EnvironmentState::Destroyed)
                ),
                "transition {transition:?} must be rejected on a destroyed environment"
            );
        }
    }

    #[test]
    fn forbidden_jumps_are_rejected() {
        // Building -> Hibernating (skips Running) is not allowed.
        let err = TransitionTable::apply(EnvironmentState::Building, StateTransition::DeepSleep)
            .unwrap_err();
        assert!(matches!(err, EnvironmentStateError::Forbidden(_, _)));

        // Running -> Woken is a no-op.
        let err =
            TransitionTable::apply(EnvironmentState::Running, StateTransition::Woken).unwrap_err();
        assert_eq!(err, EnvironmentStateError::Noop(EnvironmentState::Running));
    }
}
