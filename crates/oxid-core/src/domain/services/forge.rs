//! Telling a Git host about a preview: which pull request, what to say, and
//! when to give up.
//!
//! The awkward fact this is shaped around: **a push webhook does not carry a
//! pull-request number**, and the webhook is answered before anything is
//! built — so at the moment Oxid knows a branch was pushed, it knows neither
//! which PR that branch belongs to nor what URL the preview will have.
//!
//! So the association is learned separately, from the `pull_request` /
//! `merge_request` deliveries that already arrive and are currently
//! discarded, and the comment is written later, when the deploy resolves.
//!
//! Pure: rendering, addressing and retry policy live here so they can be
//! tested without a network. The adapter in `oxid-daemon` makes the calls.

use serde::{Deserialize, Serialize};

/// Which Git host a project lives on.
///
/// Learned from the webhook route a delivery arrived on, because nothing
/// else in a payload reliably says: a self-hosted Gitea and a GitLab both
/// answer at arbitrary domains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgeKind {
    /// GitHub, or GitHub Enterprise.
    GitHub,
    /// GitLab, self-hosted or not.
    GitLab,
    /// Gitea.
    Gitea,
    /// Gogs, which shares Gitea's API shape.
    Gogs,
}

impl ForgeKind {
    /// The wire and CLI spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::GitLab => "gitlab",
            Self::Gitea => "gitea",
            Self::Gogs => "gogs",
        }
    }

    /// Every kind, for `--help` text and validation.
    #[must_use]
    pub const fn all() -> [Self; 4] {
        [Self::GitHub, Self::GitLab, Self::Gitea, Self::Gogs]
    }
}

impl std::str::FromStr for ForgeKind {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let raw = raw.trim().to_ascii_lowercase();
        Self::all()
            .into_iter()
            .find(|k| k.as_str() == raw)
            .ok_or_else(|| format!("unknown git host `{raw}`"))
    }
}

impl std::fmt::Display for ForgeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The API root for a host, derived from the repository's own URL.
///
/// This is how self-hosting is supported without asking anyone to configure
/// a second URL: the host is already in `repo_url`, and each forge puts its
/// API at a fixed path under it. Only an unusual deployment needs an
/// override.
#[must_use]
pub fn api_base_for(kind: ForgeKind, host: &str) -> String {
    let host = host.trim_end_matches('/');
    match kind {
        // github.com's API lives on a different host entirely; GitHub
        // Enterprise puts it under the same one.
        ForgeKind::GitHub if host.eq_ignore_ascii_case("github.com") => {
            "https://api.github.com".to_owned()
        }
        ForgeKind::GitHub => format!("https://{host}/api/v3"),
        ForgeKind::GitLab => format!("https://{host}/api/v4"),
        ForgeKind::Gitea | ForgeKind::Gogs => format!("https://{host}/api/v1"),
    }
}

/// Where a preview stands, as the comment will report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum PreviewState {
    /// The image is being built.
    Building,
    /// Live, at this URL.
    Ready {
        /// The address a person can open.
        url: String,
    },
    /// The build or first start failed.
    Failed {
        /// One line saying what broke.
        reason: String,
    },
    /// The environment is gone — the branch was deleted, or its lifetime
    /// ran out.
    Destroyed,
}

/// A hidden marker identifying Oxid's own comment on a pull request.
///
/// The recovery path when a stored comment id goes stale: after a database
/// restore, or when someone deletes the comment, the id points at nothing
/// and the marker is how the existing comment is found again instead of
/// posting a second one.
#[must_use]
pub fn comment_marker(project_id: u64) -> String {
    format!("<!-- oxid:preview:project={project_id} -->")
}

/// What the comment says.
#[derive(Debug, Clone)]
pub struct CommentContext {
    /// Which project, for the marker.
    pub project_id: u64,
    /// The branch being previewed.
    pub branch: String,
    /// Its state.
    pub state: PreviewState,
    /// Short commit sha, when known.
    pub commit: Option<String>,
}

