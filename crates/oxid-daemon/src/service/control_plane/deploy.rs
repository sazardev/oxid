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
use super::LockKey;
use super::error::CpError;
use super::helpers::{
    hash_token, image_name, lowest_free_index, resolved_container_name, sanitize_identifier,
    sanitize_label, state_err,
};
use super::types::{
    Admission, AdmissionMode, DeployOutcome, DeployReport, GcSummary, InfraStatus, NodeStats,
};
use crate::adapter::config;
use crate::adapter::postgres_pool::PostgresPool;
use crate::adapter::store::{ApiTokenSummary, SqliteStore};
use crate::request_context::current_request_id;
use oxid_core::services::gc::{self, GcAction};
use oxid_core::services::stack::{RepoManifest, Stack, detect as detect_stack};
use oxid_core::services::subdomain::subdomain_for;
use oxid_core::services::var_resolution::{VarSources, set_secret};
use oxid_core::{
    AuditEvent, AuditFilter, AuditStore, Branch, BranchName, BuildSpec, CommitRef, ContainerPort,
    ContainerSpec, ContainerStatus, Dependency, EnvVarScope, Environment, EnvironmentId,
    EnvironmentState, EnvironmentStore, GitPort, HostCapacity, LogStream, OciError, OffsetDateTime,
    PoolError, PoolKind, Project, ProjectId, ProjectStore, RepoUrl, RepositoryError, SecretStore,
    SecretValue, SelfWiringStatus, StateTransition, TraefikSpec, Ttl,
};

/// How many times a queued deploy may fail for a retryable reason before it
/// is abandoned. Bounded so an unreachable repository or a branch deleted
/// upstream doesn't get retried on every scheduler tick forever.
const MAX_DEPLOY_ATTEMPTS: u32 = 5;

/// Whether a failed deploy is worth trying again later.
///
/// Only failures that happened *before* the deploy could really begin
/// qualify: a clone that couldn't resolve DNS, a storage blip, a resource
/// pool that wasn't reachable yet. A build failure is deliberately excluded —
/// a broken Dockerfile fails identically on every retry, and it now leaves a
/// `BuildFailed` environment behind that says so.
pub(crate) fn is_retryable(err: &CpError) -> bool {
    match err {
        CpError::Git(_) | CpError::Store(_) | CpError::DeployNotReady(_) => true,
        // A pool that couldn't be reached may well be reachable next tick;
        // one that isn't configured on this daemon at all never will be.
        // Retrying the latter just multiplies the failure: a branch
        // declaring a dependency the operator hasn't set up produced five
        // identical `BuildFailed` rows instead of one, burying the single
        // actionable message under copies of itself.
        CpError::Pool(PoolError::Failure(_)) => true,
        CpError::Pool(PoolError::NotConfigured(_)) => false,
        _ => false,
    }
}

