//! `HTTP` API and webhook surface (SPEC.md §5: the shared internal API).
//!
//! The CLI, TUI, dashboard and desktop app all consume this router.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use oxid_core::{
    BranchName, ContainerPort, EnvVarScope, Environment, EnvironmentId, GitPort, Project,
    ProjectId, RepositoryError,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;

use crate::{ControlPlane, CpError};

/// Shared state injected into every handler.
#[derive(Clone)]
pub struct ApiState<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
> {
    /// The application service backing all endpoints.
    pub cp: ControlPlane<G, O>,
    /// Shared secret verifying GitHub webhook signatures (`OXID_WEBHOOK_SECRET`).
    /// Webhooks are rejected while unset.
    pub webhook_secret: Option<String>,
}

/// Builds the API router.
pub fn router<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    state: ApiState<G, O>,
) -> Router {
    Router::new()
        .route("/api/v1/health", get(health))
        .route(
            "/api/v1/projects",
            post(register_project).get(list_projects),
        )
        .route("/api/v1/projects/{id}/environments", get(list_environments))
        .route("/api/v1/projects/{id}/deploy", post(deploy))
        .route("/api/v1/secrets", get(list_global_secrets).post(set_global_secret))
        .route("/api/v1/secrets/{name}", delete(delete_global_secret))
        .route(
            "/api/v1/projects/{id}/secrets",
            get(list_project_secrets).post(set_project_secret),
        )
        .route(
            "/api/v1/projects/{id}/secrets/{name}",
            delete(delete_project_secret),
        )
        .route("/api/v1/environments/{env_id}/pause", post(pause))
        .route("/api/v1/environments/{env_id}/wake", post(wake))
        .route("/api/v1/environments/{env_id}/logs", get(logs))
        .route("/api/v1/webhooks/github", post(github_webhook))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// request/response types
// ---------------------------------------------------------------------------

/// Body for `POST /api/v1/projects`.
#[derive(Debug, Deserialize)]
pub struct RegisterBody {
    /// Path to the repository containing `oxid.toml`.
    pub repo_dir: String,
}

/// Body for `POST /api/v1/projects/{id}/deploy`.
#[derive(Debug, Deserialize)]
pub struct DeployBody {
    /// Branch to deploy.
    pub branch: String,
}

/// Body for `POST /api/v1/secrets` and `POST /api/v1/projects/{id}/secrets`.
#[derive(Debug, Deserialize)]
pub struct SecretBody {
    /// Secret name, e.g. `DB_PASSWORD`.
    pub name: String,
    /// Scope of the secret (`global`, `project` or `branch`).
    pub scope: String,
    /// Value to store. Optional on delete.
    pub value: Option<String>,
    /// Branch scope (only meaningful for `scope = "branch"`).
    pub branch: Option<String>,
}

/// Body for `DELETE` secret endpoints (optional branch scope).
#[derive(Debug, Deserialize)]
pub struct SecretDeleteQuery {
    /// Branch scope to delete from.
    pub branch: Option<String>,
}

/// Query for listing secrets (optional branch scope).
#[derive(Debug, Deserialize)]
pub struct SecretListQuery {
    /// Branch scope to list.
    pub branch: Option<String>,
}

/// Unified error response.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn from_validation(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
}

impl From<CpError> for ApiError {
    fn from(err: CpError) -> Self {
        match err {
            CpError::NotFound(m) | CpError::Store(RepositoryError::NotFound(m)) => {
                Self::not_found(m)
            }
            CpError::Config(_) | CpError::Domain(_) => Self::from_validation(err.to_string()),
            CpError::Store(RepositoryError::Conflict(m)) => Self::new(StatusCode::CONFLICT, m),
            CpError::Store(RepositoryError::Storage(m)) => {
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, m)
            }
            CpError::Git(_) | CpError::Oci(_) => {
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn register_project<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Json(body): Json<RegisterBody>,
) -> ApiResult<(StatusCode, Json<Project>)> {
    let project = state
        .cp
        .register_project(std::path::Path::new(&body.repo_dir))
        .await?;
    Ok((StatusCode::CREATED, Json(project)))
}

async fn list_projects<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
) -> ApiResult<Json<Vec<Project>>> {
    Ok(Json(state.cp.list_projects().await?))
}

async fn list_environments<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
) -> ApiResult<Json<Vec<Environment>>> {
    Ok(Json(state.cp.list_environments(ProjectId(id)).await?))
}

