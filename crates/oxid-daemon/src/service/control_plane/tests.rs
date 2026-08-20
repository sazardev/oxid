use super::*;
use crate::adapter::store::SqliteStore;
use oxid_core::{
    AuditFilter, Branch, BranchName, BuildSpec, ContainerPort, ContainerSpec, ContainerStatus,
    EnvVarScope, Environment, EnvironmentState, EnvironmentStore, GitError, GitPort, HostCapacity,
    LogStream, OciError, OffsetDateTime, PoolError, PoolKind, ProjectId, ProjectStore, RepoUrl,
    StateTransition,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

#[derive(Debug, Clone, Default)]
struct FakeGit;

impl GitPort for FakeGit {
    async fn remote_url(&self, repo_dir: &Path) -> Result<RepoUrl, GitError> {
        let _ = repo_dir;
        RepoUrl::parse("https://github.com/org/app.git")
            .map_err(|e| GitError::Failure(e.to_string()))
    }
    async fn ensure_repo(
        &self,
        _url: &RepoUrl,
        _token: Option<&str>,
        cache_dir: &Path,
    ) -> Result<PathBuf, GitError> {
        Ok(cache_dir.join("app"))
    }
    async fn resolve_branch_head(
        &self,
        _repo_dir: &Path,
        branch: &BranchName,
    ) -> Result<oxid_core::CommitRef, GitError> {
        Ok(oxid_core::CommitRef {
            branch: branch.clone(),
            sha: SHA.to_owned(),
        })
    }
    async fn checkout_commit(&self, _repo_dir: &Path, _sha: &str) -> Result<(), GitError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
struct FakeOci {
    calls: Arc<Mutex<Vec<String>>>,
    /// When > 0, `run` fails and decrements this instead of succeeding.
    fail_run_times: Arc<Mutex<u32>>,
    /// Per-container overrides for `container_status`; anything not
    /// listed here defaults to `Running`.
    container_statuses: Arc<Mutex<std::collections::HashMap<String, ContainerStatus>>>,
    host_capacity: Arc<Mutex<HostCapacity>>,
    /// Docker networks `ensure_network`/`network_exists` believe exist.
    network_exists: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Whether `ensure_traefik` believes its container is already up.
    traefik_running: Arc<Mutex<bool>>,
}

impl ContainerPort for FakeOci {
    async fn build(&self, spec: &BuildSpec) -> Result<(), OciError> {
        self.calls.lock().unwrap().push(format!(
            "build:{}:context={}:dockerfile={}",
            spec.image,
            spec.context.display(),
            spec.dockerfile
        ));
        Ok(())
    }
    async fn run(&self, spec: &ContainerSpec) -> Result<Option<u16>, OciError> {
        self.calls.lock().unwrap().push(format!(
            "run:{}:env={:?}:mem={:?}:cpu={:?}",
            spec.name, spec.env, spec.memory_limit_mb, spec.cpu_limit_millicores
        ));
        let mut remaining = self.fail_run_times.lock().unwrap();
        if *remaining > 0 {
            *remaining -= 1;
            return Err(OciError::Failure("simulated transient failure".to_owned()));
        }
        Ok(spec.network.is_none().then_some(65535))
    }
    async fn published_port(
        &self,
        name: &str,
        _container_port: u16,
    ) -> Result<Option<u16>, OciError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("published_port:{name}"));
        Ok(Some(65535))
    }
    async fn start(&self, name: &str) -> Result<(), OciError> {
        self.calls.lock().unwrap().push(format!("start:{name}"));
        Ok(())
    }
    async fn pause(&self, name: &str) -> Result<(), OciError> {
        self.calls.lock().unwrap().push(format!("pause:{name}"));
        Ok(())
    }
    async fn unpause(&self, name: &str) -> Result<(), OciError> {
        self.calls.lock().unwrap().push(format!("unpause:{name}"));
        Ok(())
    }
    async fn stop(&self, name: &str) -> Result<(), OciError> {
        self.calls.lock().unwrap().push(format!("stop:{name}"));
        Ok(())
    }
    async fn remove(&self, name: &str) -> Result<(), OciError> {
        self.calls.lock().unwrap().push(format!("remove:{name}"));
        Ok(())
    }
    async fn remove_image(&self, image: &str) -> Result<(), OciError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("remove_image:{image}"));
        Ok(())
    }
    async fn logs(&self, name: &str) -> Result<String, OciError> {
        self.calls.lock().unwrap().push(format!("logs:{name}"));
        Ok("build log".to_owned())
    }
    async fn stream_logs(&self, name: &str) -> Result<LogStream, OciError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("stream_logs:{name}"));
        Ok(Box::pin(futures_util::stream::iter(vec![Ok(
            "build log".to_owned()
        )])))
    }
    async fn exec(&self, name: &str, command: &str) -> Result<(), OciError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("exec:{name}:{command}"));
        Ok(())
    }
    async fn container_status(&self, name: &str) -> Result<ContainerStatus, OciError> {
        Ok(self
            .container_statuses
            .lock()
            .unwrap()
            .get(name)
            .copied()
            .unwrap_or(ContainerStatus::Running))
    }
    async fn host_capacity(&self) -> Result<HostCapacity, OciError> {
        Ok(*self.host_capacity.lock().unwrap())
    }
    async fn network_exists(&self, name: &str) -> Result<bool, OciError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("network_exists:{name}"));
        Ok(self.network_exists.lock().unwrap().contains(name))
    }
    async fn ensure_network(&self, name: &str) -> Result<oxid_core::NetworkStatus, OciError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("ensure_network:{name}"));
        if self.network_exists.lock().unwrap().insert(name.to_owned()) {
            Ok(oxid_core::NetworkStatus::Created)
        } else {
            Ok(oxid_core::NetworkStatus::AlreadyExisted)
        }
    }
    async fn ensure_traefik(
        &self,
        spec: oxid_core::TraefikSpec,
    ) -> Result<oxid_core::TraefikStatus, OciError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("ensure_traefik:{}", spec.container_name));
        let mut running = self.traefik_running.lock().unwrap();
        // Keep `container_status(&spec.container_name)` (used by
        // `infra_status`, which never calls `ensure_traefik` itself) in
        // sync with what this fake believes it just created/started.
        self.container_statuses
            .lock()
            .unwrap()
            .insert(spec.container_name.clone(), ContainerStatus::Running);
        if *running {
            Ok(oxid_core::TraefikStatus::AlreadyRunning)
        } else {
            *running = true;
            Ok(oxid_core::TraefikStatus::Created)
        }
    }
    async fn self_wiring_status(
        &self,
        _network: &str,
    ) -> Result<oxid_core::SelfWiringStatus, OciError> {
        Ok(oxid_core::SelfWiringStatus::NotContainerized)
    }
}

