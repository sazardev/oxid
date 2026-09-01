#![allow(
    unused_imports,
    clippy::pedantic,
    clippy::nursery,
    clippy::too_many_lines,
    clippy::empty_line_after_doc_comments,
    clippy::duplicate_mod
)]
//! Fleet routes: registering, listing, draining and retiring nodes.
//!
//! Every one of these is node-wide, so `Capability::ManageNode` with no
//! project id is the gate — which means a project-scoped token is refused
//! by the existing access model without any code here, since a scoped
//! credential is denied *any* action with no project id whatever the
//! capability. That is deliberate and pinned by a test rather than left to
//! be rediscovered.

use crate::api::ApiState;
use crate::api::error::{ApiError, ApiResult};
use crate::api::middleware::{AuthedAs, authorize, operator_name};
use axum::extract::{Extension, Path, State};
use axum::{Json, http::StatusCode};
use oxid_core::services::access::Capability;
use oxid_core::{ContainerPort, GitPort, NodeId, NodeState, NodeTls};
use serde::Deserialize;

/// A node an operator is registering.
#[derive(Debug, Deserialize)]
pub struct AddNodeBody {
    /// Fleet-unique name (`eu-1`). Re-using one corrects that node rather
    /// than creating a second.
    pub name: String,
    /// A bollard endpoint (`tcp://10.0.0.4:2376`), or `local`.
    pub endpoint: String,
    /// Host the control plane's proxy dials for ports this node publishes.
    /// Distinct from `endpoint`: the Docker API and container traffic can
    /// legitimately live on different interfaces.
    #[serde(default)]
    pub address: Option<String>,
    /// Path on *this daemon's* disk to the CA that signed the node's
    /// daemon certificate.
    #[serde(default)]
    pub tls_ca: Option<String>,
    /// Path to the client certificate this control plane presents.
    #[serde(default)]
    pub tls_cert: Option<String>,
    /// Path to the private key for `tls_cert`.
    #[serde(default)]
    pub tls_key: Option<String>,
    /// Memory (MB) this particular machine owes its OS and daemons.
    /// Overrides the daemon-wide `OXID_RESERVED_MEMORY_MB` for this node.
    #[serde(default)]
    pub reserved_memory_mb: Option<u64>,
}

/// What may be changed about a node after registration.
#[derive(Debug, Deserialize)]
pub struct PatchNodeBody {
    /// `active` or `draining`. `down` is refused: it is what a failed
    /// health probe records, not something to assert by hand.
    pub state: String,
    /// With `draining`, also move every live branch off the node by
    /// redeploying it elsewhere. Off by default: a redeploy per branch is a
    /// slow, visible operation, and marking a node so it stops receiving
    /// *new* work is the smaller thing an operator often means.
    #[serde(default)]
    pub evacuate: bool,
}

/// The result of a drain that moved branches.
#[derive(Debug, serde::Serialize)]
pub struct DrainResult {
    /// The node afterwards.
    #[serde(flatten)]
    pub node: crate::NodeView,
    /// Environments that left.
    pub moved: Vec<u64>,
    /// Environments that could not, with why — a branch that will not build
    /// stays where it is, and saying which is more useful than failing the
    /// whole drain.
    pub stuck: Vec<StuckEnvironment>,
}

/// One environment a drain could not move.
#[derive(Debug, serde::Serialize)]
pub struct StuckEnvironment {
    /// Which environment.
    pub environment_id: u64,
    /// Why it is still there.
    pub reason: String,
}

pub async fn list_nodes<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    authed: Option<Extension<AuthedAs>>,
) -> ApiResult<Json<Vec<crate::NodeView>>> {
    authorize(&authed, Capability::ManageNode, None)?;
    Ok(Json(state.cp.list_nodes().await?))
}

pub async fn add_node<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    authed: Option<Extension<AuthedAs>>,
    Json(body): Json<AddNodeBody>,
) -> ApiResult<(StatusCode, Json<crate::NodeView>)> {
    authorize(&authed, Capability::ManageNode, None)?;

    // All three paths or none. Two out of three is not a usable client, and
    // accepting it would fail later with a message about certificates
    // instead of about the half-filled request that caused it.
    let tls = match (body.tls_ca, body.tls_cert, body.tls_key) {
        (Some(ca_path), Some(cert_path), Some(key_path)) => Some(NodeTls {
            ca_path,
            cert_path,
            key_path,
        }),
        (None, None, None) => None,
        _ => {
            return Err(ApiError::from_validation(crate::i18n::t("node.partialTls")));
        }
    };

    let view = state
        .cp
        .add_node(
            &body.name,
            &body.endpoint,
            body.address,
            tls,
            body.reserved_memory_mb,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(view)))
}

pub async fn update_node<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    authed: Option<Extension<AuthedAs>>,
    Path(id): Path<u64>,
    Json(body): Json<PatchNodeBody>,
) -> ApiResult<Json<DrainResult>> {
    authorize(&authed, Capability::ManageNode, None)?;
    let operator = operator_name(authed.as_ref());
    let state_value = body
        .state
        .parse::<NodeState>()
        .map_err(|e| ApiError::from_validation(e.to_string()))?;

    // State first, then evacuate. Placement refuses a draining node even to
    // a branch already on it, and that single fact is what makes each
    // redeploy *leave* instead of rebuilding in place.
    let node = state.cp.set_node_state(NodeId(id), state_value).await?;
    let mut moved = Vec::new();
    let mut stuck = Vec::new();
    if body.evacuate && state_value == NodeState::Draining {
        for (env_id, failure) in state.cp.evacuate_node(NodeId(id), operator).await? {
            match failure {
                None => moved.push(env_id.0),
                Some(reason) => stuck.push(StuckEnvironment {
                    environment_id: env_id.0,
                    reason,
                }),
            }
        }
    }
    Ok(Json(DrainResult { node, moved, stuck }))
}

pub async fn remove_node<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    authed: Option<Extension<AuthedAs>>,
    Path(id): Path<u64>,
) -> ApiResult<StatusCode> {
    authorize(&authed, Capability::ManageNode, None)?;
    state.cp.remove_node(NodeId(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Query for the fleet-wide environment listing.
#[derive(Debug, Deserialize)]
pub struct EnvironmentsQuery {
    /// Restrict to one node, by id. Omitted lists the whole fleet.
    #[serde(default)]
    pub node: Option<u64>,
}

/// Every live environment, across every project, optionally on one node.
///
/// Node-wide by construction — it spans projects — so a project-scoped
/// credential is refused, exactly as it is for the rest of `/api/v1/nodes`.
pub async fn list_fleet_environments<
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
>(
    State(state): State<ApiState<G, O>>,
    authed: Option<Extension<AuthedAs>>,
    axum::extract::Query(query): axum::extract::Query<EnvironmentsQuery>,
) -> ApiResult<Json<Vec<oxid_core::Environment>>> {
    authorize(&authed, Capability::ManageNode, None)?;
    Ok(Json(
        state.cp.environments_on(query.node.map(NodeId)).await?,
    ))
}
