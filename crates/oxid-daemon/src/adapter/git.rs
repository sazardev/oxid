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

    async fn ensure_repo(
        &self,
        url: &RepoUrl,
        token: Option<&str>,
        cache_dir: &Path,
    ) -> Result<PathBuf, GitError> {
        let url = url.clone();
        let token = token.map(str::to_owned);
        let cache_dir = cache_dir.to_owned();
        tokio::task::spawn_blocking(move || sync_ensure_repo(&url, token.as_deref(), &cache_dir))
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

/// Derives the git-cache subdirectory name for `url`. `pub(crate)` so
/// project deletion (`control_plane.rs::delete_project`) can locate and
/// remove the same directory `ensure_repo` created.
///
/// The name is `<last-segment>-<8 hex chars of SHA-256(url)>`: the short
/// content hash disambiguates same-named repos from different orgs
/// (`orgA/app` and `orgB/app` must not share one cache dir) once arbitrary
/// remotes can be registered by URL. Changing the derivation orphans old
/// cache dirs — each project simply re-clones once after upgrading
/// (self-healing; nothing else keys off these directory names).
pub(crate) fn cache_dir_name(url: &RepoUrl) -> String {
    use sha2::Digest;

    let raw = url.as_str();
    let trimmed = raw.trim_end_matches('/');
    let segment = trimmed.rsplit('/').next().unwrap_or("repo").to_owned();
    let sanitized: String = segment
        .trim_end_matches(".git")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let digest = sha2::Sha256::digest(raw.as_bytes());
    format!("{sanitized}-{}", hex::encode(&digest[..4]))
}

/// Builds the URL actually used for the clone/fetch network operation,
/// injecting `token` as HTTPS userinfo (`x-access-token:<token>@host`) when
/// given — the standard way GitHub (and most other hosts) accept a PAT over
/// HTTPS without a separate credentials callback. Non-`https` URLs (local
/// paths, `file://`, already-authenticated `ssh://`) are returned
/// unchanged; `cache_dir_name` is computed from the original `url`, not
/// this one, so a token (or a rotated one) never ends up encoded into the
/// git-cache directory name on disk.
fn authenticated_url(url: &RepoUrl, token: Option<&str>) -> String {
    let raw = url.as_str();
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return raw.to_owned();
    };
    let Some(rest) = raw.strip_prefix("https://") else {
        return raw.to_owned();
    };
    // Already has embedded userinfo (`user:pass@host/...`) — don't clobber
    // an explicitly-provided credential with the stored token.
    if rest.contains('@') {
        return raw.to_owned();
    }
    format!("https://x-access-token:{token}@{rest}")
}

fn sync_ensure_repo(
    url: &RepoUrl,
    token: Option<&str>,
    cache_dir: &Path,
) -> Result<PathBuf, GitError> {
    std::fs::create_dir_all(cache_dir).map_err(map_err)?;
    let dir = cache_dir.join(cache_dir_name(url));
    let auth_url = authenticated_url(url, token);

    if dir.exists() {
        // A cached clone can be arbitrarily stale: a webhook redeploying a
        // branch that was pushed to *after* the first-ever deploy of this
        // project must see the new commits, not the ones from whenever the
        // cache was first populated.
        fetch_origin(&auth_url, &dir)?;
        return Ok(dir);
    }

    // git2::Repository::clone accepts local paths and file:// URLs.
    let repo = Repository::clone(&auth_url, &dir).map_err(map_err)?;
    // `clone` persists whatever URL it was given as `origin` in
    // `.git/config` — reset it back to the token-free public URL right
    // away so a token never lingers on disk beyond this one clone call.
    // `fetch_origin` never needs to read this back; it always fetches
    // through its own anonymous, re-authenticated remote.
    if auth_url != url.as_str() {
        repo.remote_set_url("origin", url.as_str())
            .map_err(map_err)?;
    }
    Ok(dir)
}

/// Fetches every remote branch into `refs/remotes/origin/*`, always via an
/// anonymous (not persisted to `.git/config`) remote pointed at `auth_url` —
/// deliberately never `find_remote("origin")` + `remote_set_url`, which
/// would write the token into the cache's on-disk git config in plaintext
/// and leave it there indefinitely. Using an anonymous remote means a
/// token added, rotated or removed after the cache already exists still
/// takes effect on the very next redeploy (nothing stale is reused), while
/// never persisting the credential anywhere beyond this one fetch call.
fn fetch_origin(auth_url: &str, dir: &Path) -> Result<(), GitError> {
    let repo = Repository::open(dir).map_err(map_err)?;
    let mut remote = repo.remote_anonymous(auth_url).map_err(map_err)?;
    remote
        .fetch(&["+refs/heads/*:refs/remotes/origin/*"], None, None)
        .map_err(map_err)
}

