//! GitHub `repository_dispatch` notifier.
//!
//! When a successful `PATCH /v1/admin/models` lands on a cloud-api with
//! `ENABLE_GITHUB_DISPATCH=true`, the admin handler spawns a fire-and-forget
//! task here that POSTs to GitHub's `repos/{owner}/{name}/dispatches` API.
//! Downstream GH Actions workflows (validate / promote / rollback) listen on
//! the configured `event_type` and react to the loaded model.
//!
//! Failures are intentionally non-fatal: GitHub being unreachable must not
//! block model registration. Errors are logged at WARN by the caller so
//! operators can fall back to a manual `gh workflow run` trigger.

use async_trait::async_trait;
use config::GitHubDispatchConfig;
use serde::Serialize;
use std::{sync::Arc, time::Duration};

const GITHUB_API_BASE: &str = "https://api.github.com";
const DISPATCH_TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = "nearai-cloud-api/github-dispatch";

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct GitHubDispatchError {
    message: String,
}

impl GitHubDispatchError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[async_trait]
pub trait GitHubDispatcher: Send + Sync {
    /// Fire a `repository_dispatch` event for the given model id. Returns
    /// `Ok(())` for both the disabled-noop case and a successful POST.
    async fn dispatch_model_loaded(&self, model_id: &str) -> Result<(), GitHubDispatchError>;
}

pub struct NoopGitHubDispatcher;

