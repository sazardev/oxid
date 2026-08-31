//! Decides which branches a webhook push is allowed to deploy.
//!
//! A preview-environment product deploys every branch by default, because
//! that *is* the product: push a branch, get a URL. That default stops being
//! reasonable at a repository with two hundred branches, where most of them
//! are someone's abandoned experiment and building all of them costs an
//! image, disk and a queue slot each.
//!
//! So a project can name the branches worth deploying, and cap how many
//! environments it may hold. Three rules make this predictable:
//!
//! - **Only webhooks are filtered.** `oxid up <branch>` is a person asking
//!   for a specific branch, and a person asking is always right. The filter
//!   answers "should a *push* have deployed this", not "may this branch
//!   exist".
//! - **The decision comes from the branch name, never the commit.** A commit
//!   message is per-push, so a rule based on it has no answer for the second
//!   push: destroying a live environment and leaving it serving stale code
//!   are both wrong, and nobody can predict which they will get. A branch
//!   name gives the same answer on every push of its life, including the one
//!   that deletes it.
//! - **An empty allowlist means everything.** The filter is opt-in; a
//!   project that never configures one keeps the behaviour it had.
//!
//! The cap is a backstop for the filter, not a duplicate of it: a filter
//! only helps when someone wrote it correctly, and `["relase/*"]` is a typo
//! that silently deploys nothing — or `["*"]` one that deploys everything.
//! The cap holds regardless of how the patterns turned out.

use serde::{Deserialize, Serialize};

/// Which branches a project deploys from a push, and how many environments
/// it may hold (`[deploy]` block of `oxid.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DeployConfig {
    /// Glob patterns a branch must match to deploy. **Empty means every
    /// branch**, which is the default and the behaviour of a project that
    /// never configured this.
    pub branches: Vec<String>,
    /// Glob patterns that refuse a branch outright. Checked before
    /// `branches`, so an exclusion cannot be re-enabled by a broad
    /// allowlist — `ignore` is the more specific statement of the two.
    pub ignore: Vec<String>,
    /// Most environments this project may hold at once. A redeploy of a
    /// branch that already has one is never blocked by this; only a branch
    /// that would add to the count.
    pub max_environments: Option<u32>,
}

/// What the project currently holds, at the moment a push arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectLoad {
    /// Environments that exist and are not destroyed.
    pub live_environments: u32,
    /// Whether *this* branch is one of them. A redeploy replaces an
    /// environment rather than adding one, so the cap does not apply to it —
    /// otherwise reaching the cap would freeze every branch that is already
    /// running, which is the opposite of protecting the host.
    pub branch_already_live: bool,
}

/// Why a push was not deployed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The branch matched a pattern in `ignore`.
    Ignored(String),
    /// The project has an allowlist and the branch matched none of it.
    NotAllowed,
    /// The project is at `max_environments` and this branch would add one.
    AtCapacity(u32),
}

impl SkipReason {
    /// A one-line explanation for the audit trail and the webhook response.
    ///
    /// Deliberately not translated: it is recorded in the audit trail and
    /// returned to a Git host's delivery log, both of which are searched by
    /// their text.
    #[must_use]
    pub fn describe(&self, branch: &str) -> String {
        match self {
            Self::Ignored(pattern) => {
                format!("branch `{branch}` matches ignore pattern `{pattern}`")
            }
            Self::NotAllowed => {
                format!("branch `{branch}` matches no pattern in `[deploy].branches`")
            }
            Self::AtCapacity(max) => {
                format!("project is at its `max_environments` limit of {max}")
            }
        }
    }
}

/// Whether a pushed branch should be deployed.
///
/// `Err(reason)` is not a failure — it is the filter doing its job, and the
/// caller reports it as an accepted-but-skipped push rather than an error.
///
/// # Errors
/// The [`SkipReason`] that stopped the branch.
pub fn admit(branch: &str, config: &DeployConfig, load: ProjectLoad) -> Result<(), SkipReason> {
    if let Some(pattern) = config.ignore.iter().find(|p| glob_matches(p, branch)) {
        return Err(SkipReason::Ignored(pattern.clone()));
    }
    if !config.branches.is_empty() && !config.branches.iter().any(|p| glob_matches(p, branch)) {
        return Err(SkipReason::NotAllowed);
    }
    if let Some(max) = config.max_environments
        && !load.branch_already_live
        && load.live_environments >= max
    {
        return Err(SkipReason::AtCapacity(max));
    }
    Ok(())
}

