//! Budget callback for retries hidden inside a provider (for example NEAR fleet rotation).
//! The pool admits the first attempt; each additional inference dispatch calls this hook.
use crate::CompletionError;
use std::sync::Arc;

#[async_trait::async_trait]
pub trait RetryAdmission: Send + Sync {
    async fn admit_retry(&self) -> Result<(), CompletionError>;
    async fn mark_dispatched(&self) {}
    async fn attempt_failed(&self) {}
    async fn release_not_dispatched(&self) {}
}

/// Preserve transport behavior and release only failures known to precede dispatch.
/// HTTP errors, read timeouts and dropped futures can already have incurred usage.
#[async_trait::async_trait]
pub(crate) trait RequestAdmissionExt {
    async fn send_with_admission(self) -> Result<reqwest::Response, reqwest::Error>;
}

#[async_trait::async_trait]
impl RequestAdmissionExt for reqwest::RequestBuilder {
    async fn send_with_admission(self) -> Result<reqwest::Response, reqwest::Error> {
        if let Ok(admission) = ADMISSION.try_with(Clone::clone) {
            admission.mark_dispatched().await;
        }
        let result = self.send().await;
        if result
            .as_ref()
            .is_err_and(|e| e.is_connect() || e.is_builder())
        {
            if let Ok(admission) = ADMISSION.try_with(Clone::clone) {
                admission.release_not_dispatched().await;
            }
        }
        result
    }
}

/// Provider preparation can fail before its HTTP transport is invoked.
pub async fn attempt_failed() {
    if let Ok(admission) = ADMISSION.try_with(Clone::clone) {
        admission.attempt_failed().await;
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
    // Cloud's text-generation dispatch sites enforce admission before entering a provider.
    if let Ok(admission) = ADMISSION.try_with(Clone::clone) {
        admission.admit_retry().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[derive(Default)]
    struct Admissions(AtomicUsize);
    #[async_trait::async_trait]
    impl RetryAdmission for Admissions {
        async fn admit_retry(&self) -> Result<(), CompletionError> {
            Ok(())
        }
        async fn release_not_dispatched(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn only_pre_dispatch_transport_errors_release_the_attempt() {
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::path("/error"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(wiremock::matchers::path("/slow"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(100)))
            .mount(&server)
            .await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let admissions = Arc::new(Admissions::default());
        scope(admissions.clone(), async {
            assert_eq!(
                client
                    .post(format!("{}/error", server.uri()))
                    .send_with_admission()
                    .await
                    .unwrap()
                    .status(),
                503
            );
            assert!(client
                .post(format!("{}/slow", server.uri()))
                .timeout(Duration::from_millis(10))
                .send_with_admission()
                .await
                .unwrap_err()
                .is_timeout());
            assert_eq!(admissions.0.load(Ordering::SeqCst), 0);
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            drop(listener);
            assert!(client
                .post(format!("http://{addr}"))
                .send_with_admission()
                .await
                .unwrap_err()
                .is_connect());
            assert_eq!(admissions.0.load(Ordering::SeqCst), 1);
            assert!(client
                .post("invalid URL")
                .send_with_admission()
                .await
                .unwrap_err()
                .is_builder());
            assert_eq!(admissions.0.load(Ordering::SeqCst), 2);
        })
        .await;
    }
}
