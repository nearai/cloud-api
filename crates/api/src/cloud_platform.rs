//! Cloud-platform wiring. The connection settings are the only switch: with
//! `CLOUD_PLATFORM_URL` and `CLOUD_PLATFORM_API_KEY_FILE` both unset, nothing
//! here is constructed and every service keeps its local implementation. With
//! both set, the catalog, the chat-signature read and `/v1/check_api_key` are
//! served by the cloud-platform adapters.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use database::cp_client::{
    ApiKeyVerdicts, CatalogModels, ChatSignatureReads, CpClient, CpClientConfig, Verdict,
};
use services::attestation::ports::AttestationRepository;
use services::metrics::MetricsServiceTrait;
use services::models::ModelsRepository;

use crate::models::ErrorResponse;

/// The configured cloud-platform client and the tuning it was built with.
pub struct CloudPlatform {
    pub client: Arc<CpClient>,
    pub catalog_ttl: Duration,
    pub catalog_max_stale: Duration,
    pub check_api_key_ttl: Duration,
}

/// Builds the client when the connection settings are present. `Ok(None)` when
/// they are not, so no cloud-platform code is constructed. Errors name the
/// setting, never its value.
pub fn build(
    config: Option<&CpClientConfig>,
    metrics: Arc<dyn MetricsServiceTrait>,
) -> anyhow::Result<Option<CloudPlatform>> {
    let Some(config) = config else {
        return Ok(None);
    };
    let client = CpClient::from_config(config, Some(metrics))?;
    Ok(Some(CloudPlatform {
        client: Arc::new(client),
        catalog_ttl: config.catalog_ttl,
        catalog_max_stale: config.catalog_max_stale,
        check_api_key_ttl: config.check_api_key_ttl,
    }))
}

/// The models repository: the catalog adapter when configured, else the local one.
pub fn models_repository(
    local: Arc<dyn ModelsRepository>,
    cloud_platform: Option<&CloudPlatform>,
    metrics: Option<Arc<dyn MetricsServiceTrait>>,
) -> Arc<dyn ModelsRepository> {
    match cloud_platform {
        Some(cp) => Arc::new(CatalogModels::new(
            local,
            cp.client.clone(),
            cp.catalog_ttl,
            cp.catalog_max_stale,
            metrics,
        )),
        None => local,
    }
}

/// The attestation repository: the signature-read adapter when configured, else the local one.
pub fn signature_repository(
    local: Arc<dyn AttestationRepository>,
    cloud_platform: Option<&CloudPlatform>,
) -> Arc<dyn AttestationRepository> {
    match cloud_platform {
        Some(cp) => Arc::new(ChatSignatureReads::new(local, cp.client.clone())),
        None => local,
    }
}

/// The key verdicts for `/v1/check_api_key`, when configured.
pub fn check_api_key_verdicts(
    cloud_platform: Option<&CloudPlatform>,
) -> Option<Arc<ApiKeyVerdicts>> {
    cloud_platform.map(|cp| Arc::new(ApiKeyVerdicts::new(cp.client.clone(), cp.check_api_key_ttl)))
}

enum BearerError {
    Missing,
    Malformed,
}

/// The bearer token, read the way the local key middleware reads it.
fn bearer_token(headers: &HeaderMap) -> Result<&str, BearerError> {
    let value = headers
        .get(AUTHORIZATION)
        .ok_or(BearerError::Missing)?
        .to_str()
        .map_err(|_| BearerError::Malformed)?;
    value.strip_prefix("Bearer ").ok_or(BearerError::Malformed)
}

fn unauthorized(error: BearerError) -> Response {
    let (message, code) = match error {
        BearerError::Missing => ("Missing authorization header", "missing_auth_header"),
        BearerError::Malformed => ("Invalid authorization header format", "invalid_auth_header"),
    };
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse::new(message.to_string(), code.to_string())),
    )
        .into_response()
}

fn verdict_response(verdict: Verdict) -> Response {
    (
        verdict.status,
        [(CONTENT_TYPE, "application/json")],
        verdict.body,
    )
        .into_response()
}

/// The local key path's answer to a key that is not even well formed
/// (`AuthService::validate_api_key` returns `Unauthorized`, which the key
/// middleware renders as this 401).
fn malformed_key() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse::new(
            "Invalid or expired API key".to_string(),
            "invalid_api_key".to_string(),
        )),
    )
        .into_response()
}

