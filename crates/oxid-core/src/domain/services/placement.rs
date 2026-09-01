//! Where a deploy should run.
//!
//! Pure, and deliberately so: the interesting part of placement is a
//! handful of ordering rules, and every one of them is testable without a
//! Docker, a network or a database. The adapter's job is to gather the
//! numbers; this decides what they mean.
//!
//! Two rules, in this order, and the order is the whole design:
//!
//! 1. **Affinity wins.** A redeploy stays on the node it is already on, as
//!    long as that node will still have it. Images are not distributed —
//!    each node builds its own copy — so moving a branch means rebuilding
//!    it from scratch. Staying put keeps the layer cache warm, which is the
//!    difference between a two-second rebuild and a two-minute one.
//! 2. **Otherwise, most free memory.** Not round-robin, not least-loaded by
//!    count: the thing admission is actually rationing is memory, so the
//!    node with the most of it left is the one least likely to have to
//!    queue the deploy after this one.
//!
//! Nothing rebalances. A node that fills simply stops receiving deploys
//! and the queue accumulates behind it — a single decision at deploy time,
//! never revisited, because the alternative is moving running branches
//! around under people who are using them.

use crate::domain::node::{NodeId, NodeState};

/// What one node can take, as placement sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeCapacity {
    /// Which node this describes.
    pub id: NodeId,
    /// Whether it accepts new environments at all.
    pub state: NodeState,
    /// Memory (MB) this node has for environments: what Docker reports,
    /// minus whatever is reserved for the OS and the daemons on it.
    pub usable_mb: u64,
    /// Memory (MB) already promised to environments on it — `running` plus
    /// `building`, since a deploy that has passed admission but not yet
    /// started its container is memory already spoken for.
    pub committed_mb: u64,
    /// Whether a probe has actually reached this node. A node nobody has
    /// successfully talked to is never chosen, however much memory its row
    /// claims: the row's numbers are zeros until a probe fills them in, and
    /// zero free memory would rank it last anyway — but saying so
    /// explicitly is what stops a future change to the ranking from
    /// accidentally making an unreachable node look attractive.
    pub reachable: bool,
}

impl NodeCapacity {
    /// Memory left, in MB. Saturating: over-commitment is possible and
    /// normal (scale-to-zero exists precisely so a node hosts more
    /// environments than could ever run at once), and it means "nothing
    /// free", not a negative amount.
    #[must_use]
    pub const fn free_mb(&self) -> u64 {
        self.usable_mb.saturating_sub(self.committed_mb)
    }

    /// Whether this node could take a deploy asking for `request_mb`.
    #[must_use]
    pub const fn fits(&self, request_mb: u64) -> bool {
        self.reachable && self.state.accepts_placements() && self.free_mb() >= request_mb
    }

    /// Whether the deploy could *ever* fit here, ignoring what is running.
    /// A request larger than this is one no amount of waiting will satisfy.
    #[must_use]
    pub const fn could_ever_fit(&self, request_mb: u64) -> bool {
        self.usable_mb >= request_mb
    }
}

/// The answer placement gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// Run it here.
    Node(NodeId),
    /// Nowhere has room right now. The deploy goes on the queue and is
    /// retried; some node will free memory when a branch idles out.
    Queue,
    /// No node in the fleet is *large enough*, whatever it is running. No
    /// amount of queueing fixes this, so the deploy is refused immediately
    /// with the largest node's size, which is the number the operator needs.
    TooLarge {
        /// The most memory any single node could ever offer, in MB.
        largest_usable_mb: u64,
    },
}

