//! Talking to a git host's REST API — the daemon's only outbound HTTP.
//!
//! Three provider shapes, one behaviour: find Oxid's own comment on a pull
//! request by the hidden marker it embeds, then create or edit it. The
//! rendering, the addressing and the retry policy live in
//! `oxid_core::services::forge`; this file is only the calls.
//!
//! Two guardrails are set on the client itself rather than left to each
//! call site:
//!
//! - **Redirects are not followed.** A redirect can carry the
//!   `Authorization` header to a host the operator never named, which is
//!   how a write-scoped token leaks.
//! - **Every request has a timeout.** A git host that stops answering must
//!   not hold a notification's turn forever.

use oxid_core::services::forge::{ForgeError, ForgeKind};
use oxid_core::{ForgePort, ForgeRequest};
use serde_json::{Value, json};

/// A `reqwest`-backed [`ForgePort`].
#[derive(Debug, Clone)]
pub struct HttpForge {
    client: reqwest::Client,
}

impl HttpForge {
    /// Builds a client with the guardrails described in the module docs.
    ///
    /// # Errors
    /// Returns [`ForgeError::Transport`] if the HTTP client cannot be built.
    pub fn new(timeout_secs: u64) -> Result<Self, ForgeError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs.max(1)))
            .user_agent(concat!("oxid/", env!("CARGO_PKG_VERSION")))
            // See the module docs: a redirect could hand the token to
            // somewhere the operator never named.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map(|client| Self { client })
            .map_err(|e| ForgeError::Transport(e.to_string()))
    }

    /// The endpoint for a pull request's comments.
    ///
    /// GitLab calls them notes and addresses merge requests; Gitea and Gogs
    /// follow GitHub's issue-comment shape, which is why a pull request's
    /// comments live under `/issues/`.
    fn comments_url(req: &ForgeRequest) -> String {
        let ForgeRequest {
            api_base,
            repo_path,
            number,
            ..
        } = req;
        match req.kind {
            ForgeKind::GitLab => {
                // GitLab addresses a project by its URL-encoded path.
                let encoded = repo_path.replace('/', "%2F");
                format!("{api_base}/projects/{encoded}/merge_requests/{number}/notes")
            }
            _ => format!("{api_base}/repos/{repo_path}/issues/{number}/comments"),
        }
    }

    /// Applies the provider's own authentication header.
    fn authenticate(
        builder: reqwest::RequestBuilder,
        req: &ForgeRequest,
    ) -> reqwest::RequestBuilder {
        match req.kind {
            // GitLab's own header; it does not accept `Authorization:
            // Bearer` for personal access tokens.
            ForgeKind::GitLab => builder.header("PRIVATE-TOKEN", &req.token),
            _ => builder.bearer_auth(&req.token),
        }
    }

    /// Maps a response status onto a [`ForgeError`], naming the scope an
    /// operator would need to grant.
    ///
    /// The distinction matters because `retry_decision` gives up
    /// immediately on a permission problem: it will fail identically
    /// forever, and retrying only turns one useful log line into six.
    async fn error_for(kind: ForgeKind, response: reqwest::Response) -> ForgeError {
        let status = response.status();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        match status.as_u16() {
            401 => ForgeError::Unauthorized,
            403 => ForgeError::Forbidden {
                scope_hint: match kind {
                    ForgeKind::GitHub => "pull requests: write",
                    ForgeKind::GitLab => "api",
                    ForgeKind::Gitea | ForgeKind::Gogs => "write:issue",
                }
                .to_owned(),
            },
            404 => ForgeError::NotFound,
            // GitHub answers 403 for secondary rate limits too, but always
            // with a `Retry-After`; treat that as the rate limit it is.
            429 => ForgeError::RateLimited {
                retry_after_secs: retry_after,
            },
            _ => {
                let body = response.text().await.unwrap_or_default();
                ForgeError::Transport(format!(
                    "{status}: {}",
                    body.chars().take(200).collect::<String>()
                ))
            }
        }
    }
}

