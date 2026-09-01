//! `Node` entity — one machine in the fleet that can run environments.
//!
//! Oxid is one control plane over N Docker endpoints (see `MULTINODE.md`
//! §3). A node is therefore *addressing*, not an agent: an endpoint bollard
//! can connect to, plus the address the control plane's proxy should dial
//! for the ports that node publishes. Those two are deliberately separate —
//! a Docker API on `tcp://10.0.0.4:2376` and container traffic on a
//! different interface is an ordinary, correct configuration.
//!
//! Every install has at least node 1, `local`, seeded by migration `0020`.
//! That is what lets `Environment::node_id` be non-optional: there is never
//! an environment that lives nowhere.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::domain::error::invalid;
use crate::domain::ports::HostCapacity;

/// Stable identifier of a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub u64);

impl NodeId {
    /// The node every single-node install already is, seeded by migration
    /// `0020` and the value `Environment::new` starts from. Named rather
    /// than written as `NodeId(1)` at each site, because "1" appears in the
    /// migration, in the row mappers and in the fleet registry, and they
    /// have to agree.
    pub const LOCAL: Self = Self(1);
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Whether a node may receive new environments, and whether it is answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NodeState {
    /// Accepts placements and serves what it already runs.
    #[default]
    Active,
    /// Serves what it already runs, but takes nothing new. What
    /// `oxid node drain` sets before an operator moves branches off.
    Draining,
    /// The health probe could not reach it.
    ///
    /// This is a statement about the *node*, never about the environments on
    /// it: a network partition is indistinguishable from a dead machine, and
    /// evicting on one is how two copies of a branch end up fighting over a
    /// URL. Nothing in Oxid rewrites an environment row because its node
    /// went `down` (`MULTINODE.md` §8.3).
    Down,
}

impl NodeState {
    /// Whether a new environment may be placed here.
    #[must_use]
    pub const fn accepts_placements(self) -> bool {
        matches!(self, Self::Active)
    }
}

impl fmt::Display for NodeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Active => "active",
            Self::Draining => "draining",
            Self::Down => "down",
        })
    }
}

impl FromStr for NodeState {
    type Err = crate::domain::DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "draining" => Ok(Self::Draining),
            "down" => Ok(Self::Down),
            _ => invalid(format!(
                "unknown node state `{s}` — valid states are `active`, \
                 `draining` and `down`"
            )),
        }
    }
}

/// How to reach a node's Docker API.
///
/// `Local` is the socket this daemon already uses — the endpoint every
/// pre-multi-node install has, and the reason an upgrade needs no
/// configuration change at all.
/// Serialised as a **plain string**, never as a tagged enum.
///
/// The stored column is one `TEXT` field and the CLI, the dashboard and any
/// script read the same field, so `local` and `tcp://10.0.0.4:2376` have to
/// be the same *shape*. A derived `Serialize` gives the unit variant a bare
/// string and the tuple variant an object, so a remote node's endpoint
/// arrived as `{"remote":"tcp://…"}` — `oxid node ls` rendered it as `?`,
/// and every consumer needed two code paths for one column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeEndpoint {
    /// This daemon's own Docker socket, from the environment/defaults.
    Local,
    /// A remote Docker endpoint, e.g. `tcp://10.0.0.4:2376`.
    Remote(String),
}

impl Serialize for NodeEndpoint {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for NodeEndpoint {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from(String::deserialize(deserializer)?.as_str()))
    }
}

impl NodeEndpoint {
    /// Whether this endpoint carries the Docker API over a network, and so
    /// wants TLS material to be anything but root-on-that-box for anyone who
    /// can route to it (`MULTINODE.md` §8.1).
    #[must_use]
    pub const fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }

    /// The wire form stored in `nodes.endpoint`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Local => "local",
            Self::Remote(url) => url,
        }
    }
}

impl fmt::Display for NodeEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&str> for NodeEndpoint {
    fn from(s: &str) -> Self {
        if s == "local" {
            Self::Local
        } else {
            Self::Remote(s.to_owned())
        }
    }
}

/// Paths to the mTLS material for a remote Docker endpoint.
///
/// Paths, not bytes: the certificates are the operator's files on the
/// control plane's disk, and `service/backup.rs` snapshots the SQLite file
/// only — so a restore brings back the *rows*, and the operator is
/// responsible for the files they point at (`MULTINODE.md` §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeTls {
    /// CA that signed the node's daemon certificate.
    pub ca_path: String,
    /// Client certificate this control plane presents.
    pub cert_path: String,
    /// Private key for `cert_path`.
    pub key_path: String,
}

