//! Value objects used across the domain.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use time::Duration;

use crate::domain::DomainError;

/// A repository URL (e.g. `https://github.com/org/repo.git`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoUrl(String);

impl RepoUrl {
    /// Validates and wraps a repository URL.
    ///
    /// # Errors
    /// Returns [`DomainError::Invalid`] if the string is empty or not a valid
    /// URL scheme.
    pub fn parse(value: impl Into<String>) -> Result<Self, DomainError> {
        let raw = value.into();
        let trimmed = raw.trim();

        if trimmed.is_empty() {
            return Err(DomainError::Invalid(
                "repository URL cannot be empty".to_owned(),
            ));
        }

        let (scheme, rest) = trimmed.split_once("://").ok_or_else(|| {
            DomainError::Invalid(
                "repository URL must include a scheme (e.g. `https://` or `git@`)".to_owned(),
            )
        })?;
        if scheme.is_empty() || rest.is_empty() {
            return Err(DomainError::Invalid(
                "repository URL must have a scheme and a host".to_owned(),
            ));
        }

        Ok(Self(trimmed.to_owned()))
    }

    /// Returns the raw URL string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The repository this URL points at, independent of how it is spelled.
    ///
    /// Two URLs are the same repository when their `host/path` match once
    /// the scheme, any credentials, a trailing `.git` and surrounding
    /// slashes are removed, compared case-insensitively.
    ///
    /// This exists because a team spells the same repository several ways
    /// and every one of them is correct. An operator registers
    /// `https://github.com/org/app.git`; a developer's clone says
    /// `git@github.com:org/app.git` because they use SSH; a script drops
    /// the `.git`. Comparing the raw strings makes those three different
    /// projects, which in practice meant a developer's `oxid up` could not
    /// find the project their own team had registered — it tried to create
    /// a second one and was refused for lacking permission to.
    ///
    /// The *stored* URL is still the one registration was given, because
    /// that is the one the daemon has to clone from: a developer's SSH
    /// remote usually needs a key the daemon does not have. Identity is for
    /// recognising a repository, never for fetching it.
    #[must_use]
    pub fn identity(&self) -> String {
        let rest = self.0.split_once("://").map_or(self.0.as_str(), |(_, r)| r);
        // Credentials in the authority (`user:pass@host`, `git@host`) are
        // not part of which repository this is.
        let rest = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
        // scp-style (`host:org/repo`) separates host and path with `:`,
        // everything else with `/`.
        let (host, path) = match rest.find(['/', ':']) {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        format!(
            "{}/{}",
            host.to_ascii_lowercase(),
            path.trim_matches('/')
                .trim_end_matches(".git")
                .to_ascii_lowercase()
        )
    }

    /// Whether both URLs name the same repository. See [`Self::identity`].
    #[must_use]
    pub fn same_repository(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
}

impl fmt::Display for RepoUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for RepoUrl {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// A time-to-live duration such as `30m` or `7d`.
///
/// Parsing supports the suffixed formats used in `oxid.toml`:
/// `30s`, `30m`, `2h`, `7d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Ttl(Duration);

impl Ttl {
    /// Parses a duration string like `30m`, `2h` or `7d`.
    ///
    /// # Errors
    /// Returns [`DomainError::Invalid`] when the format is not recognized.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, DomainError> {
        let raw = value.as_ref().trim().to_ascii_lowercase();
        if raw.is_empty() {
            return Err(DomainError::Invalid(
                "duration cannot be empty, did you mean `30m` or `7d`?".to_owned(),
            ));
        }

        let split = raw.find(char::is_alphabetic).ok_or_else(|| {
            DomainError::Invalid(format!(
                "invalid duration `{raw}`: missing unit, did you mean `{raw}m`?"
            ))
        })?;
        let (number, unit) = raw.split_at(split);

        let magnitude: i64 = number.trim().parse().map_err(|_| {
            DomainError::Invalid(format!(
                "invalid duration `{raw}`: `{number}` is not a number"
            ))
        })?;

        let duration = match unit {
            "s" => Duration::seconds(magnitude),
            "m" => Duration::minutes(magnitude),
            "h" => Duration::hours(magnitude),
            "d" => Duration::days(magnitude),
            _ => {
                return Err(DomainError::Invalid(format!(
                    "invalid duration `{raw}`: unknown unit `{unit}`, did you mean `s`, `m`, `h` or `d`?"
                )));
            }
        };

        if duration.is_negative() {
            return Err(DomainError::Invalid(
                "duration cannot be negative".to_owned(),
            ));
        }

        Ok(Self(duration))
    }

    /// Returns the duration as a [`time::Duration`].
    #[must_use]
    pub fn get(self) -> Duration {
        self.0
    }

    /// Builds a TTL from a whole-seconds count (storage round-trip).
    ///
    /// # Errors
    /// Returns [`DomainError::Invalid`] if `seconds` is negative.
    pub fn from_seconds(seconds: i64) -> Result<Self, DomainError> {
        if seconds < 0 {
            return Err(DomainError::Invalid(
                "duration cannot be negative".to_owned(),
            ));
        }
        Ok(Self(Duration::seconds(seconds)))
    }

    /// Returns the duration in whole seconds.
    #[must_use]
    pub fn whole_seconds(self) -> i64 {
        self.0.whole_seconds()
    }
}

impl fmt::Display for Ttl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}s", self.whole_seconds())
    }
}

