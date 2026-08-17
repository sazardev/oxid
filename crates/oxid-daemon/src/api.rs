//! `HTTP` API and webhook surface (SPEC.md §5: the shared internal API).
//!
//! The CLI, TUI, dashboard and desktop app all consume this router.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use oxid_core::{
    BranchName, ContainerPort, Environment, EnvironmentId, GitPort, Project, ProjectId,
    RepositoryError,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ControlPlane, CpError};

/// Shared state injected into every handler.
#[derive(Clone)]
pub struct ApiState<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
> {
    /// The application service backing all endpoints.
    pub cp: ControlPlane<G, O>,
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

/// Minimal GitHub push-webhook handler.
///
/// Signature verification is not implemented yet; the branch is extracted from
/// `ref` and the project is looked up by repository `full_name`.
async fn github_webhook<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Json(payload): Json<Value>,
) -> ApiResult<(StatusCode, Json<Value>)> {
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
                .push(format!("run:{}", spec.name));
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
    }

    async fn test_app() -> (Router, FakeOci) {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let cache = tempfile::tempdir().unwrap();
        let oci = FakeOci::default();
        let cp = ControlPlane::new(store, FakeGit, oci.clone(), cache.path().to_owned());
        (router(ApiState { cp }), oci)
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
    async fn webhook_deploys_branch() {
        let repo = repo_dir_with_config();
        let (app, _) = test_app().await;

        json_request(
            &app,
            "POST",
            "/api/v1/projects",
            json!({ "repo_dir": repo.path().display().to_string() }),
        )
        .await;

        let (status, body) = json_request(
            &app,
            "POST",
            "/api/v1/webhooks/github",
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
