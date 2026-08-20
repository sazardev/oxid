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
    /// Returns [`CpError`] on storage failure.
    pub async fn create_operator_token(&self, name: &str) -> Result<(u64, String), CpError> {
        let mut raw = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut raw);
        let raw_token = hex::encode(raw);
        let id = self
            .store
            .create_api_token(name, &hash_token(&raw_token))
            .await?;
        Ok((id, raw_token))
    }

    /// Resolves a bearer token to its operator name, if it matches a live
    /// (non-revoked) named token.
    ///
    /// # Errors
    /// Returns [`CpError`] on storage failure.
    pub async fn find_operator_by_token(&self, raw_token: &str) -> Result<Option<String>, CpError> {
        Ok(self
            .store
            .find_operator_by_token_hash(&hash_token(raw_token))
            .await?)
    }

    /// Lists every named token (revoked ones included), newest first.
    ///
    /// # Errors
    /// Returns [`CpError`] on storage failure.
    pub async fn list_operator_tokens(&self) -> Result<Vec<ApiTokenSummary>, CpError> {
        Ok(self.store.list_api_tokens().await?)
    }

    /// Revokes a named token by id.
    ///
    /// # Errors
    /// [`CpError::NotFound`] if no token with that id exists.
    pub async fn revoke_operator_token(&self, id: u64) -> Result<(), CpError> {
        Ok(self.store.revoke_api_token(id).await?)
    }

    /// Re-encrypts every secret under `new_key` and swaps it in atomically
    /// (see [`SqliteStore::rotate_master_key`]) — no restart needed. The
    /// caller is still responsible for persisting `new_key` to
    /// `secret.key` (see `api.rs`'s `rotate_key` handler), since only it
    /// knows the data directory.
    ///
    /// # Errors
    /// Returns [`CpError`] on storage or crypto failure.
    pub async fn rotate_master_key(&self, new_key: [u8; 32]) -> Result<(), CpError> {
        self.store
            .rotate_master_key(crate::adapter::crypto::Cipher::from_key(new_key))
            .await
            .map_err(|e| CpError::Store(RepositoryError::Storage(e.to_string())))
    }

    /// Writes a consistent database snapshot to `dest` (see
    /// [`SqliteStore::backup_to`]). Backs `GET /api/v1/backup`.
    ///
    /// # Errors
    /// Returns [`CpError`] on storage failure.
    pub async fn backup_database(&self, dest: &std::path::Path) -> Result<(), CpError> {
        self.store
            .backup_to(dest)
            .await
            .map_err(|e| CpError::Store(RepositoryError::Storage(e.to_string())))
    }

    /// Returns the most recent audit events across every project, newest
    /// first — an operator-facing view of `AuditStore`, which until now was
    /// write-only (recorded on every deploy/pause/wake/destroy but never
    /// exposed over the API). `filter` narrows by project/branch/time
    /// range/transition kind and caps the page size — see [`AuditFilter`].
    ///
    /// # Errors
    /// Returns [`CpError`] on storage failure.
    pub async fn recent_audit_events(
        &self,
        filter: &AuditFilter,
    ) -> Result<Vec<AuditEvent>, CpError> {
        Ok(AuditStore::list_recent(&self.store, filter).await?)
    }

    /// Lists every deploy currently waiting for host capacity, oldest
    /// (highest-priority) first — see [`Self::deploy_or_queue`] and
    /// [`Self::retry_queued_deploys`].
    ///
    /// # Errors
    /// Returns [`CpError`] on persistence failures.
    pub async fn list_deploy_queue(
        &self,
    ) -> Result<Vec<crate::adapter::store::QueuedDeploy>, CpError> {
        Ok(self.store.list_deploy_queue().await?)
    }

    /// Aggregate counts + host capacity for the web dashboard's overview —
    /// one call instead of the client fetching every project's environments
    /// just to total them up.
    ///
    /// # Errors
    /// Returns [`CpError`] on storage or `docker info` failures.

    /// anything storage can fail with.
    pub async fn audit_events_for(
        &self,
        environment_id: EnvironmentId,
        filter: &AuditFilter,
    ) -> Result<Vec<AuditEvent>, CpError> {
        self.ensure_environment(environment_id).await?;
        Ok(AuditStore::list_by_environment(&self.store, environment_id, filter).await?)
    }

    /// Stores or replaces a secret at the given scope
    /// (`Global` when `project_id` is `None`).
    ///
    /// # Errors
    /// Returns [`CpError`] on persistence or encryption failures.
    pub async fn set_secret(
        &self,
        project_id: Option<ProjectId>,
        branch: Option<&BranchName>,
        name: &str,
        scope: EnvVarScope,
        value: &str,
    ) -> Result<(), CpError> {
        Ok(SecretStore::set_secret(
            &self.store,
            project_id,
            branch,
            name,
            scope,
            &SecretValue::new(value),
        )
        .await?)
    }

    /// Lists secret names and scopes for a context (values are never exposed).
    ///
    /// # Errors
    /// Returns [`CpError`] on persistence failures.
    pub async fn list_secrets(
        &self,
        project_id: Option<ProjectId>,
        branch: Option<&BranchName>,
    ) -> Result<Vec<(String, EnvVarScope)>, CpError> {
        Ok(SecretStore::list_secrets(&self.store, project_id, branch).await?)
    }

    /// Deletes a secret from a scope.
    ///
    /// # Errors
    /// Returns [`CpError::NotFound`] if the secret does not exist.
    pub async fn delete_secret(
        &self,
        project_id: Option<ProjectId>,
        branch: Option<&BranchName>,
        name: &str,
    ) -> Result<(), CpError> {
        Ok(SecretStore::delete_secret(&self.store, project_id, branch, name).await?)
    }
}
