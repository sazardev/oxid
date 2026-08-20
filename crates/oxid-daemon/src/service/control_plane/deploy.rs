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
    pub async fn deploy(
        &self,
        project_id: ProjectId,
        branch: BranchName,
    ) -> Result<Environment, CpError> {
        match self
            .deploy_at(project_id, branch, None, None, false)
            .await?
        {
            DeployOutcome::Deployed(env) => Ok(env),
            DeployOutcome::Queued { .. } => {
                unreachable!("admission control is off, so this never queues")
            }
        }
    }

    /// Identical to [`Self::deploy`], but attributes the resulting audit
    /// events to `operator` (a named API token's owner) instead of leaving
    /// them anonymous.
    ///
    /// # Errors
    /// Returns [`CpError`] on any pipeline step failure.
    #[tracing::instrument(skip(self, operator), fields(%project_id, %branch, ?operator))]
    pub async fn deploy_with_operator(
        &self,
        project_id: ProjectId,
        branch: BranchName,
        operator: Option<String>,
    ) -> Result<Environment, CpError> {
        match self
            .deploy_at(project_id, branch, None, operator, false)
            .await?
        {
            DeployOutcome::Deployed(env) => Ok(env),
            DeployOutcome::Queued { .. } => {
                unreachable!("admission control is off, so this never queues")
            }
        }
    }

    /// The capacity-aware entry point: deploys `branch` immediately if it
    /// fits the host's currently free memory (see
    /// [`Self::with_admission_control`]), or queues it (persisted — see
    /// [`SqliteStore::enqueue_deploy`]) to be retried automatically as
    /// capacity frees up, rather than either failing outright or piling
    /// onto an already-strained host. If the request alone could *never*
    /// fit (it exceeds total usable capacity by itself), it's rejected
    /// immediately instead of queued forever.
    ///
    /// # Errors
    /// Returns [`CpError`] on any pipeline step failure, or
    /// [`CpError::InsufficientCapacity`] if the request can never fit.
    #[tracing::instrument(skip(self, operator), fields(%project_id, %branch, ?operator))]
    pub async fn deploy_or_queue(
        &self,
        project_id: ProjectId,
        branch: BranchName,
        operator: Option<String>,
    ) -> Result<DeployOutcome, CpError> {
        self.deploy_at(project_id, branch, None, operator, true)
            .await
    }

    /// Retries queued deploys (oldest first) that now fit the host's
    /// currently free capacity — meant to be driven by the scheduler
    /// alongside [`Self::sweep`], so capacity freed by a GC pause/hibernate
    /// or a manual `destroy` gets handed to whoever has been waiting
    /// longest instead of sitting idle until the next manual `oxid up`.
    ///
    /// Stops at the first entry that still doesn't fit rather than skipping
    /// ahead to a smaller one further back in the queue — preserves FIFO
    /// fairness (SPEC.md "Eficiencia Absoluta": queue and wait, don't let a
    /// small request cut in line ahead of one that's been waiting longer).
    ///
    /// Returns the queue ids that failed to redeploy once retried (e.g. the
    /// branch was deleted upstream in the meantime); the queue continues
    /// past these rather than stalling on one bad entry.
    ///
    /// # Errors
    /// Returns [`CpError`] if the queue or host capacity itself can't be
    /// read at all.
    pub async fn retry_queued_deploys(&self) -> Result<Vec<(u64, CpError)>, CpError> {
        let mut failures = Vec::new();
        for queued in self.store.list_deploy_queue().await? {
            let project = match self.ensure_project(queued.project_id).await {
                Ok(p) => p,
                Err(e) => {
                    failures.push((queued.id, e));
                    continue;
                }
            };
            match self.check_admission(&project).await {
                Ok(Admission::Fits) => {}
                Ok(Admission::Queue) => break,
                Err(e) => {
                    failures.push((queued.id, e));
                    continue;
                }
            }

            self.store.remove_from_deploy_queue(queued.id).await?;
            let branch = match BranchName::parse(&queued.branch) {
                Ok(b) => b,
                Err(e) => {
                    failures.push((queued.id, e.into()));
                    continue;
                }
            };
            match self
                .deploy_at(queued.project_id, branch, None, queued.operator, false)
                .await
            {
                Ok(DeployOutcome::Deployed(_)) => {}
                Ok(DeployOutcome::Queued { .. }) => {
                    unreachable!("check_admission is off, so this never queues")
                }
                Err(e) => failures.push((queued.id, e)),
            }
        }
        Ok(failures)
    }

    /// Deploys `branch`, pinned to `sha_override` instead of the branch's
    /// current head when given — the mechanism [`Self::rollback`] reuses to
    /// redeploy a prior commit. Otherwise identical to [`Self::deploy`].
    /// When `check_admission` is set, may return
    /// [`DeployOutcome::Queued`] instead of deploying — see
    /// [`Self::deploy_or_queue`].
    ///
    /// # Errors
    /// Returns [`CpError`] on any pipeline step failure.
    #[allow(clippy::too_many_lines)]
    #[tracing::instrument(skip(self, sha_override, operator), fields(%project_id, %branch, ?operator, check_admission))]
    pub(crate) async fn deploy_at(
        &self,
        project_id: ProjectId,
        branch: BranchName,
        sha_override: Option<String>,
        operator: Option<String>,
        check_admission: bool,
    ) -> Result<DeployOutcome, CpError> {
        tracing::info!(%project_id, %branch, "deploy started");
        // Serializes the whole pipeline below (see `lifecycle_lock`'s doc
        // comment for the two race conditions this closes) — admission
        // control is decided under the same lock as the deploy it gates,
        // so two concurrent requests can't both see room and both proceed.
        let _guard = self.lifecycle_lock.lock().await;

        let project = self.ensure_project(project_id).await?;

        if check_admission && let Admission::Queue = self.check_admission(&project).await? {
            let queue_id = self
                .store
                .enqueue_deploy(project_id, &branch, operator.as_deref())
                .await?;
            // `enqueue_deploy` returns the row's own id, not its rank among
            // still-pending entries (earlier ones may already have been
            // retried and removed) — look it up so `position` reports what
            // it documents.
            let position = self
                .store
                .list_deploy_queue()
                .await?
                .iter()
                .position(|q| q.id == queue_id)
                .map_or(1, |i| i as u64 + 1);
            tracing::info!(%project_id, %branch, position, "deploy queued (insufficient host capacity)");
            return Ok(DeployOutcome::Queued { position });
        }

        // 0. A redeploy of an already-live branch (a webhook firing on a new
        // push, or a second `oxid up`) used to destroy the previous
        // container *before* building/starting the new one — always a real
        // gap where the branch was unreachable, and in direct-publish mode
        // the address itself changed underneath anyone already using it.
        // The previous instance is kept fully alive here, still serving
        // traffic, until the new one is built, started and confirmed
        // healthy — see the cutover at the end of `run_and_activate`.
        let previous = self
            .find_environment_by_branch(project_id, &branch)
            .await?
            .filter(|e| e.state != EnvironmentState::Destroyed);

        // 1. Clone cache + resolve (or reuse an explicit rollback target)
        // + checkout the commit. `git_token`, when set, authenticates the
        // clone/fetch for a private repository (see
        // `Self::set_project_git_token`) — this daemon-side cache is cloned
        // independently of whatever git credential helper an operator's own
        // shell has configured, so a private repo needs its own credential.
        let git_token = self.store.get_git_token(project.id).await?;
        let repo_dir = self
            .git
            .ensure_repo(&project.repo_url, git_token.as_deref(), &self.cache_dir)
            .await?;
        let commit = match sha_override {
            Some(sha) => CommitRef {
                branch: branch.clone(),
                sha,
            },
            None => self.git.resolve_branch_head(&repo_dir, &branch).await?,
        };
        self.git.checkout_commit(&repo_dir, &commit.sha).await?;

        // 2. Build the image.
        //
        // `[build].context` (e.g. a monorepo subdirectory like `backend/`)
        // was parsed from `oxid.toml` and persisted, but never actually
        // consulted here — every build used the whole repo checkout as its
        // context regardless. Found while wiring `docker-compose.yml`
        // support, whose `build.context`/`build.dockerfile` pair only makes
        // sense if `dockerfile` really is resolved relative to `context`.
        let image = image_name(&project, &branch);
        let build = BuildSpec {
            context: repo_dir.join(&project.config.build.context),
            dockerfile: project
                .config
                .build
                .dockerfile
                .clone()
                .unwrap_or_else(|| "Dockerfile".to_owned()),
            image: image.clone(),
        };
        self.oci.build(&build).await?;

        // 3. Create the environment (Building) and persist it.
        let url = subdomain_for(&branch, &project.config.base_domain);
        let now = OffsetDateTime::now_utc();
        let mut env = Environment::new(
            EnvironmentId(0),
            project.id,
            Branch::new(commit.branch, commit.sha)?,
            EnvironmentState::Building,
            url.clone(),
            now,
        )?;
        let env_id = EnvironmentStore::create(&self.store, &env).await?;
        env.id = env_id;
        // A per-deployment-unique container name, distinct from the
        // previous (still running) instance's — the two coexist briefly
        // during the cutover below, so they can never share a name.
        env.container_name = Some(format!(
            "oxid-{}-{}-{}",
            project.name,
            sanitize_label(&branch),
            env.id.0
        ));

        // 4-7: resolve secrets, run the container, run `on_start` hooks,
        // wait for it to be ready, then cut over from `previous` (if any)
        // and activate. Everything from here on can fail (a bad secret, a
        // Docker error, a failing hook, a readiness timeout) *after* the row
        // above was already persisted as `Building` — but `previous`, if
        // any, is never touched until the new instance is confirmed ready,
        // so a failed redeploy leaves the branch exactly as reachable as it
        // was before the redeploy started. Leaving the new row stuck as
        // `Building` on error would brick the branch permanently otherwise
        // (`Building` cannot transition to `Destroy`), see regression test
        // `failed_deploy_does_not_permanently_block_branch`.
        if let Err(err) = self
            .run_and_activate(
                &project,
                &branch,
                image,
                url,
                &mut env,
                previous.as_ref(),
                operator.as_deref(),
            )
            .await
        {
            let now = OffsetDateTime::now_utc();
            if env.transition(StateTransition::BuildFailed, now).is_ok() {
                let _ = EnvironmentStore::update(&self.store, &env).await;
                let _ = self
                    .store
                    .record(
                        &AuditEvent::with_operator(
                            u64::try_from(now.unix_timestamp()).unwrap_or_default(),
                            env.id,
                            StateTransition::BuildFailed,
                            Some(err.to_string()),
                            now,
                            operator.clone(),
                        )
                        .with_request_id(current_request_id()),
                    )
                    .await;
            }
            tracing::error!(%project_id, %branch, environment_id = %env.id, error = %err, "deploy failed");
            return Err(err);
        }

        // The new instance is live (and, per the cutover inside
        // `run_and_activate`, the previous container is already gone) —
        // now retire the previous Environment row so `status`/branch
        // resolution stop pointing at it.
        if let Some(mut prev) = previous {
            let now = OffsetDateTime::now_utc();
            if prev.transition(StateTransition::Destroy, now).is_ok() {
                let _ = EnvironmentStore::update(&self.store, &prev).await;
            }
        }

        tracing::info!(%project_id, %branch, environment_id = %env.id, "deploy succeeded");
        Ok(DeployOutcome::Deployed(env))
    }

    /// Redeploys `branch` at a prior commit instead of its current head —
    /// the safety net for a bad deploy, since `oxid up` always rebuilds from
    /// HEAD with no way back otherwise. Reuses `environments`' existing
    /// per-deploy history (a new row per deploy, the prior one marked
    /// `Destroyed` once the new one cuts over — see [`Self::deploy_at`])
    /// rather than needing any new storage: every past deploy's commit is
    /// already sitting in
    /// `Environment.branch.commit_sha`.
    ///
    /// Without `to_sha`, rolls back to the commit immediately before the
    /// current live one. With `to_sha`, rolls back to that specific commit —
    /// but only if it actually appears in this branch's history, so a typo
    /// or an unrelated sha can't be deployed under the guise of "rollback".
    ///
    /// # Errors
    /// [`CpError::NotFound`] if the branch has no prior deploy to roll back
    /// to (or `to_sha` doesn't match one), plus anything [`Self::deploy`]
    /// can fail with.
    pub async fn rollback(
        &self,
        project_id: ProjectId,
        branch: BranchName,
        to_sha: Option<String>,
    ) -> Result<Environment, CpError> {
        self.rollback_with_operator(project_id, branch, to_sha, None)
            .await
    }

    /// Identical to [`Self::rollback`], attributing the resulting audit
    /// events to `operator`.
    ///
    /// # Errors
    /// Same as [`Self::rollback`].
    pub async fn rollback_with_operator(
        &self,
        project_id: ProjectId,
        branch: BranchName,
        to_sha: Option<String>,
        operator: Option<String>,
    ) -> Result<Environment, CpError> {
        let mut history = self.store.list_by_project(project_id).await?;
        history.retain(|e| e.branch.name == branch);
        history.sort_by_key(|e| std::cmp::Reverse(e.id.0));

        let target_sha = match to_sha {
            Some(sha) => history
                .iter()
                .find(|e| e.branch.commit_sha == sha)
                .map(|e| e.branch.commit_sha.clone())
                .ok_or_else(|| {
                    CpError::NotFound(format!(
                        "commit `{sha}` in branch `{branch}`'s deploy history"
                    ))
                })?,
            None => history
                .iter()
                .skip(1) // the current live deploy
                .map(|e| e.branch.commit_sha.clone())
                .next()
                .ok_or_else(|| {
                    CpError::NotFound(format!(
                        "a prior deploy of branch `{branch}` to roll back to"
                    ))
                })?,
        };

        match self
            .deploy_at(project_id, branch, Some(target_sha), operator, false)
            .await?
        {
            DeployOutcome::Deployed(env) => Ok(env),
            DeployOutcome::Queued { .. } => {
                unreachable!("admission control is off, so this never queues")
            }
        }
    }
}
