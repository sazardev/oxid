use super::*;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::Request;
use oxid_core::{BuildReport, BuildSpec, ContainerSpec, GitError, OciError, RepoUrl};
use tower::ServiceExt;

use crate::adapter::crypto::Cipher;
use crate::adapter::store::SqliteStore;
use oxid_core::ProjectStore;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

#[derive(Debug, Clone, Default)]
struct FakeGit {
    /// Directory `ensure_repo` reports as the cloned cache entry. `None`
    /// keeps the legacy `cache_dir/app` behavior for older tests.
    checkout: Option<std::path::PathBuf>,
    /// When set, `ensure_repo` fails with this message — simulating an
    /// unreachable remote / rejected token at registration time.
    ensure_error: Option<String>,
}

impl FakeGit {
    fn at(dir: &std::path::Path) -> Self {
        Self {
            checkout: Some(dir.to_owned()),
            ..Self::default()
        }
    }

    fn unreachable(message: &str) -> Self {
        Self {
            ensure_error: Some(message.to_owned()),
            ..Self::default()
        }
    }
}

impl GitPort for FakeGit {
    async fn remote_url(&self, repo_dir: &std::path::Path) -> Result<RepoUrl, GitError> {
        let _ = repo_dir;
        RepoUrl::parse("https://github.com/org/app.git")
            .map_err(|e| GitError::Failure(e.to_string()))
    }
    async fn ensure_repo(
        &self,
        _url: &RepoUrl,
        _token: Option<&str>,
        cache_dir: &std::path::Path,
    ) -> Result<std::path::PathBuf, GitError> {
        if let Some(message) = &self.ensure_error {
            return Err(GitError::Failure(message.clone()));
        }
        // Same reason as the control-plane fake: the real one leaves a
        // checkout behind, and the deploy copies the build context out of
        // it, so this has to be a directory that exists.
        let dir = self
            .checkout
            .clone()
            .unwrap_or_else(|| cache_dir.join("app"));
        std::fs::create_dir_all(&dir).map_err(|e| GitError::Failure(e.to_string()))?;
        Ok(dir)
    }
    async fn resolve_branch_head(
        &self,
        _repo_dir: &std::path::Path,
        branch: &BranchName,
    ) -> Result<oxid_core::CommitRef, GitError> {
        Ok(oxid_core::CommitRef {
            branch: branch.clone(),
            sha: SHA.to_owned(),
        })
    }
    async fn checkout_commit(
        &self,
        _repo_dir: &std::path::Path,
        _sha: &str,
    ) -> Result<(), GitError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
struct FakeOci {
    calls: Arc<Mutex<Vec<String>>>,
    /// Docker networks `ensure_network`/`network_exists` believe exist.
    networks: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Per-container lifecycle state, kept in step with the calls below.
    /// `container_status` used to answer a constant `Running`, which made
    /// it impossible to test the wake path now that waking dispatches on
    /// what Docker actually reports rather than on the stored state.
    statuses: Arc<Mutex<std::collections::HashMap<String, oxid_core::ContainerStatus>>>,
}

impl FakeOci {
    fn set_status(&self, name: &str, status: oxid_core::ContainerStatus) {
        self.statuses
            .lock()
            .unwrap()
            .insert(name.to_owned(), status);
    }
}

impl ContainerPort for FakeOci {
    async fn pull_image(&self, image: &str) -> Result<(), OciError> {
        let _ = image;
        Ok(())
    }

    async fn build(&self, spec: &BuildSpec) -> Result<BuildReport, OciError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("build:{}", spec.image));
        Ok(BuildReport::default())
    }
    async fn run(&self, spec: &ContainerSpec) -> Result<Option<u16>, OciError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("run:{}:env={:?}", spec.name, spec.env));
        Ok(spec.network.is_none().then_some(65535))
    }
    async fn published_port(
        &self,
        _name: &str,
        _container_port: u16,
    ) -> Result<Option<u16>, OciError> {
        Ok(Some(65535))
    }
    async fn start(&self, name: &str) -> Result<(), OciError> {
        self.calls.lock().unwrap().push(format!("start:{name}"));
        self.set_status(name, oxid_core::ContainerStatus::Running);
        Ok(())
    }
    async fn pause(&self, name: &str) -> Result<(), OciError> {
        self.calls.lock().unwrap().push(format!("pause:{name}"));
        self.set_status(name, oxid_core::ContainerStatus::Paused);
        Ok(())
    }
    async fn unpause(&self, name: &str) -> Result<(), OciError> {
        self.calls.lock().unwrap().push(format!("unpause:{name}"));
        self.set_status(name, oxid_core::ContainerStatus::Running);
        Ok(())
    }
    async fn stop(&self, name: &str) -> Result<(), OciError> {
        self.calls.lock().unwrap().push(format!("stop:{name}"));
        self.set_status(name, oxid_core::ContainerStatus::Stopped);
        Ok(())
    }
    async fn remove(&self, name: &str) -> Result<(), OciError> {
        self.calls.lock().unwrap().push(format!("remove:{name}"));
        Ok(())
    }
    async fn remove_image(&self, image: &str) -> Result<(), OciError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("remove_image:{image}"));
        Ok(())
    }
    async fn logs(&self, name: &str) -> Result<String, OciError> {
        self.calls.lock().unwrap().push(format!("logs:{name}"));
        Ok("build log".to_owned())
    }
    async fn stream_logs(&self, name: &str) -> Result<oxid_core::LogStream, OciError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("stream_logs:{name}"));
        Ok(Box::pin(futures_util::stream::iter(vec![Ok(
            "build log".to_owned()
        )])))
    }
    async fn exec(&self, name: &str, command: &str) -> Result<(), OciError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("exec:{name}:{command}"));
        Ok(())
    }
    async fn container_status(&self, name: &str) -> Result<oxid_core::ContainerStatus, OciError> {
        Ok(self
            .statuses
            .lock()
            .unwrap()
            .get(name)
            .copied()
            .unwrap_or(oxid_core::ContainerStatus::Running))
    }
    async fn host_capacity(&self) -> Result<oxid_core::HostCapacity, OciError> {
        Ok(oxid_core::HostCapacity {
            total_memory_bytes: 8 * 1_073_741_824,
            cpu_count: 4,
        })
    }
    async fn network_exists(&self, name: &str) -> Result<bool, OciError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("network_exists:{name}"));
        Ok(self.networks.lock().unwrap().contains(name))
    }
    async fn ensure_network(&self, name: &str) -> Result<oxid_core::NetworkStatus, OciError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("ensure_network:{name}"));
        if self.networks.lock().unwrap().insert(name.to_owned()) {
            Ok(oxid_core::NetworkStatus::Created)
        } else {
            Ok(oxid_core::NetworkStatus::AlreadyExisted)
        }
    }
    async fn ensure_traefik(
        &self,
        spec: oxid_core::TraefikSpec,
    ) -> Result<oxid_core::TraefikStatus, OciError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("ensure_traefik:{}", spec.container_name));
        Ok(oxid_core::TraefikStatus::Created)
    }
    async fn self_wiring_status(
        &self,
        _network: &str,
    ) -> Result<oxid_core::SelfWiringStatus, OciError> {
        Ok(oxid_core::SelfWiringStatus::NotContainerized)
    }
}

/// A data dir with a dummy `secret.key`, for backup-endpoint tests.
/// `.keep()` leaks it (no auto-delete on drop) since it must outlive
/// the individual request(s) a test makes against the returned router.
fn test_data_dir() -> PathBuf {
    let dir = tempfile::tempdir().unwrap().keep();
    std::fs::write(dir.join("secret.key"), b"test-key-material").unwrap();
    dir
}

async fn test_app() -> (Router, FakeOci) {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let cache = tempfile::tempdir().unwrap();
    let oci = FakeOci::default();
    let cp = ControlPlane::new(
        store,
        FakeGit::default(),
        oci.clone(),
        cache.path().to_owned(),
    )
    .with_readiness_check(false);
    (
        router(ApiState {
            cp,
            webhook_secret: Some("test-secret".to_owned()),
            api_token: None,
            data_dir: test_data_dir(),
            allow_restore: true,
            rate_limit: None,
            auto_token: false,
            bootstrap_access: BootstrapAccess::Loopback,
        }),
        oci,
    )
}

async fn test_app_with_traefik() -> (Router, FakeOci) {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let cache = tempfile::tempdir().unwrap();
    let oci = FakeOci::default();
    let cp = ControlPlane::new(
        store,
        FakeGit::default(),
        oci.clone(),
        cache.path().to_owned(),
    )
    .with_readiness_check(false)
    .with_traefik("oxid-net", "http://oxid-daemon:8080");
    (
        router(ApiState {
            cp,
            webhook_secret: Some("test-secret".to_owned()),
            api_token: None,
            data_dir: test_data_dir(),
            allow_restore: true,
            rate_limit: None,
            auto_token: false,
            bootstrap_access: BootstrapAccess::Loopback,
        }),
        oci,
    )
}

async fn test_app_with_token(token: &str) -> Router {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let cache = tempfile::tempdir().unwrap();
    let cp = ControlPlane::new(
        store,
        FakeGit::default(),
        FakeOci::default(),
        cache.path().to_owned(),
    )
    .with_readiness_check(false);
    router(ApiState {
        cp,
        webhook_secret: Some("test-secret".to_owned()),
        api_token: Some(token.to_owned()),
        data_dir: test_data_dir(),
        allow_restore: true,
        rate_limit: None,
        auto_token: false,
        bootstrap_access: BootstrapAccess::Loopback,
    })
}

/// A router whose `ControlPlane` has admission control enabled with an
/// 8GB host (`FakeOci::host_capacity`'s fixed value) minus
/// `reserved_mb`, and `default_mem_mb` as the daemon-default memory
/// request for any project that doesn't set its own — for exercising
/// `/api/v1/projects/{id}/deploy`'s queued-response path end to end.
async fn test_app_with_admission_control(
    reserved_mb: u64,
    default_mem_mb: u64,
) -> (Router, FakeOci) {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let cache = tempfile::tempdir().unwrap();
    let oci = FakeOci::default();
    let cp = ControlPlane::new(
        store,
        FakeGit::default(),
        oci.clone(),
        cache.path().to_owned(),
    )
    .with_resource_defaults(Some(default_mem_mb), None)
    .with_admission_control(Some(reserved_mb))
    .with_readiness_check(false);
    (
        router(ApiState {
            cp,
            webhook_secret: Some("test-secret".to_owned()),
            api_token: None,
            data_dir: test_data_dir(),
            allow_restore: true,
            rate_limit: None,
            auto_token: false,
            bootstrap_access: BootstrapAccess::Loopback,
        }),
        oci,
    )
}

fn repo_dir_with_config() -> tempfile::TempDir {
    repo_dir_named("app")
}

/// A repository whose `oxid.toml` carries a `[deploy]` block, for the
/// branch-filter tests.
fn repo_dir_with_deploy(deploy: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("oxid.toml"),
        format!(
            r#"
[project]
name = "app"

[routing]
base_domain = "app.local.dev"
port = 8080

[deploy]
{deploy}
"#
        ),
    )
    .unwrap();
    dir
}

/// Like [`repo_dir_with_config`], but lets tests register several distinct
/// projects (registration derives the identity from `[project].name`).
fn repo_dir_named(name: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("oxid.toml"),
        format!(
            r#"
[project]
name = "{name}"

[routing]
base_domain = "{name}.local.dev"
port = 8080
"#
        ),
    )
    .unwrap();
    dir
}