/// Matches a branch name against a glob pattern.
///
/// `*` stands for any run of characters **including `/`**, and `?` for
/// exactly one. That is the deliberate choice: someone writing `feat/*`
/// means everything under `feat/`, and a `*` that stopped at a separator
/// would silently miss `feat/team/thing` — a filter that quietly skips
/// branches is worse than one that is slightly too generous, because
/// nothing about it looks wrong.
///
/// Matching is literal otherwise; there are no character classes or braces.
/// A branch name is not a filesystem path and does not need them.
#[must_use]
pub fn glob_matches(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    // Iterative backtracking rather than recursion: `star`/`mark` remember
    // the last `*` and where the text stood, so a dead end resumes there
    // having consumed one more character. Linear in practice and immune to
    // the stack blowups a recursive matcher hits on pathological patterns.
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);

    while ti < t.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some(pi);
                mark = ti;
                pi += 1;
            }
            Some('?') => {
                pi += 1;
                ti += 1;
            }
            Some(c) if *c == t[ti] => {
                pi += 1;
                ti += 1;
            }
            _ => match star {
                Some(s) => {
                    pi = s + 1;
                    mark += 1;
                    ti = mark;
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(live: u32, already: bool) -> ProjectLoad {
        ProjectLoad {
            live_environments: live,
            branch_already_live: already,
        }
    }

    fn cfg(branches: &[&str], ignore: &[&str], max: Option<u32>) -> DeployConfig {
        DeployConfig {
            branches: branches.iter().map(|s| (*s).to_owned()).collect(),
            ignore: ignore.iter().map(|s| (*s).to_owned()).collect(),
            max_environments: max,
        }
    }

    #[test]
    fn an_unconfigured_project_deploys_every_branch() {
        // The default has to stay what it was, or upgrading silently stops
        // deploying for everyone relying on it.
        let c = DeployConfig::default();
        for branch in ["main", "feat/x", "anything/at/all"] {
            assert!(admit(branch, &c, load(999, false)).is_ok());
        }
    }

    #[test]
    fn an_allowlist_refuses_everything_it_does_not_name() {
        let c = cfg(&["main", "release/*"], &[], None);
        assert!(admit("main", &c, load(0, false)).is_ok());
        assert!(admit("release/1.2", &c, load(0, false)).is_ok());
        assert_eq!(
            admit("feat/carrito", &c, load(0, false)),
            Err(SkipReason::NotAllowed)
        );
    }

    #[test]
    fn ignore_beats_the_allowlist() {
        // `*` allows everything and `wip/*` still refuses: the exclusion is
        // the more specific statement, so a broad allowlist cannot undo it.
        let c = cfg(&["*"], &["wip/*"], None);
        assert!(admit("feat/x", &c, load(0, false)).is_ok());
        assert_eq!(
            admit("wip/spike", &c, load(0, false)),
            Err(SkipReason::Ignored("wip/*".to_owned()))
        );
    }

    #[test]
    fn the_cap_refuses_a_new_branch_but_never_a_redeploy() {
        let c = cfg(&[], &[], Some(2));
        assert_eq!(
            admit("feat/new", &c, load(2, false)),
            Err(SkipReason::AtCapacity(2))
        );
        // The whole point: a project at its cap must still be able to ship
        // updates to what is already running.
        assert!(admit("main", &c, load(2, true)).is_ok());
        assert!(admit("feat/new", &c, load(1, false)).is_ok());
    }

    #[test]
    fn star_crosses_slashes() {
        // `feat/*` meaning "everything under feat" is what someone writing
        // it intends; stopping at `/` would skip branches silently.
        assert!(glob_matches("feat/*", "feat/team/thing"));
        assert!(glob_matches("*", "any/thing/at/all"));
        assert!(glob_matches("release/*", "release/1.2"));
        assert!(!glob_matches("feat/*", "fix/thing"));
    }

    #[test]
    fn glob_handles_the_awkward_shapes() {
        assert!(glob_matches("main", "main"));
        assert!(!glob_matches("main", "maintenance"));
        assert!(glob_matches("*main*", "pre-main-post"));
        assert!(glob_matches("v?.?", "v1.2"));
        assert!(!glob_matches("v?.?", "v1.23"));
        assert!(glob_matches("**", "anything"));
        assert!(glob_matches("*", ""));
        assert!(!glob_matches("", "x"));
        assert!(glob_matches("", ""));
        // Backtracking: the first `*` must give back characters so the
        // literal tail can land.
        assert!(glob_matches("*/*/thing", "a/b/thing"));
        assert!(glob_matches("a*b*c", "axxbyyc"));
        assert!(!glob_matches("a*b*c", "axxbyy"));
    }

    #[test]
    fn skip_reasons_name_the_branch_and_the_rule() {
        assert!(
            SkipReason::Ignored("wip/*".to_owned())
                .describe("wip/x")
                .contains("wip/*")
        );
        assert!(SkipReason::NotAllowed.describe("feat/x").contains("feat/x"));
        assert!(SkipReason::AtCapacity(25).describe("main").contains("25"));
    }
}
