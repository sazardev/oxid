//! Registering, probing and retiring the machines environments run on.
//!
//! The control plane owns the fleet; a node owns nothing but its containers.
//! Everything here is therefore bookkeeping plus one Docker call: read the
//! `nodes` table, hold a client per row, and keep the row's idea of capacity
//! roughly current.
//!
//! One rule runs through all of it and is easy to undo by accident: **none of
//! these operations ever touches an environment row.** Marking a node `down`,
//! draining it, even removing it, says nothing about the environments on it.
//! A network partition is indistinguishable from a dead machine from here,
//! and a control plane that evicted on one would, when the partition healed,
//! have two live copies of every branch fighting over one URL.

#![allow(clippy::pedantic, clippy::nursery)]

use std::sync::Arc;

use super::ControlPlane;
use super::error::CpError;
use oxid_core::{
    ContainerPort, EnvironmentStore, GitPort, HostCapacity, Node, NodeEndpoint, NodeId, NodeState,
    NodeTls, OciError,
};

/// Builds a Docker client for a node.
///
/// A closure rather than a method on `ControlPlane`, because `ControlPlane`
/// is generic over `ContainerPort` and cannot know how to *construct* one —
/// that is `adapter::oci`'s knowledge, and moving it in here would drag the
/// Docker adapter into the application layer. `main.rs` supplies
/// `DockerClient::connect_to`; the test suite supplies whatever it needs.
pub type NodeConnector<O> = Arc<dyn Fn(&Node) -> Result<O, OciError> + Send + Sync>;

/// A node plus what the control plane knows about it that the row does not.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NodeView {
    /// The stored row.
    #[serde(flatten)]
    pub node: Node,
    /// Whether this daemon currently holds a client for it. `false` on a
    /// node whose endpoint would not connect at startup — distinct from
    /// `state: down`, which means a probe reached it and failed.
    pub connected: bool,
    /// Environments on it that are not `destroyed`.
    pub environments_live: u64,
    /// Environments on it in any state, including destroyed ones. What
    /// stands between a node and removal, since `audit_events` cascades
    /// from `environments`.
    pub environments_total: u64,
}

impl<G: GitPort, O: ContainerPort> ControlPlane<G, O> {
    /// Rebuilds the fleet from the `nodes` table.
    ///
    /// Called once at startup, before reconciliation: an environment whose
    /// node has no client is left strictly alone, so connecting first is
    /// what makes the difference between reconciling a fleet and reporting
    /// every remote environment as unreachable.
    ///
    /// A node that will not connect is logged and skipped, never fatal.
    /// One misconfigured endpoint must not stop the daemon starting — the
    /// environments on every other node are still serving traffic and still
    /// need a control plane.
    ///
    /// # Errors
    /// Returns [`CpError`] only if the node table itself cannot be read.
    pub async fn reload_fleet(&self) -> Result<(), CpError> {
        let Some(connect) = self.node_connector.as_ref() else {
            return Ok(());
        };
        for node in self.store.list_nodes().await? {
            // Node 1 is already registered with the client this daemon was
            // built with, honouring `OXID_CONTAINER_HOST` and every other
            // local setting. Reconnecting it from a row that only ever says
            // `local` would throw that away.
            if node.id == NodeId::LOCAL {
                self.fleet.refresh(node);
                continue;
            }
            match connect(&node) {
                Ok(oci) => {
                    tracing::info!(node = %node.name, endpoint = %node.endpoint, "node connected");
                    self.fleet.register(node, Arc::new(oci));
                }
                Err(e) => tracing::warn!(
                    node = %node.name,
                    endpoint = %node.endpoint,
                    error = %e,
                    "could not connect to a registered node; its environments are \
                     left exactly as they are"
                ),
            }
        }
        Ok(())
    }

    /// Every node, with what the control plane knows about it.
    ///
    /// # Errors
    /// Returns [`CpError`] on a store failure.
    pub async fn list_nodes(&self) -> Result<Vec<NodeView>, CpError> {
        let mut out = Vec::new();
        for node in self.store.list_nodes().await? {
            let (environments_live, environments_total) =
                self.store.environment_count_on(node.id).await?;
            out.push(NodeView {
                connected: self.fleet.get(node.id).is_some(),
                node,
                environments_live,
                environments_total,
            });
        }
        Ok(out)
    }