async fn store() -> SqliteStore {
    SqliteStore::open_in_memory().await.unwrap()
}

/// A `GitPort` whose `resolve_branch_head` returns a fresh, incrementing
/// sha on every call — unlike `FakeGit`'s fixed `SHA`, needed to tell
/// apart successive deploys of the same branch by commit (rollback
/// tests).
#[derive(Clone, Default)]
struct SequentialGit(Arc<std::sync::atomic::AtomicU32>);

impl GitPort for SequentialGit {
    async fn remote_url(&self, repo_dir: &Path) -> Result<RepoUrl, GitError> {
        let _ = repo_dir;
        RepoUrl::parse("https://github.com/org/app.git")
            .map_err(|e| GitError::Failure(e.to_string()))
    }
    async fn ensure_repo(
        &self,
        _url: &RepoUrl,
        _token: Option<&str>,
        cache_dir: &Path,
    ) -> Result<PathBuf, GitError> {
        Ok(cache_dir.join("app"))
    }
    async fn resolve_branch_head(
        &self,
        _repo_dir: &Path,
        branch: &BranchName,
    ) -> Result<oxid_core::CommitRef, GitError> {
        let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(oxid_core::CommitRef {
            branch: branch.clone(),
            sha: format!("{n:040}"),
        })
    }
    async fn checkout_commit(&self, _repo_dir: &Path, _sha: &str) -> Result<(), GitError> {
        Ok(())
    }
}

fn repo_dir_with_config() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("oxid.toml"),
        r#"
[project]
name = "app"

[routing]
base_domain = "app.local.dev"
port = 8080
"#,
    )
    .unwrap();
    dir
}

async fn cp(oci: FakeOci) -> ControlPlane<FakeGit, FakeOci> {
    let cache = tempfile::tempdir().unwrap();
    // FakeOci doesn't simulate a real listening socket, so the
    // zero-downtime readiness gate (which does a real TCP connect)
    // would otherwise time out on every deploy — see
    // `ControlPlane::with_readiness_check`'s doc comment.
    ControlPlane::new(store().await, FakeGit, oci, cache.path().to_owned())
        .with_readiness_check(false)
}

#[tokio::test]
async fn infra_status_requires_traefik_configured() {
    // No `with_traefik(...)` call — there's no network name to check
    // against, so this must be a clear error, not a guess.
    let cp = cp(FakeOci::default()).await;
    let err = cp.infra_status().await.unwrap_err();
    assert!(matches!(err, CpError::NotFound(_)), "{err:?}");
}

#[tokio::test]
async fn infra_bootstrap_requires_traefik_configured() {
    let cp = cp(FakeOci::default()).await;
    let err = cp.infra_bootstrap().await.unwrap_err();
    assert!(matches!(err, CpError::NotFound(_)), "{err:?}");
}

#[tokio::test]
async fn infra_status_reports_missing_network_and_traefik_before_bootstrap() {
    let oci = FakeOci::default();
    let cp = cp(oci).await.with_traefik("oxid-net", "http://daemon");

    let status = cp.infra_status().await.unwrap();
    assert_eq!(status.network, "oxid-net");
    assert!(!status.network_exists);
    assert_eq!(status.traefik_status, ContainerStatus::Running);
    // `FakeOci::container_status` defaults every unlisted name to
    // `Running` (see its impl above) — this asserts the current
    // baseline so a future change to that default doesn't silently
    // break this test's assumptions; the "Missing" case is exercised
    // separately below.
    assert!(!status.next_steps.is_empty());
}

#[tokio::test]
async fn infra_bootstrap_is_idempotent() {
    let oci = FakeOci::default();
    oci.container_statuses
        .lock()
        .unwrap()
        .insert("oxid-traefik".to_owned(), ContainerStatus::Missing);
    let cp = cp(oci.clone())
        .await
        .with_traefik("oxid-net", "http://daemon");

    let first = cp.infra_bootstrap().await.unwrap();
    assert!(first.network_exists);
    assert_eq!(first.traefik_status, ContainerStatus::Running);
    assert!(
        first
            .next_steps
            .iter()
            .any(|s| s.contains("wake-on-request"))
    );

    // Running it again must not fail or duplicate anything — both
    // `ensure_network`/`ensure_traefik` calls should now be pure no-ops.
    let second = cp.infra_bootstrap().await.unwrap();
    assert_eq!(second.network, first.network);
    assert!(second.network_exists);
    assert_eq!(second.traefik_status, ContainerStatus::Running);

    let calls = oci.calls.lock().unwrap();
    assert!(
        calls
            .iter()
            .filter(|c| c.starts_with("ensure_network"))
            .count()
            >= 2,
        "{calls:?}"
    );
}

/// A project declaring one `redis` dependency — deliberately no
/// `postgres` one, so these tests never need a real Postgres instance;
/// that path is covered separately by `postgres_pool.rs`'s `#[ignore]`d
/// integration test.
fn repo_dir_with_redis_dependency() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("oxid.toml"),
        r#"
[project]
name = "app"

[routing]
base_domain = "app.local.dev"
port = 8080

[dependencies.cache]
type = "redis"
shared_instance = "local-redis"
inject_url_as = "REDIS_URL"
"#,
    )
    .unwrap();
    dir
}

#[tokio::test]
async fn register_and_deploy_happy_path() {
    let repo = repo_dir_with_config();
    let cp = cp(FakeOci::default()).await;

    let project = cp.register_project(repo.path()).await.unwrap();
    assert_eq!(project.name, "app");
    assert_eq!(project.repo_url.as_str(), "https://github.com/org/app.git");

    // Idempotent registration.
    let again = cp.register_project(repo.path()).await.unwrap();
    assert_eq!(again.id, project.id);
    assert_eq!(cp.list_projects().await.unwrap().len(), 1);

    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    assert_eq!(env.state, EnvironmentState::Running);
    assert_eq!(env.url, "feature-a.app.local.dev");
    assert_eq!(cp.list_environments(project.id).await.unwrap().len(), 1);
}

