//! Oxid control-plane daemon.
//!
//! Hosts the ports & adapters described in SPEC.md §2.2, all depending on
//! [`oxid_core`]:
//! - `adapter::config` — parses `oxid.toml` into domain configuration.
//! - `adapter::store` — `SQLite` persistence implementing the domain ports.

pub mod adapter;

/// Reserved for future `HTTP`/webhook adapters.
pub mod api {
    /// No-op marker; will become the `axum` router in a later phase.
    pub const READY: bool = false;
}
