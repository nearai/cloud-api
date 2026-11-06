//! Budget callback for retries hidden inside a provider (for example NEAR fleet rotation).
//! The pool admits the first attempt; each additional inference dispatch calls this hook.
use crate::CompletionError;
use std::sync::Arc;

#[async_trait::async_trait]
pub trait RetryAdmission: Send + Sync {
    async fn admit_retry(&self) -> Result<(), CompletionError>;
}
tokio::task_local! { static ADMISSION: Arc<dyn RetryAdmission>; }
pub async fn scope<F: std::future::Future>(
    admission: Arc<dyn RetryAdmission>,
    future: F,
) -> F::Output {
    ADMISSION.scope(admission, future).await
}
pub async fn before_retry() -> Result<(), CompletionError> {
    // Provider transport tests and non-billing consumers may use providers directly.
    // Cloud's text-generation dispatch sites enforce admission before entering a provider.
    if let Ok(admission) = ADMISSION.try_with(Clone::clone) {
        admission.admit_retry().await?;
    }
    Ok(())
}
