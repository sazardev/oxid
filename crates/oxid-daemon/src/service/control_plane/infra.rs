#![allow(
    unused_imports,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::pedantic,
    clippy::nursery,
    clippy::empty_line_after_doc_comments
)]
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::ControlPlane;
use super::error::CpError;
use super::helpers::{
    hash_token, image_name, lowest_free_index, resolved_container_name, sanitize_identifier,
    sanitize_label, state_err,
};
use super::types::{Admission, DeployOutcome, GcSummary, InfraStatus, NodeStats};
use crate::adapter::config;
use crate::adapter::postgres_pool::PostgresPool;
use crate::adapter::store::{ApiTokenSummary, SqliteStore};
use crate::request_context::current_request_id;
use oxid_core::services::gc::{self, GcAction};
use oxid_core::services::subdomain::subdomain_for;
use oxid_core::services::var_resolution::{VarSources, set_secret};
use oxid_core::{
    AuditEvent, AuditFilter, AuditStore, Branch, BranchName, BuildSpec, CommitRef, ContainerPort,
    ContainerSpec, ContainerStatus, Dependency, EnvVarScope, Environment, EnvironmentId,
    EnvironmentState, EnvironmentStore, GitPort, HostCapacity, LogStream, OciError, OffsetDateTime,
    PoolError, PoolKind, Project, ProjectId, ProjectStore, RepoUrl, RepositoryError, SecretStore,
    SecretValue, SelfWiringStatus, StateTransition, TraefikSpec, Ttl,
};

impl<G: GitPort, O: ContainerPort> ControlPlane<G, O> {
    pub async fn node_stats(&self) -> Result<NodeStats, CpError> {
        let projects = self.store.list().await?;
        let mut stats = NodeStats {
            projects: projects.len() as u64,
            ..NodeStats::default()
        };
        for env in self.store.list_all_environments().await? {
            match env.state {
                EnvironmentState::Running => stats.environments_running += 1,
                EnvironmentState::Paused => stats.environments_paused += 1,
                EnvironmentState::Building => stats.environments_building += 1,
                EnvironmentState::Hibernating => stats.environments_hibernating += 1,
                EnvironmentState::BuildFailed => stats.environments_build_failed += 1,
                EnvironmentState::Destroyed => stats.environments_destroyed += 1,
            }
        }
        stats.queue_length = self.store.list_deploy_queue().await?.len() as u64;
        let host = self.oci.host_capacity().await?;
        stats.host_total_memory_bytes = host.total_memory_bytes;
        stats.host_cpu_count = host.cpu_count;
        stats.traefik_enabled = self.docker_network.is_some();
        // Best-effort: a runtime that will not answer `version` is already
        // reported by everything else failing, and a missing label must not
        // turn `oxid stats` into an error.
        if let Ok(info) = self.oci.runtime_info().await {
            stats.runtime = format!("{} {}", info.flavor.as_str(), info.version);
            stats.runtime_limitations =
                oxid_core::limitations(&info, self.traefik_http_port(), stats.traefik_enabled)
                    .iter()
                    .map(oxid_core::Limitation::describe)
                    .collect();
        }
        Ok(stats)
    }

