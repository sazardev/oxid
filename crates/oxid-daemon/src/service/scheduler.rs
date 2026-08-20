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
    }
}
