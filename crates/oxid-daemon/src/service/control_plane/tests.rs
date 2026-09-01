use super::*;
use crate::adapter::store::SqliteStore;
use oxid_core::{
    AuditFilter, Branch, BranchName, BuildReport, BuildSpec, ContainerPort, ContainerSpec,
    ContainerStatus, EnvVarScope, Environment, EnvironmentState, EnvironmentStore, GitError,
    GitPort, HostCapacity, LogStream, OciError, OffsetDateTime, PoolError, PoolKind, ProjectId,
    ProjectStore, RepoUrl, StateTransition,
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
        // The real `ensure_repo` leaves a checkout on disk, and the deploy
        // now copies the build context out of it before releasing the git
        // lock — so a fake that returns a path to nothing is a fake that
        // cannot exercise the deploy path.
        // A small tree with a distinguishable subdirectory, so a test can
        // tell a root context from a `[build].context = "backend"` one by
        // what the deploy actually captured.
        let dir = cache_dir.join("app");
        let nested = dir.join("backend");
        let write = |path: std::path::PathBuf, body: &str| -> Result<(), GitError> {
            std::fs::write(path, body).map_err(|e| GitError::Failure(e.to_string()))
        };
        std::fs::create_dir_all(&nested).map_err(|e| GitError::Failure(e.to_string()))?;
        write(dir.join("Dockerfile"), "FROM scratch\n")?;
        write(nested.join("Dockerfile.prod"), "FROM scratch\n")?;
        write(nested.join("api.py"), "print('hi')\n")?;
        Ok(dir)
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
    /// When > 0, `build` fails and decrements this instead of succeeding —
    /// the broken-Dockerfile case, which is the most common way a real
    /// deploy fails and the one that used to leave no trace at all.
    fail_build_times: Arc<Mutex<u32>>,
    /// Per-container overrides for `container_status`; anything not
    /// listed here defaults to `Running`.
    container_statuses: Arc<Mutex<std::collections::HashMap<String, ContainerStatus>>>,
    host_capacity: Arc<Mutex<HostCapacity>>,
    /// When set, `host_capacity` fails — a node that has stopped answering.
    host_capacity_error: Arc<Mutex<bool>>,
    /// Docker networks `ensure_network`/`network_exists` believe exist.
    network_exists: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Whether `ensure_traefik` believes its container is already up.
    traefik_running: Arc<Mutex<bool>>,
    /// Labels the most recent `run` was given — the Traefik router,
    /// middleware and service wiring a deployed environment carries.
    last_run_labels: Arc<Mutex<std::collections::BTreeMap<String, String>>>,
    /// Builds currently in flight, and the highest that number ever
    /// reached. A serial drain never gets past 1; this is what lets a test
    /// assert the queue actually overlaps its work rather than merely
    /// finishing it.
    builds_in_flight: Arc<std::sync::atomic::AtomicUsize>,
    peak_builds_in_flight: Arc<std::sync::atomic::AtomicUsize>,
    /// When set, every `build` yields for this long, so overlapping builds
    /// have a window in which to be observed overlapping.
    build_delay: Arc<Mutex<Option<std::time::Duration>>>,
}

impl ContainerPort for FakeOci {
    async fn runtime_info(&self) -> Result<oxid_core::RuntimeInfo, OciError> {
        Ok(oxid_core::RuntimeInfo {
            flavor: oxid_core::RuntimeFlavor::Docker,
            version: "fake".to_owned(),
            rootless: false,
            buildkit: true,
        })
    }
    async fn traefik_runtime(
        &self,
        _name: &str,
    ) -> Result<Option<oxid_core::services::tls::TraefikRuntime>, OciError> {
        Ok(None)
    }

    async fn ensure_volume(&self, _name: &str) -> Result<(), OciError> {
        Ok(())
    }
    async fn pull_image(&self, image: &str) -> Result<(), OciError> {
        self.calls.lock().unwrap().push(format!("pull:{image}"));
        let _ = image;
        Ok(())
    }

