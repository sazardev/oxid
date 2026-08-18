//! Git adapter (SPEC.md §2.2 "Versionamiento").
//!
//! Backs the [`GitPort`] with `git2`. Git operations are blocking, so they are
//! offloaded to a blocking executor.

use std::path::{Path, PathBuf};

use git2::Repository;
use oxid_core::{BranchName, CommitRef, GitError, GitPort, RepoUrl};

/// Shared clone cache; cached repositories are reused across deployments.
#[derive(Debug, Clone, Copy, Default)]
pub struct GitClient;

impl GitClient {
    /// Creates a new git client.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl GitPort for GitClient {
    async fn remote_url(&self, repo_dir: &Path) -> Result<RepoUrl, GitError> {
        let repo_dir = repo_dir.to_owned();
        tokio::task::spawn_blocking(move || sync_remote_url(&repo_dir))
            .await
            .map_err(|e| GitError::Failure(format!("task failed: {e}")))?
    }

    async fn ensure_repo(&self, url: &RepoUrl, cache_dir: &Path) -> Result<PathBuf, GitError> {
        let url = url.clone();
        let cache_dir = cache_dir.to_owned();
        tokio::task::spawn_blocking(move || sync_ensure_repo(&url, &cache_dir))
            .await
            .map_err(|e| GitError::Failure(format!("task failed: {e}")))?
    }

    async fn resolve_branch_head(
        &self,
        repo_dir: &Path,
        branch: &BranchName,
    ) -> Result<CommitRef, GitError> {
        let repo_dir = repo_dir.to_owned();
        let branch = branch.clone();
        tokio::task::spawn_blocking(move || sync_resolve_branch_head(&repo_dir, &branch))
            .await
            .map_err(|e| GitError::Failure(format!("task failed: {e}")))?
    }

    async fn checkout_commit(&self, repo_dir: &Path, sha: &str) -> Result<(), GitError> {
        let repo_dir = repo_dir.to_owned();
        let sha = sha.to_owned();
        tokio::task::spawn_blocking(move || sync_checkout_commit(&repo_dir, &sha))
            .await
            .map_err(|e| GitError::Failure(format!("task failed: {e}")))?
    }
}

fn map_err(err: impl std::fmt::Display) -> GitError {
    GitError::Failure(err.to_string())
}

fn sync_remote_url(repo_dir: &Path) -> Result<RepoUrl, GitError> {
    let repo = Repository::open(repo_dir).map_err(map_err)?;
    let remote = repo.find_remote("origin").map_err(|_| {
        GitError::Failure(format!("no `origin` remote in `{}`", repo_dir.display()))
    })?;
    let url = remote.url().map_err(map_err)?;
    if url.is_empty() {
        return Err(GitError::Failure(format!(
            "`origin` remote has no URL in `{}`",
            repo_dir.display()
        )));
    }
    RepoUrl::parse(url).map_err(map_err)
}