async fn deploy<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
    Json(body): Json<DeployBody>,
) -> ApiResult<(StatusCode, Json<Environment>)> {
    let branch = parse_branch(&body.branch)?;
    let env = state.cp.deploy(ProjectId(id), branch).await?;
    Ok((StatusCode::CREATED, Json(env)))
}

async fn pause<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(env_id): Path<u64>,
) -> ApiResult<StatusCode> {
    state.cp.pause(EnvironmentId(env_id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn wake<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(env_id): Path<u64>,
) -> ApiResult<StatusCode> {
    state.cp.wake(EnvironmentId(env_id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn logs<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(env_id): Path<u64>,
) -> ApiResult<Json<Value>> {
    let logs = state.cp.logs(EnvironmentId(env_id)).await?;
    Ok(Json(json!({ "logs": logs })))
}

// ---------------------------------------------------------------------------
// secret handlers
// ---------------------------------------------------------------------------

async fn do_set_secret<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    state: ApiState<G, O>,
    project_id: Option<ProjectId>,
    body: SecretBody,
) -> ApiResult<StatusCode> {
    let scope = parse_scope(&body.scope)?;
    let name = validate_secret_name(&body.name)?;
    let value = body
        .value
        .clone()
        .ok_or_else(|| ApiError::from_validation("secret `value` is required"))?;

    let (pid, branch) = match (project_id, scope) {
        (None, EnvVarScope::Global) => (None, None),
        (Some(_), EnvVarScope::Global) => {
            return Err(ApiError::from_validation(
                "use the global endpoint for `global` scope",
            ));
        }
        (Some(pid), EnvVarScope::Project) => {
            if body.branch.is_some() {
                return Err(ApiError::from_validation(
                    "`branch` is only allowed with `scope = \"branch\"`",
                ));
            }
            (Some(pid), None)
        }
        (Some(pid), EnvVarScope::Branch) => {
            let raw = body.branch.as_deref().ok_or_else(|| {
                ApiError::from_validation("`branch` is required for `scope = \"branch\"`")
            })?;
            (Some(pid), Some(parse_branch(raw)?))
        }
        (None, EnvVarScope::Project | EnvVarScope::Branch) => {
            return Err(ApiError::from_validation(
                "project/branch secrets require a project id",
            ));
        }
        (_, EnvVarScope::Runtime) => {
            return Err(ApiError::from_validation(
                "`runtime` scope cannot be set by clients",
            ));
        }
    };

    state
        .cp
        .set_secret(pid, branch.as_ref(), name, scope, &value)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn do_list_secrets<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    state: ApiState<G, O>,
    project_id: Option<ProjectId>,
    query: SecretListQuery,
) -> ApiResult<Json<Value>> {
    let branch = query.branch.as_deref().map(parse_branch).transpose()?;
    let secrets = state.cp.list_secrets(project_id, branch.as_ref()).await?;
    Ok(Json(json!({
        "secrets": secrets.into_iter().map(|(name, scope)| json!({
            "name": name,
            "scope": scope.to_string(),
        })).collect::<Vec<_>>(),
    })))
}

async fn do_delete_secret<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    state: ApiState<G, O>,
    project_id: Option<ProjectId>,
    name: &str,
    query: SecretDeleteQuery,
) -> ApiResult<StatusCode> {
    let name = validate_secret_name(name)?;
    let branch = query.branch.as_deref().map(parse_branch).transpose()?;
    state
        .cp
        .delete_secret(project_id, branch.as_ref(), name)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_global_secret<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Json(body): Json<SecretBody>,
) -> ApiResult<StatusCode> {
    do_set_secret(state, None, body).await
}

async fn list_global_secrets<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    query: Query<SecretListQuery>,
) -> ApiResult<Json<Value>> {
    do_list_secrets(state, None, query.0).await
}

async fn delete_global_secret<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(name): Path<String>,
    query: Query<SecretDeleteQuery>,
) -> ApiResult<StatusCode> {
    do_delete_secret(state, None, &name, query.0).await
}

