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

        let mut committed_mb: u64 = 0;
        for state in [EnvironmentState::Running, EnvironmentState::Paused] {
            for env in self.store.list_by_state(state).await? {
                let Some(env_project) = ProjectStore::get(&self.store, env.project_id).await?
                else {
                    continue;
                };
                committed_mb += env_project
                    .config
                    .build
                    .memory_limit_mb
                    .or(self.default_memory_limit_mb)
                    .unwrap_or(0);
            }
        }

        if committed_mb + request_mb > usable_mb {
            Ok(Admission::Queue)
        } else {
            Ok(Admission::Fits)
        }
    }
}
