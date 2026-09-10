#![allow(
    clippy::pedantic,
    clippy::nursery,
    clippy::too_many_lines,
    clippy::empty_line_after_doc_comments,
    clippy::duplicate_mod
)]

use crate::api::ApiState;
use crate::api::DEFAULT_AUDIT_LIMIT;
use crate::api::error::{ApiError, ApiResult};
use crate::api::middleware::authorize;
use crate::api::middleware::{AuthedAs, operator_scopes};
use crate::api::types::AuditQuery;
use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use oxid_core::services::access::Capability;
use oxid_core::{ContainerPort, EnvironmentId, GitPort, ProjectId};
use serde_json::{Value, json};

pub async fn recent_audit<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    authed: Option<Extension<AuthedAs>>,
    Query(query): Query<AuditQuery>,
) -> ApiResult<Json<Vec<oxid_core::AuditEvent>>> {
    let limit = query.limit.unwrap_or(DEFAULT_AUDIT_LIMIT);
    let mut filter = query.into_filter()?;
    filter.limit = Some(filter.limit.unwrap_or(DEFAULT_AUDIT_LIMIT));
    // A scoped operator either names one of its own projects explicitly, or
    // gets the merged trail of exactly its projects — never anyone else's.
    match operator_scopes(authed.as_ref()) {
        None => Ok(Json(state.cp.recent_audit_events(&filter).await?)),
        Some(scopes) => {
            if let Some(project_id) = filter.project_id {
                authorize(&authed, Capability::Read, Some(project_id.0))?;
                return Ok(Json(state.cp.recent_audit_events(&filter).await?));
            }
            let mut events = Vec::new();
            for id in scopes {
                filter.project_id = Some(ProjectId(*id));
                events.extend(state.cp.recent_audit_events(&filter).await?);
            }
            events.sort_by_key(|event| std::cmp::Reverse(event.occurred_at));
            events.truncate(limit as usize);
            Ok(Json(events))
        }
    }
}

/// Lists deploys currently waiting for host capacity (see
/// [`ControlPlane::deploy_or_queue`]), oldest first. Scoped operators only
/// see their own projects' entries.
pub async fn list_queue<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    authed: Option<Extension<AuthedAs>>,
) -> ApiResult<Json<Vec<crate::adapter::store::QueuedDeploy>>> {
    let queue = state.cp.list_deploy_queue().await?;
    let queue = match operator_scopes(authed.as_ref()) {
        None => queue,
        Some(scopes) => queue
            .into_iter()
            .filter(|entry| scopes.contains(&entry.project_id.0))
            .collect(),
    };
    Ok(Json(queue))
}

/// Cancels one queued deploy.
///
/// Scoped the same way the listing is: an operator who cannot see a
/// project's queue entries cannot cancel them either, and gets the same
/// `404` they would get for an id that does not exist — telling them "that
/// exists but isn't yours" is itself information about another project.
pub async fn cancel_queued<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    authed: Option<Extension<AuthedAs>>,
    Path(id): Path<u64>,
) -> ApiResult<Json<Value>> {
    let visible = match operator_scopes(authed.as_ref()) {
        None => true,
        Some(scopes) => state
            .cp
            .list_deploy_queue()
            .await?
            .into_iter()
            .any(|entry| entry.id == id && scopes.contains(&entry.project_id.0)),
    };
    let cancelled = if visible {
        state.cp.cancel_queued_deploy(id).await?
    } else {
        None
    };
    match cancelled {
        Some(entry) => Ok(Json(json!({
            "status": "cancelled",
            "id": entry.id,
            "branch": entry.branch,
            "project_id": entry.project_id.0,
        }))),
        None => Err(ApiError::not_found(crate::i18n::tf(
            "notFound.queued",
            &[("id", &id.to_string())],
        ))),
    }
}

pub async fn environment_audit<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    Path(env_id): Path<u64>,
    authed: Option<Extension<AuthedAs>>,
    Query(query): Query<AuditQuery>,
) -> ApiResult<Json<Vec<oxid_core::AuditEvent>>> {
    let project_id = state
        .cp
        .environment_project_id(EnvironmentId(env_id))
        .await?;
    authorize(&authed, Capability::Read, Some(project_id.0))?;
    let filter = query.into_filter()?;
    Ok(Json(
        state
            .cp
            .audit_events_for(EnvironmentId(env_id), &filter)
            .await?,
    ))
}