/// `POST /v1/check_api_key` when cloud-platform is configured: cloud-platform's
/// verdict, with no local key or credit check on this route. An outage with no
/// fresh verdict, or too many key checks in flight, is a 503, never a 401.
///
/// A key that fails cloud-api's own format check (`sk-` and 35 characters,
/// `services::common::is_valid_api_key_format`, the first step of the local
/// key validation) is refused here without a call, as the local path does.
pub async fn check_api_key_from_cloud_platform(
    State(verdicts): State<Arc<ApiKeyVerdicts>>,
    headers: HeaderMap,
) -> Response {
    let bearer = match bearer_token(&headers) {
        Ok(bearer) => bearer,
        Err(error) => return unauthorized(error),
    };
    if !services::common::is_valid_api_key_format(bearer) {
        return malformed_key();
    }
    match verdicts.check(bearer).await {
        Ok(verdict) => verdict_response(verdict),
        Err(error) => {
            tracing::warn!(
                error_kind = error.kind(),
                "cloud-platform key check unavailable"
            );
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "Key verification is temporarily unavailable".to_string(),
                    "service_unavailable".to_string(),
                )),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::routing::post;
    use axum::Router;
    use services::metrics::capturing::CapturingMetricsService;
    use tower::ServiceExt;
    use wiremock::matchers::{any, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct NoModels;

    #[async_trait::async_trait]
    impl ModelsRepository for NoModels {
        async fn get_all_active_models(
            &self,
        ) -> anyhow::Result<Vec<services::models::ModelWithPricing>> {
            Ok(Vec::new())
        }
        async fn get_model_by_name(
            &self,
            _: &str,
        ) -> anyhow::Result<Option<services::models::ModelWithPricing>> {
            Ok(None)
        }
        async fn resolve_and_get_model(
            &self,
            _: &str,
        ) -> anyhow::Result<Option<services::models::ModelWithPricing>> {
            Ok(None)
        }
        async fn get_configured_model_names(&self) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    struct NoAttestations;

    #[async_trait::async_trait]
    impl AttestationRepository for NoAttestations {
        async fn add_chat_signature(
            &self,
            _: &str,
            _: services::attestation::models::ChatSignature,
        ) -> Result<(), services::attestation::models::AttestationError> {
            Ok(())
        }
        async fn get_chat_signature(
            &self,
            chat_id: &str,
            signing_algo: &str,
        ) -> Result<
            services::attestation::models::ChatSignature,
            services::attestation::models::AttestationError,
        > {
            Err(
                services::attestation::models::AttestationError::SignatureNotFound(format!(
                    "{chat_id}:{signing_algo}"
                )),
            )
        }
    }

    fn metrics() -> Arc<dyn MetricsServiceTrait> {
        Arc::new(CapturingMetricsService::new())
    }

    #[test]
    fn unconfigured_builds_nothing_and_keeps_every_local_implementation() {
        assert!(build(None, metrics()).unwrap().is_none());
        let local_models: Arc<dyn ModelsRepository> = Arc::new(NoModels);
        let chosen = models_repository(local_models.clone(), None, None);
        assert!(Arc::ptr_eq(&chosen, &local_models));
        let local_attestations: Arc<dyn AttestationRepository> = Arc::new(NoAttestations);
        let chosen = signature_repository(local_attestations.clone(), None);
        assert!(Arc::ptr_eq(&chosen, &local_attestations));
        assert!(check_api_key_verdicts(None).is_none());
    }

    #[test]
    fn an_unreadable_key_file_fails_naming_the_setting_only() {
        let config = CpClientConfig::from_lookup(|name| match name {
            "CLOUD_PLATFORM_URL" => Some("http://127.0.0.1:9".to_string()),
            "CLOUD_PLATFORM_API_KEY_FILE" => Some("/nonexistent/cp-key".to_string()),
            _ => None,
        })
        .unwrap()
        .unwrap();
        let error = format!("{:#}", build(Some(&config), metrics()).err().unwrap());
        assert!(error.contains("CLOUD_PLATFORM_API_KEY_FILE"), "{error}");
        assert!(!error.contains("/nonexistent"), "{error}");
    }

    fn cp_client(server: &MockServer) -> Arc<CpClient> {
        Arc::new(
            CpClient::new(
                &server.uri(),
                Some("sk-service".to_string()),
                &CpClientConfig::default(),
                None,
            )
            .unwrap(),
        )
    }

    fn on_router(server: &MockServer) -> Router {
        let verdicts = Arc::new(ApiKeyVerdicts::new(
            cp_client(server),
            Duration::from_secs(60),
        ));
        Router::new()
            .route("/check_api_key", post(check_api_key_from_cloud_platform))
            .with_state(verdicts)
    }

    async fn call(router: Router, bearer: Option<&str>) -> (StatusCode, Vec<u8>) {
        let mut builder = HttpRequest::builder().method("POST").uri("/check_api_key");
        if let Some(bearer) = bearer {
            builder = builder.header("authorization", format!("Bearer {bearer}"));
        }
        let response = router
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, body.to_vec())
    }

    const VALID: &str =
        r#"{"valid":true,"organization_id":"o","workspace_id":"w","api_key_id":"k"}"#;

    /// Well formed: `sk-` and 32 more characters.
    const CALLER: &str = "sk-0123456789abcdef0123456789abcdef";
    const OTHER: &str = "sk-fedcba9876543210fedcba9876543210";

    #[tokio::test]
    async fn refuses_a_missing_key_without_calling_cloud_platform() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let (status, body) = call(on_router(&server), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "missing_auth_header");
    }

    #[tokio::test]
    async fn forwards_the_verdict_body_byte_for_byte() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/check_api_key"))
            .respond_with(ResponseTemplate::new(200).set_body_string(VALID))
            .expect(1)
            .mount(&server)
            .await;
        let (status, body) = call(on_router(&server), Some(CALLER)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, VALID.as_bytes());
    }

    #[tokio::test]
    async fn answers_503_not_401_when_cloud_platform_is_down() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let (status, _) = call(on_router(&server), Some(OTHER)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn refuses_a_malformed_key_as_the_local_path_does_without_calling_cloud_platform() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_string(VALID))
            .expect(0)
            .mount(&server)
            .await;
        assert!(services::common::is_valid_api_key_format(CALLER));
        for malformed in [
            "sk-short",
            "pk-0123456789abcdef0123456789abcdef",
            &format!("{CALLER}0"),
        ] {
            let (status, body) = call(on_router(&server), Some(malformed)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{malformed}");
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["type"], "invalid_api_key");
            assert_eq!(body["error"]["message"], "Invalid or expired API key");
        }
    }
}