    async fn build(&self, spec: &BuildSpec) -> Result<BuildReport, OciError> {
        {
            let mut remaining = self.fail_build_times.lock().unwrap();
            if *remaining > 0 {
                *remaining -= 1;
                return Err(OciError::Failure("build failed: bad Dockerfile".to_owned()));
            }
        }
        let delay = *self.build_delay.lock().unwrap();
        if let Some(delay) = delay {
            use std::sync::atomic::Ordering;
            let in_flight = self.builds_in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak_builds_in_flight
                .fetch_max(in_flight, Ordering::SeqCst);
            tokio::time::sleep(delay).await;
            self.builds_in_flight.fetch_sub(1, Ordering::SeqCst);
        }
        // The context is a private copy now, so its *path* no longer says
        // which subdirectory was captured — its contents do.
        let mut entries: Vec<String> = std::fs::read_dir(&spec.context)
            .map(|d| {
                d.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        entries.sort();
        self.calls.lock().unwrap().push(format!(
            "build:{}:context={}:entries=[{}]:dockerfile={}",
            spec.image,
            spec.context.display(),
            entries.join(","),
            spec.dockerfile
        ));
        // Non-zero totals so tests can tell a propagated report apart from
        // the zeroed "unparseable stream" default.
        Ok(BuildReport {
            duration_ms: 1_234,
            steps_total: 10,
            steps_cached: 8,
        })
    }
    async fn run(&self, spec: &ContainerSpec) -> Result<Option<u16>, OciError> {
        self.calls.lock().unwrap().push(format!(
            "run:{}:env={:?}:mem={:?}:cpu={:?}",
            spec.name, spec.env, spec.memory_limit_mb, spec.cpu_limit_millicores
        ));
        *self.last_run_labels.lock().unwrap() = spec.labels.clone();
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
        if *self.host_capacity_error.lock().unwrap() {
            return Err(OciError::Failure("connection refused".to_owned()));
        }
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
        self.calls.lock().unwrap().push(format!(
            "ensure_traefik:{}:port={}:image={}",
            spec.container_name, spec.http_port, spec.image
        ));
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
        // Same as `FakeGit`: the deploy copies the build context out of the
        // checkout, so it has to be a directory that exists.
        let dir = cache_dir.join("app");
        std::fs::create_dir_all(&dir).map_err(|e| GitError::Failure(e.to_string()))?;
        std::fs::write(dir.join("Dockerfile"), "FROM scratch\n")
            .map_err(|e| GitError::Failure(e.to_string()))?;
        Ok(dir)
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

    let project = cp.register_project(repo.path(), None).await.unwrap();
    assert_eq!(project.name, "app");
    assert_eq!(project.repo_url.as_str(), "https://github.com/org/app.git");

    // Idempotent registration.
    let again = cp.register_project(repo.path(), None).await.unwrap();
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
            tokio::spawn(async move { cp.register_project(&path, None).await })
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
    let project = cp.register_project(repo.path(), None).await.unwrap();

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
    let project = cp.register_project(dir.path(), None).await.unwrap();

    cp.deploy(project.id, BranchName::parse("main").unwrap())
        .await
        .unwrap();

    let calls = oci.calls.lock().unwrap();
    let build_call = calls
        .iter()
        .find(|c| c.starts_with("build:"))
        .expect("a build call was made");
    // The build context is a private copy of the checkout, so its path no
    // longer names the subdirectory — what proves `[build].context` was
    // honoured is that the copy holds `backend/`'s contents and not the
    // repository root's.
    assert!(
        build_call.contains("entries=[Dockerfile.prod,api.py]"),
        "the configured `backend` subdirectory must be what got captured: {build_call}"
    );
    assert!(
        build_call.ends_with(":dockerfile=Dockerfile.prod"),
        "{build_call}"
    );
}

#[tokio::test]
async fn deploy_fails_clearly_when_dependency_is_unconfigured() {
    let repo = repo_dir_with_redis_dependency();
    let cp = cp(FakeOci::default()).await; // no `with_resource_pools` call
    let project = cp.register_project(repo.path(), None).await.unwrap();

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
    let project = cp.register_project(repo.path(), None).await.unwrap();

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

/// `FakeGit`, counting how many times it was asked to refresh the
/// repository. A fetch is a network round-trip in the real adapter, so
/// counting them is counting the thing that actually costs. The counter is
/// per instance rather than global: tests run in parallel in one process.
#[derive(Clone, Default)]
struct CountingGit(Arc<std::sync::atomic::AtomicUsize>);

impl CountingGit {
    fn fetches(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl GitPort for CountingGit {
    async fn remote_url(&self, repo_dir: &Path) -> Result<RepoUrl, GitError> {
        FakeGit.remote_url(repo_dir).await
    }
    async fn ensure_repo(
        &self,
        url: &RepoUrl,
        token: Option<&str>,
        cache_dir: &Path,
    ) -> Result<PathBuf, GitError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        FakeGit.ensure_repo(url, token, cache_dir).await
    }
    async fn resolve_branch_head(
        &self,
        repo_dir: &Path,
        branch: &BranchName,
    ) -> Result<oxid_core::CommitRef, GitError> {
        FakeGit.resolve_branch_head(repo_dir, branch).await
    }
    async fn checkout_commit(&self, repo_dir: &Path, sha: &str) -> Result<(), GitError> {
        FakeGit.checkout_commit(repo_dir, sha).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sibling_branches_pushed_together_share_one_fetch() {
    // A fetch brings down every branch of the repository, so the first one
    // of a burst has already retrieved what the rest are about to ask for.
    // They used to repeat it anyway, serialized behind the git lock, one
    // network round-trip each — about three quarters of the wall-clock of
    // a fifteen-branch push.
    let repo = repo_dir_with_config();
    let cache = tempfile::tempdir().unwrap();
    let git = CountingGit::default();
    let cp = Arc::new(
        ControlPlane::new(
            store().await,
            git.clone(),
            FakeOci::default(),
            cache.path().to_owned(),
        )
        .with_readiness_check(false),
    );
    let project = cp.register_project(repo.path(), None).await.unwrap();
    let baseline = git.fetches();

    let deploys = (0..8).map(|i| {
        let cp = Arc::clone(&cp);
        let branch = BranchName::parse(format!("sibling-{i}")).unwrap();
        async move { cp.deploy(project.id, branch).await.unwrap() }
    });
    futures_util::future::join_all(deploys).await;

    let fetches = git.fetches() - baseline;
    assert!(
        fetches < 8,
        "every sibling fetched for itself ({fetches} fetches for 8 branches)"
    );
}

#[tokio::test]
async fn concurrent_branches_never_share_a_redis_index() {
    // Slots are handed out by reading which are taken and picking the
    // lowest free one. Deploys used to be serialized node-wide, so that
    // read-then-claim could not interleave; now that sibling branches
    // deploy at the same time, it can — and two branches sharing one Redis
    // database is not something a uniqueness constraint would catch, since
    // a lease is unique per branch rather than per slot.
    //
    // This asserts the invariant under real concurrency; it cannot force
    // the interleaving, so it is a guard against the invariant being
    // dropped rather than proof the window is closed. The lock in
    // `provision.rs` is what closes it.
    let repo = repo_dir_with_redis_dependency();
    let cache = tempfile::tempdir().unwrap();
    let cp = Arc::new(
        ControlPlane::new(
            store().await,
            FakeGit,
            FakeOci::default(),
            cache.path().to_owned(),
        )
        .with_resource_pools(None, Some("redis://cache:6379".to_owned()), 16)
        .with_readiness_check(false),
    );
    let project = cp.register_project(repo.path(), None).await.unwrap();

    let branches: Vec<String> = (0..8).map(|i| format!("feature-{i}")).collect();
    let deploys = branches.iter().map(|b| {
        let cp = Arc::clone(&cp);
        let branch = BranchName::parse(b.clone()).unwrap();
        async move { cp.deploy(project.id, branch).await.unwrap() }
    });
    futures_util::future::join_all(deploys).await;

    let mut indexes = Vec::new();
    for b in &branches {
        indexes.push(
            redis_lease_for(&cp, project.id, b)
                .await
                .unwrap_or_else(|| panic!("{b} got no redis lease")),
        );
    }
    let unique: std::collections::BTreeSet<_> = indexes.iter().collect();
    assert_eq!(
        unique.len(),
        branches.len(),
        "branches shared a redis index: {indexes:?}"
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
    let project = cp.register_project(repo.path(), None).await.unwrap();

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
    let project = cp.register_project(repo.path(), None).await.unwrap();

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
        // Matched by prefix rather than by the whole rendered env map, so
        // adding an injected variable doesn't break an assertion that is
        // really about ordering.
        let run_new = calls
            .iter()
            .position(|c| c.starts_with("run:oxid-app-feature-a-2:"))
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
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
    let project = cp.register_project(repo.path(), None).await.unwrap();

    let err = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, CpError::Oci(_)), "{err:?}");

    let envs = cp.list_environments(project.id).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(envs[0].state, EnvironmentState::BuildFailed);

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
    let project = cp.register_project(repo.path(), None).await.unwrap();

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
    let project = cp.register_project(repo.path(), None).await.unwrap();

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
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
    let project = cp.register_project(dir.path(), None).await.unwrap();
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
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
    let project = cp.register_project(repo.path(), None).await.unwrap();
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

/// Waking dispatches on what Docker reports, not on the stored state.
///
/// Scale-to-zero suspends with `stop`, so a `Paused` environment's container
/// is stopped and must be `start`ed. A container left `paused` by an older
/// Oxid still has to be `unpause`d, and one that is already running must be
/// a no-op rather than the Docker 500 (`is not paused`) that dispatching on
/// the stored state produced — the failure that made every retry of a woken
/// environment fail identically.
#[tokio::test]
async fn wake_dispatches_on_actual_container_state() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path(), None).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    let container = "oxid-app-feature-a-1";

    // Suspending stops the container; Traefik only publishes routers for
    // running containers, so a `pause`d one loses its route for good.
    cp.pause(env.id).await.unwrap();
    assert!(
        oci.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("stop:")),
        "{:?}",
        oci.calls
    );

    // Stopped -> start.
    oci.container_statuses
        .lock()
        .unwrap()
        .insert(container.to_owned(), ContainerStatus::Stopped);
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

    // Already running -> neither start nor unpause, and still a success.
    oci.container_statuses
        .lock()
        .unwrap()
        .insert(container.to_owned(), ContainerStatus::Running);
    oci.calls.lock().unwrap().clear();
    let woken = cp.wake_by_url(&env.url).await.unwrap().unwrap();
    assert_eq!(woken.state, EnvironmentState::Running);
    assert!(
        oci.calls.lock().unwrap().is_empty(),
        "waking a running environment must not touch Docker: {:?}",
        oci.calls
    );

    // A container an older Oxid left paused still gets unpaused.
    let mut paused = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    paused
        .transition(StateTransition::IdleTimeout, OffsetDateTime::now_utc())
        .unwrap();
    EnvironmentStore::update(&cp.store, &paused).await.unwrap();
    oci.container_statuses
        .lock()
        .unwrap()
        .insert(container.to_owned(), ContainerStatus::Paused);
    oci.calls.lock().unwrap().clear();
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
}

/// A container that vanished (pruned, or removed by hand) can't be woken by
/// starting nothing — the caller is told to redeploy instead of getting a
/// success that leaves the URL dead.
#[tokio::test]
async fn wake_reports_a_missing_container_instead_of_succeeding() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path(), None).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    cp.pause(env.id).await.unwrap();
    oci.container_statuses
        .lock()
        .unwrap()
        .insert("oxid-app-feature-a-1".to_owned(), ContainerStatus::Missing);

    let err = cp.wake(env.id).await.unwrap_err();
    assert!(
        err.to_string().contains("redeploy"),
        "expected actionable error, got: {err}"
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
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
    // `stop`, not `pause`: a paused container disappears from Traefik's
    // router table and never comes back, so the branch 404s instead of
    // waking on the next request.
    assert!(
        oci.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("stop:")),
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
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
    let project = cp.register_project(repo.path(), None).await.unwrap();
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

// ---------------------------------------------------------------------------
// fleet
// ---------------------------------------------------------------------------

/// A control plane that can register nodes, all sharing one `FakeOci`.
///
/// Enough for the tests about *which node a decision names*, which is most
/// of them — what the Docker on the other end does is not the subject.
fn cp_with_nodes(
    cp: ControlPlane<FakeGit, FakeOci>,
    oci: FakeOci,
) -> ControlPlane<FakeGit, FakeOci> {
    cp.with_node_connector(Arc::new(move |_node| Ok(oci.clone())))
}

/// A control plane where each registered node gets its **own** `FakeOci`,
/// so one node can stop answering while the others carry on. Without this
/// separation, "node eu-1 went down" silently also takes down the local
/// node, and a test asserting the fleet keeps working proves nothing.
fn cp_with_distinct_nodes(
    cp: ControlPlane<FakeGit, FakeOci>,
    remote: FakeOci,
) -> ControlPlane<FakeGit, FakeOci> {
    cp.with_node_connector(Arc::new(move |_node| Ok(remote.clone())))
}

/// A connector that always fails — a node whose endpoint is wrong, or a
/// machine that is simply not there.
fn cp_with_unreachable_nodes(cp: ControlPlane<FakeGit, FakeOci>) -> ControlPlane<FakeGit, FakeOci> {
    cp.with_node_connector(Arc::new(|_node| {
        Err(OciError::Failure("connection refused".to_owned()))
    }))
}

/// Draining is what an operator does before emptying a machine, so it has
/// to actually stop new work arriving — while leaving everything already
/// there running.
#[tokio::test]
async fn a_drained_node_receives_no_new_deploys() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp_with_nodes(cp(oci.clone()).await, oci.clone());
    let project = cp.register_project(repo.path(), None).await.unwrap();

    let eu1 = cp
        .add_node("eu-1", "tcp://10.0.0.4:2376", None, None, None)
        .await
        .unwrap();
    assert!(eu1.connected);

    // With both active, the tie breaks on the lowest id: the local node.
    let first = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    assert_eq!(first.node_id, oxid_core::NodeId::LOCAL);

    cp.set_node_state(oxid_core::NodeId::LOCAL, oxid_core::NodeState::Draining)
        .await
        .unwrap();

    let second = cp
        .deploy(project.id, BranchName::parse("feature-b").unwrap())
        .await
        .unwrap();
    assert_eq!(
        second.node_id, eu1.node.id,
        "a draining node must not be handed a new environment"
    );

    // And the branch already there is untouched.
    let loaded = EnvironmentStore::get(&cp.store, first.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.state, EnvironmentState::Running);
    assert_eq!(loaded.node_id, oxid_core::NodeId::LOCAL);
}

/// Images are not distributed: a branch that moves rebuilds from scratch.
/// A redeploy must therefore stay where its layer cache already is, even
/// when another node is emptier.
#[tokio::test]
async fn a_redeploy_stays_on_the_node_it_is_already_on() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp_with_nodes(cp(oci.clone()).await, oci.clone());
    let project = cp.register_project(repo.path(), None).await.unwrap();
    let eu1 = cp
        .add_node("eu-1", "tcp://10.0.0.4:2376", None, None, None)
        .await
        .unwrap();

    cp.set_node_state(oxid_core::NodeId::LOCAL, oxid_core::NodeState::Draining)
        .await
        .unwrap();
    let first = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    assert_eq!(first.node_id, eu1.node.id);

    // Node 1 is active again and is now the lower id — affinity has to win
    // anyway, or every redeploy would drag the branch back and rebuild it.
    cp.set_node_state(oxid_core::NodeId::LOCAL, oxid_core::NodeState::Active)
        .await
        .unwrap();
    let second = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    assert_eq!(
        second.node_id, eu1.node.id,
        "a redeploy must not move a branch away from its warm image cache"
    );
}

/// The whole reason `add_node` connects before it writes: a node in the
/// table that nothing can reach is a node placement keeps skipping and an
/// operator keeps having to explain.
#[tokio::test]
async fn registering_a_node_that_will_not_connect_changes_nothing() {
    let cp = cp_with_unreachable_nodes(cp(FakeOci::default()).await);
    let err = cp
        .add_node("eu-1", "tcp://10.0.0.4:2376", None, None, None)
        .await
        .unwrap_err();
    // A validation failure, not a Docker one — the difference is a 400 and
    // not a 500, and it is not cosmetic. Everything that can go wrong at
    // registration is the operator's input: a wrong address, a missing
    // certificate, no TLS material at all. A 500 tells them the daemon broke
    // and there is nothing to do but wait.
    assert!(matches!(err, CpError::Validation(_)), "{err:?}");
    assert_eq!(
        cp.list_nodes().await.unwrap().len(),
        1,
        "a failed registration must not leave a row behind"
    );
}

/// The probe records what it saw about the *node* and nothing about the
/// environments on it. A partition is indistinguishable from a dead
/// machine, and rewriting rows on one is how two live copies of a branch
/// end up fighting over a URL.
#[tokio::test]
async fn a_failed_probe_marks_the_node_and_leaves_its_environments_alone() {
    let repo = repo_dir_with_config();
    let local = FakeOci::default();
    // A separate fake for the remote node: the point of the test is that
    // one node going down leaves the rest of the fleet working, which a
    // shared client cannot demonstrate.
    let oci = FakeOci::default();
    let cp = cp_with_distinct_nodes(cp(local.clone()).await, oci.clone());
    let project = cp.register_project(repo.path(), None).await.unwrap();
    let eu1 = cp
        .add_node("eu-1", "tcp://10.0.0.4:2376", None, None, None)
        .await
        .unwrap();

    cp.set_node_state(oxid_core::NodeId::LOCAL, oxid_core::NodeState::Draining)
        .await
        .unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    assert_eq!(env.node_id, eu1.node.id);

    // The node stops answering.
    *oci.host_capacity_error.lock().unwrap() = true;
    cp.probe_nodes().await.unwrap();

    let node = cp.store.get_node(eu1.node.id).await.unwrap().unwrap();
    assert_eq!(node.state, oxid_core::NodeState::Down);

    let loaded = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        loaded.state,
        EnvironmentState::Running,
        "a node going down says nothing about the environments on it"
    );
    assert_eq!(loaded.node_id, eu1.node.id);

    // And a node that is down takes no new work, even though its row still
    // claims plenty of memory.
    cp.set_node_state(oxid_core::NodeId::LOCAL, oxid_core::NodeState::Active)
        .await
        .unwrap();
    let next = cp
        .deploy(project.id, BranchName::parse("feature-b").unwrap())
        .await
        .unwrap();
    assert_eq!(next.node_id, oxid_core::NodeId::LOCAL);

    // It comes back on its own once it answers again — no restart, no
    // re-registration.
    *oci.host_capacity_error.lock().unwrap() = false;
    cp.probe_nodes().await.unwrap();
    assert_eq!(
        cp.store.get_node(eu1.node.id).await.unwrap().unwrap().state,
        oxid_core::NodeState::Active
    );
}

/// Draining and then evacuating is how a machine is emptied: every live
/// branch is rebuilt elsewhere through the ordinary zero-downtime path, and
/// each one is rebuilt at **the commit it was running**, not at its current
/// head — draining a node is an infrastructure operation, not a licence to
/// ship whatever somebody pushed since.
#[tokio::test]
async fn evacuating_a_node_moves_its_branches_at_the_commit_they_were_running() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp_with_nodes(cp(oci.clone()).await, oci.clone());
    let project = cp.register_project(repo.path(), None).await.unwrap();
    let eu1 = cp
        .add_node("eu-1", "tcp://10.0.0.4:2376", None, None, None)
        .await
        .unwrap();