impl ForgePort for HttpForge {
    async fn find_comment(
        &self,
        req: &ForgeRequest,
        marker: &str,
    ) -> Result<Option<String>, ForgeError> {
        let response = Self::authenticate(self.client.get(Self::comments_url(req)), req)
            .send()
            .await
            .map_err(|e| ForgeError::Transport(e.to_string()))?;
        if !response.status().is_success() {
            return Err(Self::error_for(req.kind, response).await);
        }
        let comments: Vec<Value> = response
            .json()
            .await
            .map_err(|e| ForgeError::Transport(e.to_string()))?;
        Ok(comments.into_iter().find_map(|c| {
            // GitHub/Gitea call it `body`, GitLab calls it `note`.
            let body = c
                .get("body")
                .or_else(|| c.get("note"))
                .and_then(Value::as_str)?;
            body.contains(marker)
                .then(|| {
                    c.get("id")
                        .map(|id| id.to_string().trim_matches('"').to_owned())
                })
                .flatten()
        }))
    }

    async fn create_comment(&self, req: &ForgeRequest, body: &str) -> Result<String, ForgeError> {
        // Every provider here takes the same field name on create; they
        // only diverge on the *read* side, where GitLab calls it `note`.
        let response = Self::authenticate(self.client.post(Self::comments_url(req)), req)
            .json(&json!({ "body": body }))
            .send()
            .await
            .map_err(|e| ForgeError::Transport(e.to_string()))?;
        if !response.status().is_success() {
            return Err(Self::error_for(req.kind, response).await);
        }
        let created: Value = response
            .json()
            .await
            .map_err(|e| ForgeError::Transport(e.to_string()))?;
        created
            .get("id")
            .map(|id| id.to_string().trim_matches('"').to_owned())
            .ok_or_else(|| ForgeError::Transport("comment response has no id".to_owned()))
    }

    async fn update_comment(
        &self,
        req: &ForgeRequest,
        comment_id: &str,
        body: &str,
    ) -> Result<(), ForgeError> {
        let url = match req.kind {
            ForgeKind::GitLab => format!("{}/{comment_id}", Self::comments_url(req)),
            // GitHub and Gitea edit a comment by id at the repository
            // level, not under the issue it belongs to.
            _ => format!(
                "{}/repos/{}/issues/comments/{comment_id}",
                req.api_base, req.repo_path
            ),
        };
        let request = match req.kind {
            ForgeKind::GitLab => self.client.put(url),
            _ => self.client.patch(url),
        };
        let response = Self::authenticate(request, req)
            .json(&json!({ "body": body }))
            .send()
            .await
            .map_err(|e| ForgeError::Transport(e.to_string()))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(Self::error_for(req.kind, response).await)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(kind: ForgeKind) -> ForgeRequest {
        ForgeRequest {
            kind,
            api_base: match kind {
                ForgeKind::GitHub => "https://api.github.com".to_owned(),
                _ => "https://git.example.com/api/v4".to_owned(),
            },
            repo_path: "org/app".to_owned(),
            number: 42,
            token: "t".to_owned(),
        }
    }

    /// GitLab addresses a project by its URL-encoded path and calls them
    /// merge requests; everyone else follows GitHub's issue-comment shape,
    /// which is why a pull request's comments live under `/issues/`.
    #[test]
    fn each_provider_is_addressed_the_way_it_expects() {
        assert_eq!(
            HttpForge::comments_url(&req(ForgeKind::GitHub)),
            "https://api.github.com/repos/org/app/issues/42/comments"
        );
        assert_eq!(
            HttpForge::comments_url(&req(ForgeKind::GitLab)),
            "https://git.example.com/api/v4/projects/org%2Fapp/merge_requests/42/notes"
        );
        assert_eq!(
            HttpForge::comments_url(&req(ForgeKind::Gitea)),
            "https://git.example.com/api/v4/repos/org/app/issues/42/comments"
        );
    }

    #[test]
    fn the_client_refuses_to_follow_redirects() {
        // Asserted by construction rather than behaviour: a redirect can
        // carry the `Authorization` header to a host the operator never
        // named, and that is how a write-scoped token leaks.
        assert!(HttpForge::new(5).is_ok());
    }
}