    /// Registers a node, or corrects one already registered under the same
    /// name.
    ///
    /// The connection is made *here*, synchronously, and a failure is
    /// returned rather than logged. Registration is the one moment an
    /// operator is watching, and a node that silently registers and only
    /// fails at the first deploy hands them the error hours later, attached
    /// to somebody else's push.
    ///
    /// Probing immediately, for the same reason: `oxid node ls` showing a
    /// memory figure means the endpoint answered.
    ///
    /// # Errors
    /// [`CpError::Validation`] for an unusable name or endpoint,
    /// [`CpError::Oci`] if the endpoint will not connect.
    pub async fn add_node(
        &self,
        name: &str,
        endpoint: &str,
        address: Option<String>,
        tls: Option<NodeTls>,
        reserved_memory_mb: Option<u64>,
    ) -> Result<NodeView, CpError> {
        let Some(connect) = self.node_connector.as_ref() else {
            return Err(CpError::Validation(
                "this daemon was built without a node connector".to_owned(),
            ));
        };

        let mut node = Node::new(NodeId(0), name, NodeEndpoint::from(endpoint))?;
        node.address = address;
        node.tls = tls;
        node.reserved_memory_mb = reserved_memory_mb;

        // Connect before writing the row. A node in the table that nothing
        // can reach is a node placement has to keep skipping and an operator
        // has to keep explaining; failing here leaves the fleet exactly as
        // it was.
        //
        // Reported as a validation failure (422), not as a Docker failure
        // (500), and the difference is not cosmetic: everything that can go
        // wrong here — a wrong address, a missing certificate, no TLS
        // material at all — is the operator's input, fixable by them. A 500
        // tells them the daemon broke and there is nothing to do but wait.
        // Unwrapped rather than `e.to_string()`: every message on this path
        // is Oxid's own, and the `docker failure:` prefix `OciError`'s
        // Display adds made them read as though bollard had said them —
        // sending an operator to search Docker's issue tracker for a
        // sentence this repository wrote.
        let invalid = |e: OciError| {
            CpError::Validation(match e {
                OciError::Failure(message) | OciError::NotFound(message) => message,
            })
        };
        let oci = connect(&node).map_err(invalid)?;
        let capacity = oci.host_capacity().await.map_err(invalid)?;

        let id = self.store.upsert_node(&node).await?;
        node.id = id;
        node.capacity = capacity;
        self.store
            .record_node_health(id, capacity, NodeState::Active)
            .await?;
        let node = self
            .store
            .get_node(id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("node `{id}`")))?;
        self.fleet.register(node.clone(), Arc::new(oci));