async fn set_project_secret<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
    Json(body): Json<SecretBody>,
) -> ApiResult<StatusCode> {
    do_set_secret(state, Some(ProjectId(id)), body).await
}

async fn list_project_secrets<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(id): Path<u64>,
    query: Query<SecretListQuery>,
) -> ApiResult<Json<Value>> {
    do_list_secrets(state, Some(ProjectId(id)), query.0).await
}

async fn delete_project_secret<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path((id, name)): Path<(u64, String)>,
    query: Query<SecretDeleteQuery>,
) -> ApiResult<StatusCode> {
    do_delete_secret(state, Some(ProjectId(id)), &name, query.0).await
}

fn parse_scope(raw: &str) -> ApiResult<EnvVarScope> {
    match raw {
        "global" => Ok(EnvVarScope::Global),
        "project" => Ok(EnvVarScope::Project),
        "branch" => Ok(EnvVarScope::Branch),
        _ => Err(ApiError::from_validation(format!(
            "invalid scope `{raw}`; expected `global`, `project` or `branch`"
        ))),
    }
}

fn validate_secret_name(name: &str) -> ApiResult<&str> {
    if name.trim().is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(ApiError::from_validation(format!(
            "invalid secret name `{name}`; use alphanumeric characters and underscores"
        )));
    }
    Ok(name)
}

// ---------------------------------------------------------------------------
// webhook handler
// ---------------------------------------------------------------------------

/// GitHub push-webhook handler with HMAC-SHA256 signature verification
/// (SPEC.md §4.1). The signature covers the exact raw request body, so the
/// payload is read as bytes and parsed after verification.
async fn github_webhook<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let secret = state.webhook_secret.as_deref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "webhook secret is not configured; set OXID_WEBHOOK_SECRET",
        )
    })?;
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            ApiError::new(StatusCode::UNAUTHORIZED, "missing `X-Hub-Signature-256` header")
        })?;
    verify_hmac(secret, &body, signature)?;

    let payload: Value = serde_json::from_slice(&body)
        .map_err(|e| ApiError::from_validation(format!("invalid JSON payload: {e}")))?;
    let branch = payload
        .get("ref")
        .and_then(Value::as_str)
        .and_then(strip_refs_heads)
        .ok_or_else(|| ApiError::from_validation("webhook payload is missing `ref`"))?;
    let full_name = payload
        .get("repository")
        .and_then(|r| r.get("full_name"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ApiError::from_validation("webhook payload is missing `repository.full_name`")
        })?;

    let project = state
        .cp
        .list_projects()
        .await?
        .into_iter()
        .find(|p| p.repo_url.as_str().contains(full_name))
        .ok_or_else(|| ApiError::not_found(format!("no project registered for `{full_name}`")))?;

    let env = state.cp.deploy(project.id, parse_branch(branch)?).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "status": "deployed", "environment_id": env.id.0 })),
    ))
}

/// Verifies `X-Hub-Signature-256` (`sha256=<hex hmac>`) against the raw body.
fn verify_hmac(secret: &str, body: &[u8], signature: &str) -> ApiResult<()> {
    let provided = signature
        .strip_prefix("sha256=")
        .ok_or_else(|| {
            ApiError::new(StatusCode::UNAUTHORIZED, "signature must be prefixed with `sha256=`")
        })?;
    let provided_bytes = hex::decode(provided).map_err(|_| {
        ApiError::new(StatusCode::UNAUTHORIZED, "signature is not valid hex")
    })?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "invalid secret"))?;
    mac.update(body);
    mac.verify_slice(&provided_bytes)
        .map_err(|_| ApiError::new(StatusCode::UNAUTHORIZED, "signature mismatch"))?;
    Ok(())
}

