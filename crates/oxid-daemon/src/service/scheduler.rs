//! Background scale-to-zero scheduler (SPEC.md §3.2).
//!
//! Periodically drives [`ControlPlane::sweep`] so idle environments are
//! paused, hibernated or destroyed without any external trigger.

use std::time::Duration;

use oxid_core::{ContainerPort, GitPort, OffsetDateTime};

use crate::service::control_plane::ControlPlane;

/// Runs the garbage-collection sweep every `interval`, forever.
///
/// Meant to be spawned as a background task, e.g.
/// `tokio::spawn(scheduler::run(cp, interval))`; it never returns.
pub async fn run<G, O>(cp: ControlPlane<G, O>, interval: Duration)
where
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
{
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;

        // Before the sweep and before the queue drain, because both of them
        // want an up-to-date picture of which nodes are answering: placement
        // must not send a deploy at a machine that stopped responding a
        // minute ago, and the sweep must not report every environment on it
        // as an error it could have known about.
        //
        // It records what it saw and nothing else — never an environment
        // row. A partition is indistinguishable from a dead machine, and
        // evicting on one is how two live copies of a branch end up
        // fighting over a URL.
        if let Err(err) = cp.probe_nodes().await {
            tracing::warn!(error = %err, "node health probe failed");
        }

        match cp.sweep(OffsetDateTime::now_utc()).await {
            Ok(summary) => {
                if summary.paused > 0 || summary.hibernated > 0 || summary.destroyed > 0 {
                    tracing::info!(
                        paused = summary.paused,
                        hibernated = summary.hibernated,
                        destroyed = summary.destroyed,
                        "gc sweep completed"
                    );
                }
                for (id, err) in &summary.errors {
                    tracing::warn!(environment_id = %id, error = %err, "gc sweep failed for environment");
                }
            }
            Err(err) => tracing::error!(error = %err, "gc sweep failed"),
        }

        match cp.retry_queued_deploys().await {
            Ok(failures) => {
                for (id, err) in &failures {
                    tracing::warn!(queue_id = id, error = %err, "queued deploy failed to redeploy");
                }
            }
            Err(err) => tracing::error!(error = %err, "deploy queue retry pass failed"),
        }

        // After the deploys, because a deploy is what produces something
        // worth saying. Building the client per tick rather than holding
        // one: this runs every 30 seconds and usually finds nothing to do,
        // and a client built at startup would be a connection pool kept
        // alive for a feature most installs never turn on.
        match crate::adapter::forge::HttpForge::new(forge_timeout_secs()) {
            Ok(forge) => {
                if let Err(err) = cp.drain_forge_notifications(&forge).await {
                    tracing::warn!(error = %err, "forge notification pass failed");
                }
            }
            Err(err) => tracing::debug!(error = ?err, "no forge client"),
        }
    }
}

/// Per-request timeout for git-host calls (`OXID_FORGE_TIMEOUT_SECS`).
fn forge_timeout_secs() -> u64 {
    std::env::var("OXID_FORGE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&s| s > 0)
        .unwrap_or(20)
}