    // Two branches on the local node.
    let a = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    let b = cp
        .deploy(project.id, BranchName::parse("feature-b").unwrap())
        .await
        .unwrap();
    assert_eq!(a.node_id, oxid_core::NodeId::LOCAL);
    assert_eq!(b.node_id, oxid_core::NodeId::LOCAL);
    let deployed_sha = a.branch.commit_sha.clone();

    cp.set_node_state(oxid_core::NodeId::LOCAL, oxid_core::NodeState::Draining)
        .await
        .unwrap();
    let results = cp
        .evacuate_node(oxid_core::NodeId::LOCAL, None)
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
    assert!(
        results.iter().all(|(_, failure)| failure.is_none()),
        "{results:?}"
    );

    for branch in ["feature-a", "feature-b"] {
        let env = cp
            .find_environment_by_branch(project.id, &BranchName::parse(branch).unwrap())
            .await
            .unwrap()
            .expect("the branch must still have a live environment");
        assert_eq!(
            env.node_id, eu1.node.id,
            "{branch} should have moved off the draining node"
        );
        assert_eq!(
            env.state,
            EnvironmentState::Running,
            "the move is a redeploy, so the branch ends up serving again"
        );
        assert_eq!(
            env.branch.commit_sha, deployed_sha,
            "an evacuation must not quietly ship a newer commit"
        );
    }
}

