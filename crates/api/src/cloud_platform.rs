//! Composition of the cloud-platform adapters: which implementation serves
//! each area, chosen at startup from the `CP_*` flags (see
//! `database::cp_client`).
//!
//! With every flag `off` no client is built, each service receives the local
//! repository it always had, and the gateway route is unchanged.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use database::cp_client::{
    ApiKeyVerdicts, AreaMode, CatalogModels, ChatSignatureReads, CpClient, CpClientConfig, Verdict,
};
use services::attestation::ports::AttestationRepository;
use services::metrics::MetricsServiceTrait;
use services::models::ModelsRepository;

use crate::models::ErrorResponse;

const AREA_TAG: &str = "area:check_api_key";
/// Largest body the shadow comparison reads. A key check answer is a few hundred bytes.
const SHADOW_BODY_LIMIT: usize = 64 * 1024;

/// The key-check area as the gateway route needs it.
#[derive(Clone)]
pub struct CheckApiKeyRoute {
    pub mode: AreaMode,
    pub verdicts: Arc<ApiKeyVerdicts>,
    pub metrics: Option<Arc<dyn MetricsServiceTrait>>,
}

/// The shared cloud-platform client, built only when some area is not `off`.
/// Fails when the configuration is invalid or the key file is unusable; the
/// error names the setting, never its value.
pub fn build_client(
    config: &CpClientConfig,
    metrics: Arc<dyn MetricsServiceTrait>,
) -> anyhow::Result<Option<Arc<CpClient>>> {
    if !config.flags.any_enabled() {
        return Ok(None);
    }
    Ok(Some(Arc::new(CpClient::from_config(
        config,
        Some(metrics),
    )?)))
}

/// The models repository the catalog area serves: the local one when the area
/// is `off`, otherwise [`CatalogModels`] over it.
pub fn models_repository(
    mode: AreaMode,
    local: Arc<dyn ModelsRepository>,
    client: Option<&Arc<CpClient>>,
    ttl: std::time::Duration,
    metrics: Option<Arc<dyn MetricsServiceTrait>>,
) -> Arc<dyn ModelsRepository> {
    match client {
        // `client` exists whenever an area is not off (see `build_client`).
        Some(client) if mode != AreaMode::Off => Arc::new(CatalogModels::new(
            mode,
            local,
            client.clone(),
            ttl,
            metrics,
        )),
        _ => local,
    }
}

/// The attestation repository the signature area serves: the local one when
/// the area is `off`, otherwise [`ChatSignatureReads`] over it.
pub fn signature_repository(
    mode: AreaMode,
    local: Arc<dyn AttestationRepository>,
    client: Option<&Arc<CpClient>>,
    metrics: Option<Arc<dyn MetricsServiceTrait>>,
) -> Arc<dyn AttestationRepository> {
    match client {
        Some(client) if mode != AreaMode::Off => Arc::new(ChatSignatureReads::new(
            mode,
            local,
            client.clone(),
            metrics,
        )),
        _ => local,
    }
}

