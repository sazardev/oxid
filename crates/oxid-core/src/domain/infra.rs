//! Types for the one-time infrastructure bootstrap (`oxid infra
//! status`/`setup`) that automates the Docker network + Traefik container an
//! operator otherwise has to wire up by hand for real scale-to-zero
//! (wake-on-request) to work — see `ControlPlane::infra_status`/
//! `infra_bootstrap` and `ContainerPort::ensure_network`/`ensure_traefik`/
//! `self_wiring_status`.
//!
//! Pure data only — no I/O. The adapters in `oxid-daemon` do the actual
//! Docker calls; this module just describes their inputs/outcomes so they
//! can be serialized straight to JSON from the API.

use serde::{Deserialize, Serialize};

/// Which container engine the daemon is actually talking to.
///
/// Detected rather than configured: `bollard` speaks the Docker API, and
/// Podman exposes a compatible socket, so Oxid can drive either. What
/// differs is what *works* — and a person deserves to be told which they
/// are on before they hit a limitation, not after.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeFlavor {
    /// Docker Engine.
    Docker,
    /// Podman through its Docker-compatible API.
    Podman,
    /// Something else answering the Docker API.
    Other(String),
}

impl RuntimeFlavor {
    /// The name to show a person.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Docker => "docker",
            Self::Podman => "podman",
            Self::Other(name) => name,
        }
    }
}

/// What the container runtime reports about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    /// Which engine.
    pub flavor: RuntimeFlavor,
    /// Its version string.
    pub version: String,
    /// Whether it runs without root.
    pub rootless: bool,
    /// Whether builds go through `BuildKit`, which is what produces the
    /// cache-effectiveness numbers a build report shows.
    pub buildkit: bool,
}

/// Something that does not work on this runtime, phrased for a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Limitation {
    /// No `BuildKit` means no cache statistics.
    NoBuildKitMetrics,
    /// A rootless engine cannot bind a privileged host port.
    RootlessPrivilegedPort(u16),
    /// Traefik's Docker provider against a non-Docker socket is not
    /// something Oxid has verified.
    TraefikProviderUnverified(String),
}

impl Limitation {
    /// The line an operator reads.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::NoBuildKitMetrics => "this runtime does not report BuildKit progress, so \
                 builds show a duration but no cache-hit percentage"
                .to_owned(),
            Self::RootlessPrivilegedPort(port) => format!(
                "this runtime is rootless and cannot bind host port {port}; either raise \
                 net.ipv4.ip_unprivileged_port_start, or set OXID_TRAEFIK_HTTP_PORT to a \
                 port above 1024"
            ),
            Self::TraefikProviderUnverified(flavor) => format!(
                "Traefik's Docker provider against a {flavor} socket is not verified by \
                 Oxid: direct-publish mode (OXID_DOCKER_NETWORK unset) is the supported \
                 topology on this runtime"
            ),
        }
    }
}

/// Everything about `info` that will not work as it does on Docker.
///
/// Pure so every combination is testable without a container runtime —
/// which is the point, because the combinations that matter (rootless plus
/// a privileged port, non-Docker plus Traefik) are the ones nobody has a
/// machine for when they need the answer.
#[must_use]
pub fn limitations(
    info: &RuntimeInfo,
    traefik_http_port: u16,
    traefik_enabled: bool,
) -> Vec<Limitation> {
    let mut out = Vec::new();
    if !info.buildkit {
        out.push(Limitation::NoBuildKitMetrics);
    }
    if info.rootless && traefik_http_port < 1024 {
        out.push(Limitation::RootlessPrivilegedPort(traefik_http_port));
    }
    if traefik_enabled && info.flavor != RuntimeFlavor::Docker {
        out.push(Limitation::TraefikProviderUnverified(
            info.flavor.as_str().to_owned(),
        ));
    }
    out
}

/// Outcome of [`crate::ContainerPort::ensure_network`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkStatus {
    /// The network didn't exist and was just created.
    Created,
    /// The network already existed; nothing was changed.
    AlreadyExisted,
}

/// Desired configuration for the built-in Traefik container that
/// [`crate::ContainerPort::ensure_traefik`] creates/starts. Mirrors the
/// `traefik` service an operator would otherwise hand-write in
/// `docker-compose.yml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraefikSpec {
    /// Docker network Traefik and every deployed environment share.
    pub network: String,
    /// Traefik image to run.
    pub image: String,
    /// Name of the Traefik container itself.
    pub container_name: String,
    /// Host port Traefik's `web` entrypoint is published on. The container
    /// side is always 80; this is only where it surfaces on the host, so an
    /// operator whose 80 is already taken can still run `oxid infra setup`.
    pub http_port: u16,
    /// Path to the Docker socket, mounted read-only into the container so
    /// Traefik's Docker provider can watch for label changes.
    pub docker_socket_path: String,
    /// Host port Traefik's `websecure` entrypoint is published on, when TLS
    /// is configured. `None` keeps Traefik HTTP-only, which is what every
    /// install without [`Self::acme`] gets.
    #[serde(default)]
    pub https_port: Option<u16>,
    /// Automatic certificates, or `None` for plain HTTP.
    #[serde(default)]
    pub acme: Option<AcmeConfig>,
}

