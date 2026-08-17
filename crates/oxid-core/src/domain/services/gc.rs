//! Garbage collection decisions.
//!
//! Spec (SPEC.md §2.1 / §3.2): the internal scheduler evaluates traffic
//! activity and TTLs; an environment that outlives `destroy_after` is torn
//! down, one idle past `pause_after` is suspended.

use time::{Duration, OffsetDateTime};

use crate::domain::environment::Environment;
use crate::domain::project::Project;
use crate::domain::state::{EnvironmentState, StateTransition};

/// Outcome of a GC evaluation for a single environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcAction {
    /// Keep as-is.
    Keep,
    /// Traffic idle past `pause_after`; suspend the container.
    Pause,
    /// No traffic for longer than the deep-sleep threshold.
    Hibernate,
    /// TTL (`destroy_after`) exceeded; tear the environment down.
    Destroy,
}

/// Multiplier applied to `pause_after` to derive the deep-sleep threshold.
const HIBERNATE_MULTIPLIER: i32 = 4;

impl GcAction {
    /// Returns the state transition that realises this action, if any.
    #[must_use]
    pub fn transition(self) -> Option<StateTransition> {
        match self {
            Self::Keep => None,
            Self::Pause => Some(StateTransition::IdleTimeout),
            Self::Hibernate => Some(StateTransition::DeepSleep),
            Self::Destroy => Some(StateTransition::TtlExpired),
        }
    }
}

/// Decides what to do with `environment` right now.
///
/// Rules:
/// - Destroyed environments are always ignored (terminal).
/// - If idle longer than `project.destroy_after`, destroy.
/// - If idle longer than `4 * pause_after`, hibernate.
/// - If idle longer than `project.pause_after`, pause.
/// - Otherwise keep.
#[must_use]
pub fn evaluate(environment: &Environment, project: &Project, now: OffsetDateTime) -> GcAction {
    if environment.state == EnvironmentState::Destroyed {
        return GcAction::Keep;
    }

    let idle = now - environment.last_accessed_at;
    if idle >= project.config.destroy_after.get() {
        return GcAction::Destroy;
    }
    let hibernate_after = project
        .config
        .pause_after
        .get()
        .checked_mul(HIBERNATE_MULTIPLIER)
        .unwrap_or(Duration::MAX);
    if idle >= hibernate_after {
        return GcAction::Hibernate;
    }
    if idle >= project.config.pause_after.get() {
        return GcAction::Pause;
    }

    GcAction::Keep
}

/// Convenience: `evaluate` for environments that have no project handle.
#[must_use]
pub fn is_idle(
    last_accessed_at: OffsetDateTime,
    pause_after: Duration,
    now: OffsetDateTime,
) -> bool {
    now - last_accessed_at >= pause_after
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::branch::{Branch, BranchName};
    use crate::domain::environment::{Environment, EnvironmentId};
    use crate::domain::project::{Project, ProjectId};
    use crate::domain::value_objects::{RepoUrl, Ttl};

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn project(pause_m: i64, destroy_d: i64) -> Project {
        let config = crate::domain::ProjectConfig::new(
            "app.local.dev",
            Ttl::parse(format!("{pause_m}m")).unwrap(),
            Ttl::parse(format!("{destroy_d}d")).unwrap(),
            8080,
            crate::domain::BuildConfig::default(),
            vec![],
        )
        .unwrap();
        Project::new(
            ProjectId(1),
            "app",
            RepoUrl::parse("https://github.com/org/app.git").unwrap(),
            config,
        )
        .unwrap()
    }

    fn env(now: OffsetDateTime, accessed: OffsetDateTime) -> Environment {
        Environment::new(
            EnvironmentId(1),
            ProjectId(1),
            Branch::new(BranchName::parse("feature-a").unwrap(), SHA).unwrap(),
            EnvironmentState::Running,
            "feature-a.app.local.dev",
            now,
        )
        .unwrap()
        .touch_or_panic(accessed)
    }

    trait TouchOrPanic {
        fn touch_or_panic(self, at: OffsetDateTime) -> Self;
    }

    impl TouchOrPanic for Environment {
        fn touch_or_panic(mut self, at: OffsetDateTime) -> Self {
            self.touch(at).unwrap();
            self
        }
    }

    #[test]
    fn recent_traffic_keeps_environment() {
        let now = OffsetDateTime::from_unix_timestamp(1_000_000).unwrap();
        let p = project(30, 7);
        let e = env(now, now - Duration::seconds(60));
        assert_eq!(evaluate(&e, &p, now), GcAction::Keep);
    }

    #[test]
    fn idle_beyond_pause_after_suspends() {
        let now = OffsetDateTime::from_unix_timestamp(1_000_000).unwrap();
        let p = project(30, 7);
        let e = env(now, now - Duration::minutes(31));
        assert_eq!(evaluate(&e, &p, now), GcAction::Pause);
    }

    #[test]
    fn idle_beyond_multiplier_hibernates() {
        let now = OffsetDateTime::from_unix_timestamp(1_000_000).unwrap();
        let p = project(30, 7);
        let e = env(now, now - Duration::hours(3)); // 180m > 4 * 30m
        assert_eq!(evaluate(&e, &p, now), GcAction::Hibernate);
    }

    #[test]
    fn idle_beyond_destroy_ttl_destroys() {
        let now = OffsetDateTime::from_unix_timestamp(1_000_000).unwrap();
        let p = project(30, 7);
        let e = env(now, now - Duration::days(8));
        assert_eq!(evaluate(&e, &p, now), GcAction::Destroy);
    }

    #[test]
    fn destroyed_environment_is_kept() {
        let now = OffsetDateTime::from_unix_timestamp(1_000_000).unwrap();
        let p = project(30, 7);
        let mut e = env(now, now - Duration::days(30));
        e.transition(StateTransition::TtlExpired, now).unwrap();
        assert_eq!(evaluate(&e, &p, now), GcAction::Keep);
    }

    #[test]
    fn destroy_wins_over_pause() {
        let now = OffsetDateTime::from_unix_timestamp(1_000_000).unwrap();
        // pause_after longer than destroy_after: destroy must take precedence.
        let p = project(7 * 24 * 60, 7);
        let e = env(now, now - Duration::days(8));
        assert_eq!(evaluate(&e, &p, now), GcAction::Destroy);
    }
}
