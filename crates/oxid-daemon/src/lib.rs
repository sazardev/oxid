//! Oxid control-plane daemon.
//!
//! Hosts the ports & adapters described in SPEC.md §2.2, all depending on
//! [`oxid_core`]:
//! - `adapter::config` — parses `oxid.toml` into domain configuration.
//! - `adapter::store` — `SQLite` persistence implementing the domain ports.
//! - `adapter::git` — `Git` versioning (`git2`) implementing the git port.
//! - `adapter::oci` — Docker orchestration (`bollard`) implementing the OCI port.
//! - `service` — application layer (`ControlPlane`) wiring ports together.
//! - `api` — `HTTP`/webhook surface (`axum`).

pub mod adapter;
pub mod api;
pub mod request_context;
pub mod service;

pub use service::control_plane::{ControlPlane, CpError, DeployOutcome, GcSummary, NodeStats};
