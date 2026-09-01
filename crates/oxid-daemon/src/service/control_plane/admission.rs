//! Whether a deploy fits, and where it goes.
//!
//! One decision, taken once, in `deploy_at` and after the checkout — that
//! is the first point at which the *real* memory request is known, since a
//! branch's own `oxid.toml` may ask for far more or less than the project
//! was registered with. Deciding earlier weighs a number the deploy will
//! not use.
//!
//! The rules live in `oxid_core::services::placement`, which is pure. This
//! module's whole job is gathering the numbers it ranks on.

#![allow(
    unused_imports,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::pedantic,
    clippy::nursery,
    clippy::empty_line_after_doc_comments
)]
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::ControlPlane;
use super::error::CpError;
use super::helpers::{
    hash_token, image_name, lowest_free_index, resolved_container_name, sanitize_identifier,
    sanitize_label, state_err,
};
use super::types::{Admission, DeployOutcome, GcSummary, InfraStatus, NodeStats};
use crate::adapter::config;
use crate::adapter::postgres_pool::PostgresPool;
use crate::adapter::store::{ApiTokenSummary, SqliteStore};
use crate::request_context::current_request_id;
use oxid_core::services::gc::{self, GcAction};
use oxid_core::services::placement::{NodeCapacity, Placement, place};
use oxid_core::services::subdomain::subdomain_for;
use oxid_core::services::var_resolution::{VarSources, set_secret};
use oxid_core::{
    AuditEvent, AuditFilter, AuditStore, Branch, BranchName, BuildSpec, CommitRef, ContainerPort,
    ContainerSpec, ContainerStatus, Dependency, EnvVarScope, Environment, EnvironmentId,
    EnvironmentState, EnvironmentStore, GitPort, HostCapacity, LogStream, NodeId, NodeState,
    OciError, OffsetDateTime, PoolError, PoolKind, Project, ProjectId, ProjectStore, RepoUrl,
    RepositoryError, SecretStore, SecretValue, SelfWiringStatus, StateTransition, TraefikSpec, Ttl,
};

impl<G: GitPort, O: ContainerPort> ControlPlane<G, O> {
    /// How much memory this deploy is actually asking for, in MB.
    ///
    /// Zero means "admission is not rationing this deploy" — either the
    /// operator never enabled admission control (`OXID_RESERVED_MEMORY_MB`
    /// unset) or neither the branch nor the daemon named a limit. A request
    /// of zero fits everywhere, which is exactly the behaviour an install
    /// without admission control has always had, and it still lets
    /// placement spread branches across a fleet on free memory.
    fn request_mb(&self, project: &Project) -> u64 {
        if self.reserved_memory_mb.is_none() {
            return 0;
        }
        project
            .config
            .build
            .memory_limit_mb
            .or(self.default_memory_limit_mb)
            .unwrap_or(0)
    }

    /// What each node in the fleet can take right now.
    ///
    /// Capacity is read **live** from each node's Docker rather than from
    /// the `nodes` row the health probe fills in. The row is a cache with a
    /// probe interval behind it, and admission is the one caller that
    /// cannot tolerate a stale answer: a deploy admitted against a minute-old
    /// number is a deploy admitted against memory something else has since
    /// taken.
    ///
    /// **Every node is asked at once, and each is given a deadline.** Both
    /// halves are load-bearing, and neither was there first. Walking the
    /// fleet one node at a time with no deadline meant a partitioned node —
    /// which blackholes rather than refusing, so the connection waits out
    /// the kernel — stalled a deploy aimed at a *healthy* node for 121
    /// seconds, measured. Concurrency makes the whole fleet cost one round
    /// trip; the deadline makes a dead node cost five seconds instead of
    /// two minutes.
    ///
    /// A node that will not answer in time is marked unreachable and
    /// skipped, never failed on, and **nothing about it is written**: one
    /// node being down must not stop the fleet deploying, and a single
    /// timed-out query is not evidence a machine is gone. `state` belongs to
    /// the probe.
    async fn node_capacities(&self, exclude: Option<EnvironmentId>) -> Vec<NodeCapacity> {
        let fallback_mb = self.default_memory_limit_mb.unwrap_or(0);
        let probes = self.fleet.handles().into_iter().map(|handle| async move {
            let committed_mb = self
                .store
                .committed_memory_mb(fallback_mb, exclude, handle.node.id)
                .await
                .unwrap_or(0);

            let answered =
                tokio::time::timeout(self.status_deadline, handle.oci.host_capacity()).await;

            let (usable_mb, reachable) = match answered {
                Ok(Ok(host)) => {
                    let total_mb = host.total_memory_bytes / 1_048_576;
                    // Per-node reservation first, daemon-wide second: an
                    // operator who has said how much this particular machine
                    // owes its OS meant it, and the global figure is the
                    // fallback for every node they have not spoken about.
                    let reserved_mb = handle
                        .node
                        .reserved_memory_mb
                        .or(self.reserved_memory_mb)
                        .unwrap_or(0);
                    (total_mb.saturating_sub(reserved_mb), true)
                }
                Ok(Err(e)) => {
                    tracing::debug!(
                        node = %handle.node.name,
                        error = %e,
                        "node did not answer a capacity query; excluded from placement"
                    );
                    (0, false)
                }
                Err(_) => {
                    tracing::warn!(
                        node = %handle.node.name,
                        timeout_secs = self.status_deadline.as_secs(),
                        "node did not answer a capacity query in time; excluded from \
                         placement for this deploy only"
                    );
                    (0, false)
                }
            };

            NodeCapacity {
                id: handle.node.id,
                state: handle.node.state,
                usable_mb,
                committed_mb,
                reachable: reachable && handle.node.state != NodeState::Down,
            }
        });
        futures_util::future::join_all(probes).await
    }

    /// Picks a node for a deploy, or says why none will do.
    ///
    /// `affinity` is where the branch already runs, when this is a redeploy:
    /// images are not distributed, so a branch that moves rebuilds from
    /// scratch, and staying put is worth more than a marginally emptier
    /// node.
    ///
    /// # Errors
    /// [`CpError::InsufficientCapacity`] when no node in the fleet is large
    /// enough for the request at all — a state no amount of queueing
    /// resolves.
    pub(crate) async fn place_deploy(
        &self,
        project: &Project,
        exclude: Option<EnvironmentId>,
        affinity: Option<NodeId>,
    ) -> Result<Admission, CpError> {
        let request_mb = self.request_mb(project);
        let capacities = self.node_capacities(exclude).await;

        match place(&capacities, request_mb, affinity) {
            Placement::Node(id) => Ok(Admission::Fits(id)),
            Placement::Queue => Ok(Admission::Queue),
            Placement::TooLarge { largest_usable_mb } => {
                Err(CpError::InsufficientCapacity(format!(
                    "project `{}` requests {request_mb}MB but the largest node in \
                     the fleet only has {largest_usable_mb}MB usable",
                    project.name
                )))
            }
        }
    }
}
