//! Environment lifecycle state machine.
//!
//! Spec (SPEC.md): *"Un `Environment` solo puede estar en un estado a la vez"*.
//! This module encodes the allowed transitions; the domain rejects any move
//! that is not in the table.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Lifecycle states of an ephemeral environment.
// `snake_case`, not `lowercase`: every single-word variant renders
// identically either way, but `BuildFailed` does not — `lowercase` emitted
// `buildfailed` over the wire while `Display`/`FromStr` (which is what the
// database stores) spelled it `build_failed`, so the API and the persisted
// value disagreed about the same state. See
// `state_serialization_matches_its_text_form`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentState {
    /// Image is being built and the container started.
    Building,
    /// Container is live and receiving traffic.
    Running,
    /// Container suspended via `docker pause` (idle > `pause_after`).
    Paused,
    /// Deeper sleep; only the URL mapping remains.
    Hibernating,
    /// The build or the first start of this deploy failed. The branch has
    /// no serving container; the row survives so the failure is visible in
    /// `oxid status` and the dashboard instead of being indistinguishable
    /// from a routine teardown, and so its audit trail has something to
    /// hang off. Cleaned up by `destroy_after` like anything else.
    BuildFailed,
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
            // A failed deploy can only be cleaned up. It has nothing to
            // wake or suspend, and it must not be reachable back into
            // `Running` — the way back to a working branch is a new deploy,
            // which creates its own environment.
            Self::BuildFailed => &[StateTransition::TtlExpired, StateTransition::Destroy],
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
            Self::TtlExpired | Self::Destroy => EnvironmentState::Destroyed,
            Self::BuildFailed => EnvironmentState::BuildFailed,
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
            Self::BuildFailed => "build_failed",
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
            "build_failed" => Ok(Self::BuildFailed),
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

    /// A failed deploy lands in its own state rather than being folded into
    /// `Destroyed`. Both mean "no container serving this branch", but only
    /// one of them means someone's push is broken, and `oxid status` has to
    /// be able to tell an operator which.
    #[test]
    fn build_failure_is_its_own_state() {
        let state =
            TransitionTable::apply(EnvironmentState::Building, StateTransition::BuildFailed)
                .unwrap();
        assert_eq!(state, EnvironmentState::BuildFailed);
    }

    /// From there the only way out is cleanup — never back into service.
    /// A branch recovers by deploying again, which creates its own
    /// environment; resurrecting the failed one would claim a container
    /// that was never successfully started.
    #[test]
    fn a_failed_build_can_only_be_cleaned_up() {
        for transition in [
            StateTransition::BuildSucceeded,
            StateTransition::IdleTimeout,
            StateTransition::Woken,
            StateTransition::DeepSleep,
        ] {
            assert!(
                TransitionTable::apply(EnvironmentState::BuildFailed, transition).is_err(),
                "{transition:?} must not be allowed out of BuildFailed"
            );
        }
        for transition in [StateTransition::TtlExpired, StateTransition::Destroy] {
            assert_eq!(
                TransitionTable::apply(EnvironmentState::BuildFailed, transition).unwrap(),
                EnvironmentState::Destroyed
            );
        }
    }

    /// The wire/database spelling round-trips, since the state is persisted
    /// as text and read back on every daemon start.
    #[test]
    fn build_failed_round_trips_through_text() {
        assert_eq!(EnvironmentState::BuildFailed.to_string(), "build_failed");
        assert_eq!(
            "build_failed".parse::<EnvironmentState>().unwrap(),
            EnvironmentState::BuildFailed
        );
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

#[cfg(test)]
mod wire_format_tests {
    use super::*;

    /// The JSON spelling and the text spelling must be the same word.
    ///
    /// They drifted once: `#[serde(rename_all = "lowercase")]` rendered
    /// `BuildFailed` as `buildfailed` while the database stored
    /// `build_failed`, so an API client filtering on the value it received
    /// could never match what was persisted, and the dashboard's per-state
    /// styling silently missed.
    #[test]
    fn state_serialization_matches_its_text_form() {
        for state in [
            EnvironmentState::Building,
            EnvironmentState::Running,
            EnvironmentState::Paused,
            EnvironmentState::Hibernating,
            EnvironmentState::BuildFailed,
            EnvironmentState::Destroyed,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let json = json.trim_matches('"');
            assert_eq!(json, state.to_string(), "wire vs text form for {state:?}");
            assert_eq!(json.parse::<EnvironmentState>().unwrap(), state);
        }
    }

    /// Same contract for the transitions, which are the audit `kind` values
    /// every consumer filters on.
    #[test]
    fn transition_serialization_matches_its_text_form() {
        for transition in [
            StateTransition::BuildSucceeded,
            StateTransition::BuildFailed,
            StateTransition::IdleTimeout,
            StateTransition::Woken,
            StateTransition::DeepSleep,
            StateTransition::TtlExpired,
            StateTransition::Destroy,
        ] {
            let json = serde_json::to_string(&transition).unwrap();
            let json = json.trim_matches('"');
            assert_eq!(json, transition.to_string());
            assert_eq!(json.parse::<StateTransition>().unwrap(), transition);
        }
    }
}
