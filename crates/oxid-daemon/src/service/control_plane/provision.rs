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
    pub(crate) async fn run_and_activate(
        &self,
        project: &Project,
        branch: &BranchName,
        image: String,
        url: String,
        env: &mut Environment,
        previous: Option<&Environment>,
        operator: Option<&str>,
    ) -> Result<Vec<String>, CpError> {
        // Global -> Project -> Branch secrets plus orchestrator runtime
        // variables (SPEC.md §2.1/§4.4).
        let mut sources = VarSources::default();
        let secrets = SecretStore::secrets_for(&self.store, Some(project.id), Some(branch)).await?;
        for (name, scope, value) in secrets.iter() {
            set_secret(&mut sources, name, scope, value.as_str());
        }
        // Resource pooling (SPEC.md §3.1): one shared Postgres/Redis
        // instance instead of a container per branch. Each declared
        // dependency's connection string is injected as a Runtime
        // variable, same as the other orchestrator-owned values below —
        // Runtime always wins the inheritance precedence, so a project
        // can't accidentally shadow it with a same-named secret. What each
        // lease did ("created database X" / "reusing index 3") is collected
        // for the deploy report.
        let mut dependency_lines = Vec::with_capacity(project.config.dependencies.len());
        for dependency in &project.config.dependencies {
            let (url, line) = self
                .provision_dependency(project, branch, dependency)
                .await?;
            dependency_lines.push(line);
            set_secret(
                &mut sources,
                &dependency.inject_url_as,
                EnvVarScope::Runtime,
                url,
            );
        }

        set_secret(
            &mut sources,
            "OXID_BRANCH",
            EnvVarScope::Runtime,
            branch.to_string(),
        );
        set_secret(
            &mut sources,
            "OXID_ENV_URL",
            EnvVarScope::Runtime,
            url.clone(),
        );
        // The commit actually deployed. Apps routinely want it for a
        // `/version` endpoint, a Sentry release tag or a build banner, and
        // without it there was no way to tell from inside a container which
        // revision it was running — the branch name alone moves under you on
        // every push.
        set_secret(
            &mut sources,
            "OXID_COMMIT",
            EnvVarScope::Runtime,
            env.branch.commit_sha.clone(),
        );
        let env_vars = sources
            .resolve()
            .into_iter()
            .map(|(k, v)| (k, v.as_str().to_owned()))
            .collect::<BTreeMap<_, _>>();

        let name = resolved_container_name(project, env);
        // Defensive: remove any leftover container under this exact
        // (per-deployment-unique) name, in case a prior crashed attempt for
        // this same environment id left one behind. `previous`'s container
        // (if any) has a *different* name and is deliberately left running
        // — it keeps serving traffic until the cutover below.
        match self.oci_for(env.node_id)?.remove(&name).await {
            Ok(()) | Err(OciError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }

        let mut labels = BTreeMap::from([
            ("oxid.project".to_owned(), project.name.clone()),
            ("oxid.branch".to_owned(), branch.to_string()),
            ("oxid.url".to_owned(), url.clone()),
        ]);
        labels.extend(self.traefik_labels(
            &name,
            &url,
            project.config.port,
            &project.config.base_domain,
        ));
        let spec = ContainerSpec {
            name: name.clone(),
            image,
            env: env_vars,
            container_port: project.config.port,
            labels,
            network: self.docker_network.clone(),
            memory_limit_mb: project
                .config
                .build
                .memory_limit_mb
                .or(self.default_memory_limit_mb),
            cpu_limit_millicores: project
                .config
                .build
                .cpu_limit_millicores
                .or(self.default_cpu_limit_millicores),
        };
        env.host_port = match self.oci_for(env.node_id)?.run(&spec).await {
            Ok(port) => port,
            Err(e) => {
                let _ = self.oci_for(env.node_id)?.remove(&name).await;
                return Err(e.into());
            }
        };

        for command in &project.config.build.on_start {
            if let Err(e) = self.oci_for(env.node_id)?.exec(&name, command).await {
                let _ = self.oci_for(env.node_id)?.remove(&name).await;
                return Err(e.into());
            }
        }

        // In direct-publish mode, wait for the new container to actually
        // accept connections before cutting traffic over to it — `on_start`
        // succeeding only proves those specific commands ran, not that the
        // app itself is up and listening.
        // Probed at the node's own address, not at loopback. On a fleet
        // those differ, and the difference is the whole check: `node.address`
        // is supplied by an operator and verifiable nowhere else, so a
        // mistyped one has to fail here rather than report a green deploy on
        // a branch nothing can reach.
        if self.docker_network.is_none()
            && self.readiness_check
            && let Some(port) = env.host_port
            && !crate::service::proxy::wait_until_ready(
                &self.proxy_target(env, port)?,
                std::time::Duration::from_secs(20),
            )
            .await
        {
            let _ = self.oci_for(env.node_id)?.remove(&name).await;
            return Err(CpError::DeployNotReady(format!(
                "container `{name}` did not accept connections on port {port} within 20s"
            )));
        }

        // Cutover: repoint the branch's stable proxy at the new container
        // before touching the previous one — the actual zero-downtime
        // moment. Anything already connected to the old target keeps
        // talking to it; every new connection goes to the new one.
        if self.docker_network.is_none()
            && let Some(port) = env.host_port
        {
            let public_port = self
                .proxy
                .ensure(env.project_id, branch, previous.and_then(|p| p.public_port))
                .await?;
            env.public_port = Some(public_port);
            self.proxy
                .set_target(env.project_id, branch, self.proxy_target(env, port)?)
                .await;
        }

        // Only now remove the previous instance, if this was a redeploy —
        // it has been serving traffic this entire time, right up to the
        // cutover above.
        if let Some(prev) = previous {
            let prev_name = resolved_container_name(project, prev);
            // The previous instance may well be on a different node than
            // the new one — a redeploy is allowed to move a branch — so it
            // is torn down where it actually runs, not where its
            // replacement landed.
            let _ = self.oci_for(prev.node_id)?.remove(&prev_name).await;
        }

        // Which node ran it, in the audit trail — but only when that is
        // news. A single-node install's history stays byte-for-byte what it
        // was, and on a fleet the one question the trail could not answer
        // ("where did this actually go?") now has an answer attached to the
        // event that decided it.
        let detail = match self.node(env.node_id) {
            Ok(handle) if handle.node.id != oxid_core::NodeId::LOCAL => {
                format!("{name} on node `{}`", handle.node.name)
            }
            _ => name,
        };

        let now = OffsetDateTime::now_utc();
        env.transition(StateTransition::BuildSucceeded, now)
            .map_err(|e| state_err(&e))?;
        EnvironmentStore::update(&self.store, env).await?;
        self.store
            .record(
                &AuditEvent::with_operator(
                    u64::try_from(now.unix_timestamp()).unwrap_or_default(),
                    env.id,
                    StateTransition::BuildSucceeded,
                    Some(detail),
                    now,
                    operator.map(str::to_owned),
                )
                .with_request_id(current_request_id()),
            )
            .await?;
        Ok(dependency_lines)
    }

    /// Resolves the connection string a branch should inject for
    /// `dependency` (SPEC.md §3.1), leasing a resource on first deploy and
    /// reusing the same one on every redeploy of the same branch. Returns
    /// that URL plus a human-readable line describing what happened —
    /// "created" vs "reusing" and which concrete resource — for the
    /// deploy report.
    pub(crate) async fn provision_dependency(
        &self,
        project: &Project,
        branch: &BranchName,
        dependency: &Dependency,
    ) -> Result<(String, String), CpError> {
        let describe = |resource_name: &str, reused: bool| {
            let noun = match dependency.kind {
                PoolKind::Postgres => "database",
                PoolKind::Redis => "index",
            };
            let verb = if reused { "reusing" } else { "created" };
            format!(
                "{verb} {} {noun} `{resource_name}` (shared `{}`)",
                dependency.kind, dependency.shared_instance
            )
        };
        if let Some(existing) = self
            .store
            .find_resource_lease(
                project.id,
                branch,
                dependency.kind,
                &dependency.shared_instance,
            )
            .await?
        {
            return Ok((
                self.resource_url(dependency, &existing)?,
                describe(&existing, true),
            ));
        }

        // Held from "which slots are taken?" until the lease exists, for
        // the pools that hand out a slot rather than deriving a name.
        let mut slot_guard = None;
        let resource_name = match dependency.kind {
            PoolKind::Postgres => {
                let db_name = format!(
                    "db_{}_{}",
                    sanitize_identifier(&project.name),
                    sanitize_identifier(branch.as_str())
                );
                let admin_url = self.postgres_url.as_deref().ok_or_else(|| {
                    PoolError::NotConfigured(format!(
                        "project `{}` declares a `postgres` dependency but OXID_POSTGRES_URL \
                         is not configured on this daemon",
                        project.name
                    ))
                })?;
                PostgresPool.ensure_database(admin_url, &db_name).await?;
                db_name
            }
            PoolKind::Redis => {
                if self.redis_url.is_none() {
                    return Err(PoolError::NotConfigured(format!(
                        "project `{}` declares a `redis` dependency but OXID_REDIS_URL is not \
                         configured on this daemon",
                        project.name
                    ))
                    .into());
                }
                // Held across the read-then-claim below, and released only
                // once the lease exists: picking the lowest free slot is a
                // read followed by a write, and concurrent deploys would
                // otherwise interleave into the same slot.
                slot_guard = Some(
                    self.lifecycle_lock
                        .acquire(super::LockKey::ResourcePool(
                            PoolKind::Redis,
                            dependency.shared_instance.clone(),
                        ))
                        .await,
                );
                let used = self
                    .store
                    .used_resource_names(PoolKind::Redis, &dependency.shared_instance)
                    .await?
                    .into_iter()
                    .filter_map(|n| n.parse::<u32>().ok())
                    .collect::<std::collections::BTreeSet<_>>();
                let index = lowest_free_index(&used, self.redis_pool_size).ok_or_else(|| {
                    PoolError::Failure(format!(
                        "redis pool `{}` is exhausted (capacity {})",
                        dependency.shared_instance, self.redis_pool_size
                    ))
                })?;
                index.to_string()
            }
        };

        self.store
            .create_resource_lease(
                project.id,
                branch,
                dependency.kind,
                &dependency.shared_instance,
                &resource_name,
            )
            .await?;
        drop(slot_guard);
        Ok((
            self.resource_url(dependency, &resource_name)?,
            describe(&resource_name, false),
        ))
    }

    /// Builds the connection string injected into the container for an
    /// already-resolved `resource_name` (a Postgres database name, or a
    /// Redis index as a string).
    pub(crate) fn resource_url(
        &self,
        dependency: &Dependency,
        resource_name: &str,
    ) -> Result<String, CpError> {
        match dependency.kind {
            PoolKind::Postgres => {
                // Presence already checked in `provision_dependency`'s
                // create path; on the reuse path (existing lease found) we
                // still need it to rebuild the DSN.
                let admin_url = self.postgres_url.as_deref().ok_or_else(|| {
                    PoolError::NotConfigured(
                        "OXID_POSTGRES_URL is not configured on this daemon".to_owned(),
                    )
                })?;
                Ok(crate::adapter::postgres_pool::database_url(
                    admin_url,
                    resource_name,
                )?)
            }
            PoolKind::Redis => {
                let base = self.redis_url.as_deref().ok_or_else(|| {
                    PoolError::NotConfigured(
                        "OXID_REDIS_URL is not configured on this daemon".to_owned(),
                    )
                })?;
                Ok(format!("{}/{resource_name}", base.trim_end_matches('/')))
            }
        }
    }

    /// Releases every resource this branch leased (drops the Postgres
    /// database, frees the Redis index), called when its environment is
    /// destroyed — manually or by the GC sweep.
    pub(crate) async fn release_dependencies(
        &self,
        project_id: ProjectId,
        branch: &BranchName,
    ) -> Result<(), CpError> {
        for (kind, resource_name) in self.store.take_resource_leases(project_id, branch).await? {
            if kind == PoolKind::Postgres
                && let Some(admin_url) = self.postgres_url.as_deref()
            {
                PostgresPool
                    .drop_database(admin_url, &resource_name)
                    .await?;
            }
        }
        Ok(())
    }
}