#[async_trait]
impl GitHubDispatcher for NoopGitHubDispatcher {
    async fn dispatch_model_loaded(&self, _model_id: &str) -> Result<(), GitHubDispatchError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct DispatchRequest {
    event_type: String,
    client_payload: DispatchPayload,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct DispatchPayload {
    model_id: String,
}

#[async_trait]
trait DispatchTransport: Send + Sync {
    async fn dispatch(
        &self,
        endpoint: &str,
        pat: &str,
        request: DispatchRequest,
    ) -> Result<(), GitHubDispatchError>;
}

#[derive(Clone)]
struct ReqwestDispatchTransport {
    client: reqwest::Client,
}

impl Default for ReqwestDispatchTransport {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl DispatchTransport for ReqwestDispatchTransport {
    async fn dispatch(
        &self,
        endpoint: &str,
        pat: &str,
        request: DispatchRequest,
    ) -> Result<(), GitHubDispatchError> {
        // Wrap the whole exchange — send AND error-body read — in one timeout.
        // reqwest has no client-level timeout configured, so reading the body
        // of a stalled error response would otherwise hang the spawned task.
        let exchange = async {
            let response = self
                .client
                .post(endpoint)
                .bearer_auth(pat)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .header("User-Agent", USER_AGENT)
                .json(&request)
                .send()
                .await
                .map_err(|err| {
                    GitHubDispatchError::new(format!("GitHub dispatch request failed: {err}"))
                })?;

            let status = response.status();
            if status.is_success() {
                return Ok(());
            }

            let body = response.text().await.unwrap_or_default();
            Err(GitHubDispatchError::new(format!(
                "GitHub dispatch returned HTTP {status}: {}",
                crate::email::sanitize_error(&body)
            )))
        };

        tokio::time::timeout(DISPATCH_TIMEOUT, exchange)
            .await
            .map_err(|_| {
                GitHubDispatchError::new(format!(
                    "GitHub dispatch timed out after {}s",
                    DISPATCH_TIMEOUT.as_secs()
                ))
            })?
    }
}

pub struct HttpGitHubDispatcher {
    repo: String,
    event_type: String,
    pat: String,
    base_url: String,
    transport: Arc<dyn DispatchTransport>,
}

impl HttpGitHubDispatcher {
    fn new(
        repo: String,
        event_type: String,
        pat: String,
        base_url: String,
        transport: Arc<dyn DispatchTransport>,
    ) -> Self {
        Self {
            repo,
            event_type,
            pat,
            base_url,
            transport,
        }
    }
}

#[async_trait]
impl GitHubDispatcher for HttpGitHubDispatcher {
    async fn dispatch_model_loaded(&self, model_id: &str) -> Result<(), GitHubDispatchError> {
        let endpoint = format!("{}/repos/{}/dispatches", self.base_url, self.repo);
        let request = DispatchRequest {
            event_type: self.event_type.clone(),
            client_payload: DispatchPayload {
                model_id: model_id.to_string(),
            },
        };
        self.transport.dispatch(&endpoint, &self.pat, request).await
    }
}

/// Build a dispatcher from config. Returns `NoopGitHubDispatcher` when the
/// feature flag is off or when required fields are missing (config validation
/// already enforces this when enabled, so the noop fallback here is defensive).
pub fn dispatcher_from_config(config: &GitHubDispatchConfig) -> Arc<dyn GitHubDispatcher> {
    if !config.enabled {
        return Arc::new(NoopGitHubDispatcher);
    }
    let (Some(repo), Some(pat)) = (config.repo.clone(), config.pat.clone()) else {
        return Arc::new(NoopGitHubDispatcher);
    };
    Arc::new(HttpGitHubDispatcher::new(
        repo,
        config.event_type.clone(),
        pat,
        GITHUB_API_BASE.to_string(),
        Arc::new(ReqwestDispatchTransport::default()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct StubTransport {
        outcome: Mutex<Result<(), GitHubDispatchError>>,
        calls: Mutex<Vec<(String, String, DispatchRequest)>>,
    }

    impl StubTransport {
        fn new(outcome: Result<(), GitHubDispatchError>) -> Self {
            Self {
                outcome: Mutex::new(outcome),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl DispatchTransport for StubTransport {
        async fn dispatch(
            &self,
            endpoint: &str,
            pat: &str,
            request: DispatchRequest,
        ) -> Result<(), GitHubDispatchError> {
            self.calls
                .lock()
                .unwrap()
                .push((endpoint.to_string(), pat.to_string(), request));
            self.outcome.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn noop_dispatcher_returns_ok() {
        let dispatcher = NoopGitHubDispatcher;
        let result = dispatcher.dispatch_model_loaded("glm-5.1").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn http_dispatcher_posts_expected_payload() {
        let transport = Arc::new(StubTransport::new(Ok(())));
        let dispatcher = HttpGitHubDispatcher::new(
            "nearai/cvm-ansible-playbooks".to_string(),
            "stg_model_loaded".to_string(),
            "ghp_test".to_string(),
            "https://api.example.test".to_string(),
            transport.clone(),
        );

        dispatcher.dispatch_model_loaded("glm-5.1").await.unwrap();

        let calls = transport.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let (endpoint, pat, request) = &calls[0];
        assert_eq!(
            endpoint,
            "https://api.example.test/repos/nearai/cvm-ansible-playbooks/dispatches"
        );
        assert_eq!(pat, "ghp_test");
        assert_eq!(request.event_type, "stg_model_loaded");
        assert_eq!(request.client_payload.model_id, "glm-5.1");
    }

    #[tokio::test]
    async fn http_dispatcher_propagates_transport_error() {
        let transport = Arc::new(StubTransport::new(Err(GitHubDispatchError::new("boom"))));
        let dispatcher = HttpGitHubDispatcher::new(
            "nearai/cvm-ansible-playbooks".to_string(),
            "stg_model_loaded".to_string(),
            "ghp_test".to_string(),
            "https://api.example.test".to_string(),
            transport,
        );

        let err = dispatcher
            .dispatch_model_loaded("glm-5.1")
            .await
            .unwrap_err();
        assert_eq!(format!("{err}"), "boom");
    }

    #[tokio::test]
    async fn factory_returns_noop_when_disabled() {
        let config = GitHubDispatchConfig {
            enabled: false,
            repo: Some("nearai/cvm-ansible-playbooks".to_string()),
            event_type: "stg_model_loaded".to_string(),
            pat: Some("ghp_test".to_string()),
        };

        let dispatcher = dispatcher_from_config(&config);
        // Verify by calling — noop returns Ok regardless of repo/pat presence.
        dispatcher.dispatch_model_loaded("glm-5.1").await.unwrap();
    }

    #[tokio::test]
    async fn factory_returns_noop_when_enabled_but_fields_missing() {
        let config = GitHubDispatchConfig {
            enabled: true,
            repo: None,
            event_type: "stg_model_loaded".to_string(),
            pat: None,
        };

        let dispatcher = dispatcher_from_config(&config);
        // Defensive fallback: missing fields downgrade to noop instead of panicking.
        dispatcher.dispatch_model_loaded("glm-5.1").await.unwrap();
    }
}
