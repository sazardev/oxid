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
    pub async fn sweep(&self, now: OffsetDateTime) -> Result<GcSummary, CpError> {
        let mut summary = GcSummary::default();
        if self.docker_network.is_none() {
            return Ok(summary);
        }
        let mut projects: std::collections::HashMap<ProjectId, Project> =
            std::collections::HashMap::new();

        for env in self.store.list_all_environments().await? {
            let project = match projects.get(&env.project_id) {
                Some(project) => project.clone(),
                None => match ProjectStore::get(&self.store, env.project_id).await? {
                    Some(project) => {
                        projects.insert(env.project_id, project.clone());
                        project
                    }
                    // Orphaned environment (project deleted underneath it); skip.
                    None => continue,
                },
            };

            let action = gc::evaluate(&env, &project, now);
            if action == GcAction::Keep {
                continue;
            }

            match self.apply_gc_action(env.id, &project, action, now).await {
                Ok(()) => match action {
                    GcAction::Pause => summary.paused += 1,
                    GcAction::Hibernate => summary.hibernated += 1,
                    GcAction::Destroy => summary.destroyed += 1,
                    GcAction::Keep => unreachable!("Keep is filtered out above"),
                },
                Err(err) => summary.errors.push((env.id, err.to_string())),
            }
        }

        Ok(summary)
    }

    /// Reconciles the database's belief about each live environment
    /// against Docker's actual state — meant to be called once at daemon
    /// startup, since the daemon can be down for a while (crash, restart,
    /// a host reboot) during which reality drifts from whatever was last
    /// recorded:
    /// - the container is missing entirely (removed while the daemon was
    ///   down) → the environment is marked `Destroyed`, since there's
    ///   nothing left to recover.
    /// - the database says `Paused` but the container is actually
    ///   `Running` → a paused container doesn't survive a host reboot as
    ///   paused (the cgroup freezer state doesn't persist, so
    ///   `unless-stopped` brings it back fully running); it's re-paused
    ///   to honor the original intent — don't run what wasn't supposed to
    ///   be running.
    /// - the database says `Running` but the container is `Stopped` → try
    ///   to start it back up (the restart policy should normally have
    ///   already done this once Docker itself came back, but this covers
    ///   the case where it hasn't caught up yet, or gave up); if that
    ///   fails too, mark it `Destroyed` rather than leaving a permanently
    ///   wrong "Running" row behind.
    ///
    /// A failure reconciling one environment doesn't abort the pass —
    /// errors are collected and returned, matching [`Self::sweep`]'s
    /// "one bad apple doesn't block the rest" behavior.
    ///
    /// # Errors
    /// Returns [`CpError`] only if listing environments/projects fails;
    /// per-environment reconciliation failures are returned in the `Vec`.
    pub async fn reconcile_startup_state(&self) -> Result<Vec<(EnvironmentId, String)>, CpError> {
        let mut errors = Vec::new();
        let mut projects: std::collections::HashMap<ProjectId, Project> =
            std::collections::HashMap::new();

        for mut env in self.store.list_all_environments().await? {
            // A `Building` row cannot still be building: nothing survived
            // this process's restart, so the deploy that wrote it is gone.
            // Left alone it stayed `Building` forever — and admission
            // counts `building` as memory this host has promised, so every
            // daemon killed mid-deploy leaked a reservation nothing was
            // using, until enough of them accumulated to refuse deploys the
            // node had room for. Exactly the failure `Paused` used to
            // cause, in a state nobody thought to sweep.
            //
            // Recorded as failed rather than deleted: someone pushed, and
            // the honest answer is that their deploy was interrupted, not
            // that it never happened.
            if env.state == EnvironmentState::Building {
                let now = OffsetDateTime::now_utc();
                if env.transition(StateTransition::BuildFailed, now).is_ok() {
                    match EnvironmentStore::update(&self.store, &env).await {
                        Ok(()) => tracing::warn!(
                            environment_id = %env.id,
                            branch = %env.branch.name,
                            "environment was still `building` at startup;                              its deploy was interrupted by a restart"
                        ),
                        Err(e) => errors.push((env.id, e.to_string())),
                    }
                }
                continue;
            }
            // Nothing to reconcile for a row that never had a healthy
            // container: `Destroyed` is gone, and `BuildFailed` is a record
            // of a deploy that never came up.
            if matches!(
                env.state,
                EnvironmentState::Destroyed | EnvironmentState::BuildFailed
            ) {
                continue;
            }
            let project = match projects.get(&env.project_id) {
                Some(project) => project.clone(),
                None => match ProjectStore::get(&self.store, env.project_id).await? {
                    Some(project) => {
                        projects.insert(env.project_id, project.clone());
                        project
                    }
                    None => continue,
                },
            };
            let name = resolved_container_name(&project, &env);
            // Resolved once, before anything is decided, and reported
            // rather than propagated: one node this daemon holds no client
            // for must not stop the other nodes' environments being
            // reconciled, and it must certainly not have its own rewritten.
            //
            // This is the invariant that makes the whole reconciler safe on
            // a fleet, and it has a twin one layer down: `container_status`
            // answers `Missing` only on a real 404, mapping a connection
            // failure to `OciError::Failure` instead. Both arms of the pair
            // are needed — the `Missing` branch below marks an environment
            // `Destroyed`, so a node that is merely unreachable answering
            // "no such container" would delete every record of everything
            // running on it, in the exact moment a partition is least
            // distinguishable from a dead machine.
            let handle = match self.node(env.node_id) {
                Ok(handle) => handle,
                Err(e) => {
                    errors.push((env.id, e.to_string()));
                    continue;
                }
            };
            let oci = &handle.oci;
            let status = match oci.container_status(&name).await {
                Ok(status) => status,
                Err(e) => {
                    errors.push((env.id, e.to_string()));
                    continue;
                }
            };

            // Same opportunistic backfill as `wake_env`: an environment
            // deployed before dynamic port assignment existed never got its
            // `host_port` recorded, and nothing else revisits it — a daemon
            // restart is a free chance to fix that without waiting for a
            // redeploy.
            if env.host_port.is_none()
                && self.docker_network.is_none()
                && status != ContainerStatus::Missing
                && let Ok(Some(port)) = oci.published_port(&name, project.config.port).await
            {
                env.host_port = Some(port);
                let _ = EnvironmentStore::update(&self.store, &env).await;
            }

            // The proxy registry (see `service/proxy.rs`) lives entirely in
            // memory, so a daemon restart loses every branch's stable
            // public address unless it's rebuilt here — reusing the
            // persisted `public_port` so it comes back on the exact same
            // port whenever possible instead of quietly reassigning a new
            // one out from under anyone who bookmarked it.
            if self.docker_network.is_none()
                && matches!(
                    env.state,
                    EnvironmentState::Running | EnvironmentState::Paused
                )
                && let Ok(public_port) = self
                    .proxy
                    .ensure(env.project_id, &env.branch.name, env.public_port)
                    .await
            {
                if env.public_port != Some(public_port) {
                    env.public_port = Some(public_port);
                    let _ = EnvironmentStore::update(&self.store, &env).await;
                }
                if env.state == EnvironmentState::Running
                    && let Some(port) = env.host_port
                {
                    self.proxy
                        .set_target(
                            env.project_id,
                            &env.branch.name,
                            crate::service::proxy::Target {
                                host: handle.proxy_host().to_owned(),
                                port,
                            },
                        )
                        .await;
                }
            }

            let outcome = match (env.state, status) {
                (
                    EnvironmentState::Running | EnvironmentState::Paused,
                    ContainerStatus::Missing,
                ) => self.mark_destroyed(&mut env).await,
                // `stop`, matching how suspension is applied everywhere
                // else: a `pause`d container is invisible to Traefik's
                // router table, so re-suspending a rebooted environment
                // that way would silently make it unreachable again.
                (EnvironmentState::Paused, ContainerStatus::Running) => {
                    oci.stop(&name).await.map_err(CpError::from)
                }
                (EnvironmentState::Running, ContainerStatus::Stopped) => {
                    match oci.start(&name).await {
                        Ok(()) => Ok(()),
                        Err(_) => self.mark_destroyed(&mut env).await,
                    }
                }
                // Already consistent, or a benign drift not worth
                // correcting (e.g. `Hibernating` found `Running` because
                // someone manually `docker start`ed it).
                _ => Ok(()),
            };
            if let Err(e) = outcome {
                errors.push((env.id, e.to_string()));
            }
        }
        Ok(errors)
    }

    /// Transitions `env` to `Destroyed` and persists it — the reconciler's
    /// fallback when a container can't be recovered.
    pub(crate) async fn mark_destroyed(&self, env: &mut Environment) -> Result<(), CpError> {
        let now = OffsetDateTime::now_utc();
        if env.transition(StateTransition::Destroy, now).is_ok() {
            EnvironmentStore::update(&self.store, env).await?;
        }
        Ok(())
    }

    pub(crate) async fn apply_gc_action(
        &self,
        env_id: EnvironmentId,
        project: &Project,
        action: GcAction,
        now: OffsetDateTime,
    ) -> Result<(), CpError> {
        // Re-fetch under the lock rather than trusting the snapshot `sweep`
        // read at the top of its loop: without this, a concurrent manual
        // pause/wake/destroy on the same environment between that snapshot
        // and this action being applied would have its change silently
        // clobbered by `store.update` writing back the GC's stale copy.
        let _guard = self
            .lifecycle_lock
            .acquire(self.lock_key_for(env_id).await?)
            .await;
        let mut env = self.ensure_environment(env_id).await?;
        let transition = action
            .transition()
            .expect("Keep is filtered out before calling apply_gc_action");
        let name = resolved_container_name(project, &env);

        // Every suspending action stops the container. `Pause` used to call
        // `docker pause` instead, which Traefik's Docker provider cannot
        // see: it only publishes routers for `running` containers and
        // ignores pause/unpause events entirely, so a paused branch lost
        // its route permanently and answered 404 instead of waking. See the
        // long note in `ControlPlane::pause`.
        //
        // Idempotency: Docker returns 304/404 when stopping something
        // already stopped. Treat that as success so the scheduler does not
        // spam WARNs every tick — the real guard is the state-aware
        // `gc::evaluate` above, this is belt-and-suspenders.
        match action {
            GcAction::Pause | GcAction::Hibernate | GcAction::Destroy => {
                if let Err(e) = self.oci_for(env.node_id)?.stop(&name).await {
                    let msg = e.to_string();
                    if matches!(e, OciError::NotFound(_))
                        || msg.contains("already stopped")
                        || msg.contains("is already stopped")
                        || msg.contains("304")
                    {
                        tracing::debug!(%name, "container already stopped, treating as success");
                    } else {
                        return Err(e.into());
                    }
                }
            }
            GcAction::Keep => unreachable!("Keep is filtered out before calling apply_gc_action"),
        }
        if self.docker_network.is_none() {
            if action == GcAction::Destroy {
                self.proxy.remove(env.project_id, &env.branch.name).await;
            } else {
                self.proxy
                    .mark_unavailable(env.project_id, &env.branch.name)
                    .await;
            }
        }
        if action == GcAction::Destroy {
            match self.oci_for(env.node_id)?.remove(&name).await {
                Ok(()) | Err(OciError::NotFound(_)) => {}
                Err(e) => return Err(e.into()),
            }
            // The image was built on the node that ran the container, and
            // that is the only copy this fleet has: images are not
            // distributed, each node builds its own.
            match self
                .oci_for(env.node_id)?
                .remove_image(&image_name(project, &env.branch.name))
                .await
            {
                Ok(()) | Err(OciError::NotFound(_)) => {}
                Err(e) => return Err(e.into()),
            }
            self.release_dependencies(env.project_id, &env.branch.name)
                .await?;
        }

        env.transition(transition, now).map_err(|e| state_err(&e))?;
        EnvironmentStore::update(&self.store, &env).await?;
        self.store
            .record(&AuditEvent::new(
                u64::try_from(now.unix_timestamp()).unwrap_or_default(),
                env.id,
                transition,
                None,
                now,
            ))
            .await?;
        Ok(())
    }
}