impl FromStr for Ttl {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<String> for Ttl {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<Ttl> for String {
    fn from(value: Ttl) -> Self {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::RepoUrl as _TestRepoUrl;

    fn url(s: &str) -> _TestRepoUrl {
        _TestRepoUrl::parse(s).unwrap()
    }

    /// The case that made this necessary: an operator registers over HTTPS,
    /// a developer's clone speaks SSH, and both are the same repository.
    #[test]
    fn the_same_repository_spelled_differently_is_recognised() {
        let https = url("https://github.com/org/app.git");
        for other in [
            "https://github.com/org/app",
            "https://github.com/org/app/",
            "ssh://git@github.com/org/app.git",
            "git://github.com/org/app.git",
            "https://GitHub.com/Org/App.git",
            "https://token:x-oauth-basic@github.com/org/app.git",
        ] {
            assert!(
                https.same_repository(&url(other)),
                "`{other}` should be the same repository as `{https}`"
            );
        }
    }

    #[test]
    fn different_repositories_stay_different() {
        let app = url("https://github.com/org/app.git");
        for other in [
            "https://github.com/org/app-api.git",
            "https://github.com/other/app.git",
            // Same path on a different host is a different repository —
            // a self-hosted mirror is not the origin.
            "https://gitlab.com/org/app.git",
        ] {
            assert!(
                !app.same_repository(&url(other)),
                "`{other}` must not match `{app}`"
            );
        }
    }

    #[test]
    fn identity_never_replaces_the_stored_url() {
        // The daemon has to clone from what registration was given: a
        // developer's SSH remote usually needs a key it does not have.
        let u = url("https://github.com/org/app.git");
        assert_eq!(u.as_str(), "https://github.com/org/app.git");
        assert_eq!(u.identity(), "github.com/org/app");
    }

    use super::*;

    #[test]
    fn parses_supported_units() {
        assert_eq!(Ttl::parse("30s").unwrap().whole_seconds(), 30);
        assert_eq!(Ttl::parse("30m").unwrap().whole_seconds(), 1_800);
        assert_eq!(Ttl::parse("2h").unwrap().whole_seconds(), 7_200);
        assert_eq!(Ttl::parse("7d").unwrap().whole_seconds(), 604_800);
    }

    #[test]
    fn accepts_whitespace_and_case() {
        assert_eq!(Ttl::parse(" 30M ").unwrap().whole_seconds(), 1_800);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(Ttl::parse("").is_err());
        assert!(Ttl::parse("30").is_err());
        assert!(Ttl::parse("abc").is_err());
        assert!(Ttl::parse("30w").is_err());
        assert!(Ttl::parse("-5m").is_err());
    }

    #[test]
    fn serde_roundtrip() {
        let ttl = Ttl::parse("30m").unwrap();
        let json = serde_json::to_string(&ttl).unwrap();
        assert_eq!(json, "\"1800s\"");
        let back: Ttl = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ttl);
    }
}