    /// Read-only report of the manual-bootstrap steps described in
    /// `traefik_labels`'s doc comment: does the Docker network exist, is
    /// Traefik running, and is this daemon's own container wired for
    /// wake-on-request. Never creates or changes anything — see
    /// [`Self::infra_bootstrap`] to actually fix the first two.
    ///
    /// # Errors
    /// [`CpError::NotFound`] if `with_traefik` was never called (no
    /// `OXID_DOCKER_NETWORK` configured) — there's no network name to check
    /// against, so guessing one would be worse than a clear error.
    #[tracing::instrument(skip(self))]
    pub async fn infra_status(&self) -> Result<InfraStatus, CpError> {
        let network = self
            .docker_network
            .as_deref()
            .ok_or_else(Self::no_network_configured)?;
        tracing::info!(network, "checking infra bootstrap status");

        let network_exists = self.oci.network_exists(network).await?;
        let traefik_spec = self.traefik_spec(network.to_owned());
        let traefik_status = self
            .oci
            .container_status(&traefik_spec.container_name)
            .await?;
        let self_wiring = self.oci.self_wiring_status(network).await?;

        // Whether the running Traefik still matches the one Oxid would
        // create. This check used to be "does a container by that name
        // exist", which reported a proxy missing every TLS flag as healthy
        // — the operator saw a green status and a browser warning.
        let drift = match self.oci.traefik_runtime(&traefik_spec.container_name).await {
            Ok(Some(actual)) => oxid_core::services::tls::traefik_drift(&traefik_spec, &actual)
                .into_iter()
                .map(|d| d.describe())
                .collect(),
            // A Traefik that is not there is already reported by
            // `traefik_status`; saying it twice helps nobody.
            Ok(None) => Vec::new(),
            // Inspection failing is not the same as drift, and must not be
            // reported as if the proxy were misconfigured.
            Err(e) => {
                tracing::debug!(error = %e, "could not inspect traefik for drift");
                Vec::new()
            }
        };

        Ok(InfraStatus::new(
            network.to_owned(),
            network_exists,
            traefik_status,
            self.traefik_http_port(),
            self_wiring,
        )
        .with_tls(
            traefik_spec
                .acme
                .as_ref()
                .map(|a| if a.is_wildcard() { "dns-01" } else { "http-01" }.to_owned()),
            drift,
        ))
    }

    /// Idempotently creates the Docker network and starts the built-in
    /// Traefik container if either is missing, then re-queries
    /// [`Self::infra_status`] so the response always reflects reality
    /// afterward. Safe to call repeatedly — running it twice in a row
    /// changes nothing the second time.
    ///
    /// Deliberately does **not** attempt to wire this daemon's own
    /// container onto the network or label it: Docker cannot relabel a
    /// running container without recreating it, and recreating the very
    /// process executing this call is unsafe to automate. See
    /// [`InfraStatus::next_steps`] for what to do about that instead.
    ///
    /// # Errors
    /// Same as [`Self::infra_status`], plus any Docker failure creating the
    /// network or the Traefik container.
    #[tracing::instrument(skip(self))]
    pub async fn infra_bootstrap(&self) -> Result<InfraStatus, CpError> {
        let network = self
            .docker_network
            .clone()
            .ok_or_else(Self::no_network_configured)?;
        tracing::info!(network = %network, "bootstrapping infra: network + traefik");

        let network_status = self.oci.ensure_network(&network).await?;
        tracing::info!(network = %network, ?network_status, "network ensured");

        let spec = self.traefik_spec(network.clone());
        if let Some(acme) = spec.acme.as_ref() {
            // Before Traefik, not after: it needs somewhere to keep
            // `acme.json` the moment it starts, and a volume created later
            // would leave the first certificates in the container's own
            // writable layer, to be lost on the next recreate.
            self.oci.ensure_volume(&acme.storage_volume).await?;
            tracing::info!(volume = %acme.storage_volume, "acme store ensured");
        }

        let traefik_status = self.oci.ensure_traefik(spec).await?;
        tracing::info!(?traefik_status, "traefik ensured");

        self.infra_status().await
    }

    /// The Traefik spec this daemon bootstraps, with the operator's chosen
    /// host port applied. Built in one place so `infra_status` and
    /// `infra_bootstrap` can never disagree about which container they mean.
    fn traefik_spec(&self, network: String) -> TraefikSpec {
        let spec = TraefikSpec {
            http_port: self.traefik_http_port(),
            ..TraefikSpec::new(network)
        };
        match self.acme.as_ref() {
            Some((acme, https_port)) => spec.with_acme(acme.clone(), *https_port),
            None => spec,
        }
    }

