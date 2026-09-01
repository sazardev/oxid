//! The Traefik command line, and whether the running one still matches it.
//!
//! This exists because the flags were written down twice — once in
//! `adapter::oci::create_and_start_traefik` and once in
//! `docker-compose.yml` — and nothing ever compared either against the
//! container that was actually running. `oxid infra status` only asked
//! whether a container named `oxid-traefik` existed, so a Traefik started
//! by hand, or by an older Oxid, or with TLS half-configured, reported
//! "running" and routed nothing of what the operator thought it did.
//!
//! Generating the argv here makes creation and drift detection derive from
//! the same list. Adding a flag in one place can no longer forget the
//! other, which is what makes the check cheap enough to be worth having.
//!
//! Pure: no I/O, no Docker types. The adapter turns [`traefik_cmd`] into a
//! container and [`traefik_drift`] into `next_steps` for a person to read.

use std::collections::BTreeMap;

use crate::domain::infra::{AcmeChallenge, TraefikSpec};

/// Container-side port of the `web` entrypoint. Always 80: it is where
/// Traefik listens *inside* its container, and has nothing to do with which
/// host port the operator publishes it on.
pub const WEB_ENTRYPOINT_PORT: u16 = 80;

/// Container-side port of the `websecure` entrypoint.
pub const WEBSECURE_ENTRYPOINT_PORT: u16 = 443;

/// Where the ACME store is mounted inside the Traefik container.
pub const ACME_MOUNT: &str = "/letsencrypt";

/// The full command line for a Traefik matching `spec`.
///
/// With no ACME configured this is byte-for-byte the list Oxid has always
/// used — a test pins that, because an install that never asked for
/// certificates must not have its proxy quietly change underneath it.
#[must_use]
pub fn traefik_cmd(spec: &TraefikSpec) -> Vec<String> {
    let mut cmd = vec![
        "--providers.docker=true".to_owned(),
        format!("--providers.docker.network={}", spec.network),
        "--providers.docker.exposedbydefault=false".to_owned(),
        // Config reloads are batched; the 2s default dominates every wake,
        // since a branch's route only comes back on a reload.
        "--providers.providersThrottleDuration=100ms".to_owned(),
        format!("--entrypoints.web.address=:{WEB_ENTRYPOINT_PORT}"),
        // A sleeping branch's container black-holes rather than refusing, so
        // these two timeouts are what turn a request to it into the 5xx the
        // `errors` middleware forwards to `/api/v1/wake`. Without them the
        // request hangs on Docker's default instead, and wake-on-request
        // never fires.
        "--serversTransport.forwardingTimeouts.dialTimeout=500ms".to_owned(),
        "--serversTransport.forwardingTimeouts.responseHeaderTimeout=5s".to_owned(),
    ];

    // Added *alongside* the Docker provider, never instead of it. The two
    // answer different questions: labels describe containers on this
    // machine, and the HTTP document describes every environment in the
    // fleet — including the ones on other nodes and the ones whose
    // container is stopped, neither of which the Docker socket can ever
    // report. An install that turns this on keeps routing everything it
    // already routed.
    if let Some(http) = spec.http_provider.as_ref() {
        cmd.push(format!("--providers.http.endpoint={}", http.endpoint));
        cmd.push(format!(
            "--providers.http.headers.Authorization={}",
            http.authorization
        ));
        cmd.push(format!(
            "--providers.http.pollInterval={}",
            http.poll_interval
        ));
    }

    let Some(acme) = spec.acme.as_ref() else {
        return cmd;
    };

    cmd.push(format!(
        "--entrypoints.websecure.address=:{WEBSECURE_ENTRYPOINT_PORT}"
    ));
    let resolver = &acme.resolver_name;
    cmd.push(format!(
        "--certificatesresolvers.{resolver}.acme.email={}",
        acme.email
    ));
    cmd.push(format!(
        "--certificatesresolvers.{resolver}.acme.storage={ACME_MOUNT}/acme.json"
    ));
    if let Some(ca) = acme.ca_directory.as_ref() {
        cmd.push(format!(
            "--certificatesresolvers.{resolver}.acme.caserver={ca}"
        ));
    }
    match &acme.challenge {
        AcmeChallenge::Http01 => {
            cmd.push(format!(
                "--certificatesresolvers.{resolver}.acme.httpchallenge=true"
            ));
            cmd.push(format!(
                "--certificatesresolvers.{resolver}.acme.httpchallenge.entrypoint=web"
            ));
        }
        AcmeChallenge::Dns01 { provider, .. } => {
            cmd.push(format!(
                "--certificatesresolvers.{resolver}.acme.dnschallenge=true"
            ));
            cmd.push(format!(
                "--certificatesresolvers.{resolver}.acme.dnschallenge.provider={provider}"
            ));
        }
    }
    if acme.http_redirect {
        // Entrypoint-level rather than a middleware on every router: one
        // flag instead of two labels per environment, and it also covers
        // the wake catch-all.
        cmd.push("--entrypoints.web.http.redirections.entryPoint.to=websecure".to_owned());
        cmd.push("--entrypoints.web.http.redirections.entryPoint.scheme=https".to_owned());
    }
    cmd
}

