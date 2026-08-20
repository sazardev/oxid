use oxid_core::{ContainerStatus, Environment, EnvironmentId, SelfWiringStatus};

/// Outcome of one [`crate::service::control_plane::ControlPlane::sweep`] pass across all environments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcSummary {
    /// Environments suspended via `docker pause` (idle past `pause_after`).
    pub paused: u64,
    /// Environments stopped for deep sleep (idle past the hibernate threshold).
    pub hibernated: u64,
    /// Environments torn down (idle past `destroy_after`).
    pub destroyed: u64,
    /// Per-environment failures; the sweep continues past these.
    pub errors: Vec<(EnvironmentId, String)>,
}

/// Aggregate node-wide counts for the web dashboard's overview — see
/// [`crate::service::control_plane::ControlPlane::node_stats`].
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct NodeStats {
    /// Number of registered projects.
    pub projects: u64,
    /// Environments currently `Running`.
    pub environments_running: u64,
    /// Environments currently `Paused`.
    pub environments_paused: u64,
    /// Environments currently `Building`.
    pub environments_building: u64,
    /// Environments currently `Hibernating`.
    pub environments_hibernating: u64,
    /// Environments currently `Destroyed` (kept for rollback history).
    pub environments_destroyed: u64,
    /// Deploys currently waiting for host capacity.
    pub queue_length: u64,
    /// Total host memory Docker reports, in bytes.
    pub host_total_memory_bytes: u64,
    /// Host CPU count Docker reports.
    pub host_cpu_count: u32,
    /// Whether `with_traefik` was configured (`OXID_DOCKER_NETWORK` set) —
    /// when `false`, an environment's `url` (a `branch.project-base-domain`
    /// hostname, meaningful only to a Traefik `Host()` rule) isn't reachable
    /// as a URL at all; the real address is the project's own
    /// `[routing].port` published directly on whatever host is running the
    /// daemon. The dashboard uses this to decide which one to link to.
    pub traefik_enabled: bool,
}

/// Bundled result of [`crate::service::control_plane::ControlPlane::infra_status`]/
#[derive(Debug, Clone, serde::Serialize)]
pub struct InfraStatus {
    /// The configured Docker network name (`OXID_DOCKER_NETWORK`).
    pub network: String,
    /// Whether `network` already exists.
    pub network_exists: bool,
    /// The built-in Traefik container's actual Docker state.
    pub traefik_status: ContainerStatus,
    /// Whether this daemon's own container is joined to `network` and
    /// labeled for wake-on-request — detection only, see
    /// [`SelfWiringStatus`].
    pub self_wiring: SelfWiringStatus,
    /// Human-readable, actionable instructions for whatever's missing.
    /// Empty when everything is fully wired.
    pub next_steps: Vec<String>,
}

impl InfraStatus {
    pub(crate) fn new(
        network: String,
        network_exists: bool,
        traefik_status: ContainerStatus,
        self_wiring: SelfWiringStatus,
    ) -> Self {
        let mut next_steps = Vec::new();
        if !network_exists {
            next_steps.push(format!(
                "Docker network `{network}` doesn't exist yet — run `oxid infra setup` to \
                 create it."
            ));
        }
        match traefik_status {
            ContainerStatus::Running => {}
            ContainerStatus::Paused | ContainerStatus::Stopped | ContainerStatus::Missing => {
                next_steps.push(
                    "Traefik isn't running — run `oxid infra setup` to create/start it.".to_owned(),
                );
            }
        }
        if !self_wiring.is_fully_wired() {
            next_steps.push(format!(
                "This daemon's own container isn't fully wired for wake-on-request. Docker \
                 can't relabel a running container without recreating it, so this can't be \
                 automated — recreate the daemon's container/compose entry with:\n\
                 \x20\x20networks:\n\
                 \x20\x20\x20\x20- {network}\n\
                 \x20\x20labels:\n\
                 \x20\x20\x20\x20- \"traefik.enable=true\"\n\
                 \x20\x20\x20\x20- \"traefik.http.services.oxid-wake.loadbalancer.server.port=8080\"\n\
                 (plus the per-router `errors` middleware labels documented on \
                 `ControlPlane::traefik_labels`)."
            ));
        }
        Self {
            network,
            network_exists,
            traefik_status,
            self_wiring,
            next_steps,
        }
    }
}

/// Result of a capacity-aware deploy attempt (see
/// [`crate::service::control_plane::ControlPlane::deploy_or_queue`]).
#[derive(Debug, Clone)]
pub enum DeployOutcome {
    /// The deploy ran immediately and is now live.
    Deployed(Environment),
    /// The host doesn't currently have room; the request was persisted to
    /// `deploy_queue` (see [`crate::adapter::store::SqliteStore::enqueue_deploy`]) at this 1-based
    /// position and will be retried automatically as capacity frees up.
    Queued {
        /// 1-based position in the queue at the moment of enqueueing.
        position: u64,
    },
}

/// Whether a new deploy should proceed now or wait for capacity — see
/// [`crate::service::control_plane::ControlPlane::check_admission`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Enough host memory is free (after `reserved_memory_mb` and every
    /// other live environment's own reservation) for this request too.
    Fits,
    /// Not enough room right now, but the request could fit once other
    /// environments free memory — queue it rather than fail or overcommit.
    Queue,
}
