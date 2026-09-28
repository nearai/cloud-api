//! TypeSafe-compatible System One transport. base_url includes the API version
//! (https://api.typesafe.ai/v1, or https://openrouter.ai/api/v1).
use super::backend::{BackendConfig, ExternalBackend};
use crate::{
    systemone, ChatCompletionParams, ChatCompletionResponseWithBytes, CompletionError,
    StreamingResult, SystemOneRequest, SystemOneResponseWithBytes,
};
use async_trait::async_trait;
use std::time::Duration;

#[derive(Default)]
pub struct TypeSafeBackend {
    client: reqwest::Client,
}

#[async_trait]
impl ExternalBackend for TypeSafeBackend {
    fn backend_type(&self) -> &'static str {
        "typesafe"
    }

    async fn chat_completion_stream(
        &self,
        _config: &BackendConfig,
        _model: &str,
        _params: ChatCompletionParams,
    ) -> Result<StreamingResult, CompletionError> {
        Err(CompletionError::CompletionError(
            "Use /v1/systemone for decision models".into(),
        ))
    }

    async fn chat_completion(
        &self,
        _config: &BackendConfig,
        _model: &str,
        _params: ChatCompletionParams,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        Err(CompletionError::CompletionError(
            "Use /v1/systemone for decision models".into(),
        ))
    }

    async fn systemone(
        &self,
        config: &BackendConfig,
        model: &str,
        mut request: SystemOneRequest,
    ) -> Result<SystemOneResponseWithBytes, CompletionError> {
        request.model = model.to_owned();
        let timeout_seconds = config.timeout_seconds.max(1) as u64;
        let response = self
            .client
            .post(format!(
                "{}/systemone",
                config.base_url.trim_end_matches('/')
            ))
            .bearer_auth(&config.api_key)
            .timeout(Duration::from_secs(timeout_seconds))
            .json(&request)
            .send()
            .await
            .map_err(|error| systemone::transport_error(error, timeout_seconds))?;
        systemone::read_response(response, &request, true).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExternalProvider, ExternalProviderConfig, InferenceProvider, ProviderConfig};
    use serde_json::json;
    use wiremock::{
        matchers::{body_partial_json, header, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    fn request() -> SystemOneRequest {
        serde_json::from_value(json!({"model":"catalog/jev","state":["text",{"x":1}],
            "questions":{"q":{"type":"noul"}}}))
        .unwrap()
    }

    fn provider(base_url: String) -> ExternalProvider {
        ExternalProvider::new(ExternalProviderConfig {
            model_name: "catalog/jev".into(),
            provider_config: ProviderConfig::TypeSafe {
                base_url,
                model_name: Some("jev-latest".into()),
            },
            api_key: "fixture-key".into(),
            timeout_seconds: 2,
        })
    }

    #[tokio::test]
    async fn systemone_transport_rewrites_model_and_preserves_bytes() {
        let server = MockServer::start().await;
        let raw = r#"{ "model":"jev-1.13.0", "answers":{"q":{"type":"noul","noul":0.8}},
            "usage":{"input_tokens":7,"output_tokens":1}, "extra":true }"#;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .and(header("authorization", "Bearer fixture-key"))
            .and(body_partial_json(
                json!({"model":"jev-latest","state":["text",{"x":1}]}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_raw(raw, "application/json"))
            .expect(1)
            .mount(&server)
            .await;
        let provider = provider(format!("{}/v1/", server.uri()));
        assert!(provider.supports_systemone());
        assert_eq!(provider.tier(), crate::ProviderTier::NonAttested);
        let response = provider
            .systemone(request(), "client-hash".into())
            .await
            .unwrap();
        assert_eq!(response.raw_bytes, raw.as_bytes());
    }

    #[tokio::test]
    async fn systemone_transport_keeps_status_without_echoing_state_or_secrets() {
        for status in [400, 422, 429, 529] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/systemone"))
                .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                    "detail":{"input":"private-state","api_key":"fixture-key"}
                })))
                .expect(1)
                .mount(&server)
                .await;
            let error = provider(format!("{}/v1", server.uri()))
                .systemone(request(), "hash".into())
                .await
                .unwrap_err();
            assert!(
                matches!(&error,CompletionError::HttpError{status_code,is_external:true,..} if *status_code == status)
            );
            assert!(!error.to_string().contains("private-state"));
            assert!(!error.to_string().contains("fixture-key"));
        }
    }

    #[tokio::test]
    async fn systemone_near_transport_forwards_original_hash_and_requires_receipt() {
        let server = MockServer::start().await;
        let raw = r#"{ "id":"decision-tee-123", "model":"catalog/jev",
            "answers":{"q":{"type":"noul","noul":0.8}},
            "usage":{"input_tokens":7,"output_tokens":1} }"#;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .and(header("x-request-hash", "original-client-hash"))
            .and(body_partial_json(json!({"model":"catalog/jev"})))
            .respond_with(ResponseTemplate::new(200).set_body_raw(raw, "application/json"))
            .expect(1)
            .mount(&server)
            .await;
        let near =
            crate::nearai::Provider::new(crate::nearai::Config::new(server.uri(), None, Some(2)));
        let response = near
            .systemone(request(), "original-client-hash".into())
            .await
            .unwrap();
        assert_eq!(response.raw_bytes, raw.as_bytes());
        assert_eq!(
            response.provider_signature_id().unwrap(),
            "decision-tee-123"
        );
        assert_eq!(near.tier(), crate::ProviderTier::Near);
    }
}