/// Regression test for a real race found by firing ten concurrent `oxid
/// up` at a project that had never been registered before: the
/// check-then-act between `find_project_by_repo` and `ProjectStore::
/// create` isn't atomic, so every concurrent first-time caller could
/// pass the "does it exist?" check before any of them committed,
/// leaving all but one to blow up with a raw `UNIQUE constraint failed`
/// instead of the idempotent behavior `register_project` documents.
#[tokio::test]
async fn concurrent_first_registration_is_idempotent() {
    let repo = repo_dir_with_config();
    let cp = cp(FakeOci::default()).await;

    let handles: Vec<_> = (0..10)
        .map(|_| {
            let cp = cp.clone();
            let path = repo.path().to_owned();
            tokio::spawn(async move { cp.register_project(&path).await })
        })
        .collect();

    let mut ids = std::collections::HashSet::new();
    for handle in handles {
        ids.insert(handle.await.unwrap().unwrap().id);
    }
    assert_eq!(ids.len(), 1, "every call must resolve to the same project");
    assert_eq!(cp.list_projects().await.unwrap().len(), 1);
}

#[tokio::test]
async fn deploy_records_oci_operations() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path()).await.unwrap();

    cp.deploy(project.id, BranchName::parse("feature-b").unwrap())
        .await
        .unwrap();

    let calls = oci.calls.lock().unwrap();
    assert!(calls.iter().any(|c| c.starts_with("build:")), "{calls:?}");
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("run:oxid-app-feature-b")),
        "{calls:?}"
    );
}

/// Regression test: `[build].context` was parsed from `oxid.toml` and
/// persisted, but never actually consulted when building the image —
/// every build used the whole repo checkout regardless, silently
/// ignoring a monorepo-style subdirectory context. Found while wiring
/// `docker-compose.yml` support, whose `build.context`/`build.dockerfile`
/// pair only makes sense if `dockerfile` is resolved relative to
/// `context`, not the repo root.
#[tokio::test]
async fn deploy_honors_a_non_default_build_context() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("oxid.toml"),
        r#"
[project]
name = "app"

[build]
context = "backend"
dockerfile = "Dockerfile.prod"

[routing]
base_domain = "app.local.dev"
port = 8080
"#,
    )
    .unwrap();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(dir.path()).await.unwrap();

    cp.deploy(project.id, BranchName::parse("main").unwrap())
        .await
        .unwrap();

    let calls = oci.calls.lock().unwrap();
    let build_call = calls
        .iter()
        .find(|c| c.starts_with("build:"))
        .expect("a build call was made");
    // `FakeGit::ensure_repo` always resolves to `<cache_dir>/app`; the
    // context must be that path joined with the configured `backend`
    // subdirectory, and the dockerfile must be resolved relative to it.
    assert!(
        build_call.ends_with("/app/backend:dockerfile=Dockerfile.prod"),
        "{build_call}"
    );
}

#[tokio::test]
async fn deploy_fails_clearly_when_dependency_is_unconfigured() {
    let repo = repo_dir_with_redis_dependency();
    let cp = cp(FakeOci::default()).await; // no `with_resource_pools` call
    let project = cp.register_project(repo.path()).await.unwrap();

    let err = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, CpError::Pool(PoolError::NotConfigured(m)) if m.contains("OXID_REDIS_URL")),
        "{err:?}"
    );
}

async fn redis_lease_for(
    cp: &ControlPlane<FakeGit, FakeOci>,
    project_id: ProjectId,
    branch: &str,
) -> Option<String> {
    cp.store
        .find_resource_lease(
            project_id,
            &BranchName::parse(branch).unwrap(),
            PoolKind::Redis,
            "local-redis",
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn deploy_injects_a_distinct_redis_index_per_branch_and_reuses_on_redeploy() {
    let repo = repo_dir_with_redis_dependency();
    let cache = tempfile::tempdir().unwrap();
    let cp = ControlPlane::new(
        store().await,
        FakeGit,
        FakeOci::default(),
        cache.path().to_owned(),
    )
    .with_resource_pools(None, Some("redis://cache:6379".to_owned()), 16)
    .with_readiness_check(false);
    let project = cp.register_project(repo.path()).await.unwrap();

    let env_a = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    let env_b = cp
        .deploy(project.id, BranchName::parse("feature-b").unwrap())
        .await
        .unwrap();
    assert_ne!(env_a.id, env_b.id);

    let index_a = redis_lease_for(&cp, project.id, "feature-a").await.unwrap();
    let index_b = redis_lease_for(&cp, project.id, "feature-b").await.unwrap();
    assert_ne!(index_a, index_b, "each branch must get its own index");

    // Redeploying feature-a must reuse the same index, not lease a new
    // one (which would eventually exhaust the pool for no reason).
    cp.deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    assert_eq!(
        redis_lease_for(&cp, project.id, "feature-a").await,
        Some(index_a)
    );
}

#[tokio::test]
async fn destroy_releases_the_redis_index_for_reuse() {
    let repo = repo_dir_with_redis_dependency();
    let cache = tempfile::tempdir().unwrap();
    let cp = ControlPlane::new(
        store().await,
        FakeGit,
        FakeOci::default(),
        cache.path().to_owned(),
    )
    .with_resource_pools(None, Some("redis://cache:6379".to_owned()), 1)
    .with_readiness_check(false);
    let project = cp.register_project(repo.path()).await.unwrap();

    let env_a = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    // Pool capacity is 1: a second branch must fail while feature-a
    // holds the only slot.
    let err = cp
        .deploy(project.id, BranchName::parse("feature-b").unwrap())
        .await
        .unwrap_err();
    assert!(
        matches!(err, CpError::Pool(PoolError::Failure(_))),
        "{err:?}"
    );

    cp.destroy(env_a.id, false).await.unwrap();

    // Now that feature-a released its slot, feature-b can have it.
    cp.deploy(project.id, BranchName::parse("feature-b").unwrap())
        .await
        .unwrap();
}

/// Regression test: redeploying a branch that's already live (e.g. a
/// webhook firing on a second push) must tear down the previous
/// container first instead of leaving Docker to reject a duplicate
/// container name, and must mark the old row Destroyed rather than
/// leaving two "live-looking" rows around.
#[tokio::test]
async fn redeploying_a_live_branch_replaces_the_previous_environment() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path()).await.unwrap();

    let first = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    let second = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    assert_ne!(first.id, second.id);
    assert_eq!(second.state, EnvironmentState::Running);

    let old = EnvironmentStore::get(&cp.store, first.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.state, EnvironmentState::Destroyed);

    {
        // Zero-downtime cutover: the new instance (`-2`) is built and
        // started fully before the previous one (`-1`) is ever removed
        // — the reverse of the old "destroy first, build second" order,
        // which always had a gap where the branch was unreachable.
        let calls = oci.calls.lock().unwrap();
        let run_new = calls
                .iter()
                .position(|c| c == "run:oxid-app-feature-a-2:env={\"OXID_BRANCH\": \"feature-a\", \"OXID_ENV_URL\": \"feature-a.app.local.dev\"}:mem=None:cpu=None")
                .expect("new instance must have been run");
        // The *last* removal of `-1` is the cutover teardown — its
        // *first* occurrence is just the defensive pre-run cleanup its
        // own deploy already did for itself.
        let remove_old = calls
            .iter()
            .rposition(|c| c == "remove:oxid-app-feature-a-1")
            .expect("previous container must eventually be removed");
        assert!(
            run_new < remove_old,
            "previous container must not be removed until the new one is up: {calls:?}"
        );
    }
    // Exactly one live environment remains for the branch.
    let envs = cp.list_environments(project.id).await.unwrap();
    assert_eq!(
        envs.iter()
            .filter(|e| e.state != EnvironmentState::Destroyed)
            .count(),
        1
    );
}

