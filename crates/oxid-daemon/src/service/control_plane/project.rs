#![allow(
    unused_imports,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::pedantic,
    clippy::nursery,
    clippy::empty_line_after_doc_comments
)]

use std::path::Path;

use oxid_core::{
    BranchName, ContainerPort, Environment, EnvironmentState, EnvironmentStore, GitPort, Project,
    ProjectId, ProjectStore, RepoUrl, RepositoryError, Ttl,
};

use crate::adapter::config;

use super::ControlPlane;
use super::error::CpError;

impl<G: GitPort, O: oxid_core::ContainerPort> ControlPlane<G, O> {
    /// Registers the project declared by `oxid.toml` in `repo_dir`.
    ///
    /// Idempotent: if a project with the same `origin` URL already exists, the
    /// existing record is returned.
    ///
    /// # Errors
    /// Returns [`CpError`] on config, git or persistence failures.
    /// Fetches the base images this project's builds will need, in the
    /// background.
    ///
    /// Registration is the moment Oxid learns what a project is built with,
    /// and it is usually minutes or hours before the first push arrives.
    /// Spending that gap on the pull means the first deploy — the one
    /// someone is watching, having just wired up a webhook — does not open
    /// with a download of several hundred megabytes.
    ///
    /// Detached and best-effort on purpose. Registration must not wait on a
    /// registry, and a failure here costs nothing: the build pulls the image
    /// itself, exactly as it did before. Only the images a *detected* stack
    /// named are fetched — a project with its own Dockerfile could be built
    /// on anything, and guessing at `FROM` lines to pre-fetch the wrong
    /// image would waste bandwidth on every registration.
    fn prewarm_base_images(&self, project: &Project)
    where
        O: Clone + Send + Sync + 'static,
    {
        let Some(stack) = &project.detected_stack else {
            return;
        };
        let images = stack.base_images();
        if images.is_empty() {
            return;
        }
        let oci = self.oci.clone();
        let name = project.name.clone();
        tokio::spawn(async move {
            for image in images {
                match oci.pull_image(&image).await {
                    Ok(()) => tracing::info!(project = %name, %image, "base image ready"),
                    Err(e) => tracing::debug!(
                        project = %name,
                        %image,
                        error = %e,
                        "could not pre-fetch a base image; the build will pull it"
                    ),
                }
            }
        });
    }

    /// Builds the project record for a checkout, honouring an explicit
    /// `[build].context` and reporting the workspace either way.
    ///
    /// Registering the same repository twice with the *same* context is
    /// still a duplicate; with a different one it is a second service, which
    /// is what a monorepo's API and web app are.
    fn describe(
        parsed: config::ParsedProject,
        repo_url: &RepoUrl,
        context: Option<&str>,
    ) -> Result<Project, CpError> {
        let mut parsed = parsed;
        if let Some(context) = context {
            // Named explicitly, so it wins over whatever detection guessed —
            // and the port follows it, since the service in `apps/web` is
            // not the one in `apps/api`.
            if let Some(mono) = &parsed.monorepo
                && let Some(member) = mono.deployable.iter().find(|w| w.path == context)
            {
                parsed.config.port = member.port;
            }
            parsed.config.build.context = context.to_owned();
        }
        Ok(
            Project::new(ProjectId(0), parsed.name, repo_url.clone(), parsed.config)?
                .with_detected_stack(parsed.stack)
                .with_workspace(parsed.monorepo),
        )
    }

    pub async fn register_project(
        &self,
        repo_dir: &Path,
        context: Option<&str>,
    ) -> Result<Project, CpError>
    where
        O: Clone + Send + Sync + 'static,
    {
        let repo_url = self.git.remote_url(repo_dir).await?;
        let parsed = config::parse_project(repo_dir)?;
        let mut project = Self::describe(parsed, &repo_url, context)?;

        // Idempotent per *service*, not per repository: the same repo and
        // the same part of it is the same project, a different part is a
        // different one.
        if let Some(existing) = self
            .find_projects_by_repo(&repo_url)
            .await?
            .into_iter()
            .find(|p| p.config.build.context == project.config.build.context)
        {
            return Ok(existing);
        }

        match ProjectStore::create(&self.store, &project).await {
            Ok(id) => {
                project.id = id;
                self.prewarm_base_images(&project);
                Ok(project)
            }
            Err(RepositoryError::Conflict(_)) => self
                .find_projects_by_repo(&repo_url)
                .await?
                .into_iter()
                .find(|p| p.config.build.context == project.config.build.context)
                .ok_or_else(|| CpError::NotFound(format!("project for `{repo_url}`"))),
            Err(e) => Err(e.into()),
        }
    }