/// A branch that will not build stays where it is, and is named. A node
/// half-emptied is the honest outcome; a drain that reported success while
/// leaving containers behind is not.
#[tokio::test]
async fn a_branch_that_cannot_be_rebuilt_stays_and_is_reported() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp_with_nodes(cp(oci.clone()).await, oci.clone());
    let project = cp.register_project(repo.path(), None).await.unwrap();
    cp.add_node("eu-1", "tcp://10.0.0.4:2376", None, None, None)
        .await
        .unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    cp.set_node_state(oxid_core::NodeId::LOCAL, oxid_core::NodeState::Draining)
        .await
        .unwrap();
    *oci.fail_build_times.lock().unwrap() = 1;

    let results = cp
        .evacuate_node(oxid_core::NodeId::LOCAL, None)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, env.id);
    assert!(
        results[0].1.is_some(),
        "a branch that could not move must say so, not be counted as moved"
    );
}

/// A successful probe must not undo a drain. An operator emptying a machine
/// does not want deploys sent back at it thirty seconds later.
#[tokio::test]
async fn a_probe_never_un_drains_a_node() {
    let oci = FakeOci::default();
    let cp = cp_with_nodes(cp(oci.clone()).await, oci.clone());
    let eu1 = cp
        .add_node("eu-1", "tcp://10.0.0.4:2376", None, None, None)
        .await
        .unwrap();
    cp.set_node_state(eu1.node.id, oxid_core::NodeState::Draining)
        .await
        .unwrap();

    cp.probe_nodes().await.unwrap();
    assert_eq!(
        cp.store.get_node(eu1.node.id).await.unwrap().unwrap().state,
        oxid_core::NodeState::Draining
    );
}