#[tokio::test]
async fn rollback_without_to_sha_redeploys_the_immediately_prior_commit() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cache = tempfile::tempdir().unwrap();
    let cp = ControlPlane::new(
        store().await,
        SequentialGit::default(),
        oci,
        cache.path().to_owned(),
    )
    .with_readiness_check(false);
    let project = cp.register_project(repo.path()).await.unwrap();
    let branch = BranchName::parse("main").unwrap();

    let first = cp.deploy(project.id, branch.clone()).await.unwrap();
    let second = cp.deploy(project.id, branch.clone()).await.unwrap();
    assert_ne!(first.branch.commit_sha, second.branch.commit_sha);

    let rolled_back = cp.rollback(project.id, branch, None).await.unwrap();
    assert_eq!(rolled_back.branch.commit_sha, first.branch.commit_sha);
    assert_eq!(rolled_back.state, EnvironmentState::Running);
}

#[tokio::test]
async fn rollback_with_explicit_to_sha_uses_that_commit() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cache = tempfile::tempdir().unwrap();
    let cp = ControlPlane::new(
        store().await,
        SequentialGit::default(),
        oci,
        cache.path().to_owned(),
    )
    .with_readiness_check(false);
    let project = cp.register_project(repo.path()).await.unwrap();
    let branch = BranchName::parse("main").unwrap();

    let first = cp.deploy(project.id, branch.clone()).await.unwrap();
    cp.deploy(project.id, branch.clone()).await.unwrap();
    cp.deploy(project.id, branch.clone()).await.unwrap();

    let rolled_back = cp
        .rollback(project.id, branch, Some(first.branch.commit_sha.clone()))
        .await
        .unwrap();
    assert_eq!(rolled_back.branch.commit_sha, first.branch.commit_sha);
}

#[tokio::test]
async fn rollback_rejects_a_sha_not_in_the_branchs_history() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cache = tempfile::tempdir().unwrap();
    let cp = ControlPlane::new(
        store().await,
        SequentialGit::default(),
        oci,
        cache.path().to_owned(),
    )
    .with_readiness_check(false);
    let project = cp.register_project(repo.path()).await.unwrap();
    let branch = BranchName::parse("main").unwrap();
    cp.deploy(project.id, branch.clone()).await.unwrap();

    let err = cp
        .rollback(project.id, branch, Some("not-a-real-sha".to_owned()))
        .await
        .unwrap_err();
    assert!(matches!(err, CpError::NotFound(_)));
}

#[tokio::test]
async fn rollback_with_no_prior_deploy_is_not_found() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cache = tempfile::tempdir().unwrap();
    let cp = ControlPlane::new(
        store().await,
        SequentialGit::default(),
        oci,
        cache.path().to_owned(),
    )
    .with_readiness_check(false);
    let project = cp.register_project(repo.path()).await.unwrap();
    let branch = BranchName::parse("main").unwrap();
    cp.deploy(project.id, branch.clone()).await.unwrap();

    let err = cp.rollback(project.id, branch, None).await.unwrap_err();
    assert!(matches!(err, CpError::NotFound(_)));
}

/// Regression test for a real bricking bug: a transient failure in
/// `run()` (Docker error, bad secret, failing `on_start` hook) happening
/// *after* the `Environment` row was persisted as `Building` used to
/// leave it there forever, since `Building` cannot transition to
/// `Destroy` — every subsequent `oxid up` of that branch failed with
/// "transition `Destroy` is not allowed from `Building`" instead of
/// retrying. Found by deploying, having a container-name conflict fail
/// the `run` step, then deploying again.
#[tokio::test]
async fn failed_deploy_does_not_permanently_block_branch() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    *oci.fail_run_times.lock().unwrap() = 1;
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path()).await.unwrap();

    let err = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, CpError::Oci(_)), "{err:?}");

    let envs = cp.list_environments(project.id).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(envs[0].state, EnvironmentState::Destroyed);

    // The audit trail must carry the *real* error (e.g. "port already
    // allocated"), not a blank `detail` — found live when a real
    // deploy failure showed up in the dashboard with no way to tell
    // what actually went wrong.
    let events = cp
        .audit_events_for(envs[0].id, &AuditFilter::default())
        .await
        .unwrap();
    let failed = events
        .iter()
        .find(|e| e.kind == StateTransition::BuildFailed)
        .expect("a BuildFailed audit event");
    assert_eq!(failed.detail.as_deref(), Some(err.to_string().as_str()));

    // The retry must succeed instead of hitting "Destroy not allowed
    // from Building".
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    assert_eq!(env.state, EnvironmentState::Running);
}

/// The whole point of building new-before-old for a redeploy: if the
/// new instance never comes up, the previous one — which has been
/// serving traffic this entire time — must be left running exactly as
/// it was, not torn down. Explicit user requirement ("siempre
/// levantando algo para no tener fallas"): a bad push must never take
/// an already-live branch down with it.
#[tokio::test]
async fn failed_redeploy_leaves_the_previous_instance_untouched() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path()).await.unwrap();

    let first = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    assert_eq!(first.state, EnvironmentState::Running);

    *oci.fail_run_times.lock().unwrap() = 1;
    let err = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, CpError::Oci(_)), "{err:?}");

    // The previous environment must still be exactly as it was.
    let still_live = EnvironmentStore::get(&cp.store, first.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still_live.state, EnvironmentState::Running);

    // Its container must have been removed exactly once — the
    // defensive pre-run cleanup its *own* first deploy already did for
    // itself — and never a second time because of the failed redeploy.
    {
        let calls = oci.calls.lock().unwrap();
        let removes_of_previous = calls
            .iter()
            .filter(|c| *c == "remove:oxid-app-feature-a-1")
            .count();
        assert_eq!(
            removes_of_previous, 1,
            "previous container must survive a failed redeploy untouched: {calls:?}"
        );
    }

    // A subsequent, successful redeploy must still work normally.
    let second = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    assert_eq!(second.state, EnvironmentState::Running);
    let old = EnvironmentStore::get(&cp.store, first.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.state, EnvironmentState::Destroyed);
}

