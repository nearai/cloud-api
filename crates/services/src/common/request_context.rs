//! Request IDs for structured events across service/repository boundaries.
//! Task-local scoping keeps concurrent requests isolated without exposing HTTP
//! types or changing the production formatter's span-privacy settings.
use std::future::Future;
use uuid::Uuid;

tokio::task_local! {
    static REQUEST_ID: Uuid;
}

/// Scope downstream work to an already validated request ID. Spawned tasks do
/// not inherit this context; callers must explicitly scope detached work.
pub async fn scope<T>(request_id: Uuid, work: impl Future<Output = T>) -> T {
    REQUEST_ID.scope(request_id, work).await
}

/// None for work outside an HTTP request (e.g. background jobs and unit tests).
pub fn current_request_id() -> Option<String> {
    REQUEST_ID.try_with(Uuid::to_string).ok()
}
