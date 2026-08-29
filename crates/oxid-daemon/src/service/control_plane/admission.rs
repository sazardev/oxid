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
use oxid_core::services::subdomain::subdomain_for;
use oxid_core::services::var_resolution::{VarSources, set_secret};
use oxid_core::{
    AuditEvent, AuditFilter, AuditStore, Branch, BranchName, BuildSpec, CommitRef, ContainerPort,
    ContainerSpec, ContainerStatus, Dependency, EnvVarScope, Environment, EnvironmentId,
    EnvironmentState, EnvironmentStore, GitPort, HostCapacity, LogStream, OciError, OffsetDateTime,
    PoolError, PoolKind, Project, ProjectId, ProjectStore, RepoUrl, RepositoryError, SecretStore,
    SecretValue, SelfWiringStatus, StateTransition, TraefikSpec, Ttl,
};

impl<G: GitPort, O: ContainerPort> ControlPlane<G, O> {
    pub(crate) async fn check_admission(&self, project: &Project) -> Result<Admission, CpError> {
        let Some(reserved_mb) = self.reserved_memory_mb else {
            return Ok(Admission::Fits);
        };
        let Some(request_mb) = project
            .config
            .build
            .memory_limit_mb
            .or(self.default_memory_limit_mb)
        else {
            return Ok(Admission::Fits);
        };

        let host = self.oci.host_capacity().await?;
        let total_mb = host.total_memory_bytes / 1_048_576;
        let usable_mb = total_mb.saturating_sub(reserved_mb);

        if request_mb > usable_mb {
            return Err(CpError::InsufficientCapacity(format!(
                "project `{}` requests {request_mb}MB but the host only has \
                 {usable_mb}MB usable ({total_mb}MB total minus {reserved_mb}MB reserved)",
                project.name
            )));
        }

        // Only `Running` environments hold memory.
        //
        // `Paused` used to be counted too, and had to be while suspension
        // meant `docker pause` — a frozen container keeps its whole resident
        // set. Suspension now stops the container, which frees all of it, so
        // counting paused environments reserved memory that nothing was
        // using. That deadlocks a busy node rather than merely wasting it:
        // once enough branches idle out, their phantom reservations fill the
        // budget, every new push queues behind memory no process holds, and
        // the queue can never drain because the environments blocking it are
        // asleep and will not wake on their own. Reproduced with 15 branches
        // on one host: 11 stopped containers reserved 1408MB while actually
        // consuming 0, and four deploys waited indefinitely.
        //
        // Over-committing against sleeping branches is the point of
        // scale-to-zero, not a hazard it introduces: a node hosts far more
        // environments than could ever run at once precisely because most
        // are asleep. `Building` is excluded because the deploy asking this
        // question has already persisted its own row in that state, and
        // deploys are serialized, so counting it would double-count the
        // request against itself.
        let committed_mb = self
            .store
            .committed_memory_mb(self.default_memory_limit_mb.unwrap_or(0))
            .await?;

        if committed_mb + request_mb > usable_mb {
            Ok(Admission::Queue)
        } else {
            Ok(Admission::Fits)
        }
    }
}