/// Regression test for a real race found by firing ten concurrent `oxid
/// up` calls at the same brand-new branch: without serializing
/// `deploy()`, they raced to create their own `Environment` row before
/// any of them found out only one could win the container name, so the
/// row left standing (highest id) was not necessarily the one whose
/// container actually ended up running. `deploy_lock` forces them into a
/// sequence instead, so exactly one row should end up `Running` — and it
/// should be the *last* deploy to actually run, not an arbitrary loser.
#[tokio::test]
async fn concurrent_deploys_of_the_same_branch_leave_a_consistent_state() {
    let repo = repo_dir_with_config();
    let cp = cp(FakeOci::default()).await;
    let project = cp.register_project(repo.path()).await.unwrap();

    let handles: Vec<_> = (0..10)
        .map(|_| {
            let cp = cp.clone();
            let project_id = project.id;
            tokio::spawn(async move {
                cp.deploy(project_id, BranchName::parse("feature-a").unwrap())
                    .await
            })
        })
        .collect();

    let mut successes = 0;
    for handle in handles {
        if handle.await.unwrap().is_ok() {
            successes += 1;
        }
    }
    assert_eq!(
        successes, 10,
        "the lock should let every deploy succeed in turn"
    );

    let envs = cp.list_environments(project.id).await.unwrap();
    let running: Vec<_> = envs
        .iter()
        .filter(|e| e.state == EnvironmentState::Running)
        .collect();
    assert_eq!(
        running.len(),
        1,
        "exactly one environment must be left Running: {envs:?}"
    );
    // It must be the most recent row — not a stale one left standing by
    // a race — since each deploy tears down the previous live one.
    let max_id = envs.iter().map(|e| e.id.0).max().unwrap();
    assert_eq!(running[0].id.0, max_id);
}

#[tokio::test]
async fn pause_wake_and_logs() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path()).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    cp.pause(env.id).await.unwrap();
    let paused = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paused.state, EnvironmentState::Paused);

    cp.wake(env.id).await.unwrap();
    let woken = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(woken.state, EnvironmentState::Running);

    let logs = cp.logs(env.id).await.unwrap();
    assert_eq!(logs, "build log");

    let mut stream = cp.stream_logs(env.id).await.unwrap();
    let first = futures_util::StreamExt::next(&mut stream).await;
    assert_eq!(first, Some(Ok("build log".to_owned())));
}

#[tokio::test]
async fn deploy_applies_daemon_default_resource_limits_when_project_sets_none() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone())
        .await
        .with_resource_defaults(Some(512), Some(1000));
    let project = cp.register_project(repo.path()).await.unwrap();
    cp.deploy(project.id, BranchName::parse("main").unwrap())
        .await
        .unwrap();

    let calls = oci.calls.lock().unwrap();
    assert!(
        calls.iter().any(|c| c.starts_with("run:")
            && c.contains("mem=Some(512)")
            && c.contains("cpu=Some(1000)")),
        "{calls:?}"
    );
}

#[tokio::test]
async fn deploy_lets_a_projects_own_resource_limits_win_over_the_daemon_default() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("oxid.toml"),
        r#"
[project]
name = "app"

[routing]
base_domain = "app.local.dev"
port = 8080

[build]
memory_limit_mb = 128
cpu_limit_millicores = 250
"#,
    )
    .unwrap();
    let oci = FakeOci::default();
    let cp = cp(oci.clone())
        .await
        .with_resource_defaults(Some(512), Some(1000));
    let project = cp.register_project(dir.path()).await.unwrap();
    cp.deploy(project.id, BranchName::parse("main").unwrap())
        .await
        .unwrap();

    let calls = oci.calls.lock().unwrap();
    assert!(
        calls.iter().any(|c| c.starts_with("run:")
            && c.contains("mem=Some(128)")
            && c.contains("cpu=Some(250)")),
        "{calls:?}"
    );
}

#[tokio::test]
async fn destroy_stops_removes_and_transitions() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path()).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    cp.destroy(env.id, false).await.unwrap();
    let destroyed = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(destroyed.state, EnvironmentState::Destroyed);

    let calls = oci.calls.lock().unwrap();
    assert!(calls.iter().any(|c| c.starts_with("stop:")), "{calls:?}");
    assert!(calls.iter().any(|c| c.starts_with("remove:")), "{calls:?}");
    assert!(
        calls.iter().any(|c| c.starts_with("remove_image:")),
        "destroy must also remove the branch's image, not just its container: {calls:?}"
    );
}

#[tokio::test]
async fn destroy_keeps_branch_secrets_by_default() {
    let repo = repo_dir_with_config();
    let cp = cp(FakeOci::default()).await;
    let project = cp.register_project(repo.path()).await.unwrap();
    let branch = BranchName::parse("feature-a").unwrap();
    let env = cp.deploy(project.id, branch.clone()).await.unwrap();

    cp.set_secret(
        Some(project.id),
        Some(&branch),
        "DB_PASS",
        EnvVarScope::Branch,
        "keep-me",
    )
    .await
    .unwrap();

    cp.destroy(env.id, false).await.unwrap();

    let secrets = cp
        .list_secrets(Some(project.id), Some(&branch))
        .await
        .unwrap();
    assert!(
        secrets
            .iter()
            .any(|(n, s)| n == "DB_PASS" && *s == EnvVarScope::Branch),
        "a plain `down` must not delete branch secrets: {secrets:?}"
    );
}

#[tokio::test]
async fn destroy_with_purge_secrets_deletes_only_that_branchs_branch_scope_secrets() {
    let repo = repo_dir_with_config();
    let cp = cp(FakeOci::default()).await;
    let project = cp.register_project(repo.path()).await.unwrap();
    let branch_a = BranchName::parse("feature-a").unwrap();
    let branch_b = BranchName::parse("feature-b").unwrap();
    let env_a = cp.deploy(project.id, branch_a.clone()).await.unwrap();

    cp.set_secret(
        Some(project.id),
        None,
        "SHARED",
        EnvVarScope::Project,
        "project-level",
    )
    .await
    .unwrap();
    cp.set_secret(
        Some(project.id),
        Some(&branch_a),
        "ONLY_A",
        EnvVarScope::Branch,
        "a-secret",
    )
    .await
    .unwrap();
    cp.set_secret(
        Some(project.id),
        Some(&branch_b),
        "ONLY_B",
        EnvVarScope::Branch,
        "b-secret",
    )
    .await
    .unwrap();

    cp.destroy(env_a.id, true).await.unwrap();

    let for_a = cp
        .list_secrets(Some(project.id), Some(&branch_a))
        .await
        .unwrap();
    assert!(
        for_a.iter().all(|(n, _)| n != "ONLY_A"),
        "purge_secrets must delete branch A's own secret: {for_a:?}"
    );
    assert!(
        for_a.iter().any(|(n, _)| n == "SHARED"),
        "purge_secrets must not touch project-scope secrets: {for_a:?}"
    );
    let for_b = cp
        .list_secrets(Some(project.id), Some(&branch_b))
        .await
        .unwrap();
    assert!(
        for_b.iter().any(|(n, _)| n == "ONLY_B"),
        "purge_secrets on branch A must not delete branch B's secret: {for_b:?}"
    );
}