/// Renders the whole comment body, marker included.
///
/// One comment per pull request, edited in place on every push — not one
/// comment per push. A bot that appends to a busy PR is a bot people mute.
#[must_use]
pub fn render_comment(ctx: &CommentContext) -> String {
    let marker = comment_marker(ctx.project_id);
    let (status, detail) = match &ctx.state {
        PreviewState::Building => ("⏳ building".to_owned(), String::new()),
        PreviewState::Ready { url } => ("✅ ready".to_owned(), format!("<{url}>")),
        PreviewState::Failed { reason } => {
            // The reason can be a whole build log line; a comment is not the
            // place for it, and the logs are one command away.
            let reason = reason.lines().next().unwrap_or(reason);
            let reason: String = reason.chars().take(200).collect();
            ("❌ failed".to_owned(), format!("`{reason}`"))
        }
        PreviewState::Destroyed => ("🗑 destroyed".to_owned(), String::new()),
    };
    let commit = ctx
        .commit
        .as_ref()
        .map(|c| format!(" · `{}`", c.chars().take(7).collect::<String>()))
        .unwrap_or_default();

    format!(
        "{marker}\n### Oxid preview\n\n| Branch | Status | Preview |\n\
         |---|---|---|\n| `{}`{commit} | {status} | {detail} |\n",
        ctx.branch
    )
}

/// Why a call to the Git host failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgeError {
    /// The credential was rejected.
    Unauthorized,
    /// The credential is valid but lacks the scope this needs.
    Forbidden {
        /// The scope to ask for, named so the operator can fix it.
        scope_hint: String,
    },
    /// The pull request or comment is gone.
    NotFound,
    /// The host is asking for a pause.
    RateLimited {
        /// Seconds to wait, when the host said.
        retry_after_secs: Option<u64>,
    },
    /// Anything else — DNS, TLS, a 500, a timeout.
    Transport(String),
}

/// What to do after a failed attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryDecision {
    /// Stop, for this reason.
    GiveUp(String),
    /// Try again after this many seconds.
    RetryIn(u64),
}

/// How long to wait before retrying, or whether to stop.
///
/// The distinction that matters: a missing scope or a rejected token will
/// fail identically forever, so retrying only turns one useless log line
/// into six. A rate limit or a transport blip is exactly what retrying is
/// for.
#[must_use]
pub fn retry_decision(err: &ForgeError, attempts: u32, max_attempts: u32) -> RetryDecision {
    match err {
        ForgeError::Unauthorized => RetryDecision::GiveUp(
            "the git host rejected the token — reissue it with `oxid project forge-token`"
                .to_owned(),
        ),
        ForgeError::Forbidden { scope_hint } => RetryDecision::GiveUp(format!(
            "the token is valid but lacks the `{scope_hint}` scope — reissue it with that \
             permission"
        )),
        ForgeError::NotFound => {
            RetryDecision::GiveUp("the pull request or comment no longer exists".to_owned())
        }
        _ if attempts >= max_attempts => RetryDecision::GiveUp(format!(
            "gave up after {attempts} attempts: {}",
            match err {
                ForgeError::RateLimited { .. } => "still rate limited".to_owned(),
                ForgeError::Transport(e) => e.clone(),
                _ => "unreachable".to_owned(),
            }
        )),
        ForgeError::RateLimited { retry_after_secs } => {
            // Honour the host's own number when it gave one: it knows when
            // the window resets and guessing shorter just burns the budget.
            RetryDecision::RetryIn(retry_after_secs.unwrap_or_else(|| backoff_secs(attempts)))
        }
        ForgeError::Transport(_) => RetryDecision::RetryIn(backoff_secs(attempts)),
    }
}