/// One machine in the fleet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// Unique identifier.
    pub id: NodeId,
    /// Operator-chosen name, unique across the fleet (`eu-1`, `local`).
    pub name: String,
    /// Where its Docker API lives.
    pub endpoint: NodeEndpoint,
    /// Host the control plane's proxy dials for ports this node publishes.
    /// `None` means "the same host this daemon runs on", which is exactly
    /// what `local` wants and what preserves today's `127.0.0.1` behaviour.
    pub address: Option<String>,
    /// mTLS material, required for a remote endpoint unless the operator
    /// opted out explicitly.
    pub tls: Option<NodeTls>,
    /// Whether it takes new placements, and whether it is answering.
    pub state: NodeState,
    /// Memory (MB) reserved for this node's OS and daemons, subtracted from
    /// what `docker info` reports before admission decides. `None` falls
    /// back to the daemon-wide `OXID_RESERVED_MEMORY_MB`.
    pub reserved_memory_mb: Option<u64>,
    /// What the last successful probe saw. Zeroed until one has run.
    pub capacity: HostCapacity,
    /// Unix seconds of the last successful probe, `None` if never.
    pub last_seen_at: Option<i64>,
}

impl Node {
    /// Validates and constructs a node.
    ///
    /// # Errors
    /// Returns [`DomainError::Invalid`](crate::domain::DomainError) for an
    /// empty name, or for a remote endpoint with an empty URL.
    pub fn new(
        id: NodeId,
        name: impl Into<String>,
        endpoint: NodeEndpoint,
    ) -> Result<Self, crate::domain::DomainError> {
        let name = name.into();
        if name.trim().is_empty() {
            return invalid("node name cannot be empty");
        }
        if let NodeEndpoint::Remote(url) = &endpoint
            && url.trim().is_empty()
        {
            return invalid("node endpoint cannot be empty");
        }
        Ok(Self {
            id,
            name,
            endpoint,
            address: None,
            tls: None,
            state: NodeState::Active,
            reserved_memory_mb: None,
            capacity: HostCapacity::default(),
            last_seen_at: None,
        })
    }

    /// The host the proxy should dial for a port published on this node.
    ///
    /// Falls back to loopback, which is both correct for `local` and the
    /// literal behaviour `service/proxy.rs` had before nodes existed.
    #[must_use]
    pub fn proxy_host(&self) -> &str {
        self.address.as_deref().unwrap_or("127.0.0.1")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_state_round_trips_through_its_wire_form() {
        for state in [NodeState::Active, NodeState::Draining, NodeState::Down] {
            assert_eq!(state.to_string().parse::<NodeState>().unwrap(), state);
        }
    }

    #[test]
    fn only_active_nodes_take_new_environments() {
        assert!(NodeState::Active.accepts_placements());
        assert!(!NodeState::Draining.accepts_placements());
        assert!(!NodeState::Down.accepts_placements());
    }

    /// One column, one shape. A tagged enum here gave `local` a bare string
    /// and a remote endpoint an object, which every reader had to special-case
    /// — and `oxid node ls` did not, so it printed `?`.
    #[test]
    fn an_endpoint_is_a_plain_string_on_the_wire_whichever_variant_it_is() {
        assert_eq!(
            serde_json::to_string(&NodeEndpoint::Local).unwrap(),
            "\"local\""
        );
        assert_eq!(
            serde_json::to_string(&NodeEndpoint::from("tcp://10.0.0.4:2376")).unwrap(),
            "\"tcp://10.0.0.4:2376\""
        );
        for endpoint in [NodeEndpoint::Local, NodeEndpoint::from("tcp://a:2376")] {
            let json = serde_json::to_string(&endpoint).unwrap();
            assert_eq!(
                serde_json::from_str::<NodeEndpoint>(&json).unwrap(),
                endpoint
            );
        }
    }

    #[test]
    fn local_is_the_endpoint_an_existing_install_already_has() {
        assert_eq!(NodeEndpoint::from("local"), NodeEndpoint::Local);
        assert!(!NodeEndpoint::Local.is_remote());
        assert_eq!(
            NodeEndpoint::from("tcp://10.0.0.4:2376"),
            NodeEndpoint::Remote("tcp://10.0.0.4:2376".to_owned())
        );
    }

    /// A node with no address must dial exactly what the proxy dialled
    /// before nodes existed, or upgrading moves traffic.
    #[test]
    fn an_addressless_node_is_loopback() {
        let node = Node::new(NodeId::LOCAL, "local", NodeEndpoint::Local).unwrap();
        assert_eq!(node.proxy_host(), "127.0.0.1");
    }

    #[test]
    fn a_named_address_wins() {
        let mut node = Node::new(NodeId(2), "eu-1", NodeEndpoint::from("tcp://a:2376")).unwrap();
        node.address = Some("10.0.0.4".to_owned());
        assert_eq!(node.proxy_host(), "10.0.0.4");
    }

    #[test]
    fn empty_names_and_endpoints_are_rejected() {
        assert!(Node::new(NodeId(2), "  ", NodeEndpoint::Local).is_err());
        assert!(Node::new(NodeId(2), "eu-1", NodeEndpoint::Remote(String::new())).is_err());
    }
}