#[tokio::test]
async fn delete_project_destroys_environments_removes_cache_and_row() {
    let repo = repo_dir_with_config();
    let cache = tempfile::tempdir().unwrap();
    let oci = FakeOci::default();
    let cp = ControlPlane::new(store().await, FakeGit, oci.clone(), cache.path().to_owned())
        .with_readiness_check(false);
    let project = cp.register_project(repo.path()).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    // Populate the cache dir the way `ensure_repo` would, so deletion has
    // something real to remove.
    let cache_path = cache
        .path()
        .join(crate::adapter::git::cache_dir_name(&project.repo_url));
    std::fs::create_dir_all(&cache_path).unwrap();
    std::fs::write(cache_path.join("marker"), "x").unwrap();

    cp.delete_project(project.id).await.unwrap();

    assert!(!cache_path.exists(), "git cache must be removed");
    assert!(
        ProjectStore::get(&cp.store, project.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        EnvironmentStore::get(&cp.store, env.id)
            .await
            .unwrap()
            .is_none(),
        "cascade must remove the environment row too"
    );
    let calls = oci.calls.lock().unwrap();
    assert!(
        calls.iter().any(|c| c.starts_with("remove:")),
        "project delete must tear down live containers: {calls:?}"
    );
}

#[tokio::test]
async fn delete_project_unknown_fails() {
    let cp = cp(FakeOci::default()).await;
    let err = cp.delete_project(ProjectId(999)).await.unwrap_err();
    assert!(matches!(err, CpError::NotFound(_)));
}

#[tokio::test]
async fn gc_destroy_also_removes_the_image() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    // `sweep` no-ops entirely without Traefik configured (idle detection
    // needs its heartbeat) — exercise that real path here.
    let cp = cp(oci.clone()).await.with_traefik("net", "http://daemon");
    let project = cp.register_project(repo.path()).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    let now = OffsetDateTime::now_utc();
    touch_env(&cp, env.clone(), now - time::Duration::days(8)).await;
    let summary = cp.sweep(now).await.unwrap();
    assert_eq!(summary.destroyed, 1, "{:?}", summary.errors);

    let calls = oci.calls.lock().unwrap();
    assert!(
        calls.iter().any(|c| c.starts_with("remove_image:")),
        "{calls:?}"
    );
}

#[tokio::test]
async fn find_environment_by_branch_matches_and_misses() {
    let repo = repo_dir_with_config();
    let cp = cp(FakeOci::default()).await;
    let project = cp.register_project(repo.path()).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    let found = cp
        .find_environment_by_branch(project.id, &BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    assert_eq!(found.unwrap().id, env.id);

    let missing = cp
        .find_environment_by_branch(project.id, &BranchName::parse("feature-b").unwrap())
        .await
        .unwrap();
    assert!(missing.is_none());
}

#[tokio::test]
async fn wake_by_url_unpauses_paused_and_starts_hibernating() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path()).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    cp.pause(env.id).await.unwrap();
    let woken = cp.wake_by_url(&env.url).await.unwrap().unwrap();
    assert_eq!(woken.state, EnvironmentState::Running);
    assert!(
        oci.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("unpause:")),
        "{:?}",
        oci.calls
    );

    // Force it to Hibernating directly (bypassing the multi-hour sweep
    // needed to get there naturally) to test the `start` branch of wake.
    let mut hibernating = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    hibernating
        .transition(StateTransition::IdleTimeout, OffsetDateTime::now_utc())
        .unwrap();
    hibernating
        .transition(StateTransition::DeepSleep, OffsetDateTime::now_utc())
        .unwrap();
    EnvironmentStore::update(&cp.store, &hibernating)
        .await
        .unwrap();

    let woken = cp.wake_by_url(&env.url).await.unwrap().unwrap();
    assert_eq!(woken.state, EnvironmentState::Running);
    assert!(
        oci.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("start:")),
        "{:?}",
        oci.calls
    );
}

/// Regression test for a real bug found live: an environment deployed
/// before dynamic host-port assignment existed has `host_port: None`
/// forever, since nothing but `run()` (which only fires on a *new*
/// container) ever learns it — waking it just unpauses the existing
/// container without recreating it. `wake` must opportunistically
/// backfill `host_port` in that case instead of leaving the dashboard
/// showing a dead Traefik-style URL forever after a wake.
#[tokio::test]
async fn wake_backfills_host_port_for_environments_predating_dynamic_ports() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path()).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    assert_eq!(env.host_port, Some(65535));

    // Simulate a row from before this column existed.
    let mut stale = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    stale.host_port = None;
    EnvironmentStore::update(&cp.store, &stale).await.unwrap();

    cp.pause(env.id).await.unwrap();
    cp.wake(env.id).await.unwrap();

    let refreshed = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(refreshed.host_port, Some(65535));
}

#[tokio::test]
async fn wake_by_url_unknown_host_is_none() {
    let cp = cp(FakeOci::default()).await;
    assert!(cp.wake_by_url("nobody.local.dev").await.unwrap().is_none());
}

#[tokio::test]
async fn touch_by_url_refreshes_last_access_and_ignores_unknown() {
    let repo = repo_dir_with_config();
    let cp = cp(FakeOci::default()).await;
    let project = cp.register_project(repo.path()).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    cp.touch_by_url("nobody.local.dev").await.unwrap();

    let before = env.last_accessed_at;
    touch_env(&cp, env.clone(), before - time::Duration::hours(1)).await;
    cp.touch_by_url(&env.url).await.unwrap();
    let touched = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    assert!(touched.last_accessed_at > before - time::Duration::hours(1));
}

#[tokio::test]
async fn deploy_unknown_project_fails() {
    let cp = cp(FakeOci::default()).await;
    let err = cp
        .deploy(ProjectId(999), BranchName::parse("main").unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, CpError::NotFound(_)));
}

async fn touch_env(cp: &ControlPlane<FakeGit, FakeOci>, mut env: Environment, at: OffsetDateTime) {
    env.touch(at).unwrap();
    EnvironmentStore::update(&cp.store, &env).await.unwrap();
}

