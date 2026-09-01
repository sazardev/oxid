//! The set of nodes this control plane can place containers on.
//!
//! Oxid is one control plane over N Docker endpoints — the git cache, the
//! secrets, the audit trail and every lock stay here, and a node is nothing
//! but a Docker API plus an address (`MULTINODE.md` §3). That is why this is
//! a registry of clients rather than a cluster: adding a node changes the
//! *cardinality* of `ContainerPort`, not its contract. All 22 of its methods
//! already take a container name, a spec or an image tag; none of them ever
//! meant "here".
//!
//! **The door left open.** An agent per node — a small Oxid process on each
//! machine speaking a narrow protocol instead of a raw Docker socket — buys
//! two things this does not: it can refuse to touch anything not named
//! `oxid-*`, and it can run the per-branch proxy locally, taking the control
//! plane out of the data path. Both cost a protocol: 22 methods over HTTP,
//! including a build endpoint streaming a multi-hundred-megabyte tar, a
//! `stream_logs` relay, and a version-compatibility contract that has to
//! survive staggered upgrades. Building it needs no change here — an
//! `adapter/agent.rs` implementing `ContainerPort` alongside `oci.rs` slots
//! straight into this registry, and `ControlPlane` never learns the
//! difference.
//!
//! *When the control plane's bandwidth or its restart window becomes the
//! limit, that is when the agent earns its complexity.*

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use oxid_core::{Node, NodeEndpoint, NodeId};

/// How long a *status* query to a node may take before the caller gives up
/// on that node for the decision it is making.
///
/// Separate from, and far shorter than, the connection's own timeout, which
/// has to accommodate an image pull and a build. These are `docker info` and
/// `container status` — questions a live machine answers immediately.
///
/// It exists because a partitioned node **blackholes** rather than refusing:
/// there is no RST, so the connection sits until the kernel gives up.
/// Measured with `iptables -j DROP` on a registered node's port: a deploy
/// aimed at a perfectly healthy node took **121 seconds**, and the health
/// probe needed **126 seconds** to notice, because both walked the fleet one
/// node at a time with no deadline. Correctness held throughout — no
/// environment was touched — but a single dead machine froze every deploy in
/// the fleet, which is its own kind of outage.
///
/// Five seconds is generous for a query a healthy node answers in
/// milliseconds, and short enough that a dead one costs a pause rather than
/// an outage. A node that misses it is skipped *for that decision only*;
/// nothing is written, because a node's recorded state belongs to the health
/// probe alone.
pub const STATUS_DEADLINE: Duration = Duration::from_secs(5);

/// One node and the Docker client that reaches it.
#[derive(Debug)]
pub struct NodeHandle<O> {
    /// The row, for its address, its state and its name.
    pub node: Node,
    /// A client already connected to `node.endpoint`.
    pub oci: Arc<O>,
}

impl<O> NodeHandle<O> {
    /// The host the control plane's proxy should dial for a port published
    /// on this node — loopback unless the operator named an address.
    #[must_use]
    pub fn proxy_host(&self) -> &str {
        self.node.proxy_host()
    }
}

/// Every node this daemon currently holds a client for.
///
/// `ArcSwap` because the read path is every deploy, every GC action and
/// every wake, while the write path is an operator registering a node —
/// readers must never block behind a registration, and a reader that grabs
/// the map a moment before a node is removed simply finishes its work
/// against a client that still works.
///
/// `Arc` *around* the `ArcSwap`, not just inside it: `ControlPlane` derives
/// `Clone` and axum hands every handler a fresh clone, so a fleet that
/// copied its registry would let a node registered through one clone be
/// invisible to the next request — a deploy failing with `unknown node` for
/// no reason a log would explain.
#[derive(Debug)]
pub struct Fleet<O> {
    nodes: Arc<ArcSwap<HashMap<NodeId, Arc<NodeHandle<O>>>>>,
}

impl<O> Clone for Fleet<O> {
    fn clone(&self) -> Self {
        Self {
            nodes: Arc::clone(&self.nodes),
        }
    }
}