/// The gateway key-check route for the flag, or `None` when it is `off`.
pub fn check_api_key_route(
    config: &CpClientConfig,
    client: Option<&Arc<CpClient>>,
    metrics: Option<Arc<dyn MetricsServiceTrait>>,
) -> Option<CheckApiKeyRoute> {
    let mode = config.flags.check_api_key;
    match client {
        Some(client) if mode != AreaMode::Off => Some(CheckApiKeyRoute {
            mode,
            verdicts: Arc::new(ApiKeyVerdicts::new(
                client.clone(),
                config.check_api_key_ttl,
            )),
            metrics,
        }),
        _ => None,
    }
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

/// `POST /v1/check_api_key` when the area is `on`: cloud-platform's verdict, with no
/// local key or credit check. An outage with no fresh verdict is a 503, never a 401.
pub async fn check_api_key_from_cloud_platform(
    State(verdicts): State<Arc<ApiKeyVerdicts>>,
    headers: HeaderMap,
) -> Response {
    let bearer = match bearer_token(&headers) {
        Ok(bearer) => bearer,
        Err(error) => return unauthorized(error),
    };
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

/// Shadow mode for the key check: the local route answers, and cloud-platform's
/// verdict for the same key is compared with it. A difference is counted by field
/// (`status`, or `body` for 200 and 402). A cloud-platform failure is not a difference.
pub async fn shadow_check_api_key(
    State(route): State<CheckApiKeyRoute>,
    request: Request,
    next: Next,
) -> Response {
    let bearer = bearer_token(request.headers()).ok().map(str::to_owned);
    let response = next.run(request).await;
    let Some(bearer) = bearer else {
        return response;
    };
    let Ok(verdict) = route.verdicts.check(&bearer).await else {
        return response;
    };
    let (parts, body) = response.into_parts();
    let bytes = match to_bytes(body, SHADOW_BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let field = if parts.status != verdict.status {
        Some("status")
    } else if matches!(parts.status, StatusCode::OK | StatusCode::PAYMENT_REQUIRED)
        && bytes != verdict.body
    {
        Some("body")
    } else {
        None
    };
    if let (Some(field), Some(metrics)) = (field, &route.metrics) {
        let field_tag = format!("field:{field}");
        metrics.record_count(
            database::cp_client::client::METRIC_CP_SHADOW_MISMATCH,
            1,
            &[AREA_TAG, &field_tag],
        );
    }
    axum::response::Response::from_parts(parts, Body::from(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use database::cp_client::{AreaFlags, CpClientConfig};
    use services::metrics::capturing::CapturingMetricsService;

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

    fn all_off() -> CpClientConfig {
        CpClientConfig::from_lookup(|_| None).unwrap()
    }

    #[test]
    fn all_off_builds_no_client_and_keeps_every_local_implementation() {
        let config = all_off();
        assert_eq!(config.flags, AreaFlags::default());
        let metrics: Arc<dyn MetricsServiceTrait> = Arc::new(CapturingMetricsService::new());
        assert!(build_client(&config, metrics.clone()).unwrap().is_none());
        assert!(check_api_key_route(&config, None, Some(metrics)).is_none());
    }

    #[test]
    fn a_flag_on_without_a_url_fails_at_startup_naming_the_setting() {
        let error = CpClientConfig::from_lookup(|name| match name {
            "CP_CATALOG" => Some("on".to_string()),
            _ => None,
        })
        .unwrap_err();
        assert!(error.contains("CLOUD_PLATFORM_URL"), "{error}");
    }

    #[test]
    fn a_flag_on_with_an_unreadable_key_file_fails_naming_the_setting_only() {
        let config = CpClientConfig::from_lookup(|name| match name {
            "CP_CATALOG" => Some("shadow".to_string()),
            "CLOUD_PLATFORM_URL" => Some("http://127.0.0.1:9".to_string()),
            "CLOUD_PLATFORM_API_KEY_FILE" => Some("/nonexistent/cp-key".to_string()),
            _ => None,
        })
        .unwrap();
        let metrics: Arc<dyn MetricsServiceTrait> = Arc::new(CapturingMetricsService::new());
        let error = format!("{:#}", build_client(&config, metrics).unwrap_err());
        assert!(error.contains("CLOUD_PLATFORM_API_KEY_FILE"), "{error}");
        assert!(!error.contains("/nonexistent"), "{error}");
    }

    #[test]
    fn off_areas_return_the_local_repositories_unchanged() {
        // Even with a client present, an `off` area is the local object itself.
        let config = all_off();
        let client = Arc::new(
            CpClient::new(
                "http://127.0.0.1:9",
                Some("sk-service".to_string()),
                &config,
                None,
            )
            .unwrap(),
        );
        let local_models: Arc<dyn ModelsRepository> = Arc::new(NoModels);
        let chosen = models_repository(
            AreaMode::Off,
            local_models.clone(),
            Some(&client),
            config.catalog_ttl,
            None,
        );
        assert!(Arc::ptr_eq(&chosen, &local_models));

        let local_attestations: Arc<dyn AttestationRepository> = Arc::new(NoAttestations);
        let chosen = signature_repository(
            AreaMode::Off,
            local_attestations.clone(),
            Some(&client),
            None,
        );
        assert!(Arc::ptr_eq(&chosen, &local_attestations));
        assert!(check_api_key_route(&config, Some(&client), None).is_none());
    }

    use axum::body::to_bytes as body_bytes;
    use axum::body::Body;
    use axum::middleware::from_fn_with_state;
    use axum::routing::post;
    use axum::Router;
    use std::time::Duration;
    use tower::ServiceExt;
    use wiremock::matchers::{any, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const VALID: &str =
        r#"{"valid":true,"organization_id":"o","workspace_id":"w","api_key_id":"k"}"#;

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
        let mut builder = axum::http::Request::builder()
            .method("POST")
            .uri("/check_api_key");
        if let Some(bearer) = bearer {
            builder = builder.header("authorization", format!("Bearer {bearer}"));
        }
        let response = router
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = body_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, body.to_vec())
    }

    #[tokio::test]
    async fn on_refuses_a_missing_key_without_calling_cloud_platform() {
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
    async fn on_forwards_the_verdict_body_byte_for_byte() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/check_api_key"))
            .and(header("authorization", "Bearer sk-caller"))
            .respond_with(ResponseTemplate::new(200).set_body_string(VALID))
            .expect(1)
            .mount(&server)
            .await;
        let (status, body) = call(on_router(&server), Some("sk-caller")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, VALID.as_bytes());
    }

    #[tokio::test]
    async fn on_answers_503_not_401_when_cloud_platform_is_down() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let (status, _) = call(on_router(&server), Some("sk-down")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn shadow_serves_the_local_answer_and_counts_a_status_difference() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(402).set_body_string(r#"{"error":{}}"#))
            .mount(&server)
            .await;
        let metrics = Arc::new(CapturingMetricsService::new());
        let route = CheckApiKeyRoute {
            mode: AreaMode::Shadow,
            verdicts: Arc::new(ApiKeyVerdicts::new(
                cp_client(&server),
                Duration::from_secs(60),
            )),
            metrics: Some(metrics.clone() as Arc<dyn MetricsServiceTrait>),
        };
        let router = Router::new()
            .route("/check_api_key", post(|| async { (StatusCode::OK, VALID) }))
            .layer(from_fn_with_state(route, shadow_check_api_key));
        let (status, body) = call(router, Some("sk-shadow")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, VALID.as_bytes());
        let fields: Vec<String> = metrics
            .get_metrics()
            .into_iter()
            .filter(|metric| metric.name == database::cp_client::client::METRIC_CP_SHADOW_MISMATCH)
            .flat_map(|metric| metric.tags)
            .filter(|tag| tag.starts_with("field:"))
            .collect();
        assert_eq!(fields, ["field:status"]);
    }
}
