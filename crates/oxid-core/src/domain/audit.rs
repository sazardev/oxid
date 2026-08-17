//! `AuditEvent` entity — the deployment/audit trail (SPEC.md §4.7).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::domain::environment::EnvironmentId;
use crate::domain::state::StateTransition;

/// A single recorded lifecycle event of an environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Unique identifier.
    pub id: u64,
    /// Environment this event belongs to.
    pub environment_id: EnvironmentId,
    /// The transition (reason) that caused the event.
    pub kind: StateTransition,
    /// Optional human-readable detail (e.g. build log tail).
    pub detail: Option<String>,
    /// When the event happened.
    pub occurred_at: OffsetDateTime,
}

impl AuditEvent {
    /// Creates a new audit event.
    #[must_use]
    pub fn new(
        id: u64,
        environment_id: EnvironmentId,
        kind: StateTransition,
        detail: Option<String>,
        occurred_at: OffsetDateTime,
    ) -> Self {
        Self {
            id,
            environment_id,
            kind,
            detail,
            occurred_at,
        }
    }
}