/// A way the running Traefik does not match what Oxid would create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraefikDrift {
    /// A command-line flag Oxid needs is absent.
    MissingFlag(String),
    /// A host port Oxid needs published is not.
    MissingPublishedPort(u16),
    /// The ACME store is not mounted, so certificates would be re-issued on
    /// every restart — straight into the rate limit.
    MissingAcmeVolume,
}

impl TraefikDrift {
    /// A line an operator can act on.
    ///
    /// Not translated: it also lands in logs and in `oxid infra status`
    /// output that people paste into issues.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::MissingFlag(flag) => format!(
                "the running Traefik is missing `{flag}` — recreate it with \
                 `oxid infra setup --recreate-traefik`"
            ),
            Self::MissingPublishedPort(port) => format!(
                "the running Traefik does not publish port {port} — recreate it with \
                 `oxid infra setup --recreate-traefik`"
            ),
            Self::MissingAcmeVolume => format!(
                "the running Traefik has no certificate store mounted at `{ACME_MOUNT}`; \
                 certificates would be re-issued on every restart and hit Let's Encrypt's \
                 rate limit — recreate it with `oxid infra setup --recreate-traefik`"
            ),
        }
    }
}

/// What a running Traefik container actually looks like, as far as this
/// check cares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TraefikRuntime {
    /// Its command line.
    pub cmd: Vec<String>,
    /// Host ports it publishes.
    pub published_ports: Vec<u16>,
    /// Destination paths of its mounts.
    pub mount_targets: Vec<String>,
}

/// Every way `actual` falls short of `spec`.
///
/// Deliberately one-directional: extra flags are an operator's business —
/// somebody may have added `--accesslog` — and reporting them as drift
/// would train people to ignore this. Only what Oxid needs and cannot find
/// is a finding.
#[must_use]
pub fn traefik_drift(spec: &TraefikSpec, actual: &TraefikRuntime) -> Vec<TraefikDrift> {
    let mut drift: Vec<TraefikDrift> = traefik_cmd(spec)
        .into_iter()
        .filter(|flag| !actual.cmd.contains(flag))
        .map(TraefikDrift::MissingFlag)
        .collect();

    if !actual.published_ports.contains(&spec.http_port) {
        drift.push(TraefikDrift::MissingPublishedPort(spec.http_port));
    }
    if let Some(https) = spec.https_port
        && !actual.published_ports.contains(&https)
    {
        drift.push(TraefikDrift::MissingPublishedPort(https));
    }
    if spec.acme.is_some() && !actual.mount_targets.iter().any(|m| m == ACME_MOUNT) {
        drift.push(TraefikDrift::MissingAcmeVolume);
    }
    drift
}

/// The TLS half of an environment's Traefik router labels.
///
/// Empty without ACME, which is what keeps every existing deployment's
/// label set unchanged.
#[must_use]
pub fn router_tls_labels(
    router: &str,
    base_domain: &str,
    spec: &TraefikSpec,
) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    let Some(acme) = spec.acme.as_ref() else {
        return labels;
    };

    labels.insert(
        format!("traefik.http.routers.{router}.entrypoints"),
        "websecure".to_owned(),
    );
    labels.insert(
        format!("traefik.http.routers.{router}.tls"),
        "true".to_owned(),
    );
    labels.insert(
        format!("traefik.http.routers.{router}.tls.certresolver"),
        acme.resolver_name.clone(),
    );
    if acme.is_wildcard() {
        // This is what makes one certificate serve every branch. Without
        // it Traefik asks for a certificate per hostname even under DNS-01,
        // which is the rate limit HTTP-01 was avoided to escape.
        labels.insert(
            format!("traefik.http.routers.{router}.tls.domains[0].main"),
            crate::domain::infra::AcmeConfig::wildcard_for(base_domain),
        );
        labels.insert(
            format!("traefik.http.routers.{router}.tls.domains[0].sans"),
            base_domain.to_owned(),
        );
    }
    labels
}