/// Picks a node for a deploy asking for `request_mb`.
///
/// `affinity` is where this environment already runs, if it is a redeploy.
///
/// Note what is *not* consulted: how many environments a node hosts, how
/// many CPUs it has, or how recently it was chosen. Memory is what admission
/// rations, so memory is what placement ranks on; adding a second dimension
/// would need a policy for trading them off that nobody has asked for.
#[must_use]
pub fn place(nodes: &[NodeCapacity], request_mb: u64, affinity: Option<NodeId>) -> Placement {
    if nodes.is_empty() {
        return Placement::TooLarge {
            largest_usable_mb: 0,
        };
    }

    // Affinity first, and checked against the same `fits` every other node
    // is: a node that is draining, unreachable or genuinely full does not
    // get to keep a branch just because it had it. When it does fit,
    // staying put avoids a full image rebuild.
    if let Some(current) = affinity
        && let Some(node) = nodes.iter().find(|n| n.id == current)
        && node.fits(request_mb)
    {
        return Placement::Node(node.id);
    }

    // Most free memory, ties broken by id so the same fleet in the same
    // state always answers the same thing — a placement that flickered
    // between two equally free nodes would scatter a project's branches
    // across the fleet and lose every image cache doing it.
    let best = nodes
        .iter()
        .filter(|n| n.fits(request_mb))
        .max_by_key(|n| (n.free_mb(), std::cmp::Reverse(n.id)));

    if let Some(node) = best {
        return Placement::Node(node.id);
    }

    // Nothing fits. Distinguish "not right now" from "not ever", because
    // they are different answers to the operator: the first is a wait, the
    // second is a configuration error that waiting will never resolve.
    //
    // Measured against every node the fleet *has*, including draining and
    // unreachable ones — a drain ends and a partition heals, so neither
    // makes a request permanently impossible.
    let largest_usable_mb = nodes.iter().map(|n| n.usable_mb).max().unwrap_or(0);
    if nodes.iter().any(|n| n.could_ever_fit(request_mb)) {
        Placement::Queue
    } else {
        Placement::TooLarge { largest_usable_mb }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u64, usable_mb: u64, committed_mb: u64) -> NodeCapacity {
        NodeCapacity {
            id: NodeId(id),
            state: NodeState::Active,
            usable_mb,
            committed_mb,
            reachable: true,
        }
    }

    #[test]
    fn a_single_node_fleet_places_everything_on_it() {
        let nodes = [node(1, 4096, 0)];
        assert_eq!(place(&nodes, 512, None), Placement::Node(NodeId(1)));
    }

    #[test]
    fn the_emptiest_node_wins() {
        let nodes = [node(1, 4096, 3584), node(2, 4096, 512), node(3, 4096, 2048)];
        assert_eq!(place(&nodes, 512, None), Placement::Node(NodeId(2)));
    }

    /// Images are not distributed: moving a branch rebuilds it from
    /// scratch. Staying put is worth more than a marginally emptier node.
    #[test]
    fn a_redeploy_stays_where_its_image_cache_is() {
        let nodes = [node(1, 4096, 3072), node(2, 4096, 0)];
        assert_eq!(
            place(&nodes, 512, Some(NodeId(1))),
            Placement::Node(NodeId(1)),
            "node 2 is emptier, but node 1 already has the layers"
        );
    }

    /// Affinity is a preference, not a claim. A node that cannot take the
    /// deploy does not get to keep it.
    #[test]
    fn affinity_yields_to_a_node_that_is_actually_full() {
        let nodes = [node(1, 4096, 4000), node(2, 4096, 0)];
        assert_eq!(
            place(&nodes, 512, Some(NodeId(1))),
            Placement::Node(NodeId(2))
        );
    }

    #[test]
    fn affinity_yields_to_a_draining_node() {
        let mut draining = node(1, 4096, 0);
        draining.state = NodeState::Draining;
        let nodes = [draining, node(2, 4096, 2048)];
        assert_eq!(
            place(&nodes, 512, Some(NodeId(1))),
            Placement::Node(NodeId(2)),
            "draining exists precisely to move branches off"
        );
    }

    #[test]
    fn affinity_to_a_node_that_no_longer_exists_falls_back() {
        let nodes = [node(1, 4096, 0)];
        assert_eq!(
            place(&nodes, 512, Some(NodeId(99))),
            Placement::Node(NodeId(1))
        );
    }

    /// An unreachable node keeps its environments (nothing evicts them) but
    /// must never be handed a new one — the deploy would fail at the first
    /// Docker call.
    #[test]
    fn an_unreachable_node_is_never_chosen() {
        let mut down = node(1, 8192, 0);
        down.reachable = false;
        let nodes = [down, node(2, 2048, 0)];
        assert_eq!(place(&nodes, 512, None), Placement::Node(NodeId(2)));
        assert_eq!(
            place(&nodes, 512, Some(NodeId(1))),
            Placement::Node(NodeId(2)),
            "not even affinity may send a deploy at a node nobody can reach"
        );
    }

    /// "Not right now" and "not ever" are different answers, and conflating
    /// them either queues a deploy for eternity or refuses one that a
    /// finishing branch would have made room for.
    #[test]
    fn a_full_fleet_queues_but_a_small_fleet_refuses() {
        let full = [node(1, 4096, 4096), node(2, 4096, 4096)];
        assert_eq!(place(&full, 512, None), Placement::Queue);

        let small = [node(1, 256, 0), node(2, 512, 0)];
        assert_eq!(
            place(&small, 4096, None),
            Placement::TooLarge {
                largest_usable_mb: 512
            }
        );
    }

    /// A drain ends and a partition heals, so neither makes a request
    /// permanently impossible — those nodes still count towards "could this
    /// ever fit".
    #[test]
    fn a_drained_node_still_proves_the_request_is_possible() {
        let mut draining = node(1, 8192, 0);
        draining.state = NodeState::Draining;
        let nodes = [draining, node(2, 512, 0)];
        assert_eq!(place(&nodes, 4096, None), Placement::Queue);
    }

    /// The same fleet in the same state must answer the same thing every
    /// time: a placement that flickered between two equally free nodes
    /// would scatter one project's branches across the fleet and throw away
    /// every image cache doing it.
    #[test]
    fn ties_are_broken_deterministically_by_lowest_id() {
        let nodes = [node(3, 4096, 0), node(1, 4096, 0), node(2, 4096, 0)];
        for _ in 0..8 {
            assert_eq!(place(&nodes, 512, None), Placement::Node(NodeId(1)));
        }
    }

    /// Over-commitment is normal — scale-to-zero exists so a node hosts far
    /// more environments than could ever run at once — and means "nothing
    /// free", not a negative amount that would wrap.
    #[test]
    fn over_commitment_reads_as_empty_not_as_enormous() {
        let over = node(1, 1024, 4096);
        assert_eq!(over.free_mb(), 0);
        assert_eq!(place(&[over], 1, None), Placement::Queue);
    }

    /// A fleet with no nodes at all cannot happen — node 1 is seeded by the
    /// migration — but answering with a panic if it ever did would take the
    /// daemon down over an empty slice.
    #[test]
    fn an_empty_fleet_refuses_rather_than_panics() {
        assert_eq!(
            place(&[], 512, None),
            Placement::TooLarge {
                largest_usable_mb: 0
            }
        );
    }

    /// A deploy that asks for exactly what is left fits. Off-by-one here
    /// would refuse the last environment a node has room for.
    #[test]
    fn an_exact_fit_fits() {
        let nodes = [node(1, 1024, 512)];
        assert_eq!(place(&nodes, 512, None), Placement::Node(NodeId(1)));
        assert_eq!(place(&nodes, 513, None), Placement::Queue);
    }
}
