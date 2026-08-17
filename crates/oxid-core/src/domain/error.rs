//! Domain error type.

/// Errors raised by the domain layer.
///
/// Follows the DESIGN.md principle of telling the user exactly what went
/// wrong and how to fix it.
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    /// A domain entity or value object was constructed with invalid input.
    #[error("{0}")]
    Invalid(String),
    /// A state transition was attempted that the state machine forbids.
    #[error("invalid transition from `{from}` to `{to}` for environment `{environment_id}`")]
    InvalidTransition {
        /// Identifier of the affected environment.
        environment_id: crate::EnvironmentId,
        /// State the environment is currently in.
        from: crate::EnvironmentState,
        /// State the transition attempted to reach.
        to: crate::EnvironmentState,
    },
}

/// Convenience constructor for [`DomainError::Invalid`].
pub(crate) fn invalid<T>(msg: impl Into<String>) -> Result<T, DomainError> {
    Err(DomainError::Invalid(msg.into()))
}
