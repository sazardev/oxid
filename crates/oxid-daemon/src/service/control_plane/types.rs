use oxid_core::{BuildReport, ContainerStatus, Environment, EnvironmentId, SelfWiringStatus};

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

/// What a deploy should do when the host has no room for it right now.
///
/// Admission used to be a plain `bool` decided *before* the git checkout,
/// against the project's registered config — the only thing known that
/// early. Once a branch's own `oxid.toml` started being honoured that was no
/// longer the request being made, so the gate could wave through a branch
/// asking for far more memory than was free. The check now happens once,
/// after the checkout, with the real numbers; this says what to do with the
/// answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionMode {
    /// A fresh request (`oxid up`, a webhook). Doesn't fit: put it on the
    /// queue and report its position.
    Enqueue,
    /// A retry of something already on the queue. Doesn't fit: report that
    /// without enqueuing it a second time — the caller keeps the entry it
    /// already holds.
    AlreadyQueued,
    /// Deploy regardless of capacity (a rollback, which replaces an
    /// environment that is already accounted for).
    Bypass,
}

/// Aggregate node-wide counts for the web dashboard's overview — see
/// [`crate::service::control_plane::ControlPlane::node_stats`].
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct NodeStats {
    /// Which container engine this daemon drives (`docker`, `podman`, …)
    /// and its version, so a bug report says what it ran on without anyone
    /// having to ask.
    #[serde(default)]
    pub runtime: String,
    /// Things that do not work on this runtime, already phrased for a
    /// person. Empty on Docker.
    #[serde(default)]
    pub runtime_limitations: Vec<String>,
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
    /// Environments whose deploy failed. Counted separately from
    /// `environments_destroyed` so the dashboard can surface "someone's push
    /// is broken" rather than folding it into routine teardowns.
    pub environments_build_failed: u64,
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
    /// One row per node in the fleet.
    ///
    /// `host_total_memory_bytes` and `host_cpu_count` above stay as they
    /// were and keep meaning the control plane's own machine: every client,
    /// dashboard and script reading them predates the fleet, and quietly
    /// turning them into a sum would have made every existing reader wrong
    /// without changing its code. The fleet is additional information, not
    /// a redefinition of what was there.
    ///
    /// A single-node install gets exactly one entry, which is what it
    /// always effectively reported.
    #[serde(default)]
    pub nodes: Vec<NodeUsage>,
}

/// One node's share of the fleet, for `oxid stats` and the dashboard.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct NodeUsage {
    /// Node id.
    pub id: u64,
    /// Operator-chosen name.
    pub name: String,
    /// `active`, `draining` or `down`.
    pub state: String,
    /// Whether this daemon currently holds a client for it. `false` with
    /// `state: active` means the row is fine and the connection is not —
    /// a node registered while this process could not reach it.
    pub connected: bool,
    /// Total memory the node's Docker reports, in bytes. Zero until a probe
    /// has reached it.
    pub total_memory_bytes: u64,
    /// CPUs the node's Docker reports.
    pub cpu_count: u32,
    /// Memory (MB) promised to `running` and `building` environments here.
    pub committed_memory_mb: u64,
    /// Environments on it that are not `destroyed`.
    pub environments_live: u64,
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
    /// Host port Traefik's `web` entrypoint is published on
    /// (`OXID_TRAEFIK_HTTP_PORT`, default 80). Reported because a branch URL
    /// is only reachable without a port suffix when this is 80.
    pub traefik_http_port: u16,
    /// Whether this daemon's own container is joined to `network` and
    /// labeled for wake-on-request — detection only, see
    /// [`SelfWiringStatus`].
    pub self_wiring: SelfWiringStatus,
    /// Which ACME challenge is configured (`dns-01`/`http-01`), or `None`
    /// when environments are served over plain HTTP.
    #[serde(default)]
    pub acme_challenge: Option<String>,
    /// Ways the running Traefik does not match the one Oxid would create,
    /// already phrased for a person. Empty is the healthy case.
    #[serde(default)]
    pub traefik_drift: Vec<String>,
    /// [`SelfWiringStatus::is_fully_wired`], flattened for consumers that
    /// want the verdict rather than the evidence.
    ///
    /// Serialized rather than left to the client to derive because the
    /// dashboard was already reading a `self_wiring_ok` that no version of
    /// this struct ever sent: the onboarding wizard's "wired for
    /// wake-on-request" indicator read `undefined` and therefore showed red
    /// on daemons that were perfectly wired.
    pub self_wiring_ok: bool,
    /// The URL Traefik polls this daemon at for the fleet's routers, or
    /// `None` when only the Docker label provider is configured.
    ///
    /// Worth reporting on its own rather than folding into `next_steps`,
    /// because it changes what the *other* checks mean: with it, a missing
    /// `oxid-wake-catchall` is a redundancy rather than a hole.
    #[serde(default)]
    pub http_provider: Option<String>,
    /// Human-readable, actionable instructions for whatever's missing.
    /// Empty when everything is fully wired.
    pub next_steps: Vec<String>,
}

impl InfraStatus {
    /// Attaches the TLS verdict, and folds any drift into `next_steps` so
    /// it reaches every consumer that already reads them — the CLI, the
    /// dashboard and `oxid doctor` — without each learning a new field.
    pub(crate) fn with_tls(mut self, acme_challenge: Option<String>, drift: Vec<String>) -> Self {
        self.next_steps.extend(drift.iter().cloned());
        self.acme_challenge = acme_challenge;
        self.traefik_drift = drift;
        self
    }