/// How ACME proves the operator controls the domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AcmeChallenge {
    /// Let's Encrypt fetches a token over plain HTTP on port 80. Needs no
    /// credentials, needs the host publicly reachable, and issues **one
    /// certificate per branch** — which runs into the rate limit (50 new
    /// certificates per registered domain per week) on any repository with
    /// a lot of live branches.
    Http01,
    /// A TXT record proves control of the whole domain, so a single
    /// wildcard covers every branch that will ever exist and the host needs
    /// no inbound reachability at all. Needs credentials for the DNS
    /// provider.
    Dns01 {
        /// lego provider code, e.g. `cloudflare`, `route53`, `digitalocean`.
        provider: String,
        /// Names — never values — of the environment variables the provider
        /// needs. The adapter resolves each against the daemon's own
        /// environment when it creates the container, so a credential never
        /// enters this struct, is never serialized, and can never surface in
        /// an API response or a log field.
        env_keys: Vec<String>,
    },
}

/// Automatic certificate configuration for deployed environments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcmeConfig {
    /// Contact address Let's Encrypt sends expiry warnings to.
    pub email: String,
    /// How control of the domain is proven.
    pub challenge: AcmeChallenge,
    /// ACME directory URL, or `None` for Let's Encrypt production. Point it
    /// at staging while setting this up — production rate limits are
    /// unforgiving and staging's are not.
    #[serde(default)]
    pub ca_directory: Option<String>,
    /// Docker volume holding `acme.json`.
    ///
    /// A **named volume**, not a host bind mount, and deliberately: Traefik
    /// refuses to start when `acme.json` is not `0600`, and a bind mount
    /// created by an operator is almost always `0644`. That is the single
    /// most common way ACME-on-Traefik fails, and a named volume cannot
    /// have it.
    pub storage_volume: String,
    /// Name of the certificate resolver, referenced by every router label.
    pub resolver_name: String,
    /// Whether Traefik redirects `web` to `websecure`.
    pub http_redirect: bool,
}

impl AcmeConfig {
    /// The wildcard that covers every branch of `base_domain`, for DNS-01.
    #[must_use]
    pub fn wildcard_for(base_domain: &str) -> String {
        format!("*.{base_domain}")
    }

    /// Whether this configuration yields one certificate covering every
    /// branch (DNS-01) rather than one per branch (HTTP-01).
    #[must_use]
    pub fn is_wildcard(&self) -> bool {
        matches!(self.challenge, AcmeChallenge::Dns01 { .. })
    }
}

impl TraefikSpec {
    /// Builds a spec with every field defaulted except `network`, which is
    /// always required — Traefik is useless without knowing which network
    /// to watch.
    #[must_use]
    pub fn new(network: impl Into<String>) -> Self {
        Self {
            network: network.into(),
            // `latest`, matching `docker-compose.yml` (verified live):
            // Docker Engine >= 29 rejects API versions below 1.40, and
            // Traefik up to v3.5 vendors a client that negotiates 1.24 and
            // ignores `DOCKER_API_VERSION` — every router silently 404s with
            // "client version 1.24 is too old" in the proxy log. A pinned
            // older tag here meant `oxid infra setup` produced a Traefik
            // that routed nothing on a current Docker.
            image: "traefik:latest".to_owned(),
            container_name: "oxid-traefik".to_owned(),
            http_port: 80,
            docker_socket_path: "/var/run/docker.sock".to_owned(),
            // HTTP-only, exactly as before ACME existed. Every install that
            // does not configure certificates keeps byte-for-byte today's
            // Traefik.
            https_port: None,
            acme: None,
        }
    }

    /// Returns this spec with automatic certificates enabled.
    #[must_use]
    pub fn with_acme(mut self, acme: AcmeConfig, https_port: u16) -> Self {
        self.acme = Some(acme);
        self.https_port = Some(https_port);
        self
    }
}

/// Outcome of [`crate::ContainerPort::ensure_traefik`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraefikStatus {
    /// No Traefik container existed yet; one was created and started.
    Created,
    /// A container already existed and was already running; untouched.
    AlreadyRunning,
    /// A container already existed but was stopped; it was started.
    StartedFromStopped,
}

