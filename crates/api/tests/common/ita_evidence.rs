use std::sync::Arc;

use api::{build_app_with_config, init_auth_services};
use async_trait::async_trait;
use base64::Engine;

use super::{assert_mock_user_in_db, setup_test_infrastructure, E2E_QWEN_MODEL_NAME};

pub async fn setup_test_server_with_config_and_ita_model_evidence<F>(
    mutate: F,
) -> axum_test::TestServer
where
    F: FnOnce(&mut config::ApiConfig),
{
    let mut infra = setup_test_infrastructure().await;
    mutate(&mut infra.config);

    assert_mock_user_in_db(&infra.database).await;
    let auth_components = init_auth_services(infra.database.clone(), &infra.config);
    let (inference_provider_pool, mock_provider) =
        api::init_inference_providers_with_mocks(&infra.config).await;
    let ita_provider: Arc<dyn inference_providers::InferenceProvider + Send + Sync> =
        Arc::new(ItaCompatibleModelEvidenceProvider::new(mock_provider));
    inference_provider_pool
        .register_provider(E2E_QWEN_MODEL_NAME.to_string(), ita_provider)
        .await;

    let metrics_service = Arc::new(services::metrics::MockMetricsService);
    let domain_services = api::init_domain_services_with_pool(
        infra.database.clone(),
        &infra.config,
        auth_components.organization_service.clone(),
        inference_provider_pool,
        metrics_service,
    )
    .await;
    let app = build_app_with_config(
        infra.database.clone(),
        auth_components,
        domain_services,
        Arc::new(infra.config),
    );
    axum_test::TestServer::new(app)
}

struct ItaCompatibleModelEvidenceProvider {
    inner: Arc<inference_providers::mock::MockProvider>,
}