    pub(crate) fn new(
        network: String,
        network_exists: bool,
        traefik_status: ContainerStatus,
        traefik_http_port: u16,
        self_wiring: SelfWiringStatus,
        http_provider: Option<String>,
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
        // The catch-all stops being mandatory once Traefik polls this
        // daemon for its routers.
        //
        // It exists because a stopped container publishes no labels and
        // therefore has no router, so nothing catches the request that
        // should wake it — a lowest-priority router on the *daemon's own*
        // container had to stand in. Under the HTTP provider a router comes
        // from the environment's row and exists whether its container runs
        // or not, so the request reaches the branch's own router, the proxy
        // has no target, Traefik answers 502, and the `errors` middleware
        // fires. The catch-all becomes redundant.
        //
        // Still reported, and deliberately still *supported*: it remains
        // the only wake path for the label provider and for direct-publish
        // mode, and a migration must never quietly remove behaviour that
        // works. What changes is that it is no longer a red mark on an
        // install that has something better.
        if !self_wiring.is_fully_wired() && http_provider.is_none() {
            next_steps.push(format!(
                "This daemon's own container isn't fully wired for wake-on-request. Docker \
                 can't relabel a running container without recreating it, so this can't be \
                 automated — recreate the daemon's container/compose entry with:\n\
                 \x20\x20networks:\n\
                 \x20\x20\x20\x20- {network}\n\
                 \x20\x20labels:\n\
                 \x20\x20\x20\x20- \"traefik.enable=true\"\n\
                 \x20\x20\x20\x20- \"traefik.http.services.oxid-wake.loadbalancer.server.port=8080\"\n\
                 \x20\x20\x20\x20- \"traefik.http.routers.oxid-wake-catchall.rule=HostRegexp(`^.+$`)\"\n\
                 \x20\x20\x20\x20- \"traefik.http.routers.oxid-wake-catchall.priority=1\"\n\
                 \x20\x20\x20\x20- \"traefik.http.routers.oxid-wake-catchall.entrypoints=web,websecure\"\n\
                 \x20\x20\x20\x20- \"traefik.http.routers.oxid-wake-catchall.service=oxid-wake\"\n\
                 \x20\x20\x20\x20- \"traefik.http.routers.oxid-wake-catchall.middlewares=oxid-wake-rewrite\"\n\
                 \x20\x20\x20\x20- \"traefik.http.middlewares.oxid-wake-rewrite.replacepath.path=/api/v1/wake\"\n\
                 The catch-all router is what makes a scaled-to-zero branch \
                 reachable: its container is stopped, so Traefik publishes no \
                 router of its own for it. See `docker-compose.yml`."
            ));
        }
        Self {
            network,
            network_exists,
            traefik_status,
            traefik_http_port,
            self_wiring_ok: self_wiring.is_fully_wired(),
            self_wiring,
            http_provider,
            next_steps,
            // Filled by `with_tls`; plain HTTP with no drift is the shape
            // every install without certificates keeps.
            acme_challenge: None,
            traefik_drift: Vec::new(),
        }
    }
}

/// Result of a capacity-aware deploy attempt (see
/// [`crate::service::control_plane::ControlPlane::deploy_or_queue`]).
#[derive(Debug, Clone)]
// The variant-size gap is `Environment`'s own doing; `DeployReport` is only
// three integers, and boxing it would buy an allocation per deploy for no
// readability gain.
#[allow(clippy::large_enum_variant)]
pub enum DeployOutcome {
    /// The deploy ran immediately and is now live, with a report of what
    /// the build/provisioning actually did.
    Deployed(Environment, DeployReport),
    /// The host doesn't currently have room; the request was persisted to
    /// `deploy_queue` (see [`crate::adapter::store::SqliteStore::enqueue_deploy`]) at this 1-based
    /// position and will be retried automatically as capacity frees up.
    Queued {
        /// 1-based position in the queue at the moment of enqueueing.
        position: u64,
    },
}

/// What a deploy did beyond flipping state: how the image build went
/// (duration, cache effectiveness) and what shared resources were leased
/// for the branch. Surfaced so the CLI can print DESIGN.md §3.3's
/// "[+] Shared Postgres instance detected. Created db_feature_login → [>]
/// Building image (Cache hit: 85%)" lines; note that deploys retried from
/// the queue server-side have no waiting caller to show it to and their
/// report is simply dropped.
#[derive(Debug, Clone)]
pub struct DeployReport {
    /// Image-build outcome (parsed from BuildKit's progress stream —
    /// zeroed totals when nothing could be observed).
    pub build: BuildReport,
    /// One human-readable line per declared dependency, e.g.
    /// "created postgres database `db_app_feature_x` (shared `local-pg`)".
    pub dependencies: Vec<String>,
}

/// Whether a new deploy should proceed now, and where — see
/// [`crate::service::control_plane::ControlPlane::place_deploy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    /// A node has room for it. Carries which one: with a fleet, "it fits"
    /// and "where" are one decision, and splitting them would leave a
    /// window in which the answer could change.
    Fits(oxid_core::NodeId),
    /// Nowhere has room right now, but the request could fit once some
    /// environment frees memory — queue it rather than fail or overcommit.
    /// With one node this is the same "the host is full" it always was.
    Queue,
}

/// A service the deploy is about to run, with the image it will run.
///
/// The plan (`oxid_core::services::compose_plan`) says *what* each service
/// is; this pairs that with the image the build step produced or the
/// compose file named, which is the only thing the plan cannot know.
#[derive(Debug, Clone)]
pub(crate) struct DeployableService {
    /// The compose service name — and the hostname siblings resolve it by.
    pub name: String,
    /// The image reference to run.
    pub image: String,
    /// Port it listens on inside the container. `None` is a worker.
    pub container_port: Option<u16>,
    /// Whether the branch URL points at this one.
    pub is_primary: bool,
}