    /// Translated like every other message an operator reads: the
    /// dashboard sends `Accept-Language`, and this one was hardcoded
    /// English, so a Spanish panel showed an English answer — which is the
    /// exact gap the daemon's catalog exists to close.
    pub(crate) fn no_network_configured() -> CpError {
        CpError::NotFound(crate::i18n::t("infra.noNetwork").to_owned())
    }

    pub(crate) fn traefik_labels(
        &self,
        name: &str,
        url: &str,
        container_port: u16,
        base_domain: &str,
    ) -> BTreeMap<String, String> {
        let Some(network) = &self.docker_network else {
            return BTreeMap::new();
        };
        let heartbeat = format!("{name}-heartbeat");
        let wake = format!("{name}-wake");
        BTreeMap::from([
            ("traefik.enable".to_owned(), "true".to_owned()),
            ("traefik.docker.network".to_owned(), network.clone()),
            (
                format!("traefik.http.routers.{name}.rule"),
                format!("Host(`{url}`)"),
            ),
            (
                format!("traefik.http.routers.{name}.entrypoints"),
                "web".to_owned(),
            ),
            (
                format!("traefik.http.routers.{name}.middlewares"),
                format!("{heartbeat},{wake}"),
            ),
            (
                format!("traefik.http.services.{name}.loadbalancer.server.port"),
                container_port.to_string(),
            ),
            (
                format!("traefik.http.middlewares.{heartbeat}.forwardauth.address"),
                format!("{}/api/v1/heartbeat", self.daemon_url),
            ),
            // A `Paused` container's kernel-level TCP stack still completes
            // a connection (the freeze is at the process/cgroup level), so
            // a plain proxied request hangs forever waiting for a response
            // that a frozen process can never send — it never becomes the
            // 502/503/504 the `errors` middleware below is watching for.
            // Traefik's Docker *label* provider does not support declaring
            // a `serversTransport` per-container (confirmed live: the
            // entity never lands in `/api/rawdata`, so the router 500s
            // with "servers transport not found"), so the response-header
            // timeout that turns that hang into a fast, catchable error is
            // set once, globally, in the Traefik static config instead
            // (see `--serversTransport.forwardingTimeouts.*` in
            // `docker-compose.yml`) — every Oxid-managed service picks it
            // up automatically, no per-router labels needed.
            //
            // Redirects a failed/timed-out request at this router to the
            // daemon's own `oxid-wake` service (labeled on the daemon's own
            // container — see `docker-compose.yml`), which unpauses/starts
            // the environment and returns a small auto-reloading page.
            // `Hibernating` branches (container fully stopped) fail fast
            // with connection-refused, already well inside these timeouts.
            // Gateway errors only — deliberately *not* the whole 5xx range.
            //
            // These are the codes Traefik itself produces when it cannot
            // reach a backend: 502 on connection refused, 504 on a dial or
            // response timeout, 503 when a router has no healthy server. An
            // absent container can only ever surface as one of those.
            //
            // A plain 500 is the opposite: it can only come from an app that
            // is running and answering. Catching it too meant a branch whose
            // code threw showed its developer a "Waking up…" page that
            // reloaded every two seconds forever, instead of the stack trace
            // the environment exists to show — the product hiding exactly
            // the information it was built to surface.
            (
                format!("traefik.http.middlewares.{wake}.errors.status"),
                "502-504".to_owned(),
            ),
            (
                format!("traefik.http.middlewares.{wake}.errors.service"),
                "oxid-wake".to_owned(),
            ),
            (
                format!("traefik.http.middlewares.{wake}.errors.query"),
                "/api/v1/wake".to_owned(),
            ),
        ])
        .into_iter()
        // TLS last: it *overrides* `entrypoints` (web → websecure) and adds
        // the resolver, so merging in this order is what lets a plain
        // install keep exactly the label set it has today while a TLS one
        // gets the secure entrypoint without a second copy of the rules.
        .chain(oxid_core::services::tls::router_tls_labels(
            name,
            base_domain,
            &self.traefik_spec(network.clone()),
        ))
        .collect()
    }
}