fn sync_resolve_branch_head(repo_dir: &Path, branch: &BranchName) -> Result<CommitRef, GitError> {
    let repo = Repository::open(repo_dir).map_err(map_err)?;
    // Prefer the remote-tracking ref: `fetch_origin` (in `sync_ensure_repo`)
    // only updates `refs/remotes/origin/*`, it never fast-forwards a local
    // `refs/heads/<branch>`, so on a cache hit the local ref can be stale.
    // `Repository::clone` also only ever materializes a local
    // `refs/heads/<default>` for the branch it checks out — every other
    // branch exists solely under `refs/remotes/origin/*` right after a
    // fresh clone. Falling back to `refs/heads/{branch}` covers repos that
    // were never cloned by Oxid (e.g. worked on directly in tests).
    let oid = repo
        .refname_to_id(&format!("refs/remotes/origin/{branch}"))
        .or_else(|_| repo.refname_to_id(&format!("refs/heads/{branch}")))
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

    #[test]
    fn cache_names_disambiguate_same_named_repos_from_different_orgs() {
        // The whole point of the hash suffix: registering by URL must not
        // let orgA/app and orgB/app share one cache directory.
        let a = RepoUrl::parse("https://github.com/orgA/app.git").unwrap();
        let b = RepoUrl::parse("https://github.com/orgB/app.git").unwrap();
        let name_a = cache_dir_name(&a);
        let name_b = cache_dir_name(&b);
        assert_ne!(name_a, name_b);
        assert!(name_a.starts_with("app-"), "readable prefix kept: {name_a}");

        // Deterministic across calls — project deletion relies on deriving
        // exactly the same name ensure_repo created.
        assert_eq!(name_a, cache_dir_name(&a));

        // A trailing-slash variant of the same URL is a *different* string
        // to git and to idempotency matching; it simply gets its own dir
        // like any other distinct URL.
        assert_ne!(
            name_b,
            cache_dir_name(&RepoUrl::parse("https://github.com/orgA/app").unwrap())
        );
    }

    #[tokio::test]
    async fn clones_local_repo_into_cache() {
        let src = tempfile::tempdir().unwrap();
        let origin = format!("file://{}", src.path().display());
        init_repo(src.path(), &origin);
        let cache = tempfile::tempdir().unwrap();

        let client = GitClient::new();
        let url = RepoUrl::parse(&origin).unwrap();
        let cloned = client.ensure_repo(&url, None, cache.path()).await.unwrap();
        assert!(cloned.exists());

        // Reusing the cache does not fail.
        let again = client.ensure_repo(&url, None, cache.path()).await.unwrap();
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

    /// Regression test: a fresh clone only checks out the default branch
    /// locally (`refs/heads/main`); every other branch lands in
    /// `refs/remotes/origin/*`. `deploy()` always calls `ensure_repo` then
    /// `resolve_branch_head` back to back, so resolving a non-default
    /// branch right after the first-ever clone of a project must work.
    #[tokio::test]
    async fn resolves_non_default_branch_after_fresh_clone() {
        let src = tempfile::tempdir().unwrap();
        let origin = format!("file://{}", src.path().display());
        let (repo, main_sha) = init_repo(src.path(), &origin);

        let feature_sha = {
            let head_commit = repo
                .find_commit(git2::Oid::from_str(&main_sha).unwrap())
                .unwrap();
            fs::write(src.path().join("feature.txt"), "hi").unwrap();
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("feature.txt")).unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let signature = Signature::now("oxid test", "oxid@test.local").unwrap();
            let commit_id = repo
                .commit(
                    None,
                    &signature,
                    &signature,
                    "feature",
                    &tree,
                    &[&head_commit],
                )
                .unwrap();
            repo.branch("feature-one", &repo.find_commit(commit_id).unwrap(), true)
                .unwrap();
            commit_id.to_string()
        };

        let cache = tempfile::tempdir().unwrap();
        let client = GitClient::new();
        let url = RepoUrl::parse(&origin).unwrap();
        let cloned = client.ensure_repo(&url, None, cache.path()).await.unwrap();

        let branch = BranchName::parse("feature-one").unwrap();
        let head = client.resolve_branch_head(&cloned, &branch).await.unwrap();
        assert_eq!(head.sha, feature_sha);

        client.checkout_commit(&cloned, &feature_sha).await.unwrap();
        assert!(cloned.join("feature.txt").exists());
    }

    /// Regression test: redeploying an already-cached project must see
    /// commits pushed to `origin` after the cache was first populated, not
    /// a frozen snapshot from the first clone.
    #[tokio::test]
    async fn ensure_repo_fetches_new_commits_on_cache_hit() {
        let src = tempfile::tempdir().unwrap();
        let origin = format!("file://{}", src.path().display());
        let (repo, _) = init_repo(src.path(), &origin);

        let cache = tempfile::tempdir().unwrap();
        let client = GitClient::new();
        let url = RepoUrl::parse(&origin).unwrap();
        client.ensure_repo(&url, None, cache.path()).await.unwrap();

        // Push a new commit to `main` in the source repo after the cache
        // already exists.
        let new_sha = {
            let head = repo.head().unwrap().peel_to_commit().unwrap();
            fs::write(src.path().join("new.txt"), "hi").unwrap();
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("new.txt")).unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let signature = Signature::now("oxid test", "oxid@test.local").unwrap();
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "second",
                &tree,
                &[&head],
            )
            .unwrap()
            .to_string()
        };

        let cloned = client.ensure_repo(&url, None, cache.path()).await.unwrap();
        let branch = BranchName::parse("main").unwrap();
        let head = client.resolve_branch_head(&cloned, &branch).await.unwrap();
        assert_eq!(head.sha, new_sha, "cache hit did not fetch the new commit");
    }

    #[test]
    fn authenticated_url_injects_token_only_for_plain_https() {
        let https = RepoUrl::parse("https://github.com/org/app.git").unwrap();
        assert_eq!(
            authenticated_url(&https, Some("ghp_abc123")),
            "https://x-access-token:ghp_abc123@github.com/org/app.git"
        );

        // No token given: unchanged.
        assert_eq!(
            authenticated_url(&https, None),
            "https://github.com/org/app.git"
        );
        assert_eq!(
            authenticated_url(&https, Some("")),
            "https://github.com/org/app.git"
        );

        // Already has embedded userinfo: never clobbered.
        let with_creds = RepoUrl::parse("https://user:pass@github.com/org/app.git").unwrap();
        assert_eq!(
            authenticated_url(&with_creds, Some("ghp_abc123")),
            "https://user:pass@github.com/org/app.git"
        );

        // Non-https (local/file://): never touched, even with a token set.
        let local = RepoUrl::parse("file:///tmp/repo").unwrap();
        assert_eq!(
            authenticated_url(&local, Some("ghp_abc123")),
            "file:///tmp/repo"
        );
    }

    /// Regression guard: cloning with a token must not leave it sitting in
    /// the cache's on-disk `.git/config` afterward — `sync_ensure_repo`
    /// resets `origin` back to the token-free URL right after `clone`.
    #[tokio::test]
    async fn ensure_repo_does_not_persist_the_token_in_git_config() {
        let src = tempfile::tempdir().unwrap();
        let origin = format!("file://{}", src.path().display());
        init_repo(src.path(), &origin);
        let cache = tempfile::tempdir().unwrap();

        let client = GitClient::new();
        let url = RepoUrl::parse(&origin).unwrap();
        // `file://` URLs are never token-injected (see
        // `authenticated_url`), so this exercises the "unchanged" path
        // safely without needing a real authenticated remote — the
        // property under test is specifically that whatever `origin` ends
        // up as on disk exactly matches the token-free `url`, never a
        // token-embedded variant.
        let cloned = client
            .ensure_repo(&url, Some("should-never-be-persisted"), cache.path())
            .await
            .unwrap();

        let repo = Repository::open(&cloned).unwrap();
        let remote = repo.find_remote("origin").unwrap();
        assert_eq!(remote.url().unwrap(), origin);
        assert!(
            !std::fs::read_to_string(cloned.join(".git/config"))
                .unwrap()
                .contains("should-never-be-persisted"),
            "token must never be written to .git/config"
        );
    }
}