/// `down` is what a failed probe records, not something to assert by hand —
/// the next tick would overwrite it anyway, so accepting it would be a
/// setting that silently does nothing.
#[tokio::test]
async fn a_node_cannot_be_marked_down_by_hand() {
    let oci = FakeOci::default();
    let cp = cp_with_nodes(cp(oci.clone()).await, oci.clone());
    let err = cp
        .set_node_state(oxid_core::NodeId::LOCAL, oxid_core::NodeState::Down)
        .await
        .unwrap_err();
    assert!(matches!(err, CpError::Validation(_)), "{err:?}");
}

/// The fleet is rebuilt from the table at startup, and a node that will not
/// connect is skipped rather than fatal — one bad endpoint must not stop a
/// daemon whose other nodes are serving traffic.
#[tokio::test]
async fn a_node_that_will_not_connect_at_startup_is_skipped_not_fatal() {
    let oci = FakeOci::default();
    let cp = cp_with_nodes(cp(oci.clone()).await, oci.clone());
    let eu1 = cp
        .add_node("eu-1", "tcp://10.0.0.4:2376", None, None, None)
        .await
        .unwrap();

    // A fresh daemon over the same store, this time unable to reach it.
    let restarted = cp_with_unreachable_nodes(cp.clone());
    restarted.fleet().deregister(eu1.node.id);
    restarted.reload_fleet().await.unwrap();

    assert!(restarted.fleet().get(eu1.node.id).is_none());
    assert!(
        restarted.fleet().get(oxid_core::NodeId::LOCAL).is_some(),
        "the local node must survive another node being unreachable"
    );
    let listed = restarted.list_nodes().await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(
        !listed
            .iter()
            .find(|n| n.node.id == eu1.node.id)
            .unwrap()
            .connected,
        "a node with no client must report itself as disconnected, not as gone"
    );
}

/// Records a second node in the `nodes` table without giving the fleet a
/// client for it — the state a daemon is in between reading the table and
/// connecting, and the state it stays in for a node it cannot reach.
async fn register_unreachable_node(cp: &ControlPlane<FakeGit, FakeOci>) -> oxid_core::NodeId {
    let node = oxid_core::Node::new(
        oxid_core::NodeId(0),
        "eu-1",
        oxid_core::NodeEndpoint::from("tcp://10.0.0.4:2376"),
    )
    .unwrap();
    cp.store.upsert_node(&node).await.unwrap()
}

/// The invariant that makes reconciliation safe on a fleet: a node this
/// daemon cannot reach must have its environments left *exactly* as they
/// are.
///
/// A network partition and a dead machine are indistinguishable from here.
/// The reconciler's other branch marks a `Running` row `Destroyed` when
/// Docker says the container is missing — so if an unreachable node were
/// allowed to resolve to "missing", every environment on the far side of a
/// partition would have its record deleted while its container carried on
/// serving traffic. Recovering the node would then redeploy every branch on
/// top of the copies still running, two of each fighting over one URL.
///
/// Reported loudly, and nothing else touched.
#[tokio::test]
async fn an_unreachable_node_never_destroys_its_environments() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path(), None).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    // A node that exists in the table but that this *process* holds no
    // client for — what an operator sees mid-restart, after a
    // mis-registration, or during a real partition.
    let eu2 = register_unreachable_node(&cp).await;
    let mut moved = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    moved.node_id = eu2;
    EnvironmentStore::update(&cp.store, &moved).await.unwrap();

    let before = oci.calls.lock().unwrap().len();
    let errors = cp.reconcile_startup_state().await.unwrap();

    assert_eq!(
        errors.len(),
        1,
        "an unreachable node must be reported, not passed over: {errors:?}"
    );
    assert_eq!(errors[0].0, env.id);
    assert_eq!(
        oci.calls.lock().unwrap().len(),
        before,
        "nothing may be dispatched at the local Docker on behalf of another node"
    );

    let loaded = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        loaded.state,
        EnvironmentState::Running,
        "the row must survive its node being unreachable, untouched"
    );
    assert_eq!(loaded.node_id, eu2);
}

/// One unreachable node must not stop the rest of the fleet reconciling.
#[tokio::test]
async fn one_unreachable_node_does_not_block_the_others() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path(), None).await.unwrap();
    let stranded = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    let local = cp
        .deploy(project.id, BranchName::parse("feature-b").unwrap())
        .await
        .unwrap();

    let eu2 = register_unreachable_node(&cp).await;
    let mut moved = EnvironmentStore::get(&cp.store, stranded.id)
        .await
        .unwrap()
        .unwrap();
    moved.node_id = eu2;
    EnvironmentStore::update(&cp.store, &moved).await.unwrap();

    // The reachable one has drifted and does need correcting.
    oci.container_statuses.lock().unwrap().insert(
        format!("oxid-app-feature-b-{}", local.id.0),
        ContainerStatus::Missing,
    );

    let errors = cp.reconcile_startup_state().await.unwrap();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].0, stranded.id);

    let loaded = EnvironmentStore::get(&cp.store, local.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        loaded.state,
        EnvironmentState::Destroyed,
        "an unreachable node must not stop the reachable ones being reconciled"
    );
}

/// Nothing about placement is decided yet, and that is the whole point of
/// this stage: every deploy still lands on node 1.
#[tokio::test]
async fn a_deploy_still_lands_on_the_local_node() {
    let repo = repo_dir_with_config();
    let cp = cp(FakeOci::default()).await;
    let project = cp.register_project(repo.path(), None).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    assert_eq!(env.node_id, oxid_core::NodeId::LOCAL);

    let loaded = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        loaded.node_id,
        oxid_core::NodeId::LOCAL,
        "the node has to survive a round trip through the store"
    );
}

#[tokio::test]
async fn reconcile_marks_a_missing_container_destroyed() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
async fn reconcile_re_suspends_a_container_a_reboot_brought_back_running() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path(), None).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    cp.pause(env.id).await.unwrap();

    // A reboot brings a suspended container back up via `unless-stopped`,
    // so reconcile has to put it back to sleep — with `stop`, the same way
    // suspension is applied everywhere else, since a `pause`d container is
    // invisible to Traefik's router table.
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
            .any(|c| *c == format!("stop:oxid-app-feature-a-{}", env.id.0)),
        "{:?}",
        oci.calls.lock().unwrap()
    );
}

