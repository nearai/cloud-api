//! Internal routing and tracing keys must never leave the process on any
//! external-provider operation that carries an `extra` map.

use super::*;
use crate::attested::nearai::{encryption_headers as eh, tracing_headers as th};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const INTERNAL_KEYS: [&str; 4] = [
    th::REQUEST_ID,
    th::ORG_ID,
    th::WORKSPACE_ID,
    eh::MODEL_PUB_KEY,
];

fn extra_with_internal_keys() -> HashMap<String, Value> {
    let mut extra: HashMap<String, Value> = INTERNAL_KEYS
        .iter()
        .map(|key| (key.to_string(), json!("synthetic")))
        .collect();
    extra.insert("user_field".into(), json!("kept"));
    extra
}

fn assert_stripped(extra: &HashMap<String, Value>, operation: &str) {
    for key in INTERNAL_KEYS {
        assert!(
            !extra.contains_key(key),
            "{operation}: internal key {key} reached the backend"
        );
    }
    assert_eq!(
        extra.get("user_field"),
        Some(&json!("kept")),
        "{operation}: non-internal fields must be preserved"
    );
}

#[tokio::test]
async fn image_generation_body_omits_internal_keys_on_the_wire() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "created": 0,
            "data": [{"b64_json": "AA=="}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let provider = ExternalProvider::new(ExternalProviderConfig {
        model_name: "test-image-model".into(),
        provider_config: serde_json::from_value(json!({
            "backend": "openai_compatible",
            "base_url": server.uri(),
        }))
        .unwrap(),
        api_key: "synthetic-test-key".into(),
        timeout_seconds: 5,
    });
    let params = ImageGenerationParams {
        model: "test-image-model".into(),
        prompt: "synthetic".into(),
        n: None,
        size: None,
        response_format: None,
        quality: None,
        style: None,
        extra: extra_with_internal_keys(),
    };

    let context = crate::tracing_context::TracingContext {
        request_id: uuid::Uuid::new_v4(),
        org_id: uuid::Uuid::new_v4(),
        workspace_id: uuid::Uuid::new_v4(),
    };
    let _ = crate::tracing_context::with_tracing_context(
        context,
        provider.image_generation(params, String::new()),
    )
    .await;

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: HashMap<String, Value> = serde_json::from_slice(&requests[0].body).unwrap();
    assert_stripped(&body, "image_generation");
    // Made inside an authenticated request's scope, the call still carries
    // no tenant ids: only self-hosted backends receive them.
    for name in ["x-request-id", "x-org-id", "x-workspace-id"] {
        assert!(
            requests[0].headers.get(name).is_none(),
            "{name} must never reach an external provider"
        );
    }
}

/// Records the `extra` map each operation hands to the backend.
#[derive(Default)]
struct CapturingBackend {
    seen: Mutex<Vec<(&'static str, HashMap<String, Value>)>>,
}

impl CapturingBackend {
    fn record(&self, operation: &'static str, extra: HashMap<String, Value>) {
        self.seen.lock().unwrap().push((operation, extra));
    }
}

#[async_trait]
impl ExternalBackend for Arc<CapturingBackend> {
    fn backend_type(&self) -> &'static str {
        "capturing"
    }

    async fn chat_completion_stream(
        &self,
        _config: &BackendConfig,
        _model: &str,
        _params: ChatCompletionParams,
    ) -> Result<StreamingResult, CompletionError> {
        Err(CompletionError::CompletionError("unused".into()))
    }

    async fn chat_completion(
        &self,
        _config: &BackendConfig,
        _model: &str,
        _params: ChatCompletionParams,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        Err(CompletionError::CompletionError("unused".into()))
    }

    async fn score(
        &self,
        _config: &BackendConfig,
        _model: &str,
        params: ScoreParams,
    ) -> Result<ScoreResponse, ScoreError> {
        self.record("score", params.extra);
        Err(ScoreError::GenerationError("captured".into()))
    }

    async fn rerank(
        &self,
        _config: &BackendConfig,
        _model: &str,
        params: RerankParams,
    ) -> Result<RerankResponse, RerankError> {
        self.record("rerank", params.extra);
        Err(RerankError::GenerationError("captured".into()))
    }

    async fn embeddings_raw(
        &self,
        _config: &BackendConfig,
        _body: bytes::Bytes,
        extra: HashMap<String, Value>,
    ) -> Result<bytes::Bytes, EmbeddingError> {
        self.record("embeddings", extra);
        Err(EmbeddingError::RequestFailed("captured".into()))
    }

    async fn audio_transcription(
        &self,
        _config: &BackendConfig,
        _model: &str,
        params: AudioTranscriptionParams,
    ) -> Result<AudioTranscriptionResponse, AudioTranscriptionError> {
        self.record("audio_transcription", params.extra);
        Err(AudioTranscriptionError::TranscriptionError(
            "captured".into(),
        ))
    }

    async fn privacy_classify_raw(
        &self,
        _config: &BackendConfig,
        _body: bytes::Bytes,
        extra: HashMap<String, Value>,
    ) -> Result<bytes::Bytes, PrivacyClassifyError> {
        self.record("privacy_classify", extra);
        Err(PrivacyClassifyError::RequestFailed("captured".into()))
    }
}

#[tokio::test]
async fn json_operations_hand_backends_extra_without_internal_keys() {
    let backend = Arc::new(CapturingBackend::default());
    let provider = ExternalProvider {
        backend: Arc::new(backend.clone()),
        config: BackendConfig::default(),
        model_name: "test-model".into(),
        enforced_request_body: HashMap::new(),
    };

    let _ = provider
        .score(
            ScoreParams {
                model: "test-model".into(),
                text_1: "a".into(),
                text_2: "b".into(),
                extra: extra_with_internal_keys(),
            },
            String::new(),
        )
        .await;
    let _ = provider
        .rerank(RerankParams {
            model: "test-model".into(),
            query: "q".into(),
            documents: vec!["d".into()],
            extra: extra_with_internal_keys(),
        })
        .await;
    let _ = provider
        .embeddings_raw(bytes::Bytes::from_static(b"{}"), extra_with_internal_keys())
        .await;
    let _ = provider
        .privacy_classify_raw(bytes::Bytes::from_static(b"{}"), extra_with_internal_keys())
        .await;

    let _ = provider
        .audio_transcription(
            AudioTranscriptionParams {
                model: "test-model".into(),
                file_bytes: vec![1, 2, 3],
                filename: "synthetic.wav".into(),
                language: None,
                response_format: None,
                temperature: None,
                timestamp_granularities: None,
                extra: extra_with_internal_keys(),
            },
            String::new(),
        )
        .await;

    let seen = backend.seen.lock().unwrap();
    let operations: Vec<_> = seen.iter().map(|(operation, _)| *operation).collect();
    assert_eq!(
        operations,
        [
            "score",
            "rerank",
            "embeddings",
            "privacy_classify",
            "audio_transcription"
        ]
    );
    for (operation, extra) in seen.iter() {
        assert_stripped(extra, operation);
    }
}
