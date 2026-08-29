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
    /// Suspends an environment (scale-to-zero), unattributed.
    ///
    /// # Errors
    /// Returns [`CpError`] on missing records or Docker failures.
    pub async fn pause(&self, environment_id: EnvironmentId) -> Result<(), CpError> {
        self.pause_with_operator(environment_id, None).await
    }

    /// Identical to [`Self::pause`], attributing the resulting audit event
    /// to `operator`.
    ///
    /// # Errors
    /// Same as [`Self::pause`].
    pub async fn pause_with_operator(
        &self,
        environment_id: EnvironmentId,
        operator: Option<String>,
    ) -> Result<(), CpError> {
        let _guard = self.lifecycle_lock.lock().await;
        let mut env = self.ensure_environment(environment_id).await?;
        let project = ProjectStore::get(&self.store, env.project_id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("project `{}`", env.project_id)))?;
        // Scale-to-zero suspends with `docker stop`, deliberately *not*
        // `docker pause`. Traefik's Docker provider only ever publishes
        // routers for containers in the `running` state and does not react
        // to `pause`/`unpause` events at all: a paused container's router
        // silently disappears at the provider's next refresh and never
        // comes back, so every request to a paused branch 404s at the
        // proxy. That 404 also means the `errors` middleware that powers
        // wake-on-request (see `traefik_labels`) never fires, because it
        // only triggers on a 5xx from a router that still exists —
        // measured live: a container left unpaused for 60s never regained
        // its router, while stop/start regained it in under 3s.
        //
        // `stop` costs a process restart on wake that `unpause` would not,
        // which is the honest price of a suspension the router layer can
        // actually observe.
        self.oci
            .stop(&resolved_container_name(&project, &env))
            .await?;
        if self.docker_network.is_none() {
            self.proxy
                .mark_unavailable(env.project_id, &env.branch.name)
                .await;
        }

        let now = OffsetDateTime::now_utc();
        env.transition(StateTransition::IdleTimeout, now)
            .map_err(|e| state_err(&e))?;
        EnvironmentStore::update(&self.store, &env).await?;
        // The GC sweep has always audited the pauses it performs; a pause
        // asked for by a person did not, so the trail showed the automatic
        // half of the lifecycle and none of the deliberate half.
        self.record_lifecycle(&env, StateTransition::IdleTimeout, now, operator)
            .await;
        tracing::info!(%environment_id, "environment paused");
        Ok(())
    }

    /// Best-effort audit write for a lifecycle transition that already
    /// happened. Never fails the operation it describes: the state change is
    /// committed by the time this runs, and losing the audit row is strictly
    /// better than reporting a failure for work that succeeded.
    async fn record_lifecycle(
        &self,
        env: &Environment,
        kind: StateTransition,
        now: OffsetDateTime,
        operator: Option<String>,
    ) {
        if let Err(e) = self
            .store
            .record(
                &AuditEvent::with_operator(
                    u64::try_from(now.unix_timestamp()).unwrap_or_default(),
                    env.id,
                    kind,
                    None,
                    now,
                    operator,
                )
                .with_request_id(current_request_id()),
            )
            .await
        {
            tracing::warn!(environment_id = %env.id, ?kind, error = %e, "could not record audit event");
        }
    }

    /// Wakes a suspended environment.
    ///
    /// `Paused` containers are still alive in memory (`docker unpause`);
    /// `Hibernating` ones were fully `stop`ped and must be `start`ed instead.
    ///
    /// # Errors
    /// Returns [`CpError`] on missing records or Docker failures.
    #[tracing::instrument(skip(self), fields(%environment_id))]
    pub async fn wake(&self, environment_id: EnvironmentId) -> Result<(), CpError> {
        self.wake_env_with_operator(environment_id, None).await
    }

    /// Identical to [`Self::wake`], attributing the resulting audit event to
    /// `operator`.
    ///
    /// # Errors
    /// Same as [`Self::wake`].
    pub async fn wake_with_operator(
        &self,
        environment_id: EnvironmentId,
        operator: Option<String>,
    ) -> Result<(), CpError> {
        self.wake_env_with_operator(environment_id, operator).await
    }

    /// Wakes the environment routed at `url` (matched against the `Host`
    /// header Traefik forwards). Used by the wake-on-request endpoint
    /// (SPEC.md §3.2). Returns `None` silently when no environment owns
    /// `url`, since Traefik may forward hosts Oxid does not manage.
    ///
    /// # Errors
    /// Returns [`CpError`] on persistence or Docker failures.
    pub async fn wake_by_url(&self, url: &str) -> Result<Option<Environment>, CpError> {
        let Some(env) = self.store.find_by_url(url).await? else {
            return Ok(None);
        };
        self.wake_env(env.id).await?;
        Ok(EnvironmentStore::get(&self.store, env.id).await?)
    }

    /// Refreshes `last_accessed_at` for the environment routed at `url`
    /// without changing its state. Backs the heartbeat endpoint a Traefik
    /// `forwardAuth` middleware calls on every request to a `Running`
    /// environment (SPEC.md §3.2 traffic monitor). No-ops silently when no
    /// environment owns `url`.
    ///
    /// # Errors
    /// Returns [`CpError`] on persistence failures.
    pub async fn touch_by_url(&self, url: &str) -> Result<(), CpError> {
        let Some(mut env) = self.store.find_by_url(url).await? else {
            return Ok(());
        };
        let now = OffsetDateTime::now_utc();
        // Touching is best-effort bookkeeping; a Destroyed/terminal state
        // simply can't be touched and that's fine to ignore.
        let _ = env.touch(now);
        EnvironmentStore::update(&self.store, &env).await?;
        Ok(())
    }

    pub(crate) async fn wake_env(&self, environment_id: EnvironmentId) -> Result<(), CpError> {
        self.wake_env_with_operator(environment_id, None).await
    }

    pub(crate) async fn wake_env_with_operator(
        &self,
        environment_id: EnvironmentId,
        operator: Option<String>,
    ) -> Result<(), CpError> {
        let _guard = self.lifecycle_lock.lock().await;
        let mut env = self.ensure_environment(environment_id).await?;
        let project = ProjectStore::get(&self.store, env.project_id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("project `{}`", env.project_id)))?;
        let name = resolved_container_name(&project, &env);
        // Wake against what Docker *actually* reports, never against the
        // state this database happens to hold. The two drift constantly in
        // practice — a container crashes, is OOM-killed, gets stopped by an
        // operator, or two requests race into the wake path at once — and
        // dispatching on the stored state alone meant any drift surfaced a
        // raw `Docker responded with status code 500: ... is not paused`
        // straight into the visitor's browser, with every retry reproducing
        // it identically. Waking something already awake is a success, not
        // an error.
        match self.oci.container_status(&name).await? {
            ContainerStatus::Running => {
                tracing::debug!(%environment_id, "wake: container already running");
            }
            ContainerStatus::Paused => self.oci.unpause(&name).await?,
            ContainerStatus::Stopped => self.oci.start(&name).await?,
            ContainerStatus::Missing => {
                return Err(CpError::NotFound(format!(
                    "container `{name}` no longer exists; redeploy this branch to recreate it"
                )));
            }
        }

        // Backfills `host_port` for an environment that predates dynamic
        // port assignment (deployed by an older Oxid build, so this column
        // was never populated) — otherwise it stays wrong forever, since
        // waking only starts/unpauses the *existing* container instead of
        // recreating it through `run()`, which is the only other place that
        // learns this. Best-effort: `network.is_some()` (Traefik) or a
        // lookup failure just leaves it as it was.
        if env.host_port.is_none() && self.docker_network.is_none() {
            env.host_port = self
                .oci
                .published_port(&name, project.config.port)
                .await
                .unwrap_or(None);
        }

        // Repoints the branch's stable proxy back at this container now
        // that it's alive again — without this, a woken environment stays
        // unreachable through its public address (still `mark_unavailable`d
        // from the pause that put it to sleep) even though the container
        // itself is running again.
        if self.docker_network.is_none()
            && let Some(port) = env.host_port
        {
            let public_port = self
                .proxy
                .ensure(env.project_id, &env.branch.name, env.public_port)
                .await?;
            env.public_port = Some(public_port);
            self.proxy
                .set_target(env.project_id, &env.branch.name, port)
                .await;
        }

        let now = OffsetDateTime::now_utc();
        // `Woken` out of `Running` is a no-op transition, which the state
        // machine reports as an error. That is the correct answer for the
        // domain and the wrong one here: reaching this point means the
        // container is confirmed up, so the caller got what it asked for.
        // Only a genuinely forbidden move (e.g. out of `Destroyed`) is a
        // real failure.
        match env.transition(StateTransition::Woken, now) {
            Ok(()) => {}
            Err(_) if env.state == EnvironmentState::Running => {}
            Err(e) => return Err(state_err(&e)),
        }
        // Without this, a woken environment's idle clock still reads its
        // pre-sleep timestamp: the very next GC sweep sees it as still idle
        // past `pause_after` and pauses it right back — observed live,
        // ~7s after a manual wake. The request that's about to be served
        // (or the wake page's auto-reload) is the traffic that justifies
        // staying awake, so count it now rather than waiting for a
        // follow-up heartbeat that may not land before the next tick.
        let _ = env.touch(now);
        EnvironmentStore::update(&self.store, &env).await?;
        self.record_lifecycle(&env, StateTransition::Woken, now, operator)
            .await;
        tracing::info!(%environment_id, "environment woken");
        Ok(())
    }

    /// Permanently destroys an environment (`oxid down`): stops and removes
    /// its container and image, then transitions it to `Destroyed`.
    ///
    /// Branch-scoped secrets survive by default — a recurring feature
    /// branch's config (DB passwords, API keys) shouldn't vanish just
    /// because the environment idled out and got TTL-destroyed. Pass
    /// `purge_secrets = true` (`oxid down --purge-secrets`) to explicitly
    /// clear them too.
    ///
    /// # Errors
    /// Returns [`CpError`] on missing records or Docker failures.
    #[tracing::instrument(skip(self), fields(%environment_id, purge_secrets))]
    pub async fn destroy(
        &self,
        environment_id: EnvironmentId,
        purge_secrets: bool,
    ) -> Result<(), CpError> {
        self.destroy_with_operator(environment_id, purge_secrets, None)
            .await
    }

    /// Identical to [`Self::destroy`], attributing the resulting audit
    /// event to `operator`.
    ///
    /// # Errors
    /// Same as [`Self::destroy`].
    #[tracing::instrument(skip(self, operator), fields(%environment_id, purge_secrets, ?operator))]
    pub async fn destroy_with_operator(
        &self,
        environment_id: EnvironmentId,
        purge_secrets: bool,
        operator: Option<String>,
    ) -> Result<(), CpError> {
        let _guard = self.lifecycle_lock.lock().await;
        let mut env = self.ensure_environment(environment_id).await?;
        let project = ProjectStore::get(&self.store, env.project_id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("project `{}`", env.project_id)))?;
        let name = resolved_container_name(&project, &env);
        // A container that isn't there is the desired end state, not an
        // error. `BuildFailed` environments frequently have none at all
        // (the image build never produced one), and an operator may have
        // removed one by hand — refusing to tear down the record in either
        // case would leave a row nothing could ever clean up.
        match self.oci.stop(&name).await {
            Ok(()) | Err(OciError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }
        match self.oci.remove(&name).await {
            Ok(()) | Err(OciError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }
        // Best-effort: an image that never finished building (a deploy that
        // failed at the `build` step) simply won't exist yet.
        match self
            .oci
            .remove_image(&image_name(&project, &env.branch.name))
            .await
        {
            Ok(()) | Err(OciError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }
        if self.docker_network.is_none() {
            self.proxy.remove(env.project_id, &env.branch.name).await;
        }

        if purge_secrets {
            self.purge_branch_secrets(env.project_id, &env.branch.name)
                .await?;
        }
        self.release_dependencies(env.project_id, &env.branch.name)
            .await?;

        let now = OffsetDateTime::now_utc();
        env.transition(StateTransition::Destroy, now)
            .map_err(|e| state_err(&e))?;
        EnvironmentStore::update(&self.store, &env).await?;
        self.store
            .record(
                &AuditEvent::with_operator(
                    u64::try_from(now.unix_timestamp()).unwrap_or_default(),
                    env.id,
                    StateTransition::Destroy,
                    None,
                    now,
                    operator,
                )
                .with_request_id(current_request_id()),
            )
            .await?;
        tracing::info!(%environment_id, "environment destroyed");
        Ok(())
    }

    /// Deletes every `branch`-scoped secret for `branch` (used by
    /// `destroy(.., purge_secrets: true)`). Global and project-scope
    /// secrets are untouched — this only clears config specific to this
    /// one branch.
    pub(crate) async fn purge_branch_secrets(
        &self,
        project_id: ProjectId,
        branch: &BranchName,
    ) -> Result<(), CpError> {
        let secrets =
            SecretStore::list_secrets(&self.store, Some(project_id), Some(branch)).await?;
        for (name, scope) in secrets {
            if scope == EnvVarScope::Branch {
                SecretStore::delete_secret(&self.store, Some(project_id), Some(branch), &name)
                    .await?;
            }
        }
        Ok(())
    }

    /// Finds the current environment for `branch` within a project, if any.
    /// A branch can have multiple historical rows (each `deploy` call
    /// creates a new one), so this prefers the most recent *live*
    /// (serving) row over a merely higher-id one — a redeploy that
    /// zero-downtime-cuts-over successfully leaves exactly one live row as
    /// the highest id, but a *failed* redeploy leaves a higher-id
    /// `Destroyed`/`BuildFailed` row sitting on top of a still-`Running`
    /// older one, which
    /// would otherwise "hide" it from callers that need to know whether the
    /// branch is actually still live (e.g. the webhook branch-deletion
    /// handler). Only falls back to the highest-id row overall (which will
    /// be `Destroyed`) when nothing is live at all.
    ///
    /// # Errors
    /// Returns [`CpError::NotFound`] if the project does not exist.
    pub async fn find_environment_by_branch(
        &self,
        project_id: ProjectId,
        branch: &BranchName,
    ) -> Result<Option<Environment>, CpError> {
        self.ensure_project(project_id).await?;
        let envs: Vec<Environment> = self
            .store
            .list_by_project(project_id)
            .await?
            .into_iter()
            .filter(|e| &e.branch.name == branch)
            .collect();
        // "Live" means a row that actually has (or is getting) a container,
        // which deliberately excludes `BuildFailed` as well as `Destroyed`.
        // A failed deploy leaves a higher-id row sitting on top of the
        // still-serving older one exactly like a failed redeploy does, and
        // treating it as current made a subsequent successful deploy miss
        // the real previous instance and leave it running forever.
        if let Some(live) = envs
            .iter()
            .filter(|e| {
                matches!(
                    e.state,
                    EnvironmentState::Running
                        | EnvironmentState::Paused
                        | EnvironmentState::Hibernating
                        | EnvironmentState::Building
                )
            })
            .max_by_key(|e| e.id.0)
        {
            return Ok(Some(live.clone()));
        }
        Ok(envs.into_iter().max_by_key(|e| e.id.0))
    }

    /// Returns the logs of an environment's container.
    ///
    /// # Errors
    /// Returns [`CpError`] on missing records or Docker failures.
    pub async fn logs(&self, environment_id: EnvironmentId) -> Result<String, CpError> {
        let env = self.ensure_environment(environment_id).await?;
        let project = ProjectStore::get(&self.store, env.project_id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("project `{}`", env.project_id)))?;
        Ok(self
            .oci
            .logs(&resolved_container_name(&project, &env))
            .await?)
    }

    /// Follows an environment's container logs live, yielding new lines as
    /// they're written (SPEC.md §5's SSE `/logs/stream` endpoint).
    ///
    /// # Errors
    /// Returns [`CpError`] on missing records or Docker failures.
    pub async fn stream_logs(&self, environment_id: EnvironmentId) -> Result<LogStream, CpError> {
        let env = self.ensure_environment(environment_id).await?;
        let project = ProjectStore::get(&self.store, env.project_id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("project `{}`", env.project_id)))?;
        Ok(self
            .oci
            .stream_logs(&resolved_container_name(&project, &env))
            .await?)
    }

    pub(crate) async fn ensure_environment(
        &self,
        environment_id: EnvironmentId,
    ) -> Result<Environment, CpError> {
        EnvironmentStore::get(&self.store, environment_id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("environment `{environment_id}`")))
    }

    /// The project an environment belongs to — lets the HTTP layer enforce
    /// project-scoped tokens on `/environments/{id}/...` routes, which are
    /// addressed by environment id but authorized by project.
    ///
    /// # Errors
    /// Returns [`CpError::NotFound`] if the environment does not exist.
    pub async fn environment_project_id(
        &self,
        environment_id: EnvironmentId,
    ) -> Result<ProjectId, CpError> {
        Ok(self.ensure_environment(environment_id).await?.project_id)
    }

    /// Looks up a single environment by id, or `None` when it doesn't exist.
    ///
    /// # Errors
    /// Returns [`CpError`] on persistence failures.
    pub async fn find_environment(
        &self,
        environment_id: EnvironmentId,
    ) -> Result<Option<Environment>, CpError> {
        Ok(EnvironmentStore::get(&self.store, environment_id).await?)
    }
}