impl<O> Fleet<O> {
    /// A fleet of exactly one node: this daemon's own Docker socket,
    /// registered as node 1.
    ///
    /// This is what every existing install gets, and why an upgrade needs no
    /// configuration change: node 1 is seeded by migration `0020`, every
    /// environment row is backfilled to it, and `Environment::new` starts
    /// there.
    /// # Panics
    /// Never in practice: the local node's name and endpoint are compile-time
    /// constants that `Node::new` accepts.
    pub fn single(oci: O) -> Self {
        let node = Node::new(NodeId::LOCAL, "local", NodeEndpoint::Local)
            .expect("the local node's name and endpoint are constants");
        let mut map = HashMap::with_capacity(1);
        map.insert(
            NodeId::LOCAL,
            Arc::new(NodeHandle {
                node,
                oci: Arc::new(oci),
            }),
        );
        Self {
            nodes: Arc::new(ArcSwap::from_pointee(map)),
        }
    }

    /// The handle for `id`, or `None` when this daemon holds no client for
    /// it.
    ///
    /// `None` is not the same as "the node is down": it means this process
    /// has not connected to it, which after a restart is a transient state
    /// while the fleet is being rebuilt from the `nodes` table. Callers
    /// surface it as an error rather than treating the environments there as
    /// gone — nothing in Oxid rewrites an environment row because its node
    /// is unreachable.
    #[must_use]
    pub fn get(&self, id: NodeId) -> Option<Arc<NodeHandle<O>>> {
        self.nodes.load().get(&id).cloned()
    }

    /// The node this daemon itself runs on, which always exists.
    ///
    /// Infrastructure that is *the control plane's* rather than a node's —
    /// the Traefik in front of everything, the shared Docker network, the
    /// ACME volume — belongs here and only here.
    ///
    /// # Panics
    /// Never in practice: node 1 is inserted by [`Self::single`] and
    /// [`Self::deregister`] refuses to remove it, so it is present for the
    /// whole life of the registry.
    #[must_use]
    pub fn local(&self) -> Arc<NodeHandle<O>> {
        self.get(NodeId::LOCAL)
            .expect("the local node is registered for the lifetime of the fleet")
    }

    /// Every handle, lowest id first, so listings are stable.
    #[must_use]
    pub fn handles(&self) -> Vec<Arc<NodeHandle<O>>> {
        let map = self.nodes.load();
        let mut all: Vec<_> = map.values().cloned().collect();
        all.sort_by_key(|handle| handle.node.id);
        all
    }

    /// Adds or replaces a node's client.
    pub fn register(&self, node: Node, oci: Arc<O>) {
        self.mutate(|map| {
            map.insert(node.id, Arc::new(NodeHandle { node, oci }));
        });
    }

    /// Updates the stored row for a node already registered, keeping its
    /// client. What a health probe or a `drain` calls: reconnecting a
    /// working client to record a state change would drop in-flight work for
    /// nothing.
    pub fn refresh(&self, node: Node) {
        self.mutate(|map| {
            if let Some(existing) = map.get(&node.id) {
                let oci = Arc::clone(&existing.oci);
                map.insert(node.id, Arc::new(NodeHandle { node, oci }));
            }
        });
    }

    /// Drops a node's client. Node 1 is refused: it is this daemon, and a
    /// fleet without it has nowhere to put the Traefik that fronts
    /// everything.
    pub fn deregister(&self, id: NodeId) {
        if id == NodeId::LOCAL {
            return;
        }
        self.mutate(|map| {
            map.remove(&id);
        });
    }