/// Regression test for a real bug found live: without Traefik, nothing
/// ever calls [`ControlPlane::touch_by_url`], so `last_accessed_at`
/// stays frozen at creation time forever regardless of real traffic —
/// a woken environment looked exactly as idle as the moment it was
/// created and got auto-paused again on the very next sweep. `sweep`
/// must be a complete no-op in this mode instead of acting on data it
/// knows is meaningless.
#[tokio::test]
async fn sweep_does_nothing_without_traefik_even_when_wildly_idle() {
    let repo = repo_dir_with_config();
    let cp = cp(FakeOci::default()).await; // no `with_traefik(...)` call
    let project = cp.register_project(repo.path()).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    // Idle well past every threshold (pause/hibernate/destroy).
    let now = OffsetDateTime::now_utc();
    touch_env(&cp, env.clone(), now - time::Duration::days(30)).await;

    let summary = cp.sweep(now).await.unwrap();
    assert_eq!(summary, GcSummary::default());

    let loaded = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.state, EnvironmentState::Running);
}

#[tokio::test]
async fn sweep_keeps_recently_active_environment() {
    let repo = repo_dir_with_config();
    // `sweep` no-ops entirely without Traefik configured (idle detection
    // needs its heartbeat) — exercise that real path here.
    let cp = cp(FakeOci::default())
        .await
        .with_traefik("net", "http://daemon");
    let project = cp.register_project(repo.path()).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    let now = OffsetDateTime::now_utc();
    touch_env(&cp, env.clone(), now - time::Duration::seconds(60)).await;

    let summary = cp.sweep(now).await.unwrap();
    assert_eq!(summary, GcSummary::default());
    let loaded = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.state, EnvironmentState::Running);
}

#[tokio::test]
async fn sweep_pauses_idle_environment() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    // `sweep` no-ops entirely without Traefik configured (idle detection
    // needs its heartbeat) — exercise that real path here.
    let cp = cp(oci.clone()).await.with_traefik("net", "http://daemon");
    let project = cp.register_project(repo.path()).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    // pause_after defaults to 30m.
    let now = OffsetDateTime::now_utc();
    touch_env(&cp, env.clone(), now - time::Duration::minutes(31)).await;

    let summary = cp.sweep(now).await.unwrap();
    assert_eq!(summary.paused, 1);
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);

    let loaded = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.state, EnvironmentState::Paused);
    assert!(
        oci.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("pause:")),
        "{:?}",
        oci.calls
    );
}

/// Regression test for a real race: a GC `sweep` tick and a manual
/// action on the *same* environment both do read-modify-write (fetch,
/// apply a `StateTransition`, persist) with no atomicity between the
/// read and the write. Without `lifecycle_lock` covering both,
/// interleaving them could have one silently overwrite the other's
/// transition with a stale copy. This doesn't assert a specific winner
/// (either legitimately can win) — it asserts the lock actually
/// serializes them: no panic, and the persisted state is always a
/// state genuinely reachable by one of the two actions, never a
/// corrupted/impossible one.
#[tokio::test]
async fn concurrent_sweep_and_manual_destroy_do_not_corrupt_state() {
    let repo = repo_dir_with_config();
    // `sweep` no-ops entirely without Traefik configured (idle detection
    // needs its heartbeat) — exercise that real path here.
    let cp = cp(FakeOci::default())
        .await
        .with_traefik("net", "http://daemon");
    let project = cp.register_project(repo.path()).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    // Idle well past every GC threshold, so `sweep` will try to act on
    // it at the same time a manual `destroy` races in.
    let now = OffsetDateTime::now_utc();
    touch_env(&cp, env.clone(), now - time::Duration::days(8)).await;

    let cp_a = cp.clone();
    let cp_b = cp.clone();
    let env_id = env.id;
    let (sweep_result, destroy_result) = tokio::join!(
        tokio::spawn(async move { cp_a.sweep(now).await }),
        tokio::spawn(async move { cp_b.destroy(env_id, false).await }),
    );
    sweep_result.unwrap().unwrap();
    // Exactly one of the two "destroy" paths can win the state machine;
    // the loser gets a clean `Forbidden`/`Noop` domain error, not a
    // panic or a corrupted row — both are acceptable outcomes here.
    let _ = destroy_result.unwrap();

    let loaded = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.state, EnvironmentState::Destroyed);
}

#[tokio::test]
async fn sweep_hibernates_deeply_idle_paused_environment() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    // `sweep` no-ops entirely without Traefik configured (idle detection
    // needs its heartbeat) — exercise that real path here.
    let cp = cp(oci.clone()).await.with_traefik("net", "http://daemon");
    let project = cp.register_project(repo.path()).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    // First pass: 31m idle suspends it (Running -> Paused).
    let t1 = OffsetDateTime::now_utc();
    touch_env(&cp, env.clone(), t1 - time::Duration::minutes(31)).await;
    cp.sweep(t1).await.unwrap();

    // Second pass: 3h idle (> 4 * pause_after) hibernates it from Paused.
    let t2 = t1 + time::Duration::hours(3);
    let summary = cp.sweep(t2).await.unwrap();
    assert_eq!(summary.hibernated, 1, "{:?}", summary.errors);

    let loaded = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.state, EnvironmentState::Hibernating);
}

#[tokio::test]
async fn sweep_destroys_expired_environment() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    // `sweep` no-ops entirely without Traefik configured (idle detection
    // needs its heartbeat) — exercise that real path here.
    let cp = cp(oci.clone()).await.with_traefik("net", "http://daemon");
    let project = cp.register_project(repo.path()).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    // destroy_after defaults to 7d.
    let now = OffsetDateTime::now_utc();
    touch_env(&cp, env.clone(), now - time::Duration::days(8)).await;

    let summary = cp.sweep(now).await.unwrap();
    assert_eq!(summary.destroyed, 1, "{:?}", summary.errors);

    let loaded = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.state, EnvironmentState::Destroyed);
    let calls = oci.calls.lock().unwrap();
    assert!(calls.iter().any(|c| c.starts_with("stop:")), "{calls:?}");
    assert!(calls.iter().any(|c| c.starts_with("remove:")), "{calls:?}");
}

#[tokio::test]
async fn reconcile_marks_a_missing_container_destroyed() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path()).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    oci.container_statuses.lock().unwrap().insert(
        format!("oxid-app-feature-a-{}", env.id.0),
        ContainerStatus::Missing,
    );

    let errors = cp.reconcile_startup_state().await.unwrap();
    assert!(errors.is_empty(), "{errors:?}");

    let loaded = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.state, EnvironmentState::Destroyed);
}