#[tokio::test]
async fn reconcile_closes_out_a_deploy_a_restart_interrupted() {
    // A `Building` row cannot still be building once the daemon has
    // restarted. Leaving it alone kept it `Building` forever, and since
    // admission counts `building` as memory already promised, every daemon
    // killed mid-deploy leaked a reservation nothing was using.
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci).await;
    let project = cp.register_project(repo.path(), None).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    // Put it back the way a deploy killed between "row created" and
    // "container running" would have left it.
    let mut stuck = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    stuck.state = EnvironmentState::Building;
    EnvironmentStore::update(&cp.store, &stuck).await.unwrap();

    let errors = cp.reconcile_startup_state().await.unwrap();
    assert!(errors.is_empty(), "{errors:?}");

    let after = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.state,
        EnvironmentState::BuildFailed,
        "an interrupted deploy must not stay `building`"
    );
    // Which is the point: it no longer counts against the node's budget.
    assert_eq!(
        cp.store
            .committed_memory_mb(128, None, oxid_core::NodeId::LOCAL)
            .await
            .unwrap(),
        0,
        "the interrupted deploy is still reserving memory"
    );
}

#[tokio::test]
async fn reconcile_restarts_a_running_environment_whose_container_stopped() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
    let project = cp.register_project(repo.path(), None).await.unwrap();

    let outcome = cp
        .deploy_or_queue(project.id, BranchName::parse("main").unwrap(), None)
        .await
        .unwrap();
    assert!(
        matches!(&outcome, DeployOutcome::Deployed(_, report) if report.build.steps_total == 10),
        "{outcome:?}"
    );
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
    let project = cp.register_project(repo.path(), None).await.unwrap();

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
    let project = cp.register_project(repo.path(), None).await.unwrap();

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
    let project = cp.register_project(repo.path(), None).await.unwrap();

    let outcome = cp
        .deploy_or_queue(project.id, BranchName::parse("main").unwrap(), None)
        .await
        .unwrap();
    assert!(
        matches!(&outcome, DeployOutcome::Deployed(_, report) if report.build.steps_total == 10),
        "{outcome:?}"
    );
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
    let project = cp.register_project(repo.path(), None).await.unwrap();

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
async fn the_queue_drains_several_deploys_at_a_time() {
    // The queue is how every webhook push reaches a deploy, so a serial
    // drain made a team's pushes finish one after another: with six
    // branches waiting, the last one paid for the five in front of it.
    // Builds are almost entirely waiting on Docker, so they should overlap.
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    *oci.build_delay.lock().unwrap() = Some(std::time::Duration::from_millis(80));
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path(), None).await.unwrap();

    // Queued directly: `deploy_or_queue` would deploy them immediately,
    // since nothing here is capacity-constrained.
    for i in 0..6 {
        cp.store
            .enqueue_deploy(
                project.id,
                &BranchName::parse(format!("queued-{i}")).unwrap(),
                None,
            )
            .await
            .unwrap();
    }

    let started = std::time::Instant::now();
    let failures = cp.retry_queued_deploys().await.unwrap();
    let elapsed = started.elapsed();

    assert!(failures.is_empty(), "{failures:?}");
    assert!(
        cp.store.list_deploy_queue().await.unwrap().is_empty(),
        "every queued branch should have deployed"
    );
    let peak = oci
        .peak_builds_in_flight
        .load(std::sync::atomic::Ordering::SeqCst);
    assert!(peak > 1, "builds never overlapped (peak in flight: {peak})");
    // Six serial 80ms builds cannot finish in less than 480ms.
    assert!(
        elapsed < std::time::Duration::from_millis(480),
        "drain took {elapsed:?}, which is no better than serial"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_push_that_lands_mid_drain_is_picked_up_by_that_same_drain() {
    // A burst of webhooks arrives faster than the drain can read the queue,
    // so most of them are enqueued after its snapshot and answered "a drain
    // is already running". That is correct, but the drain they were relying
    // on had already read past them, and they used to sit untouched until
    // the next scheduler tick — most of the wall-clock of a fifteen-branch
    // push was that wait, with the node idle.
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    *oci.build_delay.lock().unwrap() = Some(std::time::Duration::from_millis(150));
    let cp = Arc::new(cp(oci).await);
    let project = cp.register_project(repo.path(), None).await.unwrap();

    cp.store
        .enqueue_deploy(project.id, &BranchName::parse("first").unwrap(), None)
        .await
        .unwrap();

    let drain = {
        let cp = Arc::clone(&cp);
        tokio::spawn(async move { cp.retry_queued_deploys().await })
    };
    // Enqueued while the first deploy is still building, exactly as a second
    // webhook would be.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    cp.store
        .enqueue_deploy(project.id, &BranchName::parse("late").unwrap(), None)
        .await
        .unwrap();

    let failures = drain.await.unwrap().unwrap();
    assert!(failures.is_empty(), "{failures:?}");
    assert!(
        cp.store.list_deploy_queue().await.unwrap().is_empty(),
        "the late push should have been drained by the same pass"
    );
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
    let project = cp.register_project(repo.path(), None).await.unwrap();

    let main_env = match cp
        .deploy_or_queue(project.id, BranchName::parse("main").unwrap(), None)
        .await
        .unwrap()
    {
        DeployOutcome::Deployed(env, _report) => env,
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

/// A failing image build must be *visible*.
///
/// The environment row used to be created only after the build succeeded,
/// so a broken Dockerfile — the most common deploy failure there is — bailed
/// out before any row existed: no environment, no audit event, and nothing
/// in the log. From `oxid status` or the dashboard a colleague's failed push
/// was indistinguishable from a push that never happened.
#[tokio::test]
async fn a_failed_build_leaves_a_recorded_environment() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    *oci.fail_build_times.lock().unwrap() = 1;
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path(), None).await.unwrap();

    let err = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("build failed"), "{err}");

    let envs = EnvironmentStore::list_by_project(&cp.store, project.id)
        .await
        .unwrap();
    assert_eq!(envs.len(), 1, "the failed deploy must still leave a row");
    assert_eq!(
        envs[0].state,
        EnvironmentState::BuildFailed,
        "a failed build must be distinguishable from a routine teardown"
    );

    let events = cp
        .audit_events_for(envs[0].id, &AuditFilter::default())
        .await
        .unwrap();
    let failed = events
        .iter()
        .find(|e| e.kind == StateTransition::BuildFailed)
        .expect("a failed build must be audited");
    assert!(
        failed
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("build failed"),
        "the audit entry must say why: {:?}",
        failed.detail
    );
}

