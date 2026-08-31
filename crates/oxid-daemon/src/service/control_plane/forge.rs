//! Sending queued preview states to the git host.
//!
//! Deliberately shaped like the deploy queue next door: persisted, drained
//! on the scheduler's tick, single-flighted, with a bounded retry. The
//! reason is the same one — a webhook is answered long before a build
//! finishes, so anything that has to happen *after* a deploy resolves needs
//! somewhere to live that survives a restart.
//!
//! What is different is that failure here must never be visible to the
//! deploy. A comment is a courtesy; an environment is the product.

use oxid_core::services::forge::{
    CommentContext, ForgeKind, PreviewState, RetryDecision, api_base_for, comment_marker,
    render_comment, retry_decision,
};
use oxid_core::{ContainerPort, ForgePort, ForgeRequest, GitPort, ProjectId};

use super::ControlPlane;
use super::error::CpError;

/// How many notifications one drain pass sends.
const DRAIN_BATCH: u32 = 8;

/// How many times a retryable failure is retried before giving up.
const MAX_ATTEMPTS: u32 = 6;

impl<G: GitPort, O: ContainerPort> ControlPlane<G, O> {
    /// Sends every notification that is due.
    ///
    /// Single-flighted like the deploy drain: two passes running together
    /// would both read the same pending row and comment twice.
    ///
    /// # Errors
    /// Returns [`CpError`] only for storage failures. A git host refusing a
    /// call is handled per notification and never fails the pass — one
    /// project's bad token must not stop every other project's comments.
    pub async fn drain_forge_notifications<F: ForgePort>(&self, forge: &F) -> Result<(), CpError> {
        let Ok(_drain) = self.forge_drain_lock.try_lock() else {
            return Ok(());
        };

        for pending in self.store.due_forge_notifications(DRAIN_BATCH).await? {
            let outcome = self.send_notification(forge, &pending).await;
            match outcome {
                Ok(()) => self.store.remove_forge_notification(pending.id).await?,
                Err(err) => {
                    match retry_decision(&err, pending.attempts + 1, MAX_ATTEMPTS) {
                        RetryDecision::RetryIn(secs) => {
                            self.store
                                .defer_forge_notification(pending.id, secs)
                                .await?;
                        }
                        RetryDecision::GiveUp(reason) => {
                            // Warn, not error: the deploy this describes
                            // succeeded or failed on its own merits, and an
                            // operator reading logs for a broken deploy
                            // should not find this at the same severity.
                            tracing::warn!(
                                project_id = pending.project_id.0,
                                branch = %pending.branch,
                                reason = %reason,
                                "gave up telling the git host about a preview"
                            );
                            self.store.remove_forge_notification(pending.id).await?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Posts or edits the one comment for a notification.
    async fn send_notification<F: ForgePort>(
        &self,
        forge: &F,
        pending: &crate::adapter::store::PendingNotification,
    ) -> Result<(), oxid_core::services::forge::ForgeError> {
        let Some((req, marker)) = self.forge_request_for(pending).await else {
            // No pull request, no token, or no forge recorded: nothing to
            // do, and not a failure. A branch without a PR is the ordinary
            // case.
            return Ok(());
        };

        let body = render_comment(&CommentContext {
            project_id: pending.project_id.0,
            branch: pending.branch.clone(),
            state: match pending.state.as_str() {
                "ready" => PreviewState::Ready {
                    url: pending.url.clone().unwrap_or_default(),
                },
                "failed" => PreviewState::Failed {
                    reason: pending.detail.clone().unwrap_or_default(),
                },
                "destroyed" => PreviewState::Destroyed,
                _ => PreviewState::Building,
            },
            commit: pending.commit_sha.clone(),
            detail: pending.detail.clone(),
        });

        // Prefer the stored id, and fall back to finding the comment by its
        // marker. The fallback is what makes a stale id — a restored
        // backup, a comment someone deleted — cost one lookup instead of a
        // second comment on every push from then on.
        let existing = match self.stored_comment_id(pending).await {
            Some(id) => Some(id),
            None => forge.find_comment(&req, &marker).await?,
        };

        match existing {
            Some(id) => match forge.update_comment(&req, &id, &body).await {
                Ok(()) => Ok(()),
                // The comment was deleted between storing its id and now.
                // Forget it and post again on the next pass rather than
                // giving up on the pull request forever.
                Err(oxid_core::services::forge::ForgeError::NotFound) => {
                    let _ = self
                        .store
                        .set_pull_request_comment(pending.project_id, req.number, None)
                        .await;
                    let id = forge.create_comment(&req, &body).await?;
                    let _ = self
                        .store
                        .set_pull_request_comment(pending.project_id, req.number, Some(&id))
                        .await;
                    Ok(())
                }
                Err(e) => Err(e),
            },
            None => {
                let id = forge.create_comment(&req, &body).await?;
                let _ = self
                    .store
                    .set_pull_request_comment(pending.project_id, req.number, Some(&id))
                    .await;
                Ok(())
            }
        }
    }

    /// Everything a call needs, or `None` when this branch has nothing to
    /// comment on.
    async fn forge_request_for(
        &self,
        pending: &crate::adapter::store::PendingNotification,
    ) -> Option<(ForgeRequest, String)> {
        let project = self
            .store
            .get_project_forge(pending.project_id)
            .await
            .ok()??;
        let (number, _) = self
            .store
            .open_pull_request_for_branch(pending.project_id, &pending.branch)
            .await
            .ok()??;
        let kind: ForgeKind = project.forge.parse().ok()?;
        let origin = repo_origin(&project.repo_url)?;
        Some((
            ForgeRequest {
                kind,
                api_base: project
                    .api_base
                    .unwrap_or_else(|| api_base_for(kind, &origin)),
                repo_path: repo_path(&project.repo_url)?,
                number,
                token: project.token,
            },
            comment_marker(pending.project_id.0),
        ))
    }

    /// The comment id already stored for this branch's pull request.
    async fn stored_comment_id(
        &self,
        pending: &crate::adapter::store::PendingNotification,
    ) -> Option<String> {
        self.store
            .open_pull_request_for_branch(pending.project_id, &pending.branch)
            .await
            .ok()?
            .and_then(|(_, comment_id)| comment_id)
    }
}

/// The origin of a repository URL — scheme, host and port — which is where
/// its API lives.
///
/// The port and the scheme are both load-bearing: a self-hosted forge on
/// `http://host:3000` is the normal case, and keeping only the hostname
/// addressed `https://host` instead, where nothing answered. An scp-style
/// remote (`git@host:org/repo`) has no scheme of its own, so it gets
/// `https`, which is what such a host serves its API on.
fn repo_origin(url: &str) -> Option<String> {
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (scheme, rest),
        None => ("https", url),
    };
    let rest = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
    // For scp-style the `:` separates host from path, not host from port,
    // so only a numeric tail counts as a port.
    let authority = rest.split('/').next()?;
    let authority = match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => {
            format!("{host}:{port}")
        }
        Some((host, _)) => host.to_owned(),
        None => authority.to_owned(),
    };
    (!authority.is_empty()).then(|| format!("{scheme}://{authority}"))
}

/// The `owner/repo` part of a repository URL.
///
/// Has to know about ports for the same reason [`repo_origin`] does:
/// splitting on the first `:` turns `http://host:3000/org/app` into a path
/// of `3000/org/app`. A URL with a scheme separates the path with `/`; an
/// scp-style remote separates it with `:`.
fn repo_path(url: &str) -> Option<String> {
    let has_scheme = url.contains("://");
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
    let path = if has_scheme {
        rest.split_once('/')?.1
    } else {
        // scp-style: `host:org/repo`.
        rest.split_once(':')?.1
    };
    let path = path.trim_matches('/').trim_end_matches(".git");
    (!path.is_empty()).then(|| path.to_owned())
}

/// What a project knows about its git host.
#[derive(Debug, Clone)]
pub struct ProjectForge {
    /// `github`/`gitlab`/`gitea`/`gogs`.
    pub forge: String,
    /// The repository URL, for deriving the API base and the path.
    pub repo_url: String,
    /// An explicit API base, when the usual derivation is wrong.
    pub api_base: Option<String>,
    /// The write-scoped credential, decrypted.
    pub token: String,
}

#[allow(unused_imports)]
use ProjectId as _EnsureProjectIdUsed;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repository_url_yields_the_origin_its_api_lives_on() {
        for (url, origin, path) in [
            (
                "https://github.com/org/app.git",
                "https://github.com",
                "org/app",
            ),
            // scp-style has no scheme and its `:` is a path separator, not
            // a port.
            (
                "git@github.com:org/app.git",
                "https://github.com",
                "org/app",
            ),
            (
                "https://token@git.acme.internal/team/svc",
                "https://git.acme.internal",
                "team/svc",
            ),
            // The case that actually broke against a real Gitea.
            (
                "http://10.0.0.2:3000/oxidbot/app.git",
                "http://10.0.0.2:3000",
                "oxidbot/app",
            ),
        ] {
            assert_eq!(repo_origin(url).as_deref(), Some(origin), "{url}");
            assert_eq!(repo_path(url).as_deref(), Some(path), "{url}");
        }
    }
}