        tracing::info!(
            node = %node.name,
            endpoint = %node.endpoint,
            memory_mb = capacity.total_memory_bytes / 1_048_576,
            cpus = capacity.cpu_count,
            "node registered"
        );
        let (environments_live, environments_total) = self.store.environment_count_on(id).await?;
        Ok(NodeView {
            node,
            connected: true,
            environments_live,
            environments_total,
        })
    }

    /// Sets whether a node takes new placements.
    ///
    /// `Down` is refused: it is the health probe's word, not an operator's,
    /// and letting it be set by hand would have it overwritten by the next
    /// tick anyway. An operator who wants a node to stop receiving work
    /// wants `Draining`, which the probe never overrides.
    ///
    /// # Errors
    /// [`CpError::NotFound`] for an unknown node, [`CpError::Validation`]
    /// for `Down`.
    pub async fn set_node_state(&self, id: NodeId, state: NodeState) -> Result<NodeView, CpError> {
        if state == NodeState::Down {
            return Err(CpError::Validation(
                "`down` is what a failed health probe records, not something to \
                 set by hand — drain the node instead"
                    .to_owned(),
            ));
        }
        self.store.set_node_state(id, state).await?;
        let node = self
            .store
            .get_node(id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("node `{id}`")))?;
        self.fleet.refresh(node.clone());
        tracing::info!(node = %node.name, %state, "node state changed");
        let (environments_live, environments_total) = self.store.environment_count_on(id).await?;
        Ok(NodeView {
            node,
            connected: self.fleet.get(id).is_some(),
            environments_live,
            environments_total,
        })
    }

    /// Moves every live environment off a node.
    ///
    /// The move *is* a redeploy: build the branch on the new node, wait for
    /// it to accept connections, cut the proxy over, and only then remove
    /// the old container — the zero-downtime path that already exists, used
    /// for the one thing it was not yet used for. There is no container
    /// migration and there will not be; a checkpoint/restore of a running
    /// process is a different product.
    ///
    /// Two details are load-bearing:
    ///
    /// * **The node is set `draining` first, and by the caller.** Placement
    ///   refuses a draining node even to a branch that is already on it, so
    ///   that single fact is what makes a redeploy *leave* rather than
    ///   rebuild in place. Evacuating an `active` node would move nothing
    ///   and rebuild everything.
    /// * **Each branch is redeployed at the commit it is running**, not at
    ///   its current head. Draining a node is an infrastructure operation,
    ///   and quietly shipping whatever someone pushed since would turn it
    ///   into a deploy nobody asked for.
    ///
    /// Returns one entry per environment it tried to move, so a partial
    /// evacuation reports exactly which branches are still there rather
    /// than failing as a whole. A node with a branch that will not build is
    /// a node that stays partly full, and that is the honest outcome.
    ///
    /// # Errors
    /// Returns [`CpError`] only if the environment list cannot be read.
    pub async fn evacuate_node(
        &self,
        id: NodeId,
        operator: Option<String>,
    ) -> Result<Vec<(oxid_core::EnvironmentId, Option<String>)>, CpError> {
        use oxid_core::EnvironmentState;

        let mut moved = Vec::new();
        for env in self.store.list_all_environments().await? {
            if env.node_id != id
                || matches!(
                    env.state,
                    EnvironmentState::Destroyed | EnvironmentState::BuildFailed
                )
            {
                continue;
            }
            let sha = env.branch.commit_sha.clone();
            let outcome = self
                .deploy_at(
                    env.project_id,
                    env.branch.name.clone(),
                    Some(sha),
                    operator.clone(),
                    crate::service::control_plane::types::AdmissionMode::Enqueue,
                )
                .await;
            match outcome {
                Ok(_) => {
                    // Looked up by *branch*, not by the id we started from:
                    // a redeploy creates a new environment row and destroys
                    // the old one, so re-reading the original id reports the
                    // node the branch has just left. Getting this wrong made
                    // every successful evacuation report that nothing had
                    // moved.
                    //
                    // Re-read at all, rather than trusting the outcome,
                    // because a deploy that queued instead of running has
                    // not moved anything.
                    let landed = self
                        .find_environment_by_branch(env.project_id, &env.branch.name)
                        .await
                        .ok()
                        .flatten()
                        .map(|e| e.node_id);
                    if landed != Some(id) {
                        tracing::info!(
                            environment_id = %env.id,
                            branch = %env.branch.name,
                            "branch moved off the draining node"
                        );
                        moved.push((env.id, None));
                    } else {
                        moved.push((
                            env.id,
                            Some("redeployed, but landed on the same node".to_owned()),
                        ));
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        environment_id = %env.id,
                        branch = %env.branch.name,
                        error = %e,
                        "could not move a branch off the draining node; it stays where it is"
                    );
                    moved.push((env.id, Some(e.to_string())));
                }
            }
        }
        Ok(moved)
    }

    /// Retires a node.
    ///
    /// The store refuses while anything still references it — see
    /// [`crate::adapter::store::SqliteStore::delete_node`], which explains
    /// why destroyed environments count too.
    ///
    /// # Errors
    /// [`CpError::Store`] with a conflict while the node still holds
    /// environments or their history.
    pub async fn remove_node(&self, id: NodeId) -> Result<(), CpError> {
        self.store.delete_node(id).await?;
        self.fleet.deregister(id);
        tracing::info!(node = %id, "node removed");
        Ok(())
    }

    /// Asks every node whether it is alive and records what it says.
    ///
    /// Run on the scheduler tick. Three things it deliberately does not do:
    ///
    /// * **It never writes an environment row.** See the module note.
    /// * **It never overrides `draining`.** A drain is an operator's
    ///   intent, and a successful probe is not a reason to start sending
    ///   deploys back at a machine somebody is emptying.
    /// * **It never reconnects a node it has a working client for.**
    ///   Rebuilding a client to record a number would drop whatever that
    ///   client was in the middle of.
    ///
    /// # Errors
    /// Returns [`CpError`] only if the node table cannot be read.
    pub async fn probe_nodes(&self) -> Result<(), CpError> {
        // Asked all at once, each with a deadline, and only then acted on.
        //
        // Sequential and unbounded, this held the *whole scheduler tick*
        // hostage: the GC sweep, the deploy-queue drain and the forge
        // notifications all run behind it, and a partitioned node —
        // blackholing rather than refusing — took 126 seconds to be noticed.
        // The fleet now costs one round trip, and a dead node costs the
        // deadline rather than the kernel's patience.
        let nodes = self.store.list_nodes().await?;
        let answers = futures_util::future::join_all(nodes.iter().map(|node| async move {
            let handle = self.fleet.get(node.id);
            match &handle {
                Some(handle) => (
                    tokio::time::timeout(self.status_deadline, handle.oci.host_capacity())
                        .await
                        .map_err(|_| {
                            OciError::Failure(format!(
                                "node did not answer within {}s",
                                self.status_deadline.as_secs()
                            ))
                        })
                        .and_then(|inner| inner),
                    false,
                ),
                // Registered in the table but not in this process — a daemon
                // started while it was unreachable. The reconnect is left to
                // the sequential pass below, since it mutates the registry.
                None => (Err(OciError::NotFound("no client".to_owned())), true),
            }
        }))
        .await;

        for (node, (answer, needs_client)) in nodes.into_iter().zip(answers) {
            self.apply_probe(node, answer, needs_client).await;
        }
        Ok(())
    }

    /// Records one node's probe result. Split out so the network half can
    /// run for the whole fleet at once while the bookkeeping stays ordered.
    async fn apply_probe(
        &self,
        node: Node,
        answer: Result<HostCapacity, OciError>,
        needs_client: bool,
    ) {
        let answer = if needs_client {
            // Retrying the connection here is what lets a node registered
            // while this process could not reach it rejoin without a
            // restart.
            let Some(connect) = self.node_connector.as_ref() else {
                return;
            };
            match connect(&node) {
                Ok(oci) => {
                    tracing::info!(node = %node.name, "node reconnected");
                    self.fleet.register(node.clone(), Arc::new(oci));
                    let handle = self.fleet.get(node.id).expect("just registered");
                    tokio::time::timeout(self.status_deadline, handle.oci.host_capacity())
                        .await
                        .unwrap_or_else(|_| {
                            Err(OciError::Failure("node did not answer in time".to_owned()))
                        })
                }
                Err(e) => Err(e),
            }
        } else {
            answer
        };

        match answer {
            Ok(capacity) => {
                // A drain survives a successful probe. Anything else
                // becomes active, which is how a node that was `down`
                // comes back on its own.
                let state = if node.state == NodeState::Draining {
                    NodeState::Draining
                } else {
                    NodeState::Active
                };
                if node.state == NodeState::Down {
                    tracing::info!(node = %node.name, "node is answering again");
                }
                if let Err(e) = self
                    .store
                    .record_node_health(node.id, capacity, state)
                    .await
                {
                    tracing::warn!(node = %node.name, error = %e, "could not record node health");
                    return;
                }
                let mut refreshed = node;
                refreshed.capacity = capacity;
                refreshed.state = state;
                refreshed.last_seen_at =
                    Some(oxid_core::OffsetDateTime::now_utc().unix_timestamp());
                self.fleet.refresh(refreshed);
            }
            Err(e) => self.mark_node_down(&node, &e.to_string()).await,
        }
    }

    /// Records a node as unreachable — and nothing else. Idempotent and
    /// quiet on repeat, so a node that is down for a week does not produce
    /// a warning every tick.
    async fn mark_node_down(&self, node: &Node, error: &str) {
        if node.state != NodeState::Down {
            tracing::warn!(
                node = %node.name,
                endpoint = %node.endpoint,
                %error,
                "node stopped answering; its environments are left exactly as they are"
            );
        }
        if let Err(e) = self.store.set_node_state(node.id, NodeState::Down).await {
            tracing::warn!(node = %node.name, error = %e, "could not record node state");
            return;
        }
        let mut down = node.clone();
        down.state = NodeState::Down;
        self.fleet.refresh(down);
    }
}