impl ItaCompatibleModelEvidenceProvider {
    fn new(inner: Arc<inference_providers::mock::MockProvider>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl inference_providers::InferenceProvider for ItaCompatibleModelEvidenceProvider {
    async fn models(
        &self,
    ) -> Result<
        inference_providers::models::ModelsResponse,
        inference_providers::models::ListModelsError,
    > {
        inference_providers::InferenceProvider::models(&*self.inner).await
    }

    async fn chat_completion_stream(
        &self,
        params: inference_providers::ChatCompletionParams,
        request_hash: String,
    ) -> Result<inference_providers::StreamingResult, inference_providers::CompletionError> {
        inference_providers::InferenceProvider::chat_completion_stream(
            &*self.inner,
            params,
            request_hash,
        )
        .await
    }

    async fn chat_completion(
        &self,
        params: inference_providers::ChatCompletionParams,
        request_hash: String,
    ) -> Result<
        inference_providers::ChatCompletionResponseWithBytes,
        inference_providers::CompletionError,
    > {
        inference_providers::InferenceProvider::chat_completion(&*self.inner, params, request_hash)
            .await
    }

    async fn text_completion_stream(
        &self,
        params: inference_providers::CompletionParams,
    ) -> Result<inference_providers::StreamingResult, inference_providers::CompletionError> {
        inference_providers::InferenceProvider::text_completion_stream(&*self.inner, params).await
    }

    async fn image_generation(
        &self,
        params: inference_providers::ImageGenerationParams,
        request_hash: String,
    ) -> Result<
        inference_providers::ImageGenerationResponseWithBytes,
        inference_providers::ImageGenerationError,
    > {
        inference_providers::InferenceProvider::image_generation(&*self.inner, params, request_hash)
            .await
    }

    async fn image_edit(
        &self,
        params: Arc<inference_providers::ImageEditParams>,
        request_hash: String,
    ) -> Result<inference_providers::ImageEditResponseWithBytes, inference_providers::ImageEditError>
    {
        inference_providers::InferenceProvider::image_edit(&*self.inner, params, request_hash).await
    }

    async fn score(
        &self,
        params: inference_providers::ScoreParams,
        request_hash: String,
    ) -> Result<inference_providers::ScoreResponse, inference_providers::ScoreError> {
        inference_providers::InferenceProvider::score(&*self.inner, params, request_hash).await
    }

    async fn rerank(
        &self,
        params: inference_providers::RerankParams,
    ) -> Result<inference_providers::RerankResponse, inference_providers::RerankError> {
        inference_providers::InferenceProvider::rerank(&*self.inner, params).await
    }

    async fn embeddings_raw(
        &self,
        body: bytes::Bytes,
        extra: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bytes::Bytes, inference_providers::EmbeddingError> {
        inference_providers::InferenceProvider::embeddings_raw(&*self.inner, body, extra).await
    }

    async fn privacy_classify_raw(
        &self,
        body: bytes::Bytes,
        extra: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bytes::Bytes, inference_providers::PrivacyClassifyError> {
        inference_providers::InferenceProvider::privacy_classify_raw(&*self.inner, body, extra)
            .await
    }

    async fn get_signature(
        &self,
        chat_id: &str,
        signing_algo: Option<String>,
    ) -> Result<inference_providers::ChatSignature, inference_providers::CompletionError> {
        inference_providers::InferenceProvider::get_signature(&*self.inner, chat_id, signing_algo)
            .await
    }

    fn pin_chat_connection(&self, request_hash: &str, chat_id: &str) {
        inference_providers::InferenceProvider::pin_chat_connection(
            &*self.inner,
            request_hash,
            chat_id,
        );
    }

    fn supports_chat_signatures(&self) -> bool {
        inference_providers::InferenceProvider::supports_chat_signatures(&*self.inner)
    }

    fn tier(&self) -> inference_providers::ProviderTier {
        inference_providers::InferenceProvider::tier(&*self.inner)
    }

    fn supports_streaming(&self) -> bool {
        inference_providers::InferenceProvider::supports_streaming(&*self.inner)
    }

    fn supports_client_e2ee(&self) -> bool {
        inference_providers::InferenceProvider::supports_client_e2ee(&*self.inner)
    }

    fn unpin_chat_connection(&self, chat_id: &str) {
        inference_providers::InferenceProvider::unpin_chat_connection(&*self.inner, chat_id);
    }

    fn set_backend_count(&self, count: usize) {
        inference_providers::InferenceProvider::set_backend_count(&*self.inner, count);
    }

    async fn get_attestation_report(
        &self,
        model: String,
        signing_algo: Option<String>,
        nonce: Option<String>,
        signing_address: Option<String>,
        include_tls_fingerprint: bool,
    ) -> Result<
        serde_json::Map<String, serde_json::Value>,
        inference_providers::models::AttestationError,
    > {
        let mut report = inference_providers::InferenceProvider::get_attestation_report(
            &*self.inner,
            model,
            signing_algo,
            nonce.clone(),
            signing_address,
            include_tls_fingerprint,
        )
        .await?;
        if let Some(gpu_nonce) = nonce {
            report.insert(
                "ita_nvgpu".to_string(),
                serde_json::json!({
                    "gpu_nonce": gpu_nonce,
                    "arch": "H100",
                    "evidence_list": [{
                        "certificate": base64::engine::general_purpose::STANDARD.encode(b"test-gpu-cert"),
                        "evidence": base64::engine::general_purpose::STANDARD.encode(b"test-gpu-evidence"),
                        "firmware_version": "test-fw"
                    }]
                }),
            );
        }
        Ok(report)
    }

    async fn audio_transcription(
        &self,
        params: inference_providers::AudioTranscriptionParams,
        request_hash: String,
    ) -> Result<
        inference_providers::AudioTranscriptionResponse,
        inference_providers::AudioTranscriptionError,
    > {
        inference_providers::InferenceProvider::audio_transcription(
            &*self.inner,
            params,
            request_hash,
        )
        .await
    }
}
