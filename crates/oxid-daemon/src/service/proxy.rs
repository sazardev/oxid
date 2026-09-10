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
    /// The accept loop's task handle, so [`ProxyRegistry::remove`] can stop
    /// it. Without this the task outlives the registry entry: the listener
    /// stays bound (and the task leaks) for the life of the process, so a
    /// branch destroyed and later redeployed could never rebind its
    /// persisted `public_port`, and every destroyed branch leaked a socket.
    accept_loop: tokio::task::JoinHandle<()>,
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
        let accept_loop = spawn_accept_loop(listener, Arc::clone(&target));
        proxies.insert(
            key,
            BranchProxy {
                public_port,
                target,
                accept_loop,
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
    ///
    /// Waits for the listener task to actually stop before returning, so
    /// the port is free the moment this resolves — a redeploy that rebinds
    /// the persisted `public_port` right after does not race it.
    pub async fn remove(&self, project_id: ProjectId, branch: &BranchName) {
        let removed = self
            .proxies
            .lock()
            .await
            .remove(&(project_id, branch.to_string()));
        if let Some(proxy) = removed {
            proxy.accept_loop.abort();
            let _ = proxy.accept_loop.await;
        }
    }
}

fn spawn_accept_loop(
    listener: TcpListener,
    target: Arc<ArcSwapOption<Target>>,
) -> tokio::task::JoinHandle<()> {
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
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn branch_named(name: &str) -> BranchName {
        BranchName::parse(name).expect("test branch name is valid")
    }

    /// A loopback TCP echo server, standing in for a deployed container.
    async fn echo_server() -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 64];
                    loop {
                        let Ok(n) = socket.read(&mut buf).await else {
                            return;
                        };
                        if n == 0 || socket.write_all(&buf[..n]).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (port, handle)
    }

    /// A redeploy must keep the branch's address stable — the promise
    /// direct-publish mode exists to make.
    #[tokio::test]
    async fn a_branch_keeps_the_same_public_port_across_calls() {
        let registry = ProxyRegistry::default();
        let branch = branch_named("feature-login");

        let first = registry.ensure(ProjectId(1), &branch, None).await.unwrap();
        let second = registry.ensure(ProjectId(1), &branch, None).await.unwrap();

        assert_eq!(first, second, "a redeploy must not move the public port");
    }

    /// The daemon restart path: the environment row remembers its port and
    /// the registry is asked for exactly that one.
    #[tokio::test]
    async fn a_remembered_port_is_rebound_when_it_is_still_free() {
        let registry = ProxyRegistry::default();
        let branch = branch_named("feature-login");
        let probe = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let preferred = probe.local_addr().unwrap().port();
        drop(probe);

        let got = registry
            .ensure(ProjectId(1), &branch, Some(preferred))
            .await
            .unwrap();

        assert_eq!(got, preferred);
    }

    /// If something else grabbed the remembered port while the daemon was
    /// down, that is not worth failing a deploy over — a fresh port is fine.
    #[tokio::test]
    async fn a_remembered_port_that_is_taken_falls_back_instead_of_failing() {
        let registry = ProxyRegistry::default();
        let branch = branch_named("feature-login");
        let occupied = TcpListener::bind(("0.0.0.0", 0)).await.unwrap();
        let taken = occupied.local_addr().unwrap().port();

        let got = registry
            .ensure(ProjectId(1), &branch, Some(taken))
            .await
            .unwrap();

        assert_ne!(got, taken);
    }

    /// End to end: bytes written to the public port come back from whatever
    /// the target points at.
    #[tokio::test]
    async fn traffic_flows_to_the_branchs_current_target() {
        let registry = ProxyRegistry::default();
        let branch = branch_named("feature-login");
        let (upstream_port, upstream) = echo_server().await;
        let public_port = registry.ensure(ProjectId(1), &branch, None).await.unwrap();
        registry
            .set_target(
                ProjectId(1),
                &branch,
                Target {
                    host: "127.0.0.1".to_owned(),
                    port: upstream_port,
                },
            )
            .await;

        let exchange = async {
            let mut client = TcpStream::connect(("127.0.0.1", public_port))
                .await
                .unwrap();
            client.write_all(b"ping").await.unwrap();
            let mut echoed = [0u8; 4];
            client.read_exact(&mut echoed).await.unwrap();
            echoed
        };
        let echoed = tokio::time::timeout(Duration::from_secs(5), exchange)
            .await
            .expect("the proxy never answered");

        assert_eq!(&echoed, b"ping");
        upstream.abort();
    }

    /// The zero-downtime cutover: the public port stays put and only the
    /// target changes.
    #[tokio::test]
    async fn a_cutover_swaps_the_target_without_changing_the_port() {
        let registry = ProxyRegistry::default();
        let branch = branch_named("feature-login");
        let public_port = registry.ensure(ProjectId(1), &branch, None).await.unwrap();
        let first = Target {
            host: "127.0.0.1".to_owned(),
            port: 1111,
        };
        let second = Target {
            host: "10.0.0.7".to_owned(),
            port: 2222,
        };

        registry
            .set_target(ProjectId(1), &branch, first.clone())
            .await;
        assert_eq!(registry.target(ProjectId(1), &branch).await, Some(first));
        assert_eq!(
            registry.ensure(ProjectId(1), &branch, None).await.unwrap(),
            public_port
        );

        registry
            .set_target(ProjectId(1), &branch, second.clone())
            .await;
        assert_eq!(registry.target(ProjectId(1), &branch).await, Some(second));
        assert_eq!(
            registry.ensure(ProjectId(1), &branch, None).await.unwrap(),
            public_port
        );
    }

    /// A sleeping branch fails fast instead of hanging new clients: the
    /// connection is accepted and closed.
    #[tokio::test]
    async fn mark_unavailable_closes_new_connections_instead_of_hanging() {
        let registry = ProxyRegistry::default();
        let branch = branch_named("feature-login");
        let (upstream_port, upstream) = echo_server().await;
        let public_port = registry.ensure(ProjectId(1), &branch, None).await.unwrap();
        registry
            .set_target(
                ProjectId(1),
                &branch,
                Target {
                    host: "127.0.0.1".to_owned(),
                    port: upstream_port,
                },
            )
            .await;

        registry.mark_unavailable(ProjectId(1), &branch).await;
        assert_eq!(registry.target(ProjectId(1), &branch).await, None);

        let mut client = TcpStream::connect(("127.0.0.1", public_port))
            .await
            .unwrap();
        let mut buf = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf))
            .await
            .expect("a branch with no upstream must not hang the client");
        assert_eq!(read.unwrap(), 0, "expected an immediate EOF");
        upstream.abort();
    }

    /// `remove` is what a destroyed environment calls, and its whole point
    /// over `mark_unavailable` is that the port is free again afterwards.
    #[tokio::test]
    async fn removing_a_branch_frees_its_public_port() {
        let registry = ProxyRegistry::default();
        let branch = branch_named("feature-login");
        let port = registry.ensure(ProjectId(1), &branch, None).await.unwrap();

        registry.remove(ProjectId(1), &branch).await;
        assert_eq!(registry.target(ProjectId(1), &branch).await, None);

        TcpListener::bind(("0.0.0.0", port))
            .await
            .expect("remove() must stop the listener, not just drop the map entry");
    }

    /// Keyed by (project, branch): two branches never share a port or a
    /// target, and neither do two projects' branches with the same name.
    #[tokio::test]
    async fn proxies_are_scoped_by_project_and_branch() {
        let registry = ProxyRegistry::default();
        let one = branch_named("one");
        let two = branch_named("two");

        let local_one = registry.ensure(ProjectId(1), &one, None).await.unwrap();
        let local_two = registry.ensure(ProjectId(1), &two, None).await.unwrap();
        let other_one = registry.ensure(ProjectId(2), &one, None).await.unwrap();
        assert_ne!(local_one, local_two);
        assert_ne!(local_one, other_one);
        assert_ne!(local_two, other_one);

        registry
            .set_target(
                ProjectId(1),
                &one,
                Target {
                    host: "127.0.0.1".to_owned(),
                    port: 1,
                },
            )
            .await;
        assert_eq!(registry.target(ProjectId(1), &two).await, None);
        assert_eq!(registry.target(ProjectId(2), &one).await, None);
    }
}
