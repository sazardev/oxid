//! Application service: orchestrates the domain through the ports.
//!
//! This is the thin "application" layer of the hexagonal architecture. It
//! wires [`SqliteStore`], a [`GitPort`] and a [`ContainerPort`] together to
//! expose the operations the interfaces (CLI, HTTP API) call.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use oxid_core::services::subdomain::subdomain_for;
use oxid_core::{
    AuditEvent, AuditStore, Branch, BranchName, BuildSpec, ContainerPort, ContainerSpec,
    DomainError, Environment, EnvironmentId, EnvironmentState, EnvironmentStore, GitError, GitPort,
    OciError, OffsetDateTime, Project, ProjectId, ProjectStore, RepoUrl, RepositoryError,
    StateTransition,
};

use crate::adapter::config::{self, ConfigError};
use crate::adapter::store::SqliteStore;

/// Errors surfaced by the control plane.
#[derive(Debug, thiserror::Error)]
pub enum CpError {
    /// Configuration file could not be read or parsed.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// A persistence operation failed.
    #[error(transparent)]
    Store(#[from] RepositoryError),
    /// A git operation failed.
    #[error(transparent)]
    Git(#[from] GitError),
    /// A docker operation failed.
    #[error(transparent)]
    Oci(#[from] OciError),
    /// A domain rule was violated.
    #[error(transparent)]
    Domain(#[from] DomainError),
    /// A requested record does not exist.
    #[error("not found: {0}")]
    NotFound(String),
}

/// Orchestrates registration, deployment and lifecycle of environments.
#[derive(Clone)]
pub struct ControlPlane<G: GitPort, O: ContainerPort> {
    store: SqliteStore,
    git: G,
    oci: O,
    cache_dir: PathBuf,
}

impl<G: GitPort, O: ContainerPort> ControlPlane<G, O> {
    /// Creates a control plane bound to a store, git client and docker client.
    #[must_use]
    pub fn new(store: SqliteStore, git: G, oci: O, cache_dir: PathBuf) -> Self {
        Self {
            store,
            git,
            oci,
            cache_dir,
        }
    }

    /// Registers the project declared by `oxid.toml` in `repo_dir`.
    ///
    /// Idempotent: if a project with the same `origin` URL already exists, the
    /// existing record is returned.
    ///
    /// # Errors
    /// Returns [`CpError`] on config, git or persistence failures.
    pub async fn register_project(&self, repo_dir: &Path) -> Result<Project, CpError> {
        let repo_url = self.git.remote_url(repo_dir).await?;

        if let Some(existing) = self.find_project_by_repo(&repo_url).await? {
            return Ok(existing);
        }

        let parsed = config::parse_file(repo_dir.join("oxid.toml"))?;
        let id = self.store.next_project_id().await?;
        let project = Project::new(id, parsed.name, repo_url, parsed.config)?;
        ProjectStore::create(&self.store, &project).await?;
        Ok(project)
    }

    /// Lists all registered projects.
    ///
    /// # Errors
    /// Returns [`CpError`] on persistence failures.
    pub async fn list_projects(&self) -> Result<Vec<Project>, CpError> {
        Ok(self.store.list().await?)
    }

    /// Lists environments of a project.
    ///
    /// # Errors
    /// Returns [`CpError::NotFound`] if the project does not exist.
    pub async fn list_environments(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<Environment>, CpError> {
        self.ensure_project(project_id).await?;
        Ok(self.store.list_by_project(project_id).await?)
    }

    /// Deploys `branch` for a project: clone, build, run, then transition to
    /// `Running`.
    ///
    /// # Errors
    /// Returns [`CpError`] on any pipeline step failure.
    pub async fn deploy(
        &self,
        project_id: ProjectId,
        branch: BranchName,
    ) -> Result<Environment, CpError> {
        let project = self.ensure_project(project_id).await?;

        // 1. Clone cache + resolve + checkout the branch.
        let repo_dir = self
            .git
            .ensure_repo(&project.repo_url, &self.cache_dir)
            .await?;
        let commit = self.git.resolve_branch_head(&repo_dir, &branch).await?;
        self.git.checkout_commit(&repo_dir, &commit.sha).await?;

        // 2. Build the image.
        let image = format!("oxid/{}/{}", project.name, sanitize_label(&branch));
        let build = BuildSpec {
            context: repo_dir.clone(),
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
        let env_id = self.store.next_environment_id().await?;
        let env = Environment::new(
            env_id,
            project.id,
            Branch::new(commit.branch, commit.sha)?,
            EnvironmentState::Building,
            url.clone(),
            now,
        )?;
        EnvironmentStore::create(&self.store, &env).await?;

        // 4. Run the container.
        let env_vars = BTreeMap::from([
            ("OXID_BRANCH".to_owned(), branch.to_string()),
            ("OXID_ENV_URL".to_owned(), url.clone()),
        ]);
        let name = container_name(&project, &branch);
        let spec = ContainerSpec {
            name: name.clone(),
            image,
            env: env_vars,
            container_port: project.config.port,
            host_port: project.config.port,
            labels: BTreeMap::from([
                ("oxid.project".to_owned(), project.name.clone()),
                ("oxid.branch".to_owned(), branch.to_string()),
                ("oxid.url".to_owned(), url),
            ]),
        };
        self.oci.run(&spec).await?;

        // 5. Transition to Running and record the deployment.
        let mut env = env;
        env.transition(StateTransition::BuildSucceeded, now)
            .map_err(|e| state_err(&e))?;
        self.store.update(&env).await?;
        self.store
            .record(&AuditEvent::new(
                u64::try_from(now.unix_timestamp()).unwrap_or_default(),
                env.id,
                StateTransition::BuildSucceeded,
                Some(name),
                now,
            ))
            .await?;

        Ok(env)
    }

    /// Suspends an environment (scale-to-zero).
    ///
    /// # Errors
    /// Returns [`CpError`] on missing records or Docker failures.
    pub async fn pause(&self, environment_id: EnvironmentId) -> Result<(), CpError> {
        let env = self.ensure_environment(environment_id).await?;
        let project = ProjectStore::get(&self.store, env.project_id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("project `{}`", env.project_id)))?;
        self.oci
            .pause(&container_name(&project, &env.branch.name))
            .await?;

        let mut env = env;
        let now = OffsetDateTime::now_utc();
        env.transition(StateTransition::IdleTimeout, now)
            .map_err(|e| state_err(&e))?;
        self.store.update(&env).await?;
        Ok(())
    }

    /// Wakes a suspended environment.
    ///
    /// # Errors
    /// Returns [`CpError`] on missing records or Docker failures.
    pub async fn wake(&self, environment_id: EnvironmentId) -> Result<(), CpError> {
        let env = self.ensure_environment(environment_id).await?;
        let project = ProjectStore::get(&self.store, env.project_id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("project `{}`", env.project_id)))?;
        self.oci
            .unpause(&container_name(&project, &env.branch.name))
            .await?;

        let mut env = env;
        let now = OffsetDateTime::now_utc();
        env.transition(StateTransition::Woken, now)
            .map_err(|e| state_err(&e))?;
        self.store.update(&env).await?;
        Ok(())
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
            .logs(&container_name(&project, &env.branch.name))
            .await?)
    }

    async fn find_project_by_repo(
        &self,
        repo_url: &RepoUrl,
    ) -> Result<Option<Project>, RepositoryError> {
        for project in self.store.list().await? {
            if &project.repo_url == repo_url {
                return Ok(Some(project));
            }
        }
        Ok(None)
    }

    async fn ensure_project(&self, project_id: ProjectId) -> Result<Project, CpError> {
        ProjectStore::get(&self.store, project_id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("project `{project_id}`")))
    }

    async fn ensure_environment(
        &self,
        environment_id: EnvironmentId,
    ) -> Result<Environment, CpError> {
        EnvironmentStore::get(&self.store, environment_id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("environment `{environment_id}`")))
    }
}

fn state_err(err: &oxid_core::EnvironmentStateError) -> CpError {
    CpError::Domain(DomainError::Invalid(err.to_string()))
}

fn container_name(project: &Project, branch: &BranchName) -> String {
    format!("oxid-{}-{}", project.name, sanitize_label(branch))
}

fn sanitize_label(branch: &BranchName) -> String {
    branch
        .to_string()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
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
        async fn ensure_repo(&self, _url: &RepoUrl, cache_dir: &Path) -> Result<PathBuf, GitError> {
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
    }

    impl ContainerPort for FakeOci {
        async fn build(&self, spec: &BuildSpec) -> Result<(), OciError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("build:{}", spec.image));
            Ok(())
        }
        async fn run(&self, spec: &ContainerSpec) -> Result<(), OciError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("run:{}", spec.name));
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
        async fn logs(&self, name: &str) -> Result<String, OciError> {
            self.calls.lock().unwrap().push(format!("logs:{name}"));
            Ok("build log".to_owned())
        }
    }

    async fn store() -> SqliteStore {
        SqliteStore::open_in_memory().await.unwrap()
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
        ControlPlane::new(store().await, FakeGit, oci, cache.path().to_owned())
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
}
