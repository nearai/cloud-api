//! Budget callback for retries hidden inside a provider (for example NEAR fleet rotation).
//! The pool admits the first attempt; each additional inference dispatch calls this hook.
use crate::CompletionError;
use std::sync::Arc;

#[async_trait::async_trait]
pub trait RetryAdmission: Send + Sync {
    async fn admit_retry(&self) -> Result<(), CompletionError>;
    async fn delegated_identity(&self) -> Option<DelegatedIdentity> {
        None
    }
}

/// Set only by Cloud API's authenticated billing context, never by request JSON.
pub struct DelegatedIdentity {
    pub organization_id: String,
    pub workspace_id: String,
    pub api_key_id: String,
}
impl DelegatedIdentity {
    pub fn headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        for (key, value) in [
            ("x-nearai-billing", "per_attempt_v1"),
            ("x-nearai-billing-org", self.organization_id.as_str()),
            ("x-nearai-billing-workspace", self.workspace_id.as_str()),
            ("x-nearai-billing-key", self.api_key_id.as_str()),
        ] {
            headers.insert(key, value.parse().expect("billing identity contains UUIDs"));
        }
        headers
    }
}
pub async fn delegated_headers() -> reqwest::header::HeaderMap {
    match ADMISSION.try_with(Clone::clone) {
        Ok(admission) => admission
            .delegated_identity()
            .await
            .map(|id| id.headers())
            .unwrap_or_default(),
        Err(_) => reqwest::header::HeaderMap::new(),
    }
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
    // Cloud's pool enforces mandatory admission before entering any provider.
    if let Ok(admission) = ADMISSION.try_with(Clone::clone) {
        admission.admit_retry().await?;
    }
    Ok(())
}