    /// Copy-on-write: build the next map from the current one and swap it
    /// in. Readers hold an `Arc` to the old map and are never interrupted.
    fn mutate(&self, apply: impl FnOnce(&mut HashMap<NodeId, Arc<NodeHandle<O>>>)) {
        let mut next = (**self.nodes.load()).clone();
        apply(&mut next);
        self.nodes.store(Arc::new(next));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry stores clients; it never calls one. A unit type is
    /// therefore a complete stand-in, and keeping it one is what stops these
    /// tests from having to be rewritten every time `ContainerPort` grows a
    /// method.
    #[derive(Debug)]
    struct NoOci;

    /// The property an upgrade depends on: with no configuration at all,
    /// there is a node, it is node 1, and it is this daemon's own socket.
    #[test]
    fn a_fresh_fleet_is_exactly_the_local_node() {
        let fleet = Fleet::single(NoOci);
        assert_eq!(fleet.handles().len(), 1);
        let local = fleet.local();
        assert_eq!(local.node.id, NodeId::LOCAL);
        assert_eq!(local.node.endpoint, NodeEndpoint::Local);
        assert_eq!(local.proxy_host(), "127.0.0.1");
    }

    #[test]
    fn registering_a_node_makes_it_reachable_and_leaves_local_alone() {
        let fleet = Fleet::single(NoOci);
        let mut eu1 = Node::new(NodeId(2), "eu-1", NodeEndpoint::from("tcp://a:2376")).unwrap();
        eu1.address = Some("10.0.0.4".to_owned());
        fleet.register(eu1, Arc::new(NoOci));

        assert_eq!(fleet.get(NodeId(2)).unwrap().proxy_host(), "10.0.0.4");
        assert_eq!(fleet.local().node.id, NodeId::LOCAL);
        assert_eq!(fleet.handles().len(), 2);
    }

    /// A clone must share the registry — `ControlPlane` is cloned per
    /// request, and a node registered through one clone that the next
    /// request cannot see is a deploy that fails for no visible reason.
    #[test]
    fn clones_share_one_registry() {
        let fleet = Fleet::single(NoOci);
        let other = fleet.clone();
        other.register(
            Node::new(NodeId(2), "eu-1", NodeEndpoint::from("tcp://a:2376")).unwrap(),
            Arc::new(NoOci),
        );
        assert!(
            fleet.get(NodeId(2)).is_some(),
            "a node registered through a clone must be visible to the original"
        );
    }

    /// `refresh` records a state change without dropping the client: a
    /// probe marking a node `down` must not sever the connection that would
    /// tell it the node is back.
    #[test]
    fn refresh_keeps_the_client() {
        let fleet = Fleet::single(NoOci);
        let node = Node::new(NodeId(2), "eu-1", NodeEndpoint::from("tcp://a:2376")).unwrap();
        let oci = Arc::new(NoOci);
        fleet.register(node.clone(), Arc::clone(&oci));

        let mut drained = node;
        drained.state = oxid_core::NodeState::Draining;
        fleet.refresh(drained);

        let handle = fleet.get(NodeId(2)).unwrap();
        assert_eq!(handle.node.state, oxid_core::NodeState::Draining);
        assert!(
            Arc::ptr_eq(&handle.oci, &oci),
            "refreshing a node's row must reuse its existing client"
        );
    }

    /// `refresh` on a node nobody registered must not conjure a handle with
    /// no client — there is no client to give it.
    #[test]
    fn refresh_ignores_an_unregistered_node() {
        let fleet = Fleet::single(NoOci);
        fleet.refresh(Node::new(NodeId(9), "ghost", NodeEndpoint::Local).unwrap());
        assert!(fleet.get(NodeId(9)).is_none());
    }

    /// The local node is this daemon. Removing it would leave the fleet
    /// with nowhere to put the Traefik that fronts every environment.
    #[test]
    fn the_local_node_cannot_be_deregistered() {
        let fleet = Fleet::single(NoOci);
        fleet.deregister(NodeId::LOCAL);
        assert_eq!(fleet.handles().len(), 1);

        fleet.register(
            Node::new(NodeId(2), "eu-1", NodeEndpoint::from("tcp://a:2376")).unwrap(),
            Arc::new(NoOci),
        );
        fleet.deregister(NodeId(2));
        assert!(fleet.get(NodeId(2)).is_none());
        assert_eq!(fleet.handles().len(), 1);
    }
}