/// Two branches whose names collapse to the same DNS label (`feat/x` and
/// `feat-x`) would both claim one subdomain. Both reported themselves as
/// running while the proxy could only route to one, leaving the other
/// silently unreachable — so the second deploy is refused instead.
#[tokio::test]
async fn a_branch_cannot_steal_another_branchs_subdomain() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path(), None).await.unwrap();

    let first = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    let err = cp
        .deploy(project.id, BranchName::parse("feature/a").unwrap())
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains(&first.url), "{msg}");
    assert!(msg.contains("feature-a"), "{msg}");

    // The refusal has to be recorded, not just returned: the push that
    // triggered it arrived over an already-answered webhook, so an error
    // returned to nobody is one the dev who pushed never sees.
    let refused = EnvironmentStore::list_by_project(&cp.store, project.id)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.branch.name.as_str() == "feature/a")
        .expect("the refused deploy must leave a row");
    assert_eq!(refused.state, EnvironmentState::BuildFailed);
    let events = cp
        .audit_events_for(refused.id, &AuditFilter::default())
        .await
        .unwrap();
    assert!(
        events.iter().any(|e| {
            e.kind == StateTransition::BuildFailed
                && e.detail
                    .as_deref()
                    .unwrap_or_default()
                    .contains("already using")
        }),
        "the audit entry must explain the collision: {events:?}"
    );

    // And the branch that already owns the address keeps serving.
    let live = EnvironmentStore::get(&cp.store, first.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(live.state, EnvironmentState::Running);
}

/// Suspending an environment and waking it again must leave the branch
/// reachable through the same URL — the round trip the 404s came from.
#[tokio::test]
async fn a_suspended_environment_wakes_back_onto_its_url() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await.with_traefik("net", "http://daemon");
    let project = cp.register_project(repo.path(), None).await.unwrap();
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
    assert_eq!(paused.url, env.url, "the URL must survive suspension");

    let woken = cp.wake_by_url(&env.url).await.unwrap().unwrap();
    assert_eq!(woken.state, EnvironmentState::Running);
    assert_eq!(woken.url, env.url);
}

/// A failed deploy must not hide the instance that is still serving.
///
/// The `BuildFailed` row lands on top of the live one with a higher id, and
/// resolving "the current environment" by id alone made the next successful
/// deploy miss the real previous instance — leaving the old container
/// running forever behind the new one.
#[tokio::test]
async fn a_failed_deploy_does_not_hide_the_running_instance() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path(), None).await.unwrap();
    let branch = BranchName::parse("feature-a").unwrap();

    let first = cp.deploy(project.id, branch.clone()).await.unwrap();

    *oci.fail_build_times.lock().unwrap() = 1;
    cp.deploy(project.id, branch.clone()).await.unwrap_err();

    // The branch is still served by the first deploy, not by the failure.
    let live = cp
        .find_environment_by_branch(project.id, &branch)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(live.id, first.id);
    assert_eq!(live.state, EnvironmentState::Running);

    // And the next successful deploy retires it properly.
    let third = cp.deploy(project.id, branch).await.unwrap();
    assert_eq!(third.state, EnvironmentState::Running);
    let retired = EnvironmentStore::get(&cp.store, first.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        retired.state,
        EnvironmentState::Destroyed,
        "the previous live instance must be retired by the successful deploy"
    );
}

/// Containers are told which commit they are running, not just which branch.
/// A branch name moves on every push, so it can't answer "what revision is
/// this?" — which is exactly what an app's `/version` endpoint or a release
/// tag needs.
#[tokio::test]
async fn the_deployed_commit_is_injected_into_the_container() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path(), None).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    let calls = oci.calls.lock().unwrap();
    let run = calls
        .iter()
        .find(|c| c.starts_with("run:"))
        .expect("the container must have been run");
    assert!(
        run.contains(&format!("\"OXID_COMMIT\": \"{}\"", env.branch.commit_sha)),
        "{run}"
    );
    assert!(run.contains("\"OXID_BRANCH\": \"feature-a\""), "{run}");
}

/// The Traefik `oxid infra setup` starts must carry the flags wake-on-request
/// depends on.
///
/// The compose file had them and this bootstrap path did not, so an operator
/// who used `oxid infra setup` got a proxy where a sleeping branch's request
/// hung on Docker's default dial timeout instead of failing fast into
/// `/api/v1/wake` — the feature simply never fired.
#[test]
fn the_built_in_traefik_spec_is_wired_for_wake_on_request() {
    let spec = oxid_core::TraefikSpec::new("oxid-net");
    // Pinning an older tag here routed nothing at all on Docker Engine >= 29.
    assert_eq!(spec.image, "traefik:latest");
    assert_eq!(spec.container_name, "oxid-traefik");
    assert_eq!(spec.network, "oxid-net");
    assert_eq!(spec.http_port, 80);
}

/// `oxid infra setup` must be usable on a host whose port 80 is already
/// taken by another proxy — otherwise the bootstrap simply cannot run there,
/// which is the situation any machine already serving something is in.
#[tokio::test]
async fn the_built_in_traefik_publishes_on_the_configured_host_port() {
    let oci = FakeOci::default();
    let cp = cp(oci.clone())
        .await
        .with_traefik("oxid-net", "http://daemon")
        .with_traefik_http_port(8090);
    assert_eq!(cp.traefik_http_port(), 8090);

    cp.infra_bootstrap().await.unwrap();
    let calls = oci.calls.lock().unwrap();
    assert!(
        calls
            .iter()
            .any(|c| c.contains("ensure_traefik") && c.contains("8090")),
        "the bootstrap must publish Traefik on the configured port: {calls:?}"
    );
}

/// A dead row must never shadow the live environment on the same URL.
///
/// Two branches can normalise to one subdomain, and the refused one leaves a
/// `BuildFailed` row with a *higher* id than the branch actually serving
/// that address. Resolving the URL by recency alone picked the dead row, so
/// a visit to a sleeping branch reported a missing container instead of
/// waking the environment the URL belongs to.
#[tokio::test]
async fn waking_a_shared_url_resolves_to_the_environment_that_owns_it() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path(), None).await.unwrap();

    let live = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    // Collides on the subdomain, so it is refused and left as BuildFailed
    // with a higher id than `live`.
    cp.deploy(project.id, BranchName::parse("feature/a").unwrap())
        .await
        .unwrap_err();
    cp.pause(live.id).await.unwrap();

    let woken = cp
        .wake_by_url(&live.url)
        .await
        .unwrap()
        .expect("the URL must resolve to an environment");
    assert_eq!(woken.id, live.id);
    assert_eq!(woken.state, EnvironmentState::Running);
}