/// Blocks until an accepted webhook has actually produced an environment.
///
/// Webhook deliveries are answered `queued` and deployed on a background
/// drain, so a test that asserts on the deploy has to wait for it rather
/// than read straight after the response. Bounded, and panics rather than
/// hanging if the drain never runs.
async fn wait_for_environments(app: &Router, project_id: u64) -> Vec<Environment> {
    for _ in 0..200 {
        let (_, body) = json_request(
            app,
            "GET",
            &format!("/api/v1/projects/{project_id}/environments"),
            json!({}),
        )
        .await;
        if let Ok(envs) = serde_json::from_slice::<Vec<Environment>>(&body)
            && !envs.is_empty()
        {
            return envs;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("queued deploy never produced an environment");
}

async fn json_request(app: &Router, method: &str, uri: &str, body: Value) -> (StatusCode, Vec<u8>) {
    json_request_with_auth(app, method, uri, body, None).await
}

async fn json_request_with_auth(
    app: &Router,
    method: &str,
    uri: &str,
    body: Value,
    bearer: Option<&str>,
) -> (StatusCode, Vec<u8>) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

/// Like `json_request`, but for endpoints that consume/produce raw
/// bytes instead of JSON (`/backup`, `/backup/restore`).
async fn raw_request(
    app: &Router,
    method: &str,
    uri: &str,
    body: Vec<u8>,
) -> (StatusCode, Vec<u8>) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

/// Sends a GitHub webhook with a valid `X-Hub-Signature-256` header.
async fn signed_webhook(app: &Router, payload: Value) -> (StatusCode, Vec<u8>) {
    signed_webhook_with_event(app, payload, None).await
}

async fn signed_webhook_with_event(
    app: &Router,
    payload: Value,
    event: Option<&str>,
) -> (StatusCode, Vec<u8>) {
    let raw = payload.to_string();
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"test-secret").unwrap();
    mac.update(raw.as_bytes());
    let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/v1/webhooks/github")
        .header("content-type", "application/json")
        .header("x-hub-signature-256", signature);
    if let Some(event) = event {
        builder = builder.header("x-github-event", event);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(raw)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

#[tokio::test]
async fn health_ok() {
    let (app, _) = test_app().await;
    let (status, _) = json_request(&app, "GET", "/api/v1/health", json!({})).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn dashboard_static_assets_are_served_without_a_token() {
    let app = test_app_with_token("s3cr3t").await;
    for (path, marker) in [
        ("/", "OXID"),
        ("/index.html", "OXID"),
        ("/style.css", "--oxid-orange"),
        ("/app.js", "function dashboard()"),
        ("/i18n.js", "OxidI18n"),
        ("/vendor/alpine.min.js", "Alpine"),
        // Installable-app assets. Public like the rest of the shell: a
        // manifest behind auth makes the panel silently un-installable,
        // and a service worker that 404s takes offline support with it.
        ("/manifest.webmanifest", "\"short_name\": \"Oxid\""),
        ("/sw.js", "addEventListener"),
        ("/icon.svg", "<svg"),
        ("/icon-maskable.svg", "<svg"),
    ] {
        let (status, body) = json_request(&app, "GET", path, json!({})).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        let text = String::from_utf8(body).unwrap();
        assert!(text.contains(marker), "{path}: {text:.200}");
    }
}

/// The manifest has to parse, and name assets that actually exist.
///
/// A typo here does not fail a build or a request — the browser simply
/// declines to offer installation, with the reason buried in a devtools
/// panel nobody has open.
#[test]
fn the_pwa_manifest_is_valid_and_self_consistent() {
    let manifest: Value =
        serde_json::from_str(include_str!("../../web/manifest.webmanifest")).unwrap();

    // Chromium refuses to install without all of these.
    for key in ["name", "short_name", "start_url", "display", "icons"] {
        assert!(manifest.get(key).is_some(), "manifest is missing `{key}`");
    }
    assert_eq!(manifest["display"], "standalone");

    let served = ["/icon.svg", "/icon-maskable.svg"];
    for icon in manifest["icons"].as_array().unwrap() {
        let src = icon["src"].as_str().unwrap();
        assert!(
            served.contains(&src),
            "manifest names an unserved icon `{src}`"
        );
    }
    // A launcher crops the icon to its own shape; without a maskable one it
    // crops the square, cutting the mark.
    assert!(
        manifest["icons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["purpose"] == "maskable"),
        "no maskable icon: launchers will crop the square one"
    );

    // Every shortcut and the start URL must be routes the SPA resolves.
    let mut urls = vec![manifest["start_url"].as_str().unwrap().to_owned()];
    for s in manifest["shortcuts"].as_array().unwrap() {
        urls.push(s["url"].as_str().unwrap().to_owned());
    }
    let app_js = include_str!("../../web/app.js");
    for url in urls {
        let name = url.trim_start_matches("/ui/");
        assert!(
            app_js.contains(&format!("\\/ui\\/{name}")),
            "`{url}` is not a route the dashboard router knows"
        );
    }
}

/// The service worker must never answer for the API.
///
/// Everything under `/api/` is the live state of a cluster. A cached
/// environment list is a lie, and a lie about whether something is running
/// is worse than an error — so this is asserted rather than left to a
/// comment.
#[test]
fn the_service_worker_never_caches_api_responses() {
    let sw = include_str!("../../web/sw.js");
    assert!(
        sw.contains("url.pathname.startsWith(\"/api/\")"),
        "the worker no longer excludes /api/ from caching"
    );
    for asset in ["/style.css", "/app.js", "/i18n.js"] {
        assert!(sw.contains(asset), "the shell cache is missing `{asset}`");
    }
}

#[tokio::test]
async fn spa_deep_links_fall_back_to_the_dashboard_shell() {
    let app = test_app_with_token("s3cr3t").await;
    // The client-side router owns everything under `/ui/...` — a hard
    // refresh or a shared link on any of these has to return the same
    // `index.html` shell, not a 404, so the JS router can take over and
    // render the right page from `location.pathname`.
    for path in [
        "/ui/environments",
        "/ui/projects/1",
        "/ui/projects/1/secrets",
        "/ui/environments/1?tab=logs",
        "/ui/audit",
        "/ui/admin",
        "/this/route/does/not/exist/at/all",
    ] {
        let (status, body) = json_request(&app, "GET", path, json!({})).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        let text = String::from_utf8(body).unwrap();
        assert!(text.contains("OXID"), "{path}: {text:.200}");
    }
}

#[tokio::test]
async fn stats_endpoint_reports_aggregate_counts() {
    let (app, _) = test_app().await;
    let repo = repo_dir_with_config();
    let (_, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;
    let project: Project = serde_json::from_slice(&body).unwrap();
    json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
        json!({ "branch": "feature-login" }),
    )
    .await;

    let (status, body) = json_request(&app, "GET", "/api/v1/stats", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let node_stats: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(node_stats["projects"], 1);
    assert_eq!(node_stats["environments_running"], 1);
    assert!(node_stats["host_total_memory_bytes"].as_u64().unwrap() > 0);
    // No `with_traefik(...)` call in `test_app()` — the dashboard relies
    // on this to know an environment's `url` isn't a reachable link
    // without Traefik fronting it (SPEC.md's direct-port-publish mode).
    assert_eq!(node_stats["traefik_enabled"], false);
}

#[tokio::test]
async fn infra_status_requires_traefik_configured() {
    let (app, _) = test_app().await;
    let (status, body) = json_request(&app, "GET", "/api/v1/infra/status", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        value["error"]
            .as_str()
            .unwrap_or_default()
            .contains("OXID_DOCKER_NETWORK")
    );
}

#[tokio::test]
async fn infra_status_and_bootstrap_endpoints_round_trip() {
    let (app, _) = test_app_with_traefik().await;

    let (status, body) = json_request(&app, "GET", "/api/v1/infra/status", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let before: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(before["network"], "oxid-net");
    assert_eq!(before["network_exists"], false);

    let (status, body) = json_request(&app, "POST", "/api/v1/infra/bootstrap", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let after_bootstrap: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(after_bootstrap["network_exists"], true);

    // Idempotent: running it again through the API changes nothing and
    // still succeeds.
    let (status, body) = json_request(&app, "POST", "/api/v1/infra/bootstrap", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let second: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(second["network_exists"], true);

    let (status, body) = json_request(&app, "GET", "/api/v1/infra/status", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let after: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(after["network_exists"], true);
}

/// Same as `test_app_with_token`, started in zero-config (`OXID_AUTO_TOKEN=1`)
/// mode — what the shipped `docker-compose.yml` runs with.
async fn test_app_auto_token(token: &str) -> Router {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let cache = tempfile::tempdir().unwrap();
    let cp = ControlPlane::new(
        store,
        FakeGit::default(),
        FakeOci::default(),
        cache.path().to_owned(),
    )
    .with_readiness_check(false);
    router(ApiState {
        cp,
        webhook_secret: Some("test-secret".to_owned()),
        api_token: Some(token.to_owned()),
        data_dir: test_data_dir(),
        allow_restore: true,
        rate_limit: None,
        auto_token: true,
        bootstrap_access: BootstrapAccess::Loopback,
    })
}

#[tokio::test]
async fn setup_status_is_public_and_reports_configuration() {
    let (app, _) = test_app().await;
    // Deliberately unauthenticated — this is the pre-token onboarding probe.
    let (status, body) = json_request(&app, "GET", "/api/v1/setup/status", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert!(!value["version"].as_str().unwrap().is_empty());
    assert_eq!(value["auth_required"], false);
    assert_eq!(value["auto_token"], false);
    assert_eq!(value["webhook_secret_configured"], true);
}

#[tokio::test]
async fn setup_status_stays_public_behind_the_token_gate() {
    let app = test_app_auto_token("s3cr3t").await;
    let (status, body) = json_request(&app, "GET", "/api/v1/setup/status", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["auth_required"], true);
    assert_eq!(value["auto_token"], true);
}

#[tokio::test]
async fn webhook_secret_requires_master_and_reveals_value_to_it() {
    let app = test_app_auto_token("master-secret").await;

    let (status, _) = json_request(&app, "GET", "/api/v1/setup/webhook-secret", json!({})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, body) = json_request_with_auth(
        &app,
        "GET",
        "/api/v1/setup/webhook-secret",
        json!({}),
        Some("master-secret"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["webhook_secret"], "test-secret");
}

/// A named operator — even an *unscoped* one — must not learn the webhook
/// secret: knowing it means being able to forge `push` events that deploy
/// arbitrary branches. Master credential only.
#[tokio::test]
async fn webhook_secret_is_hidden_from_named_operators() {
    let app = test_app_with_token("master-secret").await;
    let (status, body) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/tokens",
        json!({ "name": "alice" }),
        Some("master-secret"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let created: Value = serde_json::from_slice(&body).unwrap();
    let alice = created["token"].as_str().unwrap().to_owned();

    let (status, _) = json_request_with_auth(
        &app,
        "GET",
        "/api/v1/setup/webhook-secret",
        json!({}),
        Some(&alice),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn webhook_secret_reports_404_when_nothing_is_configured() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let cache = tempfile::tempdir().unwrap();
    let cp = ControlPlane::new(
        store,
        FakeGit::default(),
        FakeOci::default(),
        cache.path().to_owned(),
    )
    .with_readiness_check(false);
    let app = router(ApiState {
        cp,
        webhook_secret: None,
        api_token: None,
        data_dir: test_data_dir(),
        allow_restore: true,
        rate_limit: None,
        auto_token: false,
        bootstrap_access: BootstrapAccess::Loopback,
    });
    let (status, body) = json_request(&app, "GET", "/api/v1/setup/webhook-secret", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        value["error"]
            .as_str()
            .unwrap_or_default()
            .contains("OXID_WEBHOOK_SECRET")
    );
}

/// A router around a `ControlPlane` with a caller-controlled [`FakeGit`] —
/// for exercising registration-by-URL without a real remote. When
/// `api_token` is `Some`, every master-level call must carry it.
async fn test_app_with_git_and_token(git: FakeGit, api_token: Option<&str>) -> Router {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let cache = tempfile::tempdir().unwrap();
    let cp = ControlPlane::new(store, git, FakeOci::default(), cache.path().to_owned())
        .with_readiness_check(false);
    router(ApiState {
        cp,
        webhook_secret: None,
        api_token: api_token.map(str::to_owned),
        data_dir: test_data_dir(),
        allow_restore: true,
        rate_limit: None,
        auto_token: false,
        bootstrap_access: BootstrapAccess::Loopback,
    })
}

/// Open-API variant (no bearer gate) for tests that don't touch credentials.
async fn test_app_with_git(git: FakeGit) -> Router {
    test_app_with_git_and_token(git, None).await
}

#[tokio::test]
async fn registering_by_url_clones_parses_and_is_idempotent() {
    let repo = repo_dir_with_config();
    // The daemon normalizes scp-style input server-side before anything
    // touches `RepoUrl::parse`:
    let app = test_app_with_git(FakeGit::at(repo.path())).await;

    let (status, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_url": "git@github.com:org/app.git", "git_token": "ghp-secret" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let project: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(project["name"], "app");
    assert_eq!(project["repo_url"], "ssh://git@github.com/org/app.git");

    // Idempotent by exact (normalized) repo URL — re-registering returns
    // the same project row instead of erroring or duplicating:
    let (status, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_url": "ssh://git@github.com/org/app.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let again: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(again["id"], project["id"]);
}

#[tokio::test]
async fn registering_by_unreachable_url_fails_eagerly_with_a_400() {
    let app = test_app_with_git(FakeGit::unreachable("authentication failed")).await;

    let (status, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_url": "https://github.com/org/private.git", "git_token": "bad" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let value: Value = serde_json::from_slice(&body).unwrap();
    let message = value["error"].as_str().unwrap_or_default();
    assert!(message.contains("cannot fetch"), "got: {message}");
    assert!(
        message.contains("with the provided git token"),
        "got: {message}"
    );
    // And nothing was registered:
    let (status, body) = json_request(&app, "GET", "/api/v1/projects", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), json!([]));
}

#[tokio::test]
async fn registering_by_url_rejects_structurally_bad_input_before_any_clone() {
    let repo = repo_dir_with_config();
    let app = test_app_with_git(FakeGit::at(repo.path())).await;

    // Both sources:
    let (status, _) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": "/repos/app", "repo_url": "https://github.com/org/app.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Neither:
    let (status, _) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "git_token": "x" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Not a URL:
    let (status, _) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_url": "not-a-url" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// A scoped operator must be able to *resolve* its own project by URL
/// (same right it has with `repo_dir`) but never trigger a clone of a new
/// remote — resolution happens before any fetch, and new registration
/// stays 403.
#[tokio::test]
async fn scoped_operator_resolves_own_project_by_url_without_cloning_new_ones() {
    const MASTER: &str = "master-secret";
    let repo = repo_dir_with_config();

    // Master (a *configured* API token this time — the scoped branch of the
    // handler only triggers when auth is actually enforced) registers the
    // project first…
    let app = test_app_with_git_and_token(FakeGit::at(repo.path()), Some(MASTER)).await;
    let (status, body) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_url": "https://github.com/org/app.git" }),
        Some(MASTER),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let project: Value = serde_json::from_slice(&body).unwrap();
    let project_id = project["id"].as_u64().unwrap();

    // …and mints alice scoped to exactly that project.
    let (status, body) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/tokens",
        json!({ "name": "alice", "projects": [project_id] }),
        Some(MASTER),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let created: Value = serde_json::from_slice(&body).unwrap();
    let alice = created["token"].as_str().unwrap().to_owned();

    // Alice resolves her own project by URL — this must NOT hit
    // `ensure_repo` at all (resolution is a pure DB lookup):
    let (status, body) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_url": "https://github.com/org/app.git" }),
        Some(&alice),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let resolved: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(resolved["id"], project_id);

    // A different (unregistered) URL is 403 — brand-new registration — and
    // provably never reached ensure_repo (this fake fails any real clone).
    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_url": "https://github.com/other/repo.git" }),
        Some(&alice),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn api_is_open_when_no_token_is_configured() {
    let (app, _) = test_app().await;
    let (status, _) = json_request(&app, "GET", "/api/v1/projects", json!({})).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn protected_routes_reject_missing_or_wrong_token() {
    let app = test_app_with_token("s3cr3t").await;

    let (status, _) = json_request(&app, "GET", "/api/v1/projects", json!({})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = json_request_with_auth(
        &app,
        "GET",
        "/api/v1/projects",
        json!({}),
        Some("wrong-token"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn protected_routes_accept_the_correct_token() {
    let app = test_app_with_token("s3cr3t").await;
    let (status, _) =
        json_request_with_auth(&app, "GET", "/api/v1/projects", json!({}), Some("s3cr3t")).await;
    assert_eq!(status, StatusCode::OK);
}

/// `/health` and the Traefik-facing `/wake`/`/heartbeat` endpoints must
/// stay reachable without a token even when one is configured — Traefik
/// has no way to attach it, and health checks shouldn't need auth.
#[tokio::test]
async fn public_routes_stay_open_even_with_a_token_configured() {
    let app = test_app_with_token("s3cr3t").await;
    let (status, _) = json_request(&app, "GET", "/api/v1/health", json!({})).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = request_with_host(&app, "GET", "/api/v1/heartbeat", "nobody").await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = request_with_host(&app, "POST", "/api/v1/wake", "nobody").await;
    assert_eq!(status, StatusCode::OK);
}

/// Issuing access needs the `admin` role, not the master token itself.
///
/// This deliberately changed: while it was master-only, a devops who wanted
/// to delegate user management had to hand out the master credential — the
/// one thing the role model exists to stop. A non-admin still cannot, which
/// is the part that matters.
#[tokio::test]
async fn issuing_access_requires_an_admin_credential() {
    let app = test_app_with_token("master-secret").await;

    // A request with no token at all isn't even authenticated.
    let (status, _) =
        json_request(&app, "POST", "/api/v1/tokens", json!({ "name": "alice" })).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The master token can mint one.
    let (status, body) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/tokens",
        json!({ "name": "alice" }),
        Some("master-secret"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let created: Value = serde_json::from_slice(&body).unwrap();
    let alice_token = created["token"].as_str().unwrap().to_owned();
    assert_eq!(alice_token.len(), 64, "expected a 32-byte hex token");

    // Alice was minted with no `role`, and no scope — which is `admin`, the
    // power an unscoped named token has always had. She *can* now delegate.
    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/tokens",
        json!({ "name": "bob" }),
        Some(&alice_token),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "an admin may issue access");

    // But a developer may not — this is the line that has to hold.
    let dev = mint_token(
        &app,
        json!({ "name": "dev", "projects": [1], "role": "developer" }),
    )
    .await;
    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/tokens",
        json!({ "name": "escalation" }),
        Some(&dev),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_named_token_authenticates_and_attributes_audit_events() {
    let repo = repo_dir_with_config();
    let app = test_app_with_token("master-secret").await;

    let (_, body) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/tokens",
        json!({ "name": "alice" }),
        Some("master-secret"),
    )
    .await;
    let alice_token = serde_json::from_slice::<Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();

    let (status, body) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
        Some(&alice_token),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let project: Project = serde_json::from_slice(&body).unwrap();

    let (status, body) = json_request_with_auth(
        &app,
        "POST",
        format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
        json!({ "branch": "feature-login" }),
        Some(&alice_token),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let env: Environment = serde_json::from_slice(&body).unwrap();

    let (status, body) = json_request_with_auth(
        &app,
        "GET",
        format!("/api/v1/environments/{}/audit", env.id.0).as_str(),
        json!({}),
        Some("master-secret"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let events: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert!(
        events.iter().any(|e| e["operator"] == "alice"),
        "{events:?}"
    );
}

#[tokio::test]
async fn revoked_tokens_stop_authenticating() {
    let app = test_app_with_token("master-secret").await;
    let (_, body) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/tokens",
        json!({ "name": "alice" }),
        Some("master-secret"),
    )
    .await;
    let created: Value = serde_json::from_slice(&body).unwrap();
    let alice_token = created["token"].as_str().unwrap().to_owned();
    let id = created["id"].as_u64().unwrap();

    let (status, _) = json_request_with_auth(
        &app,
        "GET",
        "/api/v1/projects",
        json!({}),
        Some(&alice_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = json_request_with_auth(
        &app,
        "DELETE",
        format!("/api/v1/tokens/{id}").as_str(),
        json!({}),
        Some("master-secret"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = json_request_with_auth(
        &app,
        "GET",
        "/api/v1/projects",
        json!({}),
        Some(&alice_token),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Mints a token as the master credential and returns its raw value.
async fn mint_token(app: &Router, body: Value) -> String {
    let (status, response) =
        json_request_with_auth(app, "POST", "/api/v1/tokens", body, Some("master-secret")).await;
    assert_eq!(status, StatusCode::CREATED);
    serde_json::from_slice::<Value>(&response).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// An app whose storage holds *two* distinct projects (ids 1 = `app-a`,
/// 2 = `app-b`) behind a master-token credential — the minimal fixture for
/// project-scoping tests. Project 1 deliberately carries `FakeGit`'s fixed
/// remote URL, so a `POST /projects` registration resolves to it from any
// checkout, exactly as the real dedupe path would.
async fn two_project_app() -> Router {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let cache = tempfile::tempdir().unwrap();
    for (name, url) in [
        ("app-a", "https://github.com/org/app.git"), // == FakeGit's fixed remote
        ("app-b", "https://github.com/org/app-b.git"),
    ] {
        let repo = repo_dir_named(name);
        let parsed = crate::adapter::config::parse_project(repo.path()).unwrap();
        let url = RepoUrl::parse(url).unwrap();
        let project = Project::new(ProjectId(0), parsed.name, url, parsed.config).unwrap();
        ProjectStore::create(&store, &project).await.unwrap();
    }
    let cp = ControlPlane::new(
        store,
        FakeGit::default(),
        FakeOci::default(),
        cache.path().to_owned(),
    )
    .with_readiness_check(false);
    router(ApiState {
        cp,
        webhook_secret: Some("test-secret".to_owned()),
        api_token: Some("master-secret".to_owned()),
        data_dir: test_data_dir(),
        allow_restore: true,
        rate_limit: None,
        auto_token: false,
        bootstrap_access: BootstrapAccess::Loopback,
    })
}

#[tokio::test]
async fn an_empty_project_scope_list_is_rejected() {
    let app = test_app_with_token("master-secret").await;
    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/tokens",
        json!({ "name": "useless", "projects": [] }),
        Some("master-secret"),
    )
    .await;
    // A can-do-nothing token is a client bug — refuse it loudly instead of
    // minting credentials that silently do nothing.
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn scoped_tokens_only_see_and_act_on_their_own_projects() {
    let app = two_project_app().await;
    // ids 1 (`app-a`) and 2 (`app-b`) — see `two_project_app`.

    let bob = mint_token(&app, json!({ "name": "bob", "projects": [1] })).await;

    // Listing hides out-of-scope projects entirely.
    let (status, body) =
        json_request_with_auth(&app, "GET", "/api/v1/projects", json!({}), Some(&bob)).await;
    assert_eq!(status, StatusCode::OK);
    let visible: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        visible.iter().map(|p| p["id"].as_u64()).collect::<Vec<_>>(),
        vec![Some(1)],
        "a scoped operator must see only its own projects"
    );

    // Out-of-scope project endpoints answer 404 (not 403 — no existence leak).
    // Each entry carries a body that passes *that* handler's own JSON
    // validation, so the rejection provably comes from scoping, not from a
    // malformed request.
    let cases: [(&str, &str, Value); 6] = [
        (
            "PATCH",
            "/api/v1/projects/2",
            json!({ "pause_after": "45m" }),
        ),
        ("DELETE", "/api/v1/projects/2", json!({})),
        ("GET", "/api/v1/projects/2/environments", json!({})),
        (
            "POST",
            "/api/v1/projects/2/secrets",
            json!({ "name": "K", "scope": "project", "value": "v" }),
        ),
        (
            "POST",
            "/api/v1/projects/2/deploy",
            json!({ "branch": "main" }),
        ),
        (
            "POST",
            "/api/v1/projects/2/rollback",
            json!({ "branch": "main" }),
        ),
    ];
    for (method, uri, payload) in cases {
        let (status, _) = json_request_with_auth(&app, method, uri, payload, Some(&bob)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
    }

    // In-scope actions still work end-to-end.
    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/projects/1/deploy",
        json!({ "branch": "feature-x" }),
        Some(&bob),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/projects/1/secrets",
        json!({ "name": "DB_PASS", "scope": "project", "value": "hunter2" }),
        Some(&bob),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = json_request_with_auth(
        &app,
        "PATCH",
        "/api/v1/projects/1",
        json!({ "pause_after": "45m" }),
        Some(&bob),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // `oxid up` registers before every deploy — a scoped token must resolve
    // its own project through that path.
    let repo = repo_dir_with_config();
    let (status, body) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
        Some(&bob),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let resolved: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        resolved["id"].as_u64(),
        Some(1),
        "registration resolves the in-scope project instead of creating a new one"
    );

    // The same call from a token scoped to the *other* project is a 404 —
    // registering must not leak that project 1 exists, and it certainly
    // must not create anything.
    let carol = mint_token(&app, json!({ "name": "carol", "projects": [2] })).await;
    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
        Some(&carol),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn token_scopes_are_normalized_on_creation() {
    let app = test_app_with_token("master-secret").await;
    mint_token(&app, json!({ "name": "ci", "projects": [7, 3, 7] })).await;

    let (_, body) = json_request_with_auth(
        &app,
        "GET",
        "/api/v1/tokens",
        json!({}),
        Some("master-secret"),
    )
    .await;
    let tokens: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        tokens[0]["scoped_projects"],
        json!([3, 7]),
        "scopes are sorted and deduplicated at creation"
    );
}

#[tokio::test]
async fn scoped_tokens_are_locked_out_of_node_wide_endpoints() {
    let app = test_app_with_token("master-secret").await;
    let bob = mint_token(&app, json!({ "name": "bob", "projects": [1] })).await;

    // Registering new projects is outside any scope by definition.
    let repo = repo_dir_named("anywhere");
    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
        Some(&bob),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Global secrets affect every project's deploys.
    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/secrets",
        json!({ "name": "SHARED", "scope": "global", "value": "x" }),
        Some(&bob),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Node-wide reads/writes: stats, infra and backups.
    for (method, uri) in [
        ("GET", "/api/v1/stats"),
        ("GET", "/api/v1/infra/status"),
        ("POST", "/api/v1/infra/bootstrap"),
        ("GET", "/api/v1/backup"),
    ] {
        let (status, _) = json_request_with_auth(&app, method, uri, json!({}), Some(&bob)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
    }
}

#[tokio::test]
async fn scoped_audit_and_environment_reads_stay_within_the_scope() {
    let app = two_project_app().await;

    // Two environments with audit events, one per project (ids 1 and 2).
    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/projects/1/deploy",
        json!({ "branch": "main" }),
        Some("master-secret"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/projects/2/deploy",
        json!({ "branch": "main" }),
        Some("master-secret"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let bob = mint_token(&app, json!({ "name": "bob", "projects": [1] })).await;

    // Unfiltered global audit collapses to just the scoped project's events.
    let (status, body) =
        json_request_with_auth(&app, "GET", "/api/v1/audit", json!({}), Some(&bob)).await;
    assert_eq!(status, StatusCode::OK);
    let events: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert!(!events.is_empty(), "expected at least the deploy event");
    assert!(
        events
            .iter()
            .all(|e| e["environment_id"].as_u64() == Some(1)),
        "scoped operator saw another project's audit events: {events:?}"
    );

    // An explicit out-of-scope project filter is a 404, like every other
    // out-of-scope project access.
    let (status, _) = json_request_with_auth(
        &app,
        "GET",
        "/api/v1/audit?project_id=2",
        json!({}),
        Some(&bob),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Environment-addressed routes authorize through the environment's
    // owning project.
    let (status, _) = json_request_with_auth(
        &app,
        "GET",
        "/api/v1/environments/2/audit",
        json!({}),
        Some(&bob),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = json_request_with_auth(
        &app,
        "DELETE",
        "/api/v1/environments/2",
        json!({}),
        Some(&bob),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unscoped_tokens_keep_full_access() {
    let repo = repo_dir_with_config();
    let app = test_app_with_token("master-secret").await;
    let alice = mint_token(&app, json!({ "name": "alice" })).await;

    // No `projects` field = same reach as the master credential (this is
    // what pre-scoping named tokens were, so existing deployments keep
    // working unchanged after upgrade).
    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
        Some(&alice),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _) =
        json_request_with_auth(&app, "GET", "/api/v1/stats", json!({}), Some(&alice)).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/secrets",
        json!({ "name": "SHARED", "scope": "global", "value": "x" }),
        Some(&alice),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn rotate_key_requires_master_and_keeps_secrets_readable() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let cache = tempfile::tempdir().unwrap();
    let cp = ControlPlane::new(
        store,
        FakeGit::default(),
        FakeOci::default(),
        cache.path().to_owned(),
    )
    .with_readiness_check(false);
    let data_dir = test_data_dir();
    let app = router(ApiState {
        cp,
        webhook_secret: Some("test-secret".to_owned()),
        api_token: Some("master-secret".to_owned()),
        data_dir: data_dir.clone(),
        allow_restore: true,
        rate_limit: None,
        auto_token: false,
        bootstrap_access: BootstrapAccess::Loopback,
    });

    json_request_with_auth(
        &app,
        "POST",
        "/api/v1/secrets",
        json!({ "name": "DB_PASSWORD", "scope": "global", "value": "hunter2" }),
        Some("master-secret"),
    )
    .await;

    let old_key = std::fs::read(data_dir.join("secret.key")).unwrap();

    // A named (non-master) token can't rotate the key.
    let (_, body) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/tokens",
        json!({ "name": "alice" }),
        Some("master-secret"),
    )
    .await;
    let alice_token = serde_json::from_slice::<Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/rotate-key",
        json!({}),
        Some(&alice_token),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The master token can, and the secret.key file actually changes.
    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/rotate-key",
        json!({}),
        Some("master-secret"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let new_key = std::fs::read(data_dir.join("secret.key")).unwrap();
    assert_ne!(old_key, new_key);

    // Secrets set before rotation are still readable after it — hot
    // rotation, not "wipe and start over".
    let (status, body) = json_request_with_auth(
        &app,
        "GET",
        "/api/v1/secrets",
        json!({}),
        Some("master-secret"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["secrets"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn rate_limit_blocks_a_burst_past_its_configured_size() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let cache = tempfile::tempdir().unwrap();
    let cp = ControlPlane::new(
        store,
        FakeGit::default(),
        FakeOci::default(),
        cache.path().to_owned(),
    )
    .with_readiness_check(false);
    let app = router(ApiState {
        cp,
        webhook_secret: Some("test-secret".to_owned()),
        api_token: None,
        data_dir: test_data_dir(),
        allow_restore: true,
        // 1 request/sec sustained, burst of 2 — the 3rd immediate
        // request must be rejected.
        rate_limit: Some((1, 2)),
        auto_token: false,
        bootstrap_access: BootstrapAccess::Loopback,
    });

    let mut statuses = Vec::new();
    for _ in 0..3 {
        let (status, _) = json_request(&app, "GET", "/api/v1/projects", json!({})).await;
        statuses.push(status);
    }
    assert_eq!(statuses[0], StatusCode::OK);
    assert_eq!(statuses[1], StatusCode::OK);
    assert_eq!(statuses[2], StatusCode::TOO_MANY_REQUESTS, "{statuses:?}");

    // Public routes (no auth gate) are never rate-limited by this —
    // Traefik's forwardAuth heartbeat hits `/heartbeat` on every single
    // request to a live app and must never be throttled.
    for _ in 0..5 {
        let (status, _) = request_with_host(&app, "POST", "/api/v1/wake", "nobody").await;
        assert_eq!(status, StatusCode::OK);
    }
}

/// Like `json_request`, but tags the request with a `ConnectInfo` peer
/// address, as the real serve paths do via
/// `into_make_service_with_connect_info` — this is what the per-IP rate
/// limiter keys on (`ClientIpKeyExtractor`).
async fn json_request_from_ip(
    app: &Router,
    method: &str,
    uri: &str,
    body: Value,
    ip: std::net::IpAddr,
) -> (StatusCode, Vec<u8>) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .extension(axum::extract::ConnectInfo(std::net::SocketAddr::new(ip, 0)))
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

/// Builds a router whose auto-generated master token the bootstrap
/// endpoint would hand over, under a given access policy.
async fn auto_token_router_with(access: BootstrapAccess) -> Router {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let cache = tempfile::tempdir().unwrap();
    let cp = ControlPlane::new(
        store,
        FakeGit::default(),
        FakeOci::default(),
        cache.path().to_owned(),
    )
    .with_readiness_check(false);
    router(ApiState {
        cp,
        webhook_secret: Some("test-secret".to_owned()),
        api_token: Some("auto-generated-master".to_owned()),
        data_dir: test_data_dir(),
        allow_restore: false,
        rate_limit: None,
        auto_token: true,
        bootstrap_access: access,
    })
}

/// Builds a router whose auto-generated master token the bootstrap
/// endpoint would hand over.
async fn auto_token_router() -> Router {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let cache = tempfile::tempdir().unwrap();
    let cp = ControlPlane::new(
        store,
        FakeGit::default(),
        FakeOci::default(),
        cache.path().to_owned(),
    )
    .with_readiness_check(false);
    router(ApiState {
        cp,
        webhook_secret: Some("test-secret".to_owned()),
        api_token: Some("auto-generated-master".to_owned()),
        data_dir: test_data_dir(),
        allow_restore: false,
        rate_limit: None,
        auto_token: true,
        bootstrap_access: BootstrapAccess::Loopback,
    })
}

#[tokio::test]
async fn the_bootstrap_token_is_served_to_a_caller_on_this_host() {
    let app = auto_token_router().await;
    let (status, body) = json_request_from_ip(
        &app,
        "GET",
        "/api/v1/setup/token",
        json!({}),
        "127.0.0.1".parse().unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["token"], "auto-generated-master");
}

#[tokio::test]
async fn the_bootstrap_token_is_never_served_off_host() {
    // This endpoint hands over the master credential with no authentication
    // at all, which is only defensible for a caller already on the host.
    // That used to be enforced only by the shipped compose publishing on
    // 127.0.0.1, while `OXID_ADDR` defaults to `0.0.0.0` — so
    // `OXID_AUTO_TOKEN=1` on a default bind answered this to the whole
    // network, and the token it returns opens `GET /api/v1/backup` (the
    // database and the AES master key) and the webhook secret.
    let app = auto_token_router().await;
    for peer in ["192.168.1.95", "10.0.0.4", "8.8.8.8"] {
        let (status, body) = json_request_from_ip(
            &app,
            "GET",
            "/api/v1/setup/token",
            json!({}),
            peer.parse().unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "peer {peer} was served");
        assert!(
            !String::from_utf8_lossy(&body).contains("auto-generated-master"),
            "peer {peer} was given the token in the error body"
        );
    }
}

#[tokio::test]
async fn the_bootstrap_token_denies_a_caller_whose_address_is_unknown() {
    // No `ConnectInfo` means nothing looked at where the request came from.
    // The only safe reading of that is "not local".
    let app = auto_token_router().await;
    let (status, _) = json_request(&app, "GET", "/api/v1/setup/token", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_forwarded_for_header_cannot_fake_being_local() {
    // Any client can set this header; only the peer address is evidence.
    let app = auto_token_router().await;
    let request = Request::builder()
        .method("GET")
        .uri("/api/v1/setup/token")
        .header("x-forwarded-for", "127.0.0.1")
        .header("x-real-ip", "127.0.0.1")
        .header("forwarded", "for=127.0.0.1")
        .extension(axum::extract::ConnectInfo(std::net::SocketAddr::new(
            "192.168.1.95".parse().unwrap(),
            0,
        )))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_containerized_daemon_can_still_serve_its_operator() {
    // Docker rewrites the peer address: with the port published on
    // 127.0.0.1, the operator's own request still arrives from the bridge
    // gateway, so a loopback-only rule refuses the very person it is for.
    // Confirmed against a real container — `oxid token generate` and the
    // dashboard's "generate for me" button both 404'd. The daemon cannot
    // tell that case from a stranger's request forwarded in from a public
    // publish, so the operator says which it is.
    let app = auto_token_router_with(BootstrapAccess::Any).await;
    let (status, body) = json_request_from_ip(
        &app,
        "GET",
        "/api/v1/setup/token",
        json!({}),
        "172.17.0.1".parse().unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["token"], "auto-generated-master");
}

#[tokio::test]
async fn the_bootstrap_token_can_be_switched_off_entirely() {
    let app = auto_token_router_with(BootstrapAccess::Off).await;
    for peer in ["127.0.0.1", "172.17.0.1", "192.168.1.95"] {
        let (status, _) = json_request_from_ip(
            &app,
            "GET",
            "/api/v1/setup/token",
            json!({}),
            peer.parse().unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "peer {peer} was served");
    }
}

#[test]
fn the_access_policy_defaults_to_withholding() {
    // Anything unrecognized must land on the safe side, not be treated as
    // permission.
    assert_eq!(BootstrapAccess::default(), BootstrapAccess::Loopback);
    assert!(!BootstrapAccess::Loopback.permits(None));
    assert!(!BootstrapAccess::Loopback.permits(Some("172.17.0.1".parse().unwrap())));
    assert!(BootstrapAccess::Loopback.permits(Some("127.0.0.1".parse().unwrap())));
    assert!(BootstrapAccess::Loopback.permits(Some("::1".parse().unwrap())));
    assert!(!BootstrapAccess::Off.permits(Some("127.0.0.1".parse().unwrap())));
    assert!(BootstrapAccess::Any.permits(Some("8.8.8.8".parse().unwrap())));
}

/// Two clients behind the same daemon get independent token buckets: IP A
/// exhausting its burst must not consume any of IP B's allowance (the
/// pre-per-IP global bucket failed exactly this — one noisy host throttled
/// every other client of the shared API token).
#[tokio::test]
async fn rate_limit_keys_per_client_ip_when_connect_info_present() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let cache = tempfile::tempdir().unwrap();
    let cp = ControlPlane::new(
        store,
        FakeGit::default(),
        FakeOci::default(),
        cache.path().to_owned(),
    )
    .with_readiness_check(false);
    let app = router(ApiState {
        cp,
        webhook_secret: Some("test-secret".to_owned()),
        api_token: None,
        data_dir: test_data_dir(),
        allow_restore: true,
        // 1 request/sec sustained, burst of 2 per IP.
        rate_limit: Some((1, 2)),
        auto_token: false,
        bootstrap_access: BootstrapAccess::Loopback,
    });

    let ip_a: std::net::IpAddr = "10.0.0.1".parse().unwrap();
    let ip_b: std::net::IpAddr = "10.0.0.2".parse().unwrap();

    // IP A burns through its whole burst…
    for _ in 0..2 {
        let (status, _) =
            json_request_from_ip(&app, "GET", "/api/v1/projects", json!({}), ip_a).await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, _) = json_request_from_ip(&app, "GET", "/api/v1/projects", json!({}), ip_a).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // …and IP B still has a fresh bucket of its own.
    let (status, _) = json_request_from_ip(&app, "GET", "/api/v1/projects", json!({}), ip_b).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn register_and_deploy_flow() {
    let repo = repo_dir_with_config();
    let (app, oci) = test_app().await;

    let (status, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let project: Project = serde_json::from_slice(&body).unwrap();
    assert_eq!(project.name, "app");

    let (status, body) = json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
        json!({ "branch": "feature-login" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let env: Environment = serde_json::from_slice(&body).unwrap();
    assert_eq!(env.state.to_string(), "running");
    assert_eq!(env.url, "feature-login.app.local.dev");

    let (status, body) = json_request(
        &app,
        "GET",
        format!("/api/v1/projects/{}/environments", project.id.0).as_str(),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let envs: Vec<Environment> = serde_json::from_slice(&body).unwrap();
    assert_eq!(envs.len(), 1);

    let calls = oci.calls.lock().unwrap();
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("run:oxid-app-feature-login")),
        "{calls:?}"
    );
}

#[tokio::test]
async fn backup_produces_a_tar_with_a_valid_sqlite_snapshot_and_the_secret_key() {
    // `VACUUM INTO` (what `backup_to` uses) doesn't work against a
    // `:memory:` source — a real file-backed store is needed here,
    // matching how the daemon always runs in production.
    let data_dir = test_data_dir();
    let store = SqliteStore::open(data_dir.join("audit.sqlite"), Cipher::from_key([1u8; 32]))
        .await
        .unwrap();
    let cache = tempfile::tempdir().unwrap();
    let cp = ControlPlane::new(
        store,
        FakeGit::default(),
        FakeOci::default(),
        cache.path().to_owned(),
    )
    .with_readiness_check(false);
    let app = router(ApiState {
        cp,
        webhook_secret: Some("test-secret".to_owned()),
        api_token: None,
        data_dir: data_dir.clone(),
        allow_restore: true,
        rate_limit: None,
        auto_token: false,
        bootstrap_access: BootstrapAccess::Loopback,
    });
    // Give the backup something real to capture.
    json_request(
        &app,
        "POST",
        "/api/v1/secrets",
        json!({ "name": "GLOBAL_X", "scope": "global", "value": "v" }),
    )
    .await;

    let (status, body) = raw_request(&app, "GET", "/api/v1/backup", Vec::new()).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    let dir = tempfile::tempdir().unwrap();
    let mut archive = tar::Archive::new(body.as_slice());
    archive.unpack(dir.path()).unwrap();
    assert!(dir.path().join("secret.key").exists());
    let snapshot = dir.path().join("audit.sqlite");
    assert!(snapshot.exists());

    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&snapshot);
    let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM secrets WHERE name = 'GLOBAL_X'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, 1);
}

#[tokio::test]
async fn restore_is_rejected_when_not_allowed() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let cache = tempfile::tempdir().unwrap();
    let cp = ControlPlane::new(
        store,
        FakeGit::default(),
        FakeOci::default(),
        cache.path().to_owned(),
    )
    .with_readiness_check(false);
    let app = router(ApiState {
        cp,
        webhook_secret: Some("test-secret".to_owned()),
        api_token: None,
        data_dir: test_data_dir(),
        allow_restore: false,
        rate_limit: None,
        auto_token: false,
        bootstrap_access: BootstrapAccess::Loopback,
    });

    let (status, _) = raw_request(&app, "POST", "/api/v1/backup/restore", vec![1, 2, 3]).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn restore_stages_the_upload_without_touching_the_live_database() {
    let data_dir = test_data_dir();
    let store = SqliteStore::open(data_dir.join("audit.sqlite"), Cipher::from_key([1u8; 32]))
        .await
        .unwrap();
    let cache = tempfile::tempdir().unwrap();
    let cp = ControlPlane::new(
        store,
        FakeGit::default(),
        FakeOci::default(),
        cache.path().to_owned(),
    )
    .with_readiness_check(false);
    let app = router(ApiState {
        cp,
        webhook_secret: Some("test-secret".to_owned()),
        api_token: None,
        data_dir: data_dir.clone(),
        allow_restore: true,
        rate_limit: None,
        auto_token: false,
        bootstrap_access: BootstrapAccess::Loopback,
    });

    let (status, body) = raw_request(&app, "GET", "/api/v1/backup", Vec::new()).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = raw_request(&app, "POST", "/api/v1/backup/restore", body).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // Staged for the next restart to pick up, not applied in place —
    // the running daemon's own database is left untouched.
    assert!(data_dir.join(".restore-pending.tar").exists());
    let (status, _) = json_request(&app, "GET", "/api/v1/health", json!({})).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn audit_endpoints_expose_the_previously_write_only_trail() {
    let repo = repo_dir_with_config();
    let (app, _) = test_app().await;
    let (_, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;
    let project: Project = serde_json::from_slice(&body).unwrap();
    let (_, body) = json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
        json!({ "branch": "feature-login" }),
    )
    .await;
    let env: Environment = serde_json::from_slice(&body).unwrap();

    let (status, body) = json_request(
        &app,
        "GET",
        format!("/api/v1/environments/{}/audit", env.id.0).as_str(),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let events: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert!(!events.is_empty(), "expected at least the Deploy event");
    assert_eq!(events[0]["environment_id"], env.id.0);

    let (status, body) = json_request(&app, "GET", "/api/v1/audit", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let recent: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert!(
        recent.iter().any(|e| e["environment_id"] == env.id.0),
        "{recent:?}"
    );

    // `kind`/`project_id` narrow `/api/v1/audit` to a subset.
    let (status, body) =
        json_request(&app, "GET", "/api/v1/audit?kind=build_succeeded", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let filtered: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert!(
        filtered
            .iter()
            .all(|e| e["kind"] == "build_succeeded" && e["environment_id"] == env.id.0),
        "{filtered:?}"
    );

    let (status, body) = json_request(
        &app,
        "GET",
        format!("/api/v1/audit?project_id={}", project.id.0 + 1).as_str(),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let none: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert!(none.is_empty(), "{none:?}");

    // An unparseable `kind`/`since` is a 400, not a silently-ignored filter.
    let (status, _) = json_request(&app, "GET", "/api/v1/audit?kind=not_a_kind", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = json_request(&app, "GET", "/api/v1/audit?since=not-a-date", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn deploy_request_id_is_correlated_into_the_audit_trail() {
    let repo = repo_dir_with_config();
    let (app, _) = test_app().await;
    let (_, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;
    let project: Project = serde_json::from_slice(&body).unwrap();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/projects/{}/deploy", project.id.0))
                .header("content-type", "application/json")
                .header("x-request-id", "trace-abc-123")
                .body(Body::from(json!({ "branch": "feature-login" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.headers().get("x-request-id").unwrap(),
        "trace-abc-123"
    );
    let body = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let env: Environment = serde_json::from_slice(&body).unwrap();

    let (_, body) = json_request(
        &app,
        "GET",
        format!("/api/v1/environments/{}/audit", env.id.0).as_str(),
        json!({}),
    )
    .await;
    let events: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert!(
        events
            .iter()
            .any(|e| e["request_id"] == "trace-abc-123" && e["kind"] == "build_succeeded"),
        "{events:?}"
    );
}

#[tokio::test]
async fn every_response_carries_a_request_id_header_and_echoes_a_provided_one() {
    let (app, _) = test_app().await;

    // No `X-Request-Id` sent: one is generated.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let generated = response
        .headers()
        .get("x-request-id")
        .expect("x-request-id header present")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(!generated.is_empty());

    // A caller-supplied id is echoed back unchanged.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/health")
                .header("x-request-id", "my-trace-id-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.headers().get("x-request-id").unwrap(),
        "my-trace-id-123"
    );
}

#[tokio::test]
async fn deploy_queues_past_capacity_and_the_queue_endpoint_reports_it() {
    // 8GB host (FakeOci's fixed `host_capacity`) minus 8000MB reserved
    // leaves 192MB usable; two 100MB deploys can't both fit.
    let (app, _) = test_app_with_admission_control(8000, 100).await;
    let repo = repo_dir_with_config();
    let (_, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;
    let project: Project = serde_json::from_slice(&body).unwrap();

    let (status, _) = json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
        json!({ "branch": "main" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
        json!({ "branch": "other" }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let queued: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(queued["status"], "queued");
    assert_eq!(queued["position"], 1);

    let (status, body) = json_request(&app, "GET", "/api/v1/queue", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let entries: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0]["branch"], "other");
}

#[tokio::test]
async fn one_repository_can_hold_several_services() {
    // A monorepo's API and web app deploy, scale and fail independently.
    // `repo_url` being UNIQUE made the second one impossible to register;
    // what is actually unique is the repository *plus the part being built*.
    let (app, _) = test_app().await;
    let repo = repo_dir_with_config();
    let dir = repo.path().display().to_string();

    let (status, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": dir, "context": "apps/api" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let api: Project = serde_json::from_slice(&body).unwrap();
    assert_eq!(api.config.build.context, "apps/api");

    let (status, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": dir, "context": "apps/web" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let web: Project = serde_json::from_slice(&body).unwrap();
    assert_ne!(api.id.0, web.id.0, "the second service reused the first");
    assert_eq!(web.config.build.context, "apps/web");

    // Still idempotent per service: the same repo and the same part of it
    // is the same project, not a third one.
    let (_, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": dir, "context": "apps/api" }),
    )
    .await;
    let again: Project = serde_json::from_slice(&body).unwrap();
    assert_eq!(again.id.0, api.id.0);

    let (_, body) = json_request(&app, "GET", "/api/v1/projects", json!({})).await;
    let all: Vec<Project> = serde_json::from_slice(&body).unwrap();
    assert_eq!(all.len(), 2, "{all:?}");
}

#[tokio::test]
async fn a_push_deploys_every_service_of_the_repository() {
    // Oxid cannot know which packages a commit touched without building the
    // workspace's dependency graph, and guessing wrong leaves a service
    // silently running stale code. So a push deploys all of them.
    let (app, _) = test_app().await;
    let repo = repo_dir_with_config();
    let dir = repo.path().display().to_string();
    for context in ["apps/api", "apps/web"] {
        json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": dir, "context": context }),
        )
        .await;
    }

    let (status, body) = signed_webhook(
        &app,
        json!({
            "ref": "refs/heads/main",
            "after": "abc123",
            "repository": { "full_name": "org/app", "clone_url": "https://github.com/org/app.git" },
            "pusher": { "name": "dev" }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let queued: Value = serde_json::from_slice(&body).unwrap();
    let services = queued["queued"].as_array().unwrap();
    assert_eq!(services.len(), 2, "{queued}");
    let contexts: Vec<_> = services
        .iter()
        .map(|s| s["service"].as_str().unwrap())
        .collect();
    assert!(contexts.contains(&"apps/api"), "{queued}");
    assert!(contexts.contains(&"apps/web"), "{queued}");
    // The old single-service shape is still there for scripts that read it.
    assert!(queued["position"].is_number(), "{queued}");
}

#[tokio::test]
async fn a_queued_deploy_can_be_cancelled() {
    // The drain stops at the first entry that does not fit, so a large
    // deploy is not starved by small ones behind it. The other side of that
    // is that an entry which can *never* fit holds up everything behind it,
    // and until this endpoint the only cures were making it fit or
    // restarting into an empty database.
    let (app, _) = test_app_with_admission_control(8000, 100).await;
    let repo = repo_dir_with_config();
    let (_, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;
    let project: Project = serde_json::from_slice(&body).unwrap();
    for branch in ["main", "other"] {
        json_request(
            &app,
            "POST",
            format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
            json!({ "branch": branch }),
        )
        .await;
    }

    let (_, body) = json_request(&app, "GET", "/api/v1/queue", json!({})).await;
    let entries: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(entries.len(), 1, "{entries:?}");
    let id = entries[0]["id"].as_u64().unwrap();

    let (status, body) = json_request(
        &app,
        "DELETE",
        format!("/api/v1/queue/{id}").as_str(),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let cancelled: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    // The response names the branch: a queue id means nothing to whoever
    // has to say what was dropped.
    assert_eq!(cancelled["branch"], "other");

    let (_, body) = json_request(&app, "GET", "/api/v1/queue", json!({})).await;
    let entries: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert!(entries.is_empty(), "{entries:?}");
}

#[tokio::test]
async fn a_scoped_operator_cannot_cancel_another_project_s_queued_deploy() {
    // Same rule the listing follows, and the same answer: `404`, not `403`.
    // "That exists but isn't yours" is itself information about a project
    // this credential is not supposed to know about.
    let store = SqliteStore::open_in_memory().await.unwrap();
    let cache = tempfile::tempdir().unwrap();
    let cp = ControlPlane::new(
        store,
        FakeGit::default(),
        FakeOci::default(),
        cache.path().to_owned(),
    )
    .with_resource_defaults(Some(100), None)
    .with_admission_control(Some(8000))
    .with_readiness_check(false);
    let app = router(ApiState {
        cp,
        webhook_secret: Some("test-secret".to_owned()),
        api_token: Some("master-secret".to_owned()),
        data_dir: test_data_dir(),
        allow_restore: false,
        rate_limit: None,
        auto_token: false,
        bootstrap_access: BootstrapAccess::Loopback,
    });

    let repo = repo_dir_with_config();
    json_request_with_auth(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
        Some("master-secret"),
    )
    .await;
    for branch in ["main", "other"] {
        json_request_with_auth(
            &app,
            "POST",
            "/api/v1/projects/1/deploy",
            json!({ "branch": branch }),
            Some("master-secret"),
        )
        .await;
    }
    let (_, body) = json_request_with_auth(
        &app,
        "GET",
        "/api/v1/queue",
        json!({}),
        Some("master-secret"),
    )
    .await;
    let entries: Vec<Value> = serde_json::from_slice(&body).unwrap();
    let id = entries[0]["id"].as_u64().unwrap();

    // Scoped to a project that does not exist, so project 1's entry is out
    // of scope for it.
    let bob = mint_token(&app, json!({ "name": "bob", "projects": [99] })).await;
    let (status, _) = json_request_with_auth(
        &app,
        "DELETE",
        format!("/api/v1/queue/{id}").as_str(),
        json!({}),
        Some(&bob),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // And it is still there for whoever may see it.
    let (_, body) = json_request_with_auth(
        &app,
        "GET",
        "/api/v1/queue",
        json!({}),
        Some("master-secret"),
    )
    .await;
    let entries: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(entries.len(), 1, "the entry was cancelled out of scope");
}

#[tokio::test]
async fn cancelling_a_queue_entry_that_is_gone_is_a_404() {
    // Not an error worth a 500: the drain may have deployed it a moment
    // before the click landed.
    let (app, _) = test_app_with_admission_control(8000, 100).await;
    let (status, _) = json_request(&app, "DELETE", "/api/v1/queue/9999", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rollback_endpoint_is_wired_and_errors_clearly_with_no_prior_deploy() {
    let repo = repo_dir_with_config();
    let (app, _) = test_app().await;
    let (_, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;
    let project: Project = serde_json::from_slice(&body).unwrap();

    // No deploy yet: rollback has nothing to roll back to.
    let (status, _) = json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/rollback", project.id.0).as_str(),
        json!({ "branch": "feature-login" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // One deploy: still nothing *prior* to roll back to.
    json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
        json!({ "branch": "feature-login" }),
    )
    .await;
    let (status, _) = json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/rollback", project.id.0).as_str(),
        json!({ "branch": "feature-login" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A second deploy gives rollback something to redeploy.
    json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
        json!({ "branch": "feature-login" }),
    )
    .await;
    let (status, body) = json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/rollback", project.id.0).as_str(),
        json!({ "branch": "feature-login" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let env: Environment = serde_json::from_slice(&body).unwrap();
    assert_eq!(env.state.to_string(), "running");
}

#[tokio::test]
async fn logs_stream_endpoint_emits_sse_data_events() {
    let repo = repo_dir_with_config();
    let (app, _) = test_app().await;

    let (_, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;
    let project: Project = serde_json::from_slice(&body).unwrap();

    let (_, body) = json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
        json!({ "branch": "feature-login" }),
    )
    .await;
    let env: Environment = serde_json::from_slice(&body).unwrap();

    let (status, body) = json_request(
        &app,
        "GET",
        format!("/api/v1/environments/{}/logs/stream", env.id.0).as_str(),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("data: build log"), "{text:?}");
}

#[tokio::test]
async fn webhook_deploys_branch_when_signed() {
    let repo = repo_dir_with_config();
    let (app, _) = test_app().await;

    json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;

    let (status, body) = signed_webhook(
        &app,
        json!({
            "ref": "refs/heads/feature-hook",
            "repository": { "full_name": "org/app" }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let value: Value = serde_json::from_slice(&body).unwrap();
    // The delivery is acknowledged immediately and deployed on a
    // background drain — providers abandon a webhook long before a real
    // build finishes.
    assert_eq!(value["status"], "queued");
    let envs = wait_for_environments(&app, 1).await;
    assert_eq!(envs.len(), 1);
}

/// Regression test: GitHub sends a `ping` event (no `ref` at all) the
/// moment a webhook is configured. This used to fail with "webhook
/// payload is missing `ref`" instead of just being acknowledged.
#[tokio::test]
async fn webhook_ignores_non_push_events() {
    let (app, _) = test_app().await;
    let (status, body) = signed_webhook_with_event(
        &app,
        json!({ "zen": "Anything added dilutes everything else." }),
        Some("ping"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["status"], "ignored");
}

/// Regression test: a push with `"deleted": true` (branch deletion on
/// GitHub) used to be treated like a normal push and attempt to deploy
/// a branch that no longer exists in the remote, failing with a
/// confusing git error. It should destroy the branch's environment
/// instead.
#[tokio::test]
async fn webhook_destroys_environment_on_branch_deletion() {
    let repo = repo_dir_with_config();
    let (app, oci) = test_app().await;
    json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;
    signed_webhook(
        &app,
        json!({
            "ref": "refs/heads/feature-hook",
            "repository": { "full_name": "org/app" }
        }),
    )
    .await;

    // The push is deployed asynchronously; the branch has to actually exist
    // as an environment before deleting it can destroy anything.
    wait_for_environments(&app, 1).await;

    let (status, body) = signed_webhook(
        &app,
        json!({
            "ref": "refs/heads/feature-hook",
            "deleted": true,
            "repository": { "full_name": "org/app" }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["status"], "destroyed");
    assert!(
        oci.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("remove:")),
        "{:?}",
        oci.calls
    );
}

/// A deletion push for a branch Oxid never deployed must be a no-op,
/// not an error.
#[tokio::test]
async fn webhook_branch_deletion_for_unknown_branch_is_a_noop() {
    let repo = repo_dir_with_config();
    let (app, _) = test_app().await;
    json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;

    let (status, body) = signed_webhook(
        &app,
        json!({
            "ref": "refs/heads/never-deployed",
            "deleted": true,
            "repository": { "full_name": "org/app" }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["status"], "ignored");
}

#[tokio::test]
async fn webhook_rejects_bad_signature() {
    let repo = repo_dir_with_config();
    let (app, _) = test_app().await;

    json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;

    let (status, _) = json_request(
        &app,
        "POST",
        "/api/v1/webhooks/github",
        json!({
            "ref": "refs/heads/feature-hook",
            "repository": { "full_name": "org/app" }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn webhook_rejects_wrong_secret() {
    let repo = repo_dir_with_config();
    let (app, _) = test_app().await;
    json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;

    let raw = serde_json::to_string(&json!({
        "ref": "refs/heads/feature-hook",
        "repository": { "full_name": "org/app" }
    }))
    .unwrap();
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"wrong-secret").unwrap();
    mac.update(raw.as_bytes());
    let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/webhooks/github")
                .header("content-type", "application/json")
                .header("x-hub-signature-256", signature)
                .body(Body::from(raw))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// Sends a GitLab webhook with an `X-Gitlab-Token` header.
async fn gitlab_request(
    app: &Router,
    payload: Value,
    token: Option<&str>,
) -> (StatusCode, Vec<u8>) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/v1/webhooks/gitlab")
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("x-gitlab-token", token);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(payload.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

/// Sends a Gitea/Gogs webhook with its bare-hex HMAC-SHA256 signature.
/// `provider` is the URL segment (`gitea`/`gogs`) paired with that
/// provider's signature header.
async fn gitea_family_request(
    app: &Router,
    provider: (&str, &str),
    payload: Value,
    signing_secret: &str,
) -> (StatusCode, Vec<u8>) {
    let (name, signature_header) = provider;
    let raw = payload.to_string();
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(signing_secret.as_bytes()).unwrap();
    mac.update(raw.as_bytes());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/webhooks/{name}").as_str())
                .header("content-type", "application/json")
                .header(signature_header, hex::encode(mac.finalize().into_bytes()))
                .body(Body::from(raw))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

#[tokio::test]
async fn gitlab_webhook_deploys_branch_when_token_valid() {
    let repo = repo_dir_with_config();
    let (app, _) = test_app().await;
    json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;

    let (status, body) = gitlab_request(
        &app,
        json!({
            "object_kind": "push",
            "ref": "refs/heads/feature-gl",
            "after": SHA,
            "project": { "path_with_namespace": "org/app" }
        }),
        Some("test-secret"),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let value: Value = serde_json::from_slice(&body).unwrap();
    // The delivery is acknowledged immediately and deployed on a
    // background drain — providers abandon a webhook long before a real
    // build finishes.
    assert_eq!(value["status"], "queued");
    let envs = wait_for_environments(&app, 1).await;
    assert_eq!(envs.len(), 1);
}

#[tokio::test]
async fn gitlab_webhook_rejects_missing_and_wrong_tokens() {
    let (app, _) = test_app().await;

    // No header at all.
    let (status, _) = gitlab_request(&app, json!({}), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Wrong shared secret.
    let (status, _) = gitlab_request(&app, json!({}), Some("nope")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// GitLab fires many object kinds (`tag_push`, `pipeline`, ...) at one
/// webhook URL; only branch pushes may trigger deploys.
#[tokio::test]
async fn gitlab_webhook_ignores_non_push_object_kinds() {
    let (app, _) = test_app().await;
    let (status, body) = gitlab_request(
        &app,
        json!({
            "object_kind": "pipeline",
            "ref": "refs/heads/main",
            "project": { "path_with_namespace": "org/app" }
        }),
        Some("test-secret"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["status"], "ignored");
}

/// A branch deletion on GitLab arrives as a push whose `after` is the null
/// SHA — destroy the environment instead of deploying a nonexistent ref.
#[tokio::test]
async fn gitlab_webhook_destroys_environment_on_branch_deletion() {
    let repo = repo_dir_with_config();
    let (app, oci) = test_app().await;
    json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;
    gitlab_request(
        &app,
        json!({
            "object_kind": "push",
            "ref": "refs/heads/feature-gl",
            "after": SHA,
            "project": { "path_with_namespace": "org/app" }
        }),
        Some("test-secret"),
    )
    .await;

    // The push is deployed asynchronously; the branch has to actually exist
    // as an environment before deleting it can destroy anything.
    wait_for_environments(&app, 1).await;

    let (status, body) = gitlab_request(
        &app,
        json!({
            "object_kind": "push",
            "ref": "refs/heads/feature-gl",
            "after": "0".repeat(40),
            "project": { "path_with_namespace": "org/app" }
        }),
        Some("test-secret"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["status"], "destroyed");
    assert!(
        oci.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("remove:")),
        "{:?}",
        oci.calls
    );
}

#[tokio::test]
async fn gitea_webhook_deploys_branch_when_signed() {
    let repo = repo_dir_with_config();
    let (app, _) = test_app().await;
    json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;

    let (status, body) = gitea_family_request(
        &app,
        ("gitea", "x-gitea-signature"),
        json!({
            "ref": "refs/heads/feature-gitea",
            "repository": { "full_name": "org/app" }
        }),
        "test-secret",
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let value: Value = serde_json::from_slice(&body).unwrap();
    // The delivery is acknowledged immediately and deployed on a
    // background drain — providers abandon a webhook long before a real
    // build finishes.
    assert_eq!(value["status"], "queued");
    let envs = wait_for_environments(&app, 1).await;
    assert_eq!(envs.len(), 1);
}

/// Gogs speaks the same wire format as Gitea under `x-gogs-*` headers.
#[tokio::test]
async fn gogs_webhook_deploys_branch_when_signed() {
    let repo = repo_dir_with_config();
    let (app, _) = test_app().await;
    json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;

    let (status, body) = gitea_family_request(
        &app,
        ("gogs", "x-gogs-signature"),
        json!({
            "ref": "refs/heads/feature-gogs",
            "repository": { "full_name": "org/app" }
        }),
        "test-secret",
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let value: Value = serde_json::from_slice(&body).unwrap();
    // The delivery is acknowledged immediately and deployed on a
    // background drain — providers abandon a webhook long before a real
    // build finishes.
    assert_eq!(value["status"], "queued");
    let envs = wait_for_environments(&app, 1).await;
    assert_eq!(envs.len(), 1);
}

#[tokio::test]
async fn gitea_webhook_rejects_bad_signature() {
    let repo = repo_dir_with_config();
    let (app, _) = test_app().await;
    json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;

    let (status, _) = gitea_family_request(
        &app,
        ("gitea", "x-gitea-signature"),
        json!({
            "ref": "refs/heads/feature-gitea",
            "repository": { "full_name": "org/app" }
        }),
        "wrong-secret",
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn secrets_crud_and_injection() {
    let repo = repo_dir_with_config();
    let (app, oci) = test_app().await;

    let (status, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let project: Project = serde_json::from_slice(&body).unwrap();

    let (status, _) = json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/secrets", project.id.0).as_str(),
        json!({ "name": "DB_PASSWORD", "scope": "project", "value": "hunter2" }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/secrets", project.id.0).as_str(),
        json!({
            "name": "API_TOKEN",
            "scope": "branch",
            "branch": "feature-login",
            "value": "tok-123"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Without a `branch` filter, only global+project-scope secrets are
    // visible — a `branch`-scoped secret is meaningless without a branch
    // context and must not leak into this listing.
    let (status, body) = json_request(
        &app,
        "GET",
        format!("/api/v1/projects/{}/secrets", project.id.0).as_str(),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["secrets"].as_array().unwrap().len(), 1);

    // With `?branch=feature-login`, its branch-scoped secret joins the
    // project-scope one.
    let (status, body) = json_request(
        &app,
        "GET",
        format!(
            "/api/v1/projects/{}/secrets?branch=feature-login",
            project.id.0
        )
        .as_str(),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["secrets"].as_array().unwrap().len(), 2);

    let (status, _) = json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
        json!({ "branch": "feature-login" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let calls = oci.calls.lock().unwrap();
    let run = calls
        .iter()
        .find(|c| c.starts_with("run:"))
        .expect("container was started");
    assert!(run.starts_with("run:oxid-app-feature-login"), "{run}");
    assert!(run.contains("\"DB_PASSWORD\": \"hunter2\""), "{run}");
    assert!(run.contains("\"API_TOKEN\": \"tok-123\""), "{run}");
    assert!(run.contains("\"OXID_BRANCH\": \"feature-login\""), "{run}");
}

/// Regression test for a real secret-leakage bug found by deploying two
/// real branches with same-named branch-scoped secrets: the SQL filter
/// resolving "secrets visible to this deploy" matched every row for the
/// project regardless of branch, so branch A's value could shadow branch
/// B's when both defined the same key.
#[tokio::test]
async fn branch_secrets_do_not_cross_over_on_deploy() {
    let repo = repo_dir_with_config();
    let (app, oci) = test_app().await;
    let (_, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;
    let project: Project = serde_json::from_slice(&body).unwrap();

    json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/secrets", project.id.0).as_str(),
        json!({
            "name": "DB_PASSWORD", "scope": "branch",
            "branch": "feature-a", "value": "secret-a"
        }),
    )
    .await;
    json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/secrets", project.id.0).as_str(),
        json!({
            "name": "DB_PASSWORD", "scope": "branch",
            "branch": "feature-b", "value": "secret-b"
        }),
    )
    .await;

    json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
        json!({ "branch": "feature-a" }),
    )
    .await;
    json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
        json!({ "branch": "feature-b" }),
    )
    .await;

    let calls = oci.calls.lock().unwrap();
    let run_a = calls
        .iter()
        .find(|c| c.starts_with("run:oxid-app-feature-a-"))
        .expect("feature-a container was started");
    let run_b = calls
        .iter()
        .find(|c| c.starts_with("run:oxid-app-feature-b-"))
        .expect("feature-b container was started");
    assert!(run_a.contains("\"DB_PASSWORD\": \"secret-a\""), "{run_a}");
    assert!(run_b.contains("\"DB_PASSWORD\": \"secret-b\""), "{run_b}");
}

#[tokio::test]
async fn global_secret_endpoint() {
    let (app, _) = test_app().await;
    let (status, _) = json_request(
        &app,
        "POST",
        "/api/v1/secrets",
        json!({ "name": "GLOBAL_FLAG", "scope": "global", "value": "1" }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = json_request(&app, "GET", "/api/v1/secrets", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["secrets"].as_array().unwrap().len(), 1);

    let (status, _) = json_request(&app, "DELETE", "/api/v1/secrets/GLOBAL_FLAG", json!({})).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn rejects_invalid_scope() {
    let (app, _) = test_app().await;
    let (status, _) = json_request(
        &app,
        "POST",
        "/api/v1/secrets",
        json!({ "name": "X", "scope": "runtime", "value": "1" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn missing_project_is_404() {
    let (app, _) = test_app().await;
    let (status, _) = json_request(
        &app,
        "POST",
        "/api/v1/projects/999/deploy",
        json!({ "branch": "main" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn request_with_host(
    app: &Router,
    method: &str,
    uri: &str,
    host: &str,
) -> (StatusCode, Vec<u8>) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("host", host)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

#[tokio::test]
async fn destroy_removes_environment() {
    let repo = repo_dir_with_config();
    let (app, oci) = test_app().await;
    let (_, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;
    let project: Project = serde_json::from_slice(&body).unwrap();
    let (_, body) = json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
        json!({ "branch": "feature-login" }),
    )
    .await;
    let env: Environment = serde_json::from_slice(&body).unwrap();

    let (status, _) = json_request(
        &app,
        "DELETE",
        format!("/api/v1/environments/{}", env.id.0).as_str(),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        oci.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("remove:")),
        "{:?}",
        oci.calls
    );
}

#[tokio::test]
async fn destroy_with_purge_secrets_query_param_deletes_branch_secrets() {
    let repo = repo_dir_with_config();
    let (app, _) = test_app().await;
    let (_, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;
    let project: Project = serde_json::from_slice(&body).unwrap();
    json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/secrets", project.id.0).as_str(),
        json!({
            "name": "API_KEY", "scope": "branch",
            "branch": "feature-login", "value": "x"
        }),
    )
    .await;
    let (_, body) = json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
        json!({ "branch": "feature-login" }),
    )
    .await;
    let env: Environment = serde_json::from_slice(&body).unwrap();

    let (status, _) = json_request(
        &app,
        "DELETE",
        format!("/api/v1/environments/{}?purge_secrets=true", env.id.0).as_str(),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, body) = json_request(
        &app,
        "GET",
        format!(
            "/api/v1/projects/{}/secrets?branch=feature-login",
            project.id.0
        )
        .as_str(),
        json!({}),
    )
    .await;
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        value["secrets"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["name"] != "API_KEY"),
        "{value}"
    );
}

#[tokio::test]
async fn delete_project_endpoint_removes_project_and_environments() {
    let repo = repo_dir_with_config();
    let (app, oci) = test_app().await;
    let (_, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;
    let project: Project = serde_json::from_slice(&body).unwrap();
    json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
        json!({ "branch": "feature-login" }),
    )
    .await;

    let (status, _) = json_request(
        &app,
        "DELETE",
        format!("/api/v1/projects/{}", project.id.0).as_str(),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        oci.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("remove_image:")),
        "{:?}",
        oci.calls
    );

    let (status, _) = json_request(
        &app,
        "GET",
        format!("/api/v1/projects/{}/environments", project.id.0).as_str(),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn update_project_changes_ttls_and_rejects_bad_durations() {
    let repo = repo_dir_with_config();
    let (app, _) = test_app().await;
    let (_, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;
    let project: Project = serde_json::from_slice(&body).unwrap();

    let (status, body) = json_request(
        &app,
        "PATCH",
        format!("/api/v1/projects/{}", project.id.0).as_str(),
        json!({ "pause_after": "45m" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let updated: Project = serde_json::from_slice(&body).unwrap();
    assert_eq!(updated.config.pause_after.to_string(), "2700s");
    // Omitted field stays whatever it already was.
    assert_eq!(updated.config.destroy_after, project.config.destroy_after);

    let (status, body) = json_request(
        &app,
        "PATCH",
        format!("/api/v1/projects/{}", project.id.0).as_str(),
        json!({ "destroy_after": "not-a-duration" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "{}",
        String::from_utf8_lossy(&body)
    );
}

#[tokio::test]
async fn list_environments_filters_by_branch() {
    let repo = repo_dir_with_config();
    let (app, _) = test_app().await;
    let (_, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;
    let project: Project = serde_json::from_slice(&body).unwrap();
    json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
        json!({ "branch": "feature-login" }),
    )
    .await;

    let (status, body) = json_request(
        &app,
        "GET",
        format!(
            "/api/v1/projects/{}/environments?branch=feature-login",
            project.id.0
        )
        .as_str(),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let envs: Vec<Environment> = serde_json::from_slice(&body).unwrap();
    assert_eq!(envs.len(), 1);

    let (status, body) = json_request(
        &app,
        "GET",
        format!("/api/v1/projects/{}/environments?branch=nope", project.id.0).as_str(),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let envs: Vec<Environment> = serde_json::from_slice(&body).unwrap();
    assert!(envs.is_empty());
}

#[tokio::test]
async fn wake_by_host_wakes_matching_environment() {
    let repo = repo_dir_with_config();
    let (app, oci) = test_app().await;
    let (_, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;
    let project: Project = serde_json::from_slice(&body).unwrap();
    let (_, body) = json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
        json!({ "branch": "feature-login" }),
    )
    .await;
    let env: Environment = serde_json::from_slice(&body).unwrap();
    json_request(
        &app,
        "POST",
        format!("/api/v1/environments/{}/pause", env.id.0).as_str(),
        json!({}),
    )
    .await;

    let (status, body) = request_with_host(&app, "POST", "/api/v1/wake", &env.url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("feature-login"));
    // Suspending stops the container (a paused one loses its Traefik
    // router), so waking it starts it again.
    assert!(
        oci.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("start:")),
        "{:?}",
        oci.calls
    );
}

#[tokio::test]
async fn wake_by_host_unknown_host_is_ok_noop() {
    let (app, _) = test_app().await;
    let (status, _) = request_with_host(&app, "POST", "/api/v1/wake", "nobody.local.dev").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn heartbeat_always_ok() {
    let repo = repo_dir_with_config();
    let (app, _) = test_app().await;
    let (_, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;
    let project: Project = serde_json::from_slice(&body).unwrap();
    let (_, body) = json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
        json!({ "branch": "feature-login" }),
    )
    .await;
    let env: Environment = serde_json::from_slice(&body).unwrap();

    let (status, _) = request_with_host(&app, "GET", "/api/v1/heartbeat", &env.url).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request_with_host(&app, "GET", "/api/v1/heartbeat", "nobody.local.dev").await;
    assert_eq!(status, StatusCode::OK);
}

/// A webhook whose repository name is merely a *prefix* of a registered
/// clone URL must not deploy that project.
///
/// Matching used to be a substring test, so a push from `org/ap` — or from
/// any repository that was never registered but whose name happened to be a
/// prefix — was accepted and deployed `org/app`. In an organisation with
/// both `app` and `app-api`, pushes silently crossed between projects.
#[tokio::test]
async fn webhook_rejects_a_repository_that_only_prefixes_a_registered_one() {
    let repo = repo_dir_with_config();
    let (app, _) = test_app().await;
    json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;

    for hint in ["org/ap", "org/a", "rg/app"] {
        let (status, _) = signed_webhook(
            &app,
            json!({
                "ref": "refs/heads/feature-hook",
                "repository": { "full_name": hint }
            }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "`{hint}` must not resolve to the registered project"
        );
    }
}

/// The exact registered repository still resolves, including when the
/// webhook spells it with a `.git` suffix or different casing.
#[tokio::test]
async fn webhook_accepts_the_exact_repository_in_any_spelling() {
    let repo = repo_dir_with_config();
    let (app, _) = test_app().await;
    json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;

    for hint in ["org/app", "org/app.git", "Org/App"] {
        let (status, _) = signed_webhook(
            &app,
            json!({
                "ref": "refs/heads/feature-hook",
                "repository": { "full_name": hint }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "`{hint}` should resolve");
    }
}

/// A misspelled scope key must not mint a token with full access.
///
/// Unknown fields used to be ignored, so `project_ids` (instead of
/// `projects`) silently produced an *unscoped* token carrying the same reach
/// as the master credential, while the caller believed they had restricted
/// it. Failing the request is the only behaviour that can't fail open.
#[tokio::test]
async fn creating_a_token_rejects_a_misspelled_scope_key() {
    let (app, _) = test_app().await;
    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/tokens",
        json!({ "name": "ci", "project_ids": [1] }),
        Some("test-token"),
    )
    .await;
    assert_ne!(
        status,
        StatusCode::CREATED,
        "a misspelled scope key must not create a full-access token"
    );

    let (status, body) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/tokens",
        json!({ "name": "ci", "projects": [1] }),
        Some("test-token"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["projects"], json!([1]));
}

/// The wake interstitial identifies itself with a header.
///
/// It is served on the environment's own URL, so without a marker the page's
/// poll cannot tell "still waking" from "the app is answering again" — and
/// falls back to sitting out a fixed reload timer, which was most of the
/// visible wake time.
#[tokio::test]
async fn the_wake_page_marks_itself_so_the_poll_can_tell_when_it_is_gone() {
    let repo = repo_dir_with_config();
    let (app, _) = test_app_with_traefik().await;
    let (_, body) = json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;
    let project: Project = serde_json::from_slice(&body).unwrap();
    let (_, body) = json_request(
        &app,
        "POST",
        format!("/api/v1/projects/{}/deploy", project.id.0).as_str(),
        json!({ "branch": "feature-login" }),
    )
    .await;
    let env: Environment = serde_json::from_slice(&body).unwrap();
    json_request(
        &app,
        "POST",
        format!("/api/v1/environments/{}/pause", env.id.0).as_str(),
        json!({}),
    )
    .await;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/wake")
                .header("host", env.url.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-oxid-waking")
            .and_then(|v| v.to_str().ok()),
        Some("1")
    );
}

/// Every string the dashboard asks for must exist in every language.
///
/// The catalog and the markup are separate files, so a renamed key or a
/// language missing an entry is invisible until someone loads that page in
/// that language and finds the raw key — or an empty element — where the
/// text should be. Checked here rather than by eye because the catalog has
/// hundreds of entries and grows with every feature.
#[test]
fn every_dashboard_string_exists_in_every_language() {
    let catalog = include_str!("../../web/i18n.js");
    let markup = include_str!("../../web/index.html");
    let script = include_str!("../../web/app.js");

    let locales = ["en", "es"];
    let keys_in = |locale: &str| -> Vec<String> {
        // Each locale is a `<code>: { ... }` block of `"key": "value"`
        // pairs; the keys are what matters here, not the prose.
        let start = catalog
            .find(&format!("\n    {locale}: {{"))
            .unwrap_or_else(|| panic!("catalog has no `{locale}` block"));
        let rest = &catalog[start + 1..];
        let end = rest.find("\n    },").expect("unterminated locale block");
        rest[..end]
            .lines()
            .filter_map(|line| {
                // Only `"key":` starts an entry. A value long enough to wrap
                // continues on its own line starting with a quote too, so
                // requiring the colon is what keeps prose out of the keys.
                let line = line.trim();
                let (key, rest) = line.strip_prefix('"')?.split_once('"')?;
                rest.starts_with(':').then(|| key.to_owned())
            })
            .collect()
    };

    let english = keys_in("en");
    assert!(
        english.len() > 100,
        "catalog looks truncated: {}",
        english.len()
    );
    for locale in locales {
        let keys = keys_in(locale);
        for key in &english {
            assert!(keys.contains(key), "`{locale}` is missing `{key}`");
        }
        assert_eq!(keys.len(), english.len(), "`{locale}` has extra keys");
    }

    // And every key the UI actually asks for must be one of them.
    for source in [markup, script] {
        for fragment in source.split("t('").skip(1) {
            let Some((key, _)) = fragment.split_once('\'') else {
                continue;
            };
            if key.is_empty()
                || !key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
            {
                continue;
            }
            assert!(
                english.contains(&key.to_owned()),
                "UI uses undefined key `{key}`"
            );
        }
    }
}

/// Keys the UI builds at runtime still have to exist.
///
/// `every_dashboard_string_exists_in_every_language` can only see literal
/// `t('...')` calls. Two constructions escape it: `tn('key', n)`, which
/// resolves to `key.one`/`key.other` depending on the count, and the bulk
/// confirmations, which append the action name. Both would fail as a
/// missing translation in front of a user rather than in CI — the plural
/// one only when someone happens to select exactly one row.
#[test]
fn every_runtime_built_dashboard_key_exists() {
    let catalog = include_str!("../../web/i18n.js");
    let markup = include_str!("../../web/index.html");
    let script = include_str!("../../web/app.js");

    let defined = |key: &str| catalog.contains(&format!("\"{key}\":"));

    let mut checked = 0;
    for source in [markup, script] {
        for fragment in source.split("tn('").skip(1) {
            let Some((key, _)) = fragment.split_once('\'') else {
                continue;
            };
            for form in ["one", "other"] {
                assert!(
                    defined(&format!("{key}.{form}")),
                    "UI builds `{key}.{form}` at runtime but no catalog defines it"
                );
            }
            checked += 1;
        }
    }
    // The bulk confirmations append the action, then a plural form.
    for action in ["pause", "wake", "destroy"] {
        for form in ["one", "other"] {
            assert!(
                defined(&format!("confirm.bulk.{action}.{form}")),
                "bulk confirmation `confirm.bulk.{action}.{form}` is undefined"
            );
        }
        assert!(
            defined(&format!("action.{action}")),
            "bulk notices name the action with `action.{action}`"
        );
        checked += 1;
    }
    assert!(
        checked >= 4,
        "found only {checked} runtime-built keys to check"
    );
}

/// The API answers in the caller's language.
///
/// The dashboard sends `Accept-Language` with whatever its switcher is set
/// to, so a Spanish UI showing an English error is the gap this closes. The
/// locale rides the request as task-local context, so this also exercises
/// that a message built outside the handler — in the auth middleware here —
/// still sees it.
#[tokio::test]
async fn errors_are_returned_in_the_requested_language() {
    let app = test_app_with_token("s3cr3t").await;

    let ask = async |accept: Option<&str>| {
        let mut builder = Request::builder().method("GET").uri("/api/v1/stats");
        if let Some(accept) = accept {
            builder = builder.header("accept-language", accept);
        }
        let response = app
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    };

    assert!(
        ask(None).await.contains("bearer"),
        "default must be English"
    );
    assert!(ask(Some("en")).await.contains("bearer"));
    let spanish = ask(Some("es-ES,es;q=0.9")).await;
    assert!(spanish.contains("token bearer"), "{spanish}");

    // An unknown language is not an error — it falls back rather than
    // failing a request over a header nobody meant as a demand.
    assert!(ask(Some("de-DE")).await.contains("bearer"));
    // And an unknown tag must not stop the search before a known one.
    assert!(ask(Some("de, es")).await.contains("token bearer"));
}

/// A push to a branch the project does not deploy is *accepted* and does
/// nothing. It is not an error: the push was valid, and reporting a failure
/// would paint a Git host's delivery log red for the repository's ordinary
/// traffic.
#[tokio::test]
async fn a_push_to_an_unlisted_branch_is_accepted_and_skipped() {
    let repo = repo_dir_with_deploy(r#"branches = ["main", "release/*"]"#);
    let (app, _) = test_app().await;
    json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;

    let (status, body) = signed_webhook(
        &app,
        json!({
            "ref": "refs/heads/feature-carrito",
            "repository": { "full_name": "org/app" }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::ACCEPTED);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["status"], "skipped", "{value}");
    assert!(
        value["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("feature-carrito"),
        "the reason should name the branch: {value}"
    );

    // Nothing was built. This is the whole point of the feature.
    let (_, body) = json_request(&app, "GET", "/api/v1/projects/1/environments", json!({})).await;
    let envs: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(envs.as_array().map(Vec::len), Some(0), "{envs}");
}

#[tokio::test]
async fn a_push_to_a_listed_branch_still_deploys() {
    let repo = repo_dir_with_deploy(r#"branches = ["main", "release/*"]"#);
    let (app, _) = test_app().await;
    json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;

    let (status, body) = signed_webhook(
        &app,
        json!({
            "ref": "refs/heads/release/1.2",
            "repository": { "full_name": "org/app" }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["status"], "queued", "{value}");
    assert_eq!(wait_for_environments(&app, 1).await.len(), 1);
}

/// `ignore` beats the allowlist, so the usual "allow everything except the
/// bot" shape works.
#[tokio::test]
async fn an_ignored_branch_is_skipped_even_with_a_wide_allowlist() {
    let repo = repo_dir_with_deploy("branches = [\"*\"]\nignore = [\"dependabot/*\"]");
    let (app, _) = test_app().await;
    json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;

    let (_, body) = signed_webhook(
        &app,
        json!({
            "ref": "refs/heads/dependabot/npm/lodash",
            "repository": { "full_name": "org/app" }
        }),
    )
    .await;
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["status"], "skipped", "{value}");
}

/// The filter is for pushes. A person naming a branch is asking for that
/// branch, and must always get it — otherwise the escape hatch for "I need
/// to see this one today" does not exist.
#[tokio::test]
async fn an_explicit_deploy_ignores_the_branch_filter() {
    let repo = repo_dir_with_deploy(r#"branches = ["main"]"#);
    let (app, _) = test_app().await;
    json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;

    let (status, _) = json_request(
        &app,
        "POST",
        "/api/v1/projects/1/deploy",
        json!({ "branch": "feature-carrito" }),
    )
    .await;
    assert!(
        status.is_success(),
        "an explicit deploy must not be filtered, got {status}"
    );
}

/// The cap is the backstop for a filter nobody wrote correctly: a typo like
/// `["relase/*"]` deploys nothing and `["*"]` deploys everything, and only
/// the cap holds either way.
#[tokio::test]
async fn the_environment_cap_refuses_a_new_branch_but_never_a_redeploy() {
    let repo = repo_dir_with_deploy("max_environments = 1");
    let (app, _) = test_app().await;
    json_request(
        &app,
        "POST",
        "/api/v1/projects",
        json!({ "repo_dir": repo.path().display().to_string() }),
    )
    .await;

    // First branch fills the single slot.
    signed_webhook(
        &app,
        json!({ "ref": "refs/heads/main", "repository": { "full_name": "org/app" } }),
    )
    .await;
    assert_eq!(wait_for_environments(&app, 1).await.len(), 1);

    // A second, different branch is refused — with a reason, not an error.
    let (status, body) = signed_webhook(
        &app,
        json!({ "ref": "refs/heads/second", "repository": { "full_name": "org/app" } }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["status"], "skipped", "{value}");
    assert!(
        value["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("max_environments"),
        "{value}"
    );

    // But the branch that already holds a slot can still ship updates —
    // otherwise reaching the cap freezes everything already running.
    let (_, body) = signed_webhook(
        &app,
        json!({ "ref": "refs/heads/main", "repository": { "full_name": "org/app" } }),
    )
    .await;
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        value["status"], "queued",
        "a redeploy must not hit the cap: {value}"
    );
}

// ---------------------------------------------------------------------------
// roles: what a person may do, where, and for how long
// ---------------------------------------------------------------------------

/// A viewer is the role you hand a product owner: they watch previews and
/// can break nothing.
#[tokio::test]
async fn a_viewer_reads_everything_in_scope_and_changes_nothing() {
    let app = two_project_app().await;
    let tok = mint_token(
        &app,
        json!({ "name": "pm", "projects": [1], "role": "viewer" }),
    )
    .await;

    let (status, _) = json_request_with_auth(
        &app,
        "GET",
        "/api/v1/projects/1/environments",
        json!({}),
        Some(&tok),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a viewer must be able to read");

    for (method, uri, body) in [
        (
            "POST",
            "/api/v1/projects/1/deploy",
            json!({ "branch": "main" }),
        ),
        (
            "PATCH",
            "/api/v1/projects/1",
            json!({ "pause_after": "10m" }),
        ),
        (
            "POST",
            "/api/v1/projects/1/secrets",
            json!({ "name": "K", "scope": "project", "value": "v" }),
        ),
    ] {
        let (status, body) = json_request_with_auth(&app, method, uri, body, Some(&tok)).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {uri} should be forbidden for a viewer: {}",
            String::from_utf8_lossy(&body)
        );
    }
}

/// The line that matters most in the whole model: a developer ships code,
/// and never reads the credentials that code is handed.
#[tokio::test]
async fn a_developer_deploys_but_cannot_touch_secrets_or_settings() {
    let app = two_project_app().await;
    let tok = mint_token(
        &app,
        json!({ "name": "juan", "projects": [1], "role": "developer" }),
    )
    .await;

    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/projects/1/deploy",
        json!({ "branch": "main" }),
        Some(&tok),
    )
    .await;
    assert!(status.is_success(), "a developer must be able to deploy");

    for (method, uri, body) in [
        ("GET", "/api/v1/projects/1/secrets", json!({})),
        (
            "POST",
            "/api/v1/projects/1/secrets",
            json!({ "name": "K", "scope": "project", "value": "v" }),
        ),
        (
            "PATCH",
            "/api/v1/projects/1",
            json!({ "pause_after": "10m" }),
        ),
    ] {
        let (status, _) = json_request_with_auth(&app, method, uri, body, Some(&tok)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
    }
}

#[tokio::test]
async fn a_maintainer_owns_its_project_but_not_the_node() {
    let app = two_project_app().await;
    let tok = mint_token(
        &app,
        json!({ "name": "lead", "projects": [1], "role": "maintainer" }),
    )
    .await;

    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/projects/1/secrets",
        json!({ "name": "K", "scope": "project", "value": "v" }),
        Some(&tok),
    )
    .await;
    assert!(
        status.is_success(),
        "a maintainer owns its project's secrets"
    );

    for uri in ["/api/v1/stats", "/api/v1/infra/status", "/api/v1/tokens"] {
        let (status, _) = json_request_with_auth(&app, "GET", uri, json!({}), Some(&tok)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
    }
}

/// The point of having roles at all: a devops can delegate user management
/// instead of handing out the master token.
#[tokio::test]
async fn an_admin_token_can_issue_access_without_the_master_token() {
    let app = two_project_app().await;
    let admin = mint_token(&app, json!({ "name": "devops", "role": "admin" })).await;

    let (status, body) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/tokens",
        json!({ "name": "issued-by-admin", "projects": [1], "role": "developer" }),
        Some(&admin),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );

    // But an admin *of a project* is not an admin of the server.
    let scoped_admin = mint_token(
        &app,
        json!({ "name": "team-lead", "projects": [1], "role": "admin" }),
    )
    .await;
    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/tokens",
        json!({ "name": "nope" }),
        Some(&scoped_admin),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Access that ends on its own is the difference between a policy and an
/// intention: nobody remembers to revoke the contractor's token.
#[tokio::test]
async fn access_stops_working_once_it_expires() {
    let app = two_project_app().await;
    let (status, body) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/tokens",
        json!({ "name": "contractor", "role": "admin", "expires_in": "1s" }),
        Some("master-secret"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let tok = serde_json::from_slice::<Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();

    let (status, _) =
        json_request_with_auth(&app, "GET", "/api/v1/stats", json!({}), Some(&tok)).await;
    assert!(status.is_success(), "it should work before expiring");

    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let (status, body) =
        json_request_with_auth(&app, "GET", "/api/v1/stats", json!({}), Some(&tok)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // And it says *why*, so the person opens a ticket instead of a bug.
    assert!(
        String::from_utf8_lossy(&body).contains("expired"),
        "{}",
        String::from_utf8_lossy(&body)
    );
}

#[tokio::test]
async fn an_expiry_in_the_past_is_refused_rather_than_minted_dead() {
    let app = two_project_app().await;
    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/tokens",
        json!({ "name": "oops", "expires_in": "0s" }),
        Some("master-secret"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Suspension is the reversible one — for somebody on leave, without
/// reissuing a token and updating everywhere it is configured.
#[tokio::test]
async fn suspending_access_is_reversible_unlike_revoking_it() {
    let app = two_project_app().await;
    let (_, body) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/tokens",
        json!({ "name": "on-leave", "projects": [1], "role": "developer" }),
        Some("master-secret"),
    )
    .await;
    let created: Value = serde_json::from_slice(&body).unwrap();
    let (id, tok) = (
        created["id"].as_u64().unwrap(),
        created["token"].as_str().unwrap().to_owned(),
    );
    let env_url = "/api/v1/projects/1/environments";

    let (status, _) = json_request_with_auth(&app, "GET", env_url, json!({}), Some(&tok)).await;
    assert!(status.is_success());

    let (status, _) = json_request_with_auth(
        &app,
        "PATCH",
        &format!("/api/v1/tokens/{id}"),
        json!({ "suspended": true }),
        Some("master-secret"),
    )
    .await;
    assert!(status.is_success());
    let (status, body) = json_request_with_auth(&app, "GET", env_url, json!({}), Some(&tok)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(String::from_utf8_lossy(&body).contains("suspended"));

    // Back on, same token — nothing had to be reissued.
    json_request_with_auth(
        &app,
        "PATCH",
        &format!("/api/v1/tokens/{id}"),
        json!({ "suspended": false }),
        Some("master-secret"),
    )
    .await;
    let (status, _) = json_request_with_auth(&app, "GET", env_url, json!({}), Some(&tok)).await;
    assert!(status.is_success(), "resuming must restore the same token");
}

/// An upgrade must never quietly remove a permission somebody relies on, so
/// omitting `role` reproduces exactly what a token could do before roles
/// existed: everything within its scope, or everything at all when unscoped.
#[tokio::test]
async fn a_token_created_without_a_role_keeps_the_power_it_used_to_have() {
    let app = two_project_app().await;
    // No `role` in the request is the shape every pre-roles client sends.
    let scoped = mint_token(&app, json!({ "name": "legacy", "projects": [1] })).await;
    let (status, _) = json_request_with_auth(
        &app,
        "POST",
        "/api/v1/projects/1/secrets",
        json!({ "name": "K", "scope": "project", "value": "v" }),
        Some(&scoped),
    )
    .await;
    assert!(
        status.is_success(),
        "a scoped token with no role must still reach secrets, as before"
    );

    let unscoped = mint_token(&app, json!({ "name": "legacy-ci" })).await;
    let (status, _) =
        json_request_with_auth(&app, "GET", "/api/v1/stats", json!({}), Some(&unscoped)).await;
    assert!(
        status.is_success(),
        "an unscoped token with no role must still be node-wide, as before"
    );
}
