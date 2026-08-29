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
        }
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
