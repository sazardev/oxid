//! Built-in per-branch TCP reverse proxy for direct-publish mode
//! (no Traefik configured).
//!
//! In direct-publish mode a branch's "address" is literally whichever host
//! port Docker happened to bind its container to — dynamic by design (see
//! `control_plane.rs`'s dynamic host-port assignment), which means a
//! redeploy that stops the old container before the new one is up always
//! has a gap, and even once the new one is up, its address has *changed*
//! out from under anyone already using the old one. Traefik mode doesn't
//! have this problem (containers join/leave behind a stable `Host()` rule),
//! but direct-publish mode has no proxy layer at all today.
//!
//! This gives every branch a small, stable "public port" — bound once,
//! reused across every redeploy of that branch — whose upstream target can
//! be swapped atomically. A redeploy builds and starts the new container
//! first, waits for it to actually accept connections, swaps the proxy's
//! target to it, and only *then* removes the old container. Nothing
//! observing the branch's public port ever sees a gap.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use oxid_core::{BranchName, ProjectId};
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::sleep;

type BranchKey = (ProjectId, String);

struct BranchProxy {
    public_port: u16,
    /// `0` means "no upstream yet" — connections are dropped instead of
    /// hanging, so a client sees a fast, clean failure instead of a timeout.
    target_port: Arc<AtomicU16>,
}

/// In-memory registry of every branch's stable proxy. Not itself
/// persisted — the port each one binds is persisted on
/// `Environment::public_port` so a daemon restart can ask to rebind the
/// exact same port instead of silently handing out a new one.
#[derive(Clone, Default)]
pub struct ProxyRegistry {
    proxies: Arc<Mutex<HashMap<BranchKey, BranchProxy>>>,
}

/// A proxy listener couldn't be bound.
#[derive(Debug, thiserror::Error)]
#[error("failed to bind proxy listener: {0}")]
pub struct ProxyError(#[source] std::io::Error);

impl ProxyRegistry {
    /// Ensures a branch has a running proxy, returning its stable public
    /// port. Reuses an already-running proxy for this branch if one
    /// exists; otherwise binds a new listener, preferring `preferred_port`
    /// (typically the environment's previously-persisted `public_port`)
    /// when it's still free, falling back to letting the OS pick one.
    ///
    /// # Errors
    /// Returns [`ProxyError`] only if no port at all could be bound.
    pub async fn ensure(
        &self,
        project_id: ProjectId,
        branch: &BranchName,
        preferred_port: Option<u16>,
    ) -> Result<u16, ProxyError> {
        let key = (project_id, branch.to_string());
        let mut proxies = self.proxies.lock().await;
        if let Some(existing) = proxies.get(&key) {
            return Ok(existing.public_port);
        }

        let listener = match preferred_port {
            Some(port) => match TcpListener::bind(("0.0.0.0", port)).await {
                Ok(listener) => listener,
                // The remembered port may no longer be free (something
                // else grabbed it while the daemon was down) — that's not
                // worth failing the deploy over, just pick a fresh one.
                Err(_) => TcpListener::bind(("0.0.0.0", 0))
                    .await
                    .map_err(ProxyError)?,
            },
            None => TcpListener::bind(("0.0.0.0", 0))
                .await
                .map_err(ProxyError)?,
        };
        let public_port = listener.local_addr().map_err(ProxyError)?.port();
        let target_port = Arc::new(AtomicU16::new(0));
        spawn_accept_loop(listener, Arc::clone(&target_port));
        proxies.insert(
            key,
            BranchProxy {
                public_port,
                target_port,
            },
        );
        Ok(public_port)
    }

    /// Atomically repoints a branch's proxy at a newly-deployed container's
    /// host port — the actual zero-downtime cutover moment. Connections
    /// already accepted keep talking to whatever they were already
    /// connected to; every connection accepted from this point on goes to
    /// the new target.
    pub async fn set_target(&self, project_id: ProjectId, branch: &BranchName, host_port: u16) {
        let key = (project_id, branch.to_string());
        if let Some(proxy) = self.proxies.lock().await.get(&key) {
            proxy.target_port.store(host_port, Ordering::Release);
        }
    }

    /// Clears a branch's current target (paused/hibernating/destroyed) so
    /// new connections fail fast instead of hanging.
    pub async fn mark_unavailable(&self, project_id: ProjectId, branch: &BranchName) {
        let key = (project_id, branch.to_string());
        if let Some(proxy) = self.proxies.lock().await.get(&key) {
            proxy.target_port.store(0, Ordering::Release);
        }
    }

    /// Drops a branch's proxy entirely, freeing its public port. Used when
    /// an environment is permanently destroyed.
    pub async fn remove(&self, project_id: ProjectId, branch: &BranchName) {
        self.proxies
            .lock()
            .await
            .remove(&(project_id, branch.to_string()));
    }
}

fn spawn_accept_loop(listener: TcpListener, target_port: Arc<AtomicU16>) {
    tokio::spawn(async move {
        loop {
            let Ok((mut inbound, _)) = listener.accept().await else {
                continue;
            };
            let target_port = Arc::clone(&target_port);
            tokio::spawn(async move {
                let port = target_port.load(Ordering::Acquire);
                if port == 0 {
                    return;
                }
                let Ok(mut outbound) = TcpStream::connect(("127.0.0.1", port)).await else {
                    return;
                };
                let _ = copy_bidirectional(&mut inbound, &mut outbound).await;
            });
        }
    });
}

/// Polls a freshly-started container's published port until it accepts TCP
/// connections, or `timeout` elapses (`false`). This is the "wait until
/// ready" gate a zero-downtime cutover depends on — swapping the proxy at a
/// container before it's actually listening would just move the outage
/// instead of removing it.
pub async fn wait_until_ready(host_port: u16, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if TcpStream::connect(("127.0.0.1", host_port)).await.is_ok() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(200)).await;
    }
}