fn strip_refs_heads(reference: &str) -> Option<&str> {
    reference.strip_prefix("refs/heads/")
}

fn parse_branch(raw: &str) -> ApiResult<BranchName> {
    BranchName::parse(raw).map_err(|e| ApiError::from_validation(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::http::Request;
    use oxid_core::{BuildSpec, ContainerSpec, GitError, OciError, RepoUrl};
    use tower::ServiceExt;

    use crate::adapter::store::SqliteStore;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[derive(Debug, Clone, Default)]
    struct FakeGit;

    impl GitPort for FakeGit {
        async fn remote_url(&self, repo_dir: &std::path::Path) -> Result<RepoUrl, GitError> {
            let _ = repo_dir;
            RepoUrl::parse("https://github.com/org/app.git")
                .map_err(|e| GitError::Failure(e.to_string()))
        }
        async fn ensure_repo(
            &self,
            _url: &RepoUrl,
            cache_dir: &std::path::Path,
        ) -> Result<std::path::PathBuf, GitError> {
            Ok(cache_dir.join("app"))
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
    }

    impl ContainerPort for FakeOci {
        async fn build(&self, spec: &BuildSpec) -> Result<(), OciError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("build:{}", spec.image));
            Ok(())
        }
        async fn run(&self, spec: &ContainerSpec) -> Result<(), OciError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("run:{}:env={:?}", spec.name, spec.env));
            Ok(())
        }
        async fn pause(&self, name: &str) -> Result<(), OciError> {
            self.calls.lock().unwrap().push(format!("pause:{name}"));
            Ok(())
        }
        async fn unpause(&self, name: &str) -> Result<(), OciError> {
            self.calls.lock().unwrap().push(format!("unpause:{name}"));
            Ok(())
        }
        async fn stop(&self, name: &str) -> Result<(), OciError> {
            self.calls.lock().unwrap().push(format!("stop:{name}"));
            Ok(())
        }
        async fn remove(&self, name: &str) -> Result<(), OciError> {
            self.calls.lock().unwrap().push(format!("remove:{name}"));
            Ok(())
        }
        async fn logs(&self, name: &str) -> Result<String, OciError> {
            self.calls.lock().unwrap().push(format!("logs:{name}"));
            Ok("build log".to_owned())
        }
        async fn exec(&self, name: &str, command: &str) -> Result<(), OciError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("exec:{name}:{command}"));
            Ok(())
        }
    }

    async fn test_app() -> (Router, FakeOci) {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let cache = tempfile::tempdir().unwrap();
        let oci = FakeOci::default();
        let cp = ControlPlane::new(store, FakeGit, oci.clone(), cache.path().to_owned());
        (
            router(ApiState {
                cp,
                webhook_secret: Some("test-secret".to_owned()),
            }),
            oci,
        )
    }

    fn repo_dir_with_config() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("oxid.toml"),
            r#"
[project]
name = "app"

[routing]
base_domain = "app.local.dev"
port = 8080
"#,
        )
        .unwrap();
        dir
    }

    async fn json_request(
        app: &Router,
        method: &str,
        uri: &str,
        body: Value,
    ) -> (StatusCode, Vec<u8>) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
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

    /// Sends a GitHub webhook with a valid `X-Hub-Signature-256` header.
    async fn signed_webhook(app: &Router, payload: Value) -> (StatusCode, Vec<u8>) {
        let raw = payload.to_string();
        let mut mac =
            hmac::Hmac::<sha2::Sha256>::new_from_slice(b"test-secret").unwrap();
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
        assert_eq!(value["status"], "deployed");
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

        let (status, body) = json_request(
            &app,
            "GET",
            format!("/api/v1/projects/{}/secrets", project.id.0).as_str(),
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

        let (status, body) =
            json_request(&app, "GET", "/api/v1/secrets", json!({})).await;
        assert_eq!(status, StatusCode::OK);
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["secrets"].as_array().unwrap().len(), 1);

        let (status, _) = json_request(
            &app,
            "DELETE",
            "/api/v1/secrets/GLOBAL_FLAG",
            json!({}),
        )
        .await;
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
}
