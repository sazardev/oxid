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
use std::time::Duration;

use arc_swap::ArcSwapOption;
use oxid_core::{BranchName, ProjectId};
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::sleep;

type BranchKey = (ProjectId, String);

/// Where a branch's traffic currently goes.
///
/// A host as well as a port, because a branch's container need not be on
/// this machine: with a fleet, the control plane's proxy is what bridges a
/// stable local port to a container published on `node.address`. For the
/// local node the host is loopback, which is exactly what this dialled
/// before nodes existed — so a single-node install sees no change at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Host to dial: `127.0.0.1` for the local node, the node's address
    /// otherwise.
    pub host: String,
    /// The container's published host port on that machine.
    pub port: u16,
}

struct BranchProxy {
    public_port: u16,
    /// `None` means "no upstream yet" — connections are dropped instead of
    /// hanging, so a client sees a fast, clean failure instead of a timeout.
    ///
    /// `ArcSwapOption` rather than an atomic port: the target is now two
    /// values, and they must change together. A host swapped a moment
    /// before its port would, for that moment, send a branch's traffic at
    /// the wrong machine's right port — which is not a connection failure
    /// but somebody else's application answering.
    target: Arc<ArcSwapOption<Target>>,
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
        let target = Arc::new(ArcSwapOption::empty());
        spawn_accept_loop(listener, Arc::clone(&target));
        proxies.insert(
            key,
            BranchProxy {
                public_port,
                target,
            },
        );
        Ok(public_port)
    }

    /// Atomically repoints a branch's proxy at a newly-deployed container's
    /// host port — the actual zero-downtime cutover moment. Connections
    /// already accepted keep talking to whatever they were already
    /// connected to; every connection accepted from this point on goes to
    /// the new target.
    pub async fn set_target(&self, project_id: ProjectId, branch: &BranchName, target: Target) {
        let key = (project_id, branch.to_string());
        if let Some(proxy) = self.proxies.lock().await.get(&key) {
            proxy.target.store(Some(Arc::new(target)));
        }
    }

    /// Where a branch's traffic currently goes, if anywhere. Test-facing,
    /// and the only way to observe a cutover without opening a socket.
    pub async fn target(&self, project_id: ProjectId, branch: &BranchName) -> Option<Target> {
        let key = (project_id, branch.to_string());
        self.proxies
            .lock()
            .await
            .get(&key)
            .and_then(|proxy| proxy.target.load_full())
            .map(|target| (*target).clone())
    }

    /// Clears a branch's current target (paused/hibernating/destroyed) so
    /// new connections fail fast instead of hanging.
    pub async fn mark_unavailable(&self, project_id: ProjectId, branch: &BranchName) {
        let key = (project_id, branch.to_string());
        if let Some(proxy) = self.proxies.lock().await.get(&key) {
            proxy.target.store(None);
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

fn spawn_accept_loop(listener: TcpListener, target: Arc<ArcSwapOption<Target>>) {
    tokio::spawn(async move {
        loop {
            let Ok((mut inbound, _)) = listener.accept().await else {
                continue;
            };
            let target = Arc::clone(&target);
            tokio::spawn(async move {
                // Read once, then use that one snapshot for the whole
                // connection: a cutover mid-handshake would otherwise dial
                // the new container with the old one's half-open state.
                let Some(current) = target.load_full() else {
                    return;
                };
                let Ok(mut outbound) =
                    TcpStream::connect((current.host.as_str(), current.port)).await
                else {
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
/// Probing `target.host` rather than loopback is also what turns a
/// mistyped `node.address` into an honest deploy failure instead of a green
/// report on a branch nobody can reach: the address the operator supplied is
/// not verifiable any other way, and this is the one moment Oxid uses it
/// before anybody else does.
pub async fn wait_until_ready(target: &Target, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if TcpStream::connect((target.host.as_str(), target.port))
            .await
            .is_ok()
        {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(200)).await;
    }
}