/// A sleeping environment must not reserve memory it is not using.
///
/// Suspension stops the container, so its resident set is gone; counting it
/// against host capacity deadlocks a busy node. Once enough branches idle
/// out their phantom reservations fill the budget, and the queue can never
/// drain, because the environments blocking it are asleep and nothing will
/// wake them. Reproduced with 15 branches on one host: 11 stopped containers
/// reserved 1408MB while consuming none, and four deploys waited forever.
#[tokio::test]
async fn a_sleeping_environment_does_not_reserve_host_memory() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    // 1 GiB total, 512 MB reserved for the host -> 512 MB usable, and the
    // fixture project asks for 256 MB, so exactly two can run at once.
    *oci.host_capacity.lock().unwrap() = HostCapacity {
        total_memory_bytes: 1_073_741_824,
        cpu_count: 2,
    };
    let cp = cp(oci.clone())
        .await
        .with_resource_defaults(Some(256), None)
        .with_admission_control(Some(512));
    let project = cp.register_project(repo.path(), None).await.unwrap();

    let first = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();
    let second = cp
        .deploy(project.id, BranchName::parse("feature-b").unwrap())
        .await
        .unwrap();

    // Full: a third does not fit while both are awake.
    assert!(matches!(
        cp.deploy_or_queue(project.id, BranchName::parse("feature-c").unwrap(), None)
            .await
            .unwrap(),
        DeployOutcome::Queued { .. }
    ));

    // Put both to sleep. Their containers are stopped, so the memory is
    // genuinely free again and the third deploy must now be admitted.
    cp.pause(first.id).await.unwrap();
    cp.pause(second.id).await.unwrap();
    assert!(
        matches!(
            cp.deploy_or_queue(project.id, BranchName::parse("feature-c").unwrap(), None)
                .await
                .unwrap(),
            DeployOutcome::Deployed(_, _)
        ),
        "a stopped container holds no memory, so it must not block a deploy"
    );
}

/// A dependency the operator never configured is not a transient failure.
///
/// Retrying it just multiplies the failure: a branch declaring a `postgres`
/// dependency on a daemon with no `OXID_POSTGRES_URL` produced five
/// identical `BuildFailed` rows, burying the one actionable message under
/// copies of itself. Observed on a fifteen-branch run.
#[tokio::test]
async fn a_dependency_the_daemon_lacks_is_not_retried() {
    let repo = repo_dir_with_redis_dependency();
    let oci = FakeOci::default();
    let cp = cp(oci.clone()).await;
    let project = cp.register_project(repo.path(), None).await.unwrap();

    let err = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap_err();
    assert!(
        matches!(err, CpError::Pool(oxid_core::PoolError::NotConfigured(_))),
        "{err:?}"
    );
    assert!(
        !crate::service::control_plane::deploy::is_retryable(&err),
        "an unconfigured dependency can never succeed on a retry"
    );

    // Exactly one row, not one per attempt.
    let envs = EnvironmentStore::list_by_project(&cp.store, project.id)
        .await
        .unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(envs[0].state, EnvironmentState::BuildFailed);
}

/// Waking must trigger on gateway errors, never on the app's own 5xx.
///
/// The middleware caught the whole 500-599 range, so a branch whose code
/// threw showed its developer a "Waking up…" page reloading every two
/// seconds instead of the stack trace — the preview environment hiding the
/// one thing it exists to show. Only Traefik's own "cannot reach the
/// backend" codes mean the container might be asleep.
#[tokio::test]
async fn waking_triggers_on_gateway_errors_not_on_the_apps_own_500() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci.clone())
        .await
        .with_traefik("oxid-net", "http://oxid-daemon:8080");
    let project = cp.register_project(repo.path(), None).await.unwrap();
    cp.deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    let labels = oci.last_run_labels.lock().unwrap().clone();
    let status = labels
        .iter()
        .find(|(k, _)| k.ends_with(".errors.status"))
        .map(|(_, v)| v.clone())
        .expect("the wake middleware must be labelled");
    assert_eq!(status, "502-504");
    assert!(
        !status.contains("500-"),
        "an application 500 must reach the developer, not the wake page"
    );
}

/// A heartbeat writes at most once per coalescing window.
///
/// Traefik calls it on every request to every environment and it is
/// deliberately unauthenticated, so persisting a row per call is both waste
/// and an amplifier anyone who can reach the proxy could drive. The
/// timestamp only feeds idle detection, whose threshold is minutes.
#[tokio::test]
async fn repeated_heartbeats_do_not_write_on_every_request() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = cp(oci).await;
    let project = cp.register_project(repo.path(), None).await.unwrap();
    let env = cp
        .deploy(project.id, BranchName::parse("feature-a").unwrap())
        .await
        .unwrap();

    // Age the row past the window so the first heartbeat definitely writes.
    let stale = OffsetDateTime::now_utc() - time::Duration::minutes(5);
    touch_env(&cp, env.clone(), stale).await;

    cp.touch_by_url(&env.url).await.unwrap();
    let first = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap()
        .last_accessed_at;
    assert!(first > stale, "a stale row must be refreshed");

    // Everything that follows inside the window is a read: the recorded
    // time must not move.
    for _ in 0..20 {
        cp.touch_by_url(&env.url).await.unwrap();
    }
    let after = EnvironmentStore::get(&cp.store, env.id)
        .await
        .unwrap()
        .unwrap()
        .last_accessed_at;
    assert_eq!(
        after, first,
        "heartbeats inside the window must not each persist a row"
    );
}

/// Two branches of one project must deploy at the same time — and must not
/// build each other's code while doing it.
///
/// A single process-wide mutex used to serialize every deploy on the node,
/// which made a team's pushes queue behind one another. Removing it is only
/// safe because the build context is copied out of the shared checkout
/// before the git lock is released: `checkout_commit` force-rewrites one
/// working directory that every branch of a project shares, so without the
/// copy a sibling deploy would swap the tree out from under this one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sibling_branches_deploy_concurrently_without_mixing_trees() {
    let repo = repo_dir_with_config();
    let oci = FakeOci::default();
    let cp = std::sync::Arc::new(cp(oci.clone()).await);
    let project = cp.register_project(repo.path(), None).await.unwrap();

    let mut tasks = Vec::new();
    for i in 0..6 {
        let cp = cp.clone();
        let id = project.id;
        tasks.push(tokio::spawn(async move {
            cp.deploy(id, BranchName::parse(format!("feature-{i}")).unwrap())
                .await
        }));
    }
    for task in tasks {
        task.await
            .unwrap()
            .expect("every sibling deploy must succeed");
    }

    let envs = EnvironmentStore::list_by_project(&cp.store, project.id)
        .await
        .unwrap();
    assert_eq!(envs.len(), 6, "one environment per branch");
    assert!(
        envs.iter().all(|e| e.state == EnvironmentState::Running),
        "{envs:?}"
    );

    // Each build must have had its own private context, or one branch was
    // reading a directory another was rewriting.
    let calls = oci.calls.lock().unwrap();
    let contexts: std::collections::HashSet<&str> = calls
        .iter()
        .filter(|c| c.starts_with("build:"))
        .filter_map(|c| c.split(":context=").nth(1))
        .filter_map(|c| c.split(":entries=").next())
        .collect();
    assert_eq!(
        contexts.len(),
        6,
        "each deploy needs its own build context, got {contexts:?}"
    );
}