impl<G: GitPort, O: ContainerPort> ControlPlane<G, O> {
    pub async fn deploy(
        &self,
        project_id: ProjectId,
        branch: BranchName,
    ) -> Result<Environment, CpError> {
        match self
            .deploy_at(project_id, branch, None, None, AdmissionMode::Enqueue)
            .await?
        {
            DeployOutcome::Deployed(env, _) => Ok(env),
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
            .deploy_at(project_id, branch, None, operator, AdmissionMode::Enqueue)
            .await?
        {
            DeployOutcome::Deployed(env, _) => Ok(env),
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
        self.deploy_at(project_id, branch, None, operator, AdmissionMode::Enqueue)
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
        // Both the scheduler and every accepted webhook call this. Only one
        // drain may run at a time or two of them can read the same pending
        // row before either removes it and deploy the same push twice;
        // bailing out is correct rather than merely safe, because the drain
        // already running will reach anything just enqueued.
        let Ok(_drain) = self.deploy_drain_lock.try_lock() else {
            tracing::debug!("deploy queue drain already in progress; skipping");
            return Ok(Vec::new());
        };
        let mut failures = Vec::new();
        // Ids this pass has already tried. A bumped entry stays in the
        // queue on purpose, so without this the re-read below would retry
        // it immediately, forever.
        let mut seen = std::collections::HashSet::new();
        let mut queue = self.store.list_deploy_queue().await?.into_iter();

        // Drained in waves rather than one at a time.
        //
        // The queue is how every webhook push reaches a deploy, so a serial
        // drain made a team's pushes finish one after another however many
        // cores the host had: fifteen branches took as long as fifteen
        // builds run back to back. A build is mostly waiting on Docker, so
        // overlapping them is nearly free.
        //
        // Order is still respected between waves, and a wave stops the drain
        // the moment one of its entries reports it does not fit — a big
        // deploy is never starved by a stream of small ones behind it.
        loop {
            let wave: Vec<_> = queue.by_ref().take(self.deploy_concurrency).collect();
            if wave.is_empty() {
                break;
            }
            let mut running = Vec::new();
            for queued in wave {
                seen.insert(queued.id);
                let branch = match BranchName::parse(&queued.branch) {
                    Ok(b) => b,
                    Err(e) => {
                        // An unparseable branch will never become parseable.
                        let _ = self.store.remove_from_deploy_queue(queued.id).await;
                        failures.push((queued.id, e.into()));
                        continue;
                    }
                };
                // The entry is held until the deploy resolves, rather than
                // removed up front. A drain that crashed — or a daemon
                // restarted mid-deploy — used to lose the push outright.
                running.push(async move {
                    let outcome = self
                        .deploy_at(
                            queued.project_id,
                            branch,
                            None,
                            queued.operator.clone(),
                            AdmissionMode::AlreadyQueued,
                        )
                        .await;
                    (queued, outcome)
                });
            }

            let mut full = false;
            for (queued, outcome) in futures_util::future::join_all(running).await {
                match outcome {
                    Ok(DeployOutcome::Deployed(_, _)) => {
                        self.store.remove_from_deploy_queue(queued.id).await?;
                    }
                    Ok(DeployOutcome::Queued { .. }) => full = true,
                    Err(e) => {
                        let attempts = queued.attempts + 1;
                        if is_retryable(&e) && attempts < MAX_DEPLOY_ATTEMPTS {
                            tracing::warn!(
                                queue_id = queued.id,
                                attempts,
                                error = %e,
                                "queued deploy failed transiently; keeping it queued"
                            );
                            self.store.bump_deploy_attempts(queued.id).await?;
                        } else {
                            self.store.remove_from_deploy_queue(queued.id).await?;
                            failures.push((queued.id, e));
                        }
                    }
                }
            }
            if full {
                break;
            }

            // A burst of pushes arrives faster than the first drain can read
            // the queue, so most of them land *after* the snapshot above and
            // are answered "a drain is already running" — correctly, but the
            // drain they were counting on had already read past them. They
            // used to wait out a whole scheduler tick before anyone looked
            // again: measured on fifteen simultaneous pushes, sixteen of the
            // twenty-eight seconds were that wait, with the node idle.
            if queue.len() == 0 {
                let fresh: Vec<_> = self
                    .store
                    .list_deploy_queue()
                    .await?
                    .into_iter()
                    .filter(|q| !seen.contains(&q.id))
                    .collect();
                if fresh.is_empty() {
                    break;
                }
                queue = fresh.into_iter();
            }
        }
        Ok(failures)
    }

    /// Accepts a push for `branch` without deploying it inline: the deploy
    /// is persisted on the queue and returns its position.
    ///
    /// Webhook deliveries used to run the whole pipeline — clone, build,
    /// start — inside the HTTP request. GitHub abandons a delivery after 10
    /// seconds and does not retry push events, while a first build of a
    /// branch with real dependencies measured 54 seconds, so the delivery
    /// was reported as failed even though the environment came up fine;
    /// re-sending it by hand then deployed the same commit a second time.
    /// Queueing makes the response immediate and survives a daemon restart,
    /// which inline deploys never did.
    ///
    /// # Errors
    /// Returns [`CpError`] if the queue can't be written.
    pub async fn enqueue_push(
        &self,
        project_id: ProjectId,
        branch: &BranchName,
        operator: Option<&str>,
    ) -> Result<u64, CpError> {
        let queue_id = self
            .store
            .enqueue_deploy(project_id, branch, operator)
            .await?;
        let position = self
            .store
            .list_deploy_queue()
            .await?
            .iter()
            .position(|q| q.id == queue_id)
            .map_or(1, |i| i as u64 + 1);
        tracing::info!(%project_id, %branch, position, ?operator, "push queued for deploy");
        Ok(position)
    }

    /// Reports a deploy that doesn't currently fit, enqueuing it first
    /// unless it is already on the queue.
    ///
    /// # Errors
    /// Returns [`CpError`] if the queue can't be read or written.
    async fn queue_or_report(
        &self,
        project_id: ProjectId,
        branch: &BranchName,
        operator: Option<String>,
        admission: AdmissionMode,
    ) -> Result<DeployOutcome, CpError> {
        let position = match admission {
            AdmissionMode::Enqueue => {
                self.enqueue_push(project_id, branch, operator.as_deref())
                    .await?
            }
            // Already on the queue: report where it sits without adding a
            // duplicate entry the drain would deploy twice.
            AdmissionMode::AlreadyQueued | AdmissionMode::Bypass => self
                .store
                .list_deploy_queue()
                .await?
                .iter()
                .position(|q| q.project_id == project_id && q.branch == branch.as_str())
                .map_or(1, |i| i as u64 + 1),
        };
        tracing::info!(%project_id, %branch, position, "deploy waiting for host capacity");
        Ok(DeployOutcome::Queued { position })
    }

    /// Returns `project` with the build settings, routed port and
    /// dependencies taken from the checked-out commit's `oxid.toml`.
    ///
    /// A malformed or invalid `oxid.toml` fails the deploy — the branch
    /// asked for something specific and guessing at it is how the silent
    /// mismatch this replaces came about. A branch with *no* config file at
    /// all keeps the project's settings and only warns: dropping the file is
    /// a normal thing to do on a branch, and `parse_project` already falls
    /// back to a `docker-compose.yml`/`Dockerfile` when one is present.
    fn branch_config(&self, project: &Project, repo_dir: &Path) -> Result<Project, CpError> {
        // Only an `oxid.toml` on the commit overrides anything.
        //
        // `parse_project` also succeeds by *inferring* a config from a
        // `docker-compose.yml` or a bare `Dockerfile`, which is the right
        // behaviour when registering a project that has never been
        // configured — but the wrong one here: a branch that simply has no
        // `oxid.toml` would have the project's registered build settings,
        // port and dependencies silently replaced by zero-config defaults.
        // Keeping the project's config is what the branch asked for by not
        // saying anything.
        if !repo_dir.join("oxid.toml").exists() {
            return Ok(project.clone());
        }
        let parsed = match config::parse_project(repo_dir) {
            Ok(parsed) => parsed,
            Err(err @ (config::ConfigError::Parse(_) | config::ConfigError::Validation(_))) => {
                return Err(CpError::from(err));
            }
            Err(err) => {
                tracing::warn!(
                    project_id = %project.id,
                    error = %err,
                    "no readable oxid.toml on this commit; keeping the project's registered config"
                );
                return Ok(project.clone());
            }
        };

        let mut effective = project.clone();
        if effective.config.build != parsed.config.build {
            tracing::info!(project_id = %project.id, "branch overrides [build] from its own oxid.toml");
        }
        if effective.config.port != parsed.config.port {
            tracing::info!(
                project_id = %project.id,
                from = effective.config.port,
                to = parsed.config.port,
                "branch overrides [routing].port from its own oxid.toml"
            );
        }
        if effective.config.dependencies != parsed.config.dependencies {
            tracing::info!(
                project_id = %project.id,
                count = parsed.config.dependencies.len(),
                "branch overrides [dependencies] from its own oxid.toml"
            );
        }
        effective.config.build = parsed.config.build;
        effective.config.port = parsed.config.port;
        effective.config.dependencies = parsed.config.dependencies;
        Ok(effective)
    }

    /// Copies the build context out of the shared checkout into a private
    /// directory, so the build can run while another branch of the same
    /// project checks out over the original.
    ///
    /// Symlinks are recreated as symlinks rather than followed, matching
    /// what the build context tar does: a dangling one somewhere in a repo
    /// is not a reason to fail a deploy, and dereferencing it would be.
    async fn capture_build_context(
        &self,
        repo_dir: &Path,
        context: &str,
        environment_id: EnvironmentId,
    ) -> Result<BuildContext, CpError> {
        let source = repo_dir.join(context);
        let target = self
            .cache_dir
            .join("contexts")
            .join(environment_id.0.to_string());
        let cleanup = target.clone();
        tokio::task::spawn_blocking(move || {
            // A copy is normally removed when its deploy ends, but a daemon
            // killed mid-build leaves one behind, and merging a new commit
            // into it would resurrect files that commit deleted.
            match std::fs::remove_dir_all(&target) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            copy_tree(&source, &target)
        })
        .await
        .map_err(|e| CpError::Validation(format!("capturing the build context panicked: {e}")))?
        .map_err(|e| CpError::Validation(format!("cannot capture the build context: {e}")))?;
        Ok(BuildContext { path: cleanup })
    }

    /// Writes a generated Dockerfile into `context` when it has none.
    ///
    /// Returns what was detected, or `None` when the context already has a
    /// Dockerfile (the overwhelmingly common case, and the one where Oxid
    /// must keep its hands off) or when nothing recognisable was found —
    /// in which case the build fails with Docker's own "Dockerfile not
    /// found", which is the honest error rather than a generated build that
    /// dies halfway through for reasons nobody can trace.
    ///
    /// Writing into the copy rather than the checkout matters twice over:
    /// the developer's `git status` stays clean, and the moment they commit
    /// a Dockerfile of their own it wins, with no state to clear first.
    fn materialise_dockerfile(context: &Path, dockerfile: &str) -> Option<Stack> {
        let target = context.join(dockerfile);
        if target.exists() {
            return None;
        }
        let stack = detect_stack(&read_context_manifest(context))?;
        match std::fs::write(&target, stack.dockerfile()) {
            Ok(()) => Some(stack),
            Err(e) => {
                // Not fatal on its own: the build is about to fail with
                // Docker's own message, and that is clearer than replacing
                // it with a filesystem error about a file the developer
                // never asked for.
                tracing::warn!(path = %target.display(), error = %e, "could not write the generated Dockerfile");
                None
            }
        }
    }

    /// Records a failed deploy everywhere an operator might look for it:
    /// the environment row flips to `BuildFailed`, an audit event captures
    /// the reason and who triggered it, and an ERROR line lands in the log.
    ///
    /// Every failure path in `deploy_at` funnels through here. Before this
    /// existed the recovery was inlined in a single `match` arm that a
    /// failing image build never reached, so the most common failure in the
    /// product was also its most invisible one.
    ///
    /// Deliberately infallible: this runs while already returning an error,
    /// and a bookkeeping failure must not mask the original cause. Each step
    /// is best-effort and logged rather than propagated.
    async fn record_deploy_failure(
        &self,
        env: &mut Environment,
        operator: Option<&String>,
        err: &CpError,
    ) {
        let now = OffsetDateTime::now_utc();
        if env.transition(StateTransition::BuildFailed, now).is_ok() {
            if let Err(e) = EnvironmentStore::update(&self.store, env).await {
                tracing::warn!(environment_id = %env.id, error = %e, "could not persist failed deploy state");
            }
            if let Err(e) = self
                .store
                .record(
                    &AuditEvent::with_operator(
                        u64::try_from(now.unix_timestamp()).unwrap_or_default(),
                        env.id,
                        StateTransition::BuildFailed,
                        Some(err.to_string()),
                        now,
                        operator.cloned(),
                    )
                    .with_request_id(current_request_id()),
                )
                .await
            {
                tracing::warn!(environment_id = %env.id, error = %e, "could not record failed deploy audit event");
            }
        }
        tracing::error!(
            environment_id = %env.id,
            project_id = %env.project_id,
            branch = %env.branch.name,
            operator = ?operator,
            error = %err,
            "deploy failed"
        );
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
    #[tracing::instrument(skip(self, sha_override, operator), fields(%project_id, %branch, ?operator, ?admission))]
    pub(crate) async fn deploy_at(
        &self,
        project_id: ProjectId,
        branch: BranchName,
        sha_override: Option<String>,
        operator: Option<String>,
        admission: AdmissionMode,
    ) -> Result<DeployOutcome, CpError> {
        tracing::info!(%project_id, %branch, "deploy started");
        // Exclusive against this *branch* only. Two deploys of the same
        // branch still race on its environment rows, its container name and
        // its cutover, so they serialize; two different branches share none
        // of that and no longer wait on each other.
        let _branch_guard = self
            .lifecycle_lock
            .acquire(LockKey::Branch(project_id, branch.to_string()))
            .await;

        let project = self.ensure_project(project_id).await?;

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
            // A `BuildFailed` row is kept around so the failure stays
            // visible, but it never had a healthy container serving
            // traffic — treating it as the live previous instance would
            // make the cutover try to retire something that was never up.
            .filter(|e| {
                !matches!(
                    e.state,
                    EnvironmentState::Destroyed | EnvironmentState::BuildFailed
                )
            });

        // 1. Clone cache + resolve (or reuse an explicit rollback target)
        // + checkout the commit. `git_token`, when set, authenticates the
        // clone/fetch for a private repository (see
        // `Self::set_project_git_token`) — this daemon-side cache is cloned
        // independently of whatever git credential helper an operator's own
        // shell has configured, so a private repo needs its own credential.
        //
        // The fetch is deliberately *outside* the git lock and shared with
        // any sibling deploy that asked at the same time. It brings down
        // every branch of the repository, so fifteen branches pushed at once
        // need one fetch, not fifteen — and it is a network round-trip, so
        // repeating it under a lock made a burst of pushes finish one
        // round-trip at a time. Measured on this repository against GitHub,
        // fourteen redundant fetches were about three quarters of the
        // wall-clock of the whole burst.
        let asked_at = std::time::Instant::now();
        let git_token = self.store.get_git_token(project.id).await?;
        let repo_url = project.repo_url.clone();
        let repo_dir = self
            .git_fetches
            .run(project_id, asked_at, || async {
                self.git
                    .ensure_repo(&repo_url, git_token.as_deref(), &self.cache_dir)
                    .await
            })
            .await?;

        // Everything from here to the build context being captured touches
        // the one working directory every branch of this project shares.
        // Held per project rather than globally, and released before the
        // build — the build is the long pole and the part worth overlapping.
        let git_guard = self
            .lifecycle_lock
            .acquire(LockKey::GitCache(project_id))
            .await;
        let commit = match sha_override {
            Some(sha) => CommitRef {
                branch: branch.clone(),
                sha,
            },
            None => self.git.resolve_branch_head(&repo_dir, &branch).await?,
        };
        self.git.checkout_commit(&repo_dir, &commit.sha).await?;

        // 2. Create the environment row (`Building`) *before* anything that
        // can fail.
        //
        // It used to be created after the image build, which meant a broken
        // Dockerfile — by far the most common way a deploy fails — bailed
        // out through `?` before any row existed, skipping the recovery
        // block below entirely: no environment, no audit event, and not one
        // ERROR line in the log. The only symptom was a 500 on the webhook,
        // so from inside the dashboard or `oxid status` a colleague's failed
        // push was indistinguishable from a push that never happened.
        // Persisting the row first is what gives every later failure
        // somewhere to be recorded against.
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

        // Two different branches can normalise to one subdomain —
        // `feat/dup-x` and `feat-dup-x` both become `feat-dup-x`, which is
        // exactly what happens when someone renames a branch and pushes
        // both. Both environments then report themselves as running on the
        // same URL while the proxy can only ever route to one of them, so
        // the loser is unreachable forever with nothing anywhere saying why.
        //
        // Checked after the row exists rather than before, so the refusal is
        // recorded against it: the push arrived over a webhook that was
        // already answered, so an error returned to nobody is an error the
        // dev who pushed will never see.
        if let Some(other) = EnvironmentStore::list_by_project(&self.store, project_id)
            .await?
            .into_iter()
            .find(|other| {
                !matches!(
                    other.state,
                    EnvironmentState::Destroyed | EnvironmentState::BuildFailed
                ) && other.url == url
                    && other.branch.name != branch
            })
        {
            let err = CpError::Validation(crate::i18n::tf(
                "deploy.subdomainTaken",
                &[
                    ("branch", branch.as_str()),
                    ("url", &url),
                    ("other", other.branch.name.as_str()),
                ],
            ));
            self.record_deploy_failure(&mut env, operator.as_ref(), &err)
                .await;
            return Err(err);
        }

        // Re-read `oxid.toml` from the commit actually being deployed.
        //
        // Every deploy used to run against the config captured when the
        // project was first registered, so anything a branch changed in its
        // own `oxid.toml` was silently ignored: a branch declaring a new
        // Postgres dependency deployed "successfully" with no database and
        // no `DATABASE_URL`, and a branch asking for more memory quietly got
        // the project's. Nothing warned, because nothing ever looked.
        //
        // Build settings, the routed port and dependencies are properties of
        // the commit, so they come from the branch. The base domain and the
        // idle/lifetime policy stay with the project: those are operator
        // decisions owned by `oxid configure`, and letting any branch rewrite
        // them would let one push change another branch's URL or TTL.
        let project = match self.branch_config(&project, &repo_dir) {
            Ok(effective) => effective,
            Err(err) => {
                self.record_deploy_failure(&mut env, operator.as_ref(), &err)
                    .await;
                return Err(err);
            }
        };

        // The one and only admission decision, taken here because this is
        // the first point where the *actual* request is known: the branch's
        // own `oxid.toml` may ask for far more (or less) memory than the
        // project was registered with. Deciding earlier — the only option
        // before branch config was honoured — meant the gate was weighing a
        // number the deploy wasn't going to use.
        // A rollback replaces an environment the host is already carrying,
        // so gating it on free capacity would refuse the one deploy whose
        // whole purpose is getting back to a working state.
        let admitted = if admission == AdmissionMode::Bypass {
            Ok(Admission::Fits)
        } else {
            // Serialized node-wide, but only around the check itself — it is
            // one query, and two concurrent deploys must not each see the
            // same free memory and both take it.
            let _admission_guard = self.lifecycle_lock.acquire(LockKey::Admission).await;
            self.check_admission(&project, Some(env.id)).await
        };
        match admitted {
            Ok(Admission::Fits) => {}
            // Doesn't fit right now, but could later. Drop the row created
            // above: nothing outside this lock has seen it, and leaving a
            // `Building` row behind for a deploy that hasn't started would
            // show the branch as deploying while it sits in a queue.
            Ok(Admission::Queue) => {
                let _ = EnvironmentStore::delete(&self.store, env.id).await;
                return self
                    .queue_or_report(project_id, &branch, operator, admission)
                    .await;
            }
            // Cannot ever fit on this host, so queueing would mean waiting
            // forever. Recorded against the row so it is visible, then
            // failed.
            Err(err) => {
                let detail = match &err {
                    CpError::InsufficientCapacity(detail) => detail.clone(),
                    other => other.to_string(),
                };
                let err = CpError::InsufficientCapacity(format!("branch `{branch}` of {detail}"));
                self.record_deploy_failure(&mut env, operator.as_ref(), &err)
                    .await;
                return Err(err);
            }
        }

        // 3. Build the image.
        //
        // `[build].context` (e.g. a monorepo subdirectory like `backend/`)
        // was parsed from `oxid.toml` and persisted, but never actually
        // consulted here — every build used the whole repo checkout as its
        // context regardless. Found while wiring `docker-compose.yml`
        // support, whose `build.context`/`build.dockerfile` pair only makes
        // sense if `dockerfile` really is resolved relative to `context`.
        let image = image_name(&project, &branch);

        // Copy the checked-out context aside before letting go of the git
        // lock. The tar Docker receives is read *inside* `build`, long after
        // this point, and the next deploy of a sibling branch force-rewrites
        // that same working directory — without a private copy one branch
        // would build another branch's tree, silently and wrongly. The copy
        // costs about what tarring it costs, which the build pays anyway.
        let context = match self
            .capture_build_context(&repo_dir, &project.config.build.context, env.id)
            .await
        {
            Ok(context) => context,
            Err(err) => {
                drop(git_guard);
                self.record_deploy_failure(&mut env, operator.as_ref(), &err)
                    .await;
                return Err(err);
            }
        };
        drop(git_guard);

        let dockerfile = project
            .config
            .build
            .dockerfile
            .clone()
            .unwrap_or_else(|| "Dockerfile".to_owned());

        // A repository with no Dockerfile used to be refused outright, which
        // asked every team wanting preview environments to become Docker
        // authors first. If the context has none, work out what the project
        // is and write one — into the private copy, never into the
        // developer's checkout, so nothing appears in their `git status` and
        // committing their own Dockerfile silently takes over from the next
        // deploy onward.
        let detected = Self::materialise_dockerfile(context.path(), &dockerfile);
        if let Some(stack) = &detected {
            tracing::info!(
                branch = %branch,
                stack = %stack.label(),
                confidence = ?stack.confidence,
                evidence = ?stack.evidence,
                "no Dockerfile in the build context; generated one from the detected stack"
            );
        }

        let build = BuildSpec {
            context: context.path().to_owned(),
            dockerfile,
            image: image.clone(),
        };
        let build_report = match self.oci.build(&build).await {
            Ok(report) => report,
            Err(err) => {
                let err = CpError::from(err);
                self.record_deploy_failure(&mut env, operator.as_ref(), &err)
                    .await;
                return Err(err);
            }
        };

        // 4-7: resolve secrets, run the container, run `on_start` hooks,
        // wait for it to be ready, then cut over from `previous` (if any)
        // and activate. Everything from here on can fail (a bad secret, a
        // Docker error, a failing hook, a readiness timeout) — but
        // `previous`, if any, is never touched until the new instance is
        // confirmed ready, so a failed redeploy leaves the branch exactly as
        // reachable as it was before the redeploy started. Leaving the new
        // row stuck as `Building` on error would brick the branch
        // permanently otherwise (`Building` cannot transition to `Destroy`),
        // see regression test
        // `failed_deploy_does_not_permanently_block_branch`.
        let dependencies = match self
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
            Ok(dependency_lines) => dependency_lines,
            Err(err) => {
                self.record_deploy_failure(&mut env, operator.as_ref(), &err)
                    .await;
                return Err(err);
            }
        };

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
        Ok(DeployOutcome::Deployed(
            env,
            DeployReport {
                build: build_report,
                dependencies,
            },
        ))
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
        let (env, _report) = self
            .rollback_with_operator(project_id, branch, to_sha, None)
            .await?;
        Ok(env)
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
    ) -> Result<(Environment, DeployReport), CpError> {
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
            .deploy_at(
                project_id,
                branch,
                Some(target_sha),
                operator,
                AdmissionMode::Bypass,
            )
            .await?
        {
            DeployOutcome::Deployed(env, report) => Ok((env, report)),
            DeployOutcome::Queued { .. } => {
                unreachable!("admission control is off, so this never queues")
            }
        }
    }
}

/// A private copy of a build context, removed when the deploy is done with
/// it. Cleanup is best-effort in `Drop`: a leftover directory under the
/// cache costs disk, while failing a deploy that already succeeded over a
/// failed `remove_dir_all` would cost the deploy.
struct BuildContext {
    path: PathBuf,
}

impl BuildContext {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for BuildContext {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.path.display(), error = %e, "could not remove build context copy");
        }
    }
}

/// Recursive copy that preserves symlinks as symlinks.
/// Describes a build context for stack detection. Same contract as the
/// registration-time reader in `adapter::config`: root entries only, and
/// only the manifests the domain asks to see.
fn read_context_manifest(context: &Path) -> RepoManifest {
    let mut manifest = RepoManifest::default();
    let Ok(entries) = std::fs::read_dir(context) else {
        return manifest;
    };
    for entry in entries.flatten() {
        manifest
            .entries
            .push(entry.file_name().to_string_lossy().into_owned());
    }
    for name in RepoManifest::files_worth_reading() {
        if manifest.entries.iter().any(|e| e == name)
            && let Ok(body) = std::fs::read_to_string(context.join(name))
        {
            manifest.files.insert((*name).to_owned(), body);
        }
    }
    manifest
}

fn copy_tree(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        // `symlink_metadata` does not dereference, so a dangling link is
        // just an entry to recreate rather than an error.
        let meta = std::fs::symlink_metadata(&from)?;
        if meta.is_symlink() {
            let dest = std::fs::read_link(&from)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(dest, &to)?;
            #[cfg(not(unix))]
            let _ = dest;
        } else if meta.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