/// Read-only detection of whether the daemon's *own* running container is
/// wired for wake-on-request: joined to the Traefik network and carrying
/// the labels that let Traefik's `errors` middleware forward a 502/504 to
/// this daemon's `/api/v1/wake` via the global `oxid-wake` service.
///
/// Deliberately detection-only — see
/// [`crate::ContainerPort::self_wiring_status`] for why this never
/// self-corrects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum SelfWiringStatus {
    /// Not running inside a Docker container at all (e.g. `cargo run`
    /// directly on the host, or `HOSTNAME` was overridden) — there's
    /// nothing to detect.
    NotContainerized,
    /// Running in Docker, but the daemon's own container couldn't be
    /// inspected (e.g. no socket access, or the reported hostname doesn't
    /// match any container Docker knows about).
    Unknown,
    /// The daemon's own container was found and inspected.
    Detected {
        /// The container id/hostname that was inspected.
        container_id: String,
        /// Whether it's attached to the Traefik network.
        joined_network: bool,
        /// Whether it carries `traefik.enable=true`.
        has_traefik_enable_label: bool,
        /// Whether any label references the global `oxid-wake` service
        /// (the `errors` middleware wiring described in
        /// `control_plane.rs`'s `traefik_labels` doc comment).
        references_oxid_wake: bool,
        /// Whether the daemon carries the lowest-priority catch-all router
        /// that makes wake-on-request reachable at all. A scaled-to-zero
        /// branch is a stopped container, and Traefik only publishes
        /// routers for running ones — without this router its host matches
        /// nothing and the request 404s at the proxy instead of waking the
        /// environment.
        has_wake_catchall: bool,
    },
}

impl SelfWiringStatus {
    /// True only when fully wired for wake-on-request: detected, joined to
    /// the network, and carrying every required label.
    #[must_use]
    pub fn is_fully_wired(&self) -> bool {
        matches!(
            self,
            Self::Detected {
                joined_network: true,
                has_traefik_enable_label: true,
                references_oxid_wake: true,
                has_wake_catchall: true,
                ..
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_containerized_and_unknown_are_never_fully_wired() {
        assert!(!SelfWiringStatus::NotContainerized.is_fully_wired());
        assert!(!SelfWiringStatus::Unknown.is_fully_wired());
    }

    #[test]
    fn detected_is_fully_wired_only_when_every_flag_is_true() {
        let full = SelfWiringStatus::Detected {
            container_id: "abc123".to_owned(),
            joined_network: true,
            has_traefik_enable_label: true,
            references_oxid_wake: true,
            has_wake_catchall: true,
        };
        assert!(full.is_fully_wired());

        let missing_label = SelfWiringStatus::Detected {
            container_id: "abc123".to_owned(),
            joined_network: true,
            has_traefik_enable_label: false,
            references_oxid_wake: true,
            has_wake_catchall: true,
        };
        assert!(!missing_label.is_fully_wired());

        let missing_network = SelfWiringStatus::Detected {
            container_id: "abc123".to_owned(),
            joined_network: false,
            has_traefik_enable_label: true,
            references_oxid_wake: true,
            has_wake_catchall: true,
        };
        assert!(!missing_network.is_fully_wired());

        let missing_wake = SelfWiringStatus::Detected {
            container_id: "abc123".to_owned(),
            joined_network: true,
            has_traefik_enable_label: true,
            references_oxid_wake: false,
            has_wake_catchall: true,
        };
        assert!(!missing_wake.is_fully_wired());

        let missing_catchall = SelfWiringStatus::Detected {
            container_id: "abc123".to_owned(),
            joined_network: true,
            has_traefik_enable_label: true,
            references_oxid_wake: true,
            has_wake_catchall: false,
        };
        assert!(!missing_catchall.is_fully_wired());
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;

    fn info(flavor: RuntimeFlavor, rootless: bool, buildkit: bool) -> RuntimeInfo {
        RuntimeInfo {
            flavor,
            version: "test".to_owned(),
            rootless,
            buildkit,
        }
    }

    #[test]
    fn docker_with_buildkit_on_port_80_has_nothing_to_warn_about() {
        let docker = info(RuntimeFlavor::Docker, false, true);
        assert!(limitations(&docker, 80, true).is_empty());
    }

    /// Verified live against Podman 6.1: builds succeed and report a
    /// duration with no cache percentage, because the progress stream is
    /// not `BuildKit`'s.
    #[test]
    fn a_runtime_without_buildkit_loses_only_the_cache_numbers() {
        let podman = info(RuntimeFlavor::Podman, false, false);
        let found = limitations(&podman, 8080, false);
        assert_eq!(found, vec![Limitation::NoBuildKitMetrics]);
        assert!(found[0].describe().contains("cache-hit"));
    }

    #[test]
    fn rootless_cannot_take_a_privileged_port_and_says_which() {
        let rootless = info(RuntimeFlavor::Podman, true, false);
        let found = limitations(&rootless, 80, false);
        assert!(found.contains(&Limitation::RootlessPrivilegedPort(80)));
        // Above 1024 is fine, so the warning must not fire for it.
        assert!(
            !limitations(&rootless, 8080, false)
                .contains(&Limitation::RootlessPrivilegedPort(8080))
        );
    }

    /// Traefik's Docker provider is the real portability question on a
    /// non-Docker socket — not bollard, which already works. Say so instead
    /// of letting someone find out from routes that never appear.
    #[test]
    fn traefik_is_flagged_as_unverified_off_docker_and_only_when_enabled() {
        let podman = info(RuntimeFlavor::Podman, false, false);
        assert!(
            limitations(&podman, 8080, true)
                .iter()
                .any(|l| matches!(l, Limitation::TraefikProviderUnverified(_)))
        );
        assert!(
            !limitations(&podman, 8080, false)
                .iter()
                .any(|l| matches!(l, Limitation::TraefikProviderUnverified(_)))
        );
    }
}