    /// Registers a project straight from a remote Git URL — no local
    /// checkout required (how the dashboard's onboarding wizard and
    /// `oxid up --repo <url>` register against a containerized daemon).
    ///
    /// The remote is cloned/fetched **now** rather than at first deploy:
    /// an eager probe means a wrong URL or token surfaces here, where the
    /// API maps it to a 400 with the git error in the message, instead of a
    /// 500 mid-deploy later. It also warms the git cache so the first
    /// deploy doesn't pay for the clone. Config is parsed from the cloned
    /// tree exactly as [`Self::register_project`] would from a local one,
    /// and `git_token` (when given) is persisted encrypted alongside the
    /// project row so private repos work from the very first deploy.
    ///
    /// Idempotent by exact `repo_url`, like [`Self::register_project`].
    ///
    /// # Errors
    /// Returns [`CpError`] on fetch failures (mapped to
    /// [`CpError::Config`], which the HTTP layer answers 400 — the input is
    /// what's wrong, not the server), config errors and persistence
    /// failures.
    pub async fn register_project_by_url(
        &self,
        repo_url: &RepoUrl,
        git_token: Option<&str>,
        context: Option<&str>,
    ) -> Result<Project, CpError>
    where
        O: Clone + Send + Sync + 'static,
    {
        let cloned_dir = self
            .git
            .ensure_repo(repo_url, git_token, &self.cache_dir)
            .await
            // Mapped to 400 by the HTTP layer (`CpError::Validation`) — a
            // failed probe means the *input* (URL or token) is wrong, not
            // the server. Deploy-time git failures stay `CpError::Git`/500.
            .map_err(|e| {
                CpError::Validation(format!(
                    "cannot fetch `{repo_url}`{}: {e}",
                    git_token.map_or(String::new(), |_| " with the provided git token".to_owned())
                ))
            })?;

        let parsed = config::parse_project(&cloned_dir)?;
        let mut project = Self::describe(parsed, repo_url, context)?;

        // Same rule as the directory form: idempotent per service. The
        // clone above happens first because the workspace is not knowable
        // until the repository is on disk.
        if let Some(existing) = self
            .find_projects_by_repo(repo_url)
            .await?
            .into_iter()
            .find(|p| p.config.build.context == project.config.build.context)
        {
            return Ok(existing);
        }

        match ProjectStore::create(&self.store, &project).await {
            Ok(id) => {
                project.id = id;
                if let Some(token) = git_token.filter(|t| !t.is_empty()) {
                    self.store.set_git_token(project.id, Some(token)).await?;
                }
                self.prewarm_base_images(&project);
                Ok(project)
            }
            Err(RepositoryError::Conflict(_)) => self
                .find_project_by_repo(repo_url)
                .await?
                .ok_or_else(|| CpError::NotFound(format!("project for `{repo_url}`"))),
            Err(e) => Err(e.into()),
        }
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

    /// Permanently deletes a project: destroys every environment that isn't
    /// already `Destroyed` (tearing down its container, image and any leased
    /// resource-pool slots), removes the project's git-cache clone, then
    /// deletes the project row — which cascades to its `secrets` and
    /// `environments` rows at the database level (`ON DELETE CASCADE`).
    ///
    /// # Errors
    /// Returns [`CpError::NotFound`] if the project does not exist.
    pub async fn delete_project(&self, project_id: ProjectId) -> Result<(), CpError> {
        let project = self.ensure_project(project_id).await?;

        for env in self.store.list_by_project(project_id).await? {
            if env.state != EnvironmentState::Destroyed {
                self.destroy(env.id, false).await?;
            }
        }

        let cache_path = self
            .cache_dir
            .join(crate::adapter::git::cache_dir_name(&project.repo_url));
        if let Err(e) = tokio::fs::remove_dir_all(&cache_path).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(RepositoryError::Storage(format!(
                "could not remove git cache `{}`: {e}",
                cache_path.display()
            ))
            .into());
        }

        Ok(ProjectStore::delete(&self.store, project_id).await?)
    }

    /// Updates a project's idle/lifetime policy (`pause_after`/
    /// `destroy_after`) — the two settings `oxid.toml` otherwise only ever
    /// seeds once, at first registration, with no way to change them again
    /// short of re-registering. Either can be omitted to leave it as-is;
    /// takes effect on the *next* GC sweep, no redeploy needed.
    ///
    /// # Errors
    /// Returns [`CpError::NotFound`] if the project does not exist.
    pub async fn update_project_ttls(
        &self,
        project_id: ProjectId,
        pause_after: Option<Ttl>,
        destroy_after: Option<Ttl>,
    ) -> Result<Project, CpError> {
        let mut project = self.ensure_project(project_id).await?;
        if let Some(pause_after) = pause_after {
            project.config.pause_after = pause_after;
        }
        if let Some(destroy_after) = destroy_after {
            project.config.destroy_after = destroy_after;
        }
        ProjectStore::update(&self.store, &project).await?;
        Ok(project)
    }