/// Exponential backoff, capped so a stuck notification does not drift into
/// never being retried at all.
fn backoff_secs(attempts: u32) -> u64 {
    const CAP: u64 = 900;
    30u64.saturating_mul(1 << attempts.min(5)).min(CAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(state: PreviewState) -> CommentContext {
        CommentContext {
            project_id: 7,
            branch: "feature/carrito".to_owned(),
            state,
            commit: Some("8dfe800b7c8f8d2".to_owned()),
        }
    }

    #[test]
    fn a_self_hosted_forge_is_addressed_from_its_own_repository_url() {
        // The whole reason this is derived rather than configured: nobody
        // should have to tell Oxid where their Gitea's API is when the
        // repository URL already said.
        assert_eq!(
            api_base_for(ForgeKind::GitHub, "github.com"),
            "https://api.github.com"
        );
        assert_eq!(
            api_base_for(ForgeKind::GitHub, "git.acme.internal"),
            "https://git.acme.internal/api/v3"
        );
        assert_eq!(
            api_base_for(ForgeKind::GitLab, "gitlab.acme.internal"),
            "https://gitlab.acme.internal/api/v4"
        );
        assert_eq!(
            api_base_for(ForgeKind::Gitea, "gitea.acme.internal/"),
            "https://gitea.acme.internal/api/v1"
        );
    }

    #[test]
    fn every_comment_carries_the_marker_that_finds_it_again() {
        // The stored comment id can go stale — a restored backup, a deleted
        // comment — and the marker is what stops that becoming a second
        // comment on every push from then on.
        for state in [
            PreviewState::Building,
            PreviewState::Ready {
                url: "https://x.example.com/".to_owned(),
            },
            PreviewState::Failed {
                reason: "boom".to_owned(),
            },
            PreviewState::Destroyed,
        ] {
            let body = render_comment(&ctx(state));
            assert!(body.contains(&comment_marker(7)), "{body}");
            assert!(body.contains("feature/carrito"), "{body}");
        }
    }

    #[test]
    fn a_ready_comment_links_the_preview_and_a_failed_one_does_not() {
        let ready = render_comment(&ctx(PreviewState::Ready {
            url: "https://feature-carrito.app.example.com/".to_owned(),
        }));
        assert!(ready.contains("https://feature-carrito.app.example.com/"));
        assert!(ready.contains("ready"));

        let failed = render_comment(&ctx(PreviewState::Failed {
            reason: "npm ci failed".to_owned(),
        }));
        assert!(
            !failed.contains("http"),
            "a failed preview has no URL to give"
        );
        assert!(failed.contains("npm ci failed"));
    }

    /// A build error can be a whole log. A PR comment is not where it goes.
    #[test]
    fn a_failure_reason_is_trimmed_to_one_short_line() {
        let long = format!("first line\n{}", "x".repeat(5_000));
        let body = render_comment(&ctx(PreviewState::Failed { reason: long }));
        assert!(body.contains("first line"));
        assert!(body.len() < 400, "comment was {} bytes", body.len());
    }

    #[test]
    fn a_missing_scope_is_never_retried_and_names_the_scope() {
        let err = ForgeError::Forbidden {
            scope_hint: "issues:write".to_owned(),
        };
        let RetryDecision::GiveUp(reason) = retry_decision(&err, 1, 6) else {
            panic!("a permission error will fail identically forever");
        };
        assert!(reason.contains("issues:write"), "{reason}");

        assert!(matches!(
            retry_decision(&ForgeError::Unauthorized, 1, 6),
            RetryDecision::GiveUp(_)
        ));
        assert!(matches!(
            retry_decision(&ForgeError::NotFound, 1, 6),
            RetryDecision::GiveUp(_)
        ));
    }

    #[test]
    fn a_rate_limit_waits_as_long_as_the_host_asked() {
        assert_eq!(
            retry_decision(
                &ForgeError::RateLimited {
                    retry_after_secs: Some(120)
                },
                1,
                6
            ),
            RetryDecision::RetryIn(120)
        );
        // With no hint, back off rather than hammer.
        let RetryDecision::RetryIn(secs) = retry_decision(
            &ForgeError::RateLimited {
                retry_after_secs: None,
            },
            1,
            6,
        ) else {
            panic!("a rate limit is exactly what retrying is for");
        };
        assert!(secs >= 30);
    }

    #[test]
    fn backoff_grows_but_stays_bounded_and_eventually_gives_up() {
        let a = retry_decision(&ForgeError::Transport("dns".to_owned()), 1, 6);
        let b = retry_decision(&ForgeError::Transport("dns".to_owned()), 4, 6);
        let (RetryDecision::RetryIn(a), RetryDecision::RetryIn(b)) = (a, b) else {
            panic!("a transport blip is retryable");
        };
        assert!(b > a, "backoff should grow: {a} then {b}");
        assert!(b <= 900, "and stay bounded: {b}");

        assert!(matches!(
            retry_decision(&ForgeError::Transport("dns".to_owned()), 6, 6),
            RetryDecision::GiveUp(_)
        ));
    }

    #[test]
    fn forge_kinds_round_trip_through_their_wire_name() {
        for kind in ForgeKind::all() {
            assert_eq!(kind.as_str().parse::<ForgeKind>().unwrap(), kind);
        }
        assert!("bitbucket".parse::<ForgeKind>().is_err());
    }
}