#[tokio::test]
async fn reconcile_re_pauses_a_container_a_reboot_brought_back_running() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path()).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    cp.pause(env.id).await.unwrap();

    // A reboot doesn't preserve the cgroup-freezer "paused" state —
    // `unless-stopped` brings the container back fully running.
    oci.container_statuses.lock().unwrap().insert(
        format!("oxid-app-feature-a-{}", env.id.0),
        ContainerStatus::Running,
    );

    let errors = cp.reconcile_startup_state().await.unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    assert!(
        oci.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| *c == format!("pause:oxid-app-feature-a-{}", env.id.0)),
        "{:?}",
        oci.calls.lock().unwrap()
    );
}

#[tokio::test]
async fn reconcile_restarts_a_running_environment_whose_container_stopped() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path()).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    oci.container_statuses.lock().unwrap().insert(
        format!("oxid-app-feature-a-{}", env.id.0),
        ContainerStatus::Stopped,
    );

    let errors = cp.reconcile_startup_state().await.unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    assert!(
        oci.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| *c == format!("start:oxid-app-feature-a-{}", env.id.0)),
        "{:?}",
        oci.calls.lock().unwrap()
    );
}

#[tokio::test]
async fn deploy_or_queue_deploys_immediately_when_capacity_is_available() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    *oci.host_capacity.lock().unwrap() = HostCapacity {
        total_memory_bytes: 1024 * 1_048_576,
        cpu_count: 4,
    };
    let cp = cp(oci)
        .await
        .with_resource_defaults(Some(200), None)
        .with_admission_control(Some(100));
    let project = cp.register_project(repo.path()).await.unwrap();

    let outcome = cp
        .deploy_or_queue(project.id, BranchName::parse("main").unwrap(), None)
        .await
        .unwrap();
    assert!(matches!(outcome, DeployOutcome::Deployed(_)), "{outcome:?}");
}

#[tokio::test]
async fn deploy_or_queue_queues_when_the_host_is_already_committed() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    *oci.host_capacity.lock().unwrap() = HostCapacity {
        total_memory_bytes: 300 * 1_048_576,
        cpu_count: 4,
    };
    let cp = cp(oci)
        .await
        .with_resource_defaults(Some(200), None)
        .with_admission_control(Some(50));
    let project = cp.register_project(repo.path()).await.unwrap();

    // First deploy fits alone (200MB request <= 250MB usable) and stays
    // Running, committing its 200MB.
    cp.deploy_or_queue(project.id, BranchName::parse("main").unwrap(), None)
        .await
        .unwrap();

    // A second branch's 200MB request would push committed usage to
    // 400MB against only 250MB usable — must queue, not overcommit.
    let outcome = cp
        .deploy_or_queue(project.id, BranchName::parse("other").unwrap(), None)
        .await
        .unwrap();
    let DeployOutcome::Queued { position } = outcome else {
        panic!("expected Queued, got {outcome:?}");
    };
    assert_eq!(position, 1);

    let queued = cp.store.list_deploy_queue().await.unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].branch, "other");
}

#[tokio::test]
async fn deploy_or_queue_rejects_a_request_that_could_never_fit() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    *oci.host_capacity.lock().unwrap() = HostCapacity {
        total_memory_bytes: 1024 * 1_048_576,
        cpu_count: 4,
    };
    let cp = cp(oci)
        .await
        .with_resource_defaults(Some(2000), None)
        .with_admission_control(Some(1000));
    let project = cp.register_project(repo.path()).await.unwrap();

    let err = cp
        .deploy_or_queue(project.id, BranchName::parse("main").unwrap(), None)
        .await
        .unwrap_err();
    assert!(matches!(err, CpError::InsufficientCapacity(_)), "{err:?}");
}

#[tokio::test]
async fn deploy_or_queue_always_deploys_when_admission_control_is_disabled() {
    let repo = repo_dir_with_config();
    // Zero capacity by default — if admission control were mistakenly
    // active this would queue or reject, not deploy.
    let cp = cp(FakeOci::default()).await;
    let project = cp.register_project(repo.path()).await.unwrap();

    let outcome = cp
        .deploy_or_queue(project.id, BranchName::parse("main").unwrap(), None)
        .await
        .unwrap();
    assert!(matches!(outcome, DeployOutcome::Deployed(_)), "{outcome:?}");
}

#[tokio::test]
async fn retry_queued_deploys_leaves_the_queue_untouched_when_nothing_fits_yet() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    *oci.host_capacity.lock().unwrap() = HostCapacity {
        total_memory_bytes: 300 * 1_048_576,
        cpu_count: 4,
    };
    let cp = cp(oci)
        .await
        .with_resource_defaults(Some(200), None)
        .with_admission_control(Some(50));
    let project = cp.register_project(repo.path()).await.unwrap();

    cp.deploy_or_queue(project.id, BranchName::parse("main").unwrap(), None)
        .await
        .unwrap();
    cp.deploy_or_queue(project.id, BranchName::parse("other").unwrap(), None)
        .await
        .unwrap();

    let failures = cp.retry_queued_deploys().await.unwrap();
    assert!(failures.is_empty(), "{failures:?}");
    let queued = cp.store.list_deploy_queue().await.unwrap();
    assert_eq!(queued.len(), 1, "{queued:?}");
    assert_eq!(queued[0].branch, "other");
}

#[tokio::test]
async fn retry_queued_deploys_deploys_once_capacity_frees_up() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    *oci.host_capacity.lock().unwrap() = HostCapacity {
        total_memory_bytes: 300 * 1_048_576,
        cpu_count: 4,
    };
    let cp = cp(oci.clone())
        .await
        .with_resource_defaults(Some(200), None)
        .with_admission_control(Some(50));
    let project = cp.register_project(repo.path()).await.unwrap();

    let main_env = match cp
        .deploy_or_queue(project.id, BranchName::parse("main").unwrap(), None)
        .await
        .unwrap()
    {
        DeployOutcome::Deployed(env) => env,
        other @ DeployOutcome::Queued { .. } => panic!("expected Deployed, got {other:?}"),
    };
    cp.deploy_or_queue(project.id, BranchName::parse("other").unwrap(), None)
        .await
        .unwrap();

    // Freeing `main`'s 200MB should let `other`'s queued 200MB request
    // through on the next retry pass.
    cp.destroy(main_env.id, false).await.unwrap();

    let failures = cp.retry_queued_deploys().await.unwrap();
    assert!(failures.is_empty(), "{failures:?}");
    assert!(cp.store.list_deploy_queue().await.unwrap().is_empty());
    assert!(
        oci.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("run:oxid-app-other")),
        "{:?}",
        oci.calls.lock().unwrap()
    );
}