fn cache_dir_name(url: &RepoUrl) -> String {
    let raw = url.as_str();
    let trimmed = raw.trim_end_matches('/');
    let segment = trimmed.rsplit('/').next().unwrap_or("repo").to_owned();
    segment
        .trim_end_matches(".git")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn sync_ensure_repo(url: &RepoUrl, cache_dir: &Path) -> Result<PathBuf, GitError> {
    std::fs::create_dir_all(cache_dir).map_err(map_err)?;
    let dir = cache_dir.join(cache_dir_name(url));

    if dir.exists() {
        return Ok(dir);
    }

    // git2::Repository::clone accepts local paths and file:// URLs.
    Repository::clone(url.as_str(), &dir).map_err(map_err)?;
    Ok(dir)
}

fn sync_resolve_branch_head(repo_dir: &Path, branch: &BranchName) -> Result<CommitRef, GitError> {
    let repo = Repository::open(repo_dir).map_err(map_err)?;
    let oid = repo
        .refname_to_id(&format!("refs/heads/{branch}"))
        .map_err(|_| {
            GitError::Failure(format!(
                "branch `{branch}` not found in `{}`",
                repo_dir.display()
            ))
        })?;
    Ok(CommitRef {
        branch: branch.clone(),
        sha: oid.to_string(),
    })
}

fn sync_checkout_commit(repo_dir: &Path, sha: &str) -> Result<(), GitError> {
    let repo = Repository::open(repo_dir).map_err(map_err)?;
    let commit = repo
        .revparse_single(sha)
        .map_err(|_| {
            GitError::Failure(format!(
                "commit `{sha}` not found in `{}`",
                repo_dir.display()
            ))
        })?
        .peel_to_commit()
        .map_err(map_err)?;

    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout
        .force()
        .recreate_missing(true)
        .remove_untracked(true);

    repo.checkout_tree(commit.as_object(), Some(&mut checkout))
        .map_err(map_err)?;
    repo.set_head_detached(commit.id()).map_err(map_err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Signature;
    use std::fs;

    /// Initializes a repository with one commit on `main` and an `origin` remote.
    fn init_repo(dir: &Path, origin_url: &str) -> (Repository, String) {
        let repo = Repository::init(dir).unwrap();
        fs::write(dir.join("Dockerfile"), "FROM alpine:3.20\n").unwrap();

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("Dockerfile")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();

        let signature = Signature::now("oxid test", "oxid@test.local").unwrap();
        let commit_id = repo
            .commit(Some("HEAD"), &signature, &signature, "init", &tree, &[])
            .unwrap();
        drop(tree);

        // Force the branch name to `main` regardless of `init.defaultBranch`.
        let head_ref = repo
            .head()
            .unwrap()
            .name()
            .unwrap_or("refs/heads/main")
            .to_owned();
        let commit = repo.find_commit(commit_id).unwrap();
        if head_ref != "refs/heads/main" {
            repo.branch("main", &commit, true).unwrap();
            repo.set_head("refs/heads/main").unwrap();
        }
        drop(commit);
        repo.remote("origin", origin_url).unwrap();

        (repo, commit_id.to_string())
    }

    #[tokio::test]
    async fn resolves_branch_head_and_checks_out() {
        let src = tempfile::tempdir().unwrap();
        let (_, sha) = init_repo(src.path(), "https://github.com/org/app.git");
        let client = GitClient::new();
        let branch = BranchName::parse("main").unwrap();

        let head = client
            .resolve_branch_head(src.path(), &branch)
            .await
            .unwrap();
        assert_eq!(head.sha, sha);
        assert_eq!(head.branch, branch);

        client.checkout_commit(src.path(), &sha).await.unwrap();
    }

    #[tokio::test]
    async fn reads_origin_remote_url() {
        let src = tempfile::tempdir().unwrap();
        init_repo(src.path(), "https://github.com/org/app.git");
        let client = GitClient::new();
        let url = client.remote_url(src.path()).await.unwrap();
        assert_eq!(url.as_str(), "https://github.com/org/app.git");
    }

    #[tokio::test]
    async fn clones_local_repo_into_cache() {
        let src = tempfile::tempdir().unwrap();
        let origin = format!("file://{}", src.path().display());
        init_repo(src.path(), &origin);
        let cache = tempfile::tempdir().unwrap();

        let client = GitClient::new();
        let url = RepoUrl::parse(&origin).unwrap();
        let cloned = client.ensure_repo(&url, cache.path()).await.unwrap();
        assert!(cloned.exists());

        // Reusing the cache does not fail.
        let again = client.ensure_repo(&url, cache.path()).await.unwrap();
        assert_eq!(again, cloned);

        // The cloned repo can resolve its own branch head.
        let branch = BranchName::parse("main").unwrap();
        let head = client.resolve_branch_head(&cloned, &branch).await.unwrap();
        assert_eq!(head.sha.len(), 40);
    }

    #[tokio::test]
    async fn missing_branch_errors() {
        let src = tempfile::tempdir().unwrap();
        init_repo(src.path(), "https://github.com/org/app.git");
        let client = GitClient::new();
        let missing = BranchName::parse("nope").unwrap();
        let err = client
            .resolve_branch_head(src.path(), &missing)
            .await
            .unwrap_err();
        assert!(matches!(err, GitError::Failure(_)));
    }
}