/// Why a TLS configuration cannot work, checked before anything is started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsConfigError {
    /// HTTP-01 validates on port 80 and nowhere else.
    Http01NeedsPort80(u16),
    /// An email address is mandatory for an ACME account.
    MissingEmail,
    /// DNS-01 without any credential names cannot possibly authenticate.
    Dns01NeedsCredentials(String),
}

impl TlsConfigError {
    /// The message an operator sees at startup.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Http01NeedsPort80(port) => format!(
                "the HTTP-01 challenge is validated on port 80 and nowhere else, but \
                 OXID_TRAEFIK_HTTP_PORT is {port}. Publish Traefik on 80, or use the \
                 DNS-01 challenge by setting OXID_ACME_DNS_PROVIDER."
            ),
            Self::MissingEmail => {
                "OXID_ACME_EMAIL is required to register an ACME account".to_owned()
            }
            Self::Dns01NeedsCredentials(provider) => format!(
                "the DNS-01 challenge with provider `{provider}` needs credentials: set \
                 OXID_ACME_DNS_ENV to the names of the environment variables it reads \
                 (e.g. CF_DNS_API_TOKEN), and set those variables on this daemon."
            ),
        }
    }
}

/// Validates a TLS configuration against the port it will be published on.
///
/// Refusing at startup beats issuing certificates that never arrive: an
/// HTTP-01 daemon on a port other than 80 looks healthy, serves a
/// self-signed certificate, and gives no clue why.
///
/// # Errors
/// The first [`TlsConfigError`] found.
pub fn validate(spec: &TraefikSpec) -> Result<(), TlsConfigError> {
    let Some(acme) = spec.acme.as_ref() else {
        return Ok(());
    };
    if acme.email.trim().is_empty() {
        return Err(TlsConfigError::MissingEmail);
    }
    match &acme.challenge {
        AcmeChallenge::Http01 if spec.http_port != WEB_ENTRYPOINT_PORT => {
            Err(TlsConfigError::Http01NeedsPort80(spec.http_port))
        }
        AcmeChallenge::Dns01 { provider, env_keys } if env_keys.is_empty() => {
            Err(TlsConfigError::Dns01NeedsCredentials(provider.clone()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::infra::AcmeConfig;

    fn plain() -> TraefikSpec {
        TraefikSpec::new("oxid-net")
    }

    fn acme(challenge: AcmeChallenge) -> AcmeConfig {
        AcmeConfig {
            email: "ops@example.com".to_owned(),
            challenge,
            ca_directory: None,
            storage_volume: "oxid-acme".to_owned(),
            resolver_name: "oxid".to_owned(),
            http_redirect: true,
        }
    }

    fn dns01() -> AcmeChallenge {
        AcmeChallenge::Dns01 {
            provider: "cloudflare".to_owned(),
            env_keys: vec!["CF_DNS_API_TOKEN".to_owned()],
        }
    }

    /// The golden test. An install that never asked for certificates must
    /// get exactly the proxy it had before this module existed — the
    /// argv equivalent of "a migration never silently removes behaviour".
    #[test]
    fn a_spec_without_acme_produces_exactly_the_historical_command_line() {
        assert_eq!(
            traefik_cmd(&plain()),
            vec![
                "--providers.docker=true",
                "--providers.docker.network=oxid-net",
                "--providers.docker.exposedbydefault=false",
                "--providers.providersThrottleDuration=100ms",
                "--entrypoints.web.address=:80",
                "--serversTransport.forwardingTimeouts.dialTimeout=500ms",
                "--serversTransport.forwardingTimeouts.responseHeaderTimeout=5s",
            ]
        );
    }

    #[test]
    fn dns01_asks_for_a_dns_challenge_and_http01_for_an_http_one() {
        let dns = traefik_cmd(&plain().with_acme(acme(dns01()), 443));
        assert!(
            dns.iter()
                .any(|f| f == "--certificatesresolvers.oxid.acme.dnschallenge.provider=cloudflare")
        );
        assert!(!dns.iter().any(|f| f.contains("httpchallenge")));

        let http = traefik_cmd(&plain().with_acme(acme(AcmeChallenge::Http01), 443));
        assert!(
            http.iter()
                .any(|f| f == "--certificatesresolvers.oxid.acme.httpchallenge.entrypoint=web")
        );
        assert!(!http.iter().any(|f| f.contains("dnschallenge")));
    }

    #[test]
    fn a_staging_directory_is_passed_through() {
        let mut cfg = acme(dns01());
        cfg.ca_directory =
            Some("https://acme-staging-v02.api.letsencrypt.org/directory".to_owned());
        let cmd = traefik_cmd(&plain().with_acme(cfg, 443));
        assert!(cmd.iter().any(|f| {
            f.starts_with("--certificatesresolvers.oxid.acme.caserver=https://acme-staging")
        }));
    }

    #[test]
    fn drift_is_empty_for_a_traefik_that_matches_and_names_what_is_missing() {
        let spec = plain().with_acme(acme(dns01()), 443);
        let matching = TraefikRuntime {
            cmd: traefik_cmd(&spec),
            published_ports: vec![80, 443],
            mount_targets: vec![ACME_MOUNT.to_owned()],
        };
        assert!(traefik_drift(&spec, &matching).is_empty());

        // A Traefik from before TLS was configured: right container name,
        // running, and wrong in every way that matters. This is the case
        // `infra status` used to report as OK.
        let stale = TraefikRuntime {
            cmd: traefik_cmd(&plain()),
            published_ports: vec![80],
            mount_targets: vec![],
        };
        let drift = traefik_drift(&spec, &stale);
        assert!(drift.contains(&TraefikDrift::MissingPublishedPort(443)));
        assert!(drift.contains(&TraefikDrift::MissingAcmeVolume));
        assert!(
            drift.iter().any(|d| matches!(
                d, TraefikDrift::MissingFlag(f) if f.contains("websecure")
            )),
            "{drift:?}"
        );
        assert!(drift.iter().all(|d| !d.describe().is_empty()));
    }

    /// Extra flags are the operator's business. Reporting them would train
    /// people to ignore this check.
    #[test]
    fn extra_flags_are_not_drift() {
        let spec = plain();
        let mut cmd = traefik_cmd(&spec);
        cmd.push("--accesslog=true".to_owned());
        let actual = TraefikRuntime {
            cmd,
            published_ports: vec![80],
            mount_targets: vec![],
        };
        assert!(traefik_drift(&spec, &actual).is_empty());
    }

    #[test]
    fn tls_labels_are_empty_without_acme_and_wildcard_only_under_dns01() {
        assert!(router_tls_labels("r", "app.example.com", &plain()).is_empty());

        let wild = router_tls_labels(
            "r",
            "app.example.com",
            &plain().with_acme(acme(dns01()), 443),
        );
        assert_eq!(
            wild.get("traefik.http.routers.r.tls.domains[0].main")
                .map(String::as_str),
            Some("*.app.example.com")
        );
        assert_eq!(
            wild.get("traefik.http.routers.r.entrypoints")
                .map(String::as_str),
            Some("websecure")
        );

        // HTTP-01 gets a certificate per hostname, so asking for a wildcard
        // would simply fail the challenge.
        let per_host = router_tls_labels(
            "r",
            "app.example.com",
            &plain().with_acme(acme(AcmeChallenge::Http01), 443),
        );
        assert!(!per_host.contains_key("traefik.http.routers.r.tls.domains[0].main"));
        assert_eq!(
            per_host
                .get("traefik.http.routers.r.tls.certresolver")
                .map(String::as_str),
            Some("oxid")
        );
    }

    #[test]
    fn http01_on_a_port_other_than_80_is_refused_before_anything_starts() {
        let mut spec = plain().with_acme(acme(AcmeChallenge::Http01), 443);
        spec.http_port = 8080;
        assert_eq!(
            validate(&spec),
            Err(TlsConfigError::Http01NeedsPort80(8080))
        );
        assert!(validate(&spec).unwrap_err().describe().contains("DNS-01"));

        // DNS-01 does not validate over HTTP, so the same port is fine.
        let mut dns = plain().with_acme(acme(dns01()), 443);
        dns.http_port = 8080;
        assert!(validate(&dns).is_ok());
    }

    #[test]
    fn dns01_without_credential_names_cannot_work_and_says_so() {
        let spec = plain().with_acme(
            acme(AcmeChallenge::Dns01 {
                provider: "route53".to_owned(),
                env_keys: vec![],
            }),
            443,
        );
        let err = validate(&spec).unwrap_err();
        assert!(err.describe().contains("OXID_ACME_DNS_ENV"), "{err:?}");
    }

    #[test]
    fn a_plain_spec_needs_no_validation_and_never_fails_it() {
        assert!(validate(&plain()).is_ok());
    }
}
