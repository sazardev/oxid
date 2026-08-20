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
    pub async fn register_project(&self, repo_dir: &Path) -> Result<Project, CpError> {
        let repo_url = self.git.remote_url(repo_dir).await?;

        if let Some(existing) = self.find_project_by_repo(&repo_url).await? {
            return Ok(existing);
        }

        let parsed = config::parse_project(repo_dir)?;
        let mut project = Project::new(ProjectId(0), parsed.name, repo_url.clone(), parsed.config)?;
        match ProjectStore::create(&self.store, &project).await {
            Ok(id) => {
                project.id = id;
                Ok(project)
            }
            Err(RepositoryError::Conflict(_)) => self
                .find_project_by_repo(&repo_url)
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

    pub(crate) async fn ensure_project(&self, project_id: ProjectId) -> Result<Project, CpError> {
        ProjectStore::get(&self.store, project_id)
            .await?
            .ok_or_else(|| CpError::NotFound(format!("project `{project_id}`")))
    }
}
