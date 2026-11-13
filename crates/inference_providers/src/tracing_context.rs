//! The authenticated tenant of the request being served, carried to the
//! self-hosted inference calls that request makes.
//!
//! The API-key middleware enters the scope once authentication succeeds. The
//! NEAR provider reads it when it builds an inference request and sends the
//! ids as `X-Request-Id`, `X-Org-Id` and `X-Workspace-Id`. Discovery,
//! attestation and model-list calls do not read it, and external providers
//! never do. Values come only from the authenticated request, never from
//! client input.
//!
//! A task-local does not follow `tokio::spawn` and is gone once the handler
//! returns, so a future polled later (a spawned task, or a response stream
//! polled by the HTTP server) must capture the context first with
//! [`scoped`] or [`spawn_with_tracing_context`].

use std::future::Future;
use uuid::Uuid;

/// Ids of the authenticated request. All three are UUIDs, so each is always
/// a valid header value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TracingContext {
    pub request_id: Uuid,
    pub org_id: Uuid,
    pub workspace_id: Uuid,
}

tokio::task_local! {
    static TRACING_CONTEXT: Option<TracingContext>;
}

/// Run `fut` with `context` in scope.
pub async fn with_tracing_context<F: Future>(context: TracingContext, fut: F) -> F::Output {
    TRACING_CONTEXT.scope(Some(context), fut).await
}

/// Run `fut` with no context, even inside an authenticated request's scope.
/// For backends that must not receive the ids. Spawned or deferred work
/// started from inside `fut` inherits the cleared scope.
pub async fn without_tracing_context<F: Future>(fut: F) -> F::Output {
    TRACING_CONTEXT.scope(None, fut).await
}

/// The context in scope, if the current task is serving an authenticated
/// request and the ids have not been cleared.
pub fn current_tracing_context() -> Option<TracingContext> {
    TRACING_CONTEXT.try_with(|context| *context).ok().flatten()
}

/// Capture the context in scope now and re-enter it whenever `fut` is
/// polled. With no context in scope `fut` runs unchanged.
pub fn scoped<F: Future>(fut: F) -> impl Future<Output = F::Output> {
    let context = current_tracing_context();
    async move {
        match context {
            Some(context) => with_tracing_context(context, fut).await,
            None => fut.await,
        }
    }
}

/// `tokio::spawn`, carrying the context in scope (if any) into the spawned
/// task.
pub fn spawn_with_tracing_context<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(scoped(fut))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> TracingContext {
        TracingContext {
            request_id: Uuid::new_v4(),
            org_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
        }
    }

    #[tokio::test]
    async fn a_spawned_task_sees_the_context_only_when_carried() {
        let expected = context();
        let (carried, plain) = with_tracing_context(expected, async {
            (
                spawn_with_tracing_context(async { current_tracing_context() })
                    .await
                    .unwrap(),
                tokio::spawn(async { current_tracing_context() })
                    .await
                    .unwrap(),
            )
        })
        .await;
        assert_eq!(carried, Some(expected));
        // Why the helper exists: a plain spawn drops the task-local.
        assert_eq!(plain, None);
    }

    #[tokio::test]
    async fn a_scoped_future_polled_after_the_scope_ends_keeps_the_context() {
        let expected = context();
        let deferred = TRACING_CONTEXT.sync_scope(Some(expected), || {
            scoped(async { current_tracing_context() })
        });
        assert_eq!(current_tracing_context(), None);
        assert_eq!(deferred.await, Some(expected));
    }

    #[tokio::test]
    async fn without_a_context_in_scope_there_is_none() {
        assert_eq!(current_tracing_context(), None);
        let seen = spawn_with_tracing_context(async { current_tracing_context() })
            .await
            .unwrap();
        assert_eq!(seen, None);
    }

    #[tokio::test]
    async fn a_cleared_scope_hides_the_context_from_inline_and_spawned_work() {
        let (inline, spawned) = with_tracing_context(context(), async {
            without_tracing_context(async {
                (
                    current_tracing_context(),
                    spawn_with_tracing_context(async { current_tracing_context() })
                        .await
                        .unwrap(),
                )
            })
            .await
        })
        .await;
        assert_eq!(inline, None);
        assert_eq!(spawned, None);
    }
}
