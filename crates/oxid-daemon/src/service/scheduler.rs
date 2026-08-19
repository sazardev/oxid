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
                    println!(
                        "[gc] paused={} hibernated={} destroyed={}",
                        summary.paused, summary.hibernated, summary.destroyed
                    );
                }
                for (id, err) in &summary.errors {
                    eprintln!("[gc] environment `{id}` sweep failed: {err}");
                }
            }
            Err(err) => eprintln!("[gc] sweep failed: {err}"),
        }

        match cp.retry_queued_deploys().await {
            Ok(failures) => {
                for (id, err) in &failures {
                    eprintln!("[queue] queued deploy `{id}` failed to redeploy: {err}");
                }
            }
            Err(err) => eprintln!("[queue] retry pass failed: {err}"),
        }
    }
}