    /// Replaces a project's `[deploy]` rules — which branches a push may
    /// deploy, and how many environments it may hold.
    ///
    /// Lives on the project rather than being re-read from each commit like
    /// `[build]`, because the filter has to answer before the checkout: its
    /// whole purpose is to avoid fetching and building a branch nobody
    /// wanted. `oxid.toml` seeds it at registration; this is how it changes
    /// afterwards, the same shape as the TTL policy above.
    ///
    /// Each argument is optional so a caller only sends what it is changing,
    /// and `max_environments` takes a nested `Option` because clearing the
    /// cap and leaving it alone are different requests.
    ///
    /// # Errors
    /// Returns [`CpError::NotFound`] if the project does not exist.
    pub async fn update_project_deploy(
        &self,
        project_id: ProjectId,
        branches: Option<Vec<String>>,
        ignore: Option<Vec<String>>,
        max_environments: Option<Option<u32>>,
    ) -> Result<Project, CpError> {
        let mut project = self.ensure_project(project_id).await?;
        if let Some(branches) = branches {
            project.config.deploy.branches = branches;
        }
        if let Some(ignore) = ignore {
            project.config.deploy.ignore = ignore;
        }
        if let Some(max) = max_environments {
            project.config.deploy.max_environments = max;
        }
        ProjectStore::update(&self.store, &project).await?;
        Ok(project)
    }

    /// Sets (or, with `token: None`/an empty string, clears) a project's git
    /// access token — required for the daemon to clone/fetch a *private*
    /// repository, since its own git-cache clone is independent of any
    /// credential helper the operator's own shell has configured. Never
    /// returned by any API response: it lives only in the encrypted
    /// `projects.git_token_enc` column, decrypted just-in-time by
    /// [`Self::deploy_at`] right before the git operation that needs it.
    ///
    /// # Errors
    /// Returns [`CpError::NotFound`] if the project does not exist.
    pub async fn set_project_git_token(
        &self,
        project_id: ProjectId,
        token: Option<&str>,
    ) -> Result<(), CpError> {
        self.ensure_project(project_id).await?;
        Ok(self.store.set_git_token(project_id, token).await?)
    }
}

impl<G: GitPort, O: ContainerPort> ControlPlane<G, O> {
    /// Every project registered against `repo_url`.
    ///
    /// A repository can hold several deployable services — a monorepo's
    /// API, web app and worker — and a push to it deploys all of them. What
    /// distinguishes them is `[build].context`, which is what the schema now
    /// makes unique alongside the URL.
    ///
    /// # Errors
    /// Returns [`CpError`] if the projects cannot be listed.
    pub async fn find_projects_by_repo(&self, repo_url: &RepoUrl) -> Result<Vec<Project>, CpError> {
        Ok(self
            .list_projects()
            .await?
            .into_iter()
            .filter(|p| p.repo_url == *repo_url)
            .collect())
    }

    pub(crate) async fn find_project_by_repo(
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

    /// Resolves the registered project a local checkout belongs to, by its
    /// remote URL — the read half of [`Self::register_project`], exposed so
    /// the HTTP layer can let a project-scoped token re-resolve *its own*
    /// project (what `oxid up` does first on every run) without granting
    /// the ability to create new ones.
    ///
    /// # Errors
    /// Returns [`CpError`] when the git remote cannot be determined.
    pub async fn project_for_repo(&self, repo_dir: &Path) -> Result<Option<Project>, CpError> {
        let repo_url = self.git.remote_url(repo_dir).await?;
        Ok(self.find_project_by_repo(&repo_url).await?)
    }

    /// URL-form twin of [`Self::project_for_repo`]: resolves a project by
    /// exact `repo_url` with no filesystem access at all — what the scoped-
    /// token path of registration-by-URL uses (it must answer before any
    /// clone happens, or a scoped token could make the daemon fetch an
    /// arbitrary remote).
    ///
    /// # Errors
    /// Returns [`CpError`] on persistence failures.
    pub async fn project_for_repo_url(
        &self,
        repo_url: &RepoUrl,
    ) -> Result<Option<Project>, CpError> {
        Ok(self.find_project_by_repo(repo_url).await?)
    }

    pub(crate) async fn ensure_project(&self, project_id: ProjectId) -> Result<Project, CpError> {
        ProjectStore::get(&self.store, project_id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("project `{project_id}`")))
    }
}
