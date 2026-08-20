//! Propagates the per-request `X-Request-Id` from `api.rs`'s middleware
//! down to wherever an [`oxid_core::AuditEvent`] is recorded in
//! `service::control_plane`, without threading an extra parameter through
//! every `ControlPlane` method that can produce one.
//!
//! `deploy`/`pause`/`wake`/`destroy` all run to completion inside the same
//! `tokio` task that's handling the originating HTTP request (none of them
//! are `tokio::spawn`ed off to run detached) — a `tokio::task_local!` is
//! exactly the tool for passing ambient, request-scoped context through an
//! arbitrarily deep call chain in that situation, the same way `tracing`
//! spans do for logs. The scheduler's background GC sweep and deploy-queue
//! retry pass never enter this scope, so events they record correctly see
//! `None` — there's no originating request to attribute them to.

use std::future::Future;

tokio::task_local! {
    static REQUEST_ID: String;
}

/// Runs `fut` with `request_id` available to [`current_request_id`] for its
/// entire (possibly `.await`-suspended) execution.
pub async fn scope<F: Future>(request_id: String, fut: F) -> F::Output {
    REQUEST_ID.scope(request_id, fut).await
}

/// The current request's id, if called from within a task set up by
/// [`scope`] (i.e. while handling an HTTP request through `api.rs`'s
/// `request_id_middleware`). `None` from the GC scheduler or any other
/// context with no originating request.
#[must_use]
pub fn current_request_id() -> Option<String> {
    REQUEST_ID.try_with(Clone::clone).ok()
}
