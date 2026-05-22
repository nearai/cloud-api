//! OpenAI-compatible backend implementation
//!
//! This backend handles providers that use OpenAI's API format, including:
//! - OpenAI (api.openai.com)
//! - Azure OpenAI
//! - Together AI
//! - Groq
//! - Fireworks AI
//! - Anyscale
//! - Any other OpenAI-compatible provider

use super::backend::{BackendConfig, ExternalBackend};
use crate::{
    models::StreamOptions, sse_parser::new_external_sse_parser, AudioTranscriptionError,
    AudioTranscriptionParams, AudioTranscriptionResponse, ChatCompletionParams,
    ChatCompletionResponse, ChatCompletionResponseWithBytes, CompletionError, ImageGenerationError,
    ImageGenerationParams, ImageGenerationResponse, ImageGenerationResponseWithBytes,
    StreamingResult,
};
use async_trait::async_trait;
use reqwest::{header::HeaderValue, Client};

/// OpenAI-compatible backend
///
/// Provides a pass-through implementation for providers that implement OpenAI's API format.
/// The main differences from vLLM are auth headers and no TEE-specific headers.
pub struct OpenAiCompatibleBackend {
    client: Client,
}

impl OpenAiCompatibleBackend {
    pub fn new() -> Self {
        let client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()
            .expect("Failed to create HTTP client");

        Self { client }
    }

    fn build_headers(&self, config: &BackendConfig) -> Result<reqwest::header::HeaderMap, String> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));

        // Authorization header
        let auth_value = format!("Bearer {}", config.api_key);
        let header_value = HeaderValue::from_str(&auth_value)
            .map_err(|e| format!("Invalid API key format: {e}"))?;
        headers.insert("Authorization", header_value);

        // OpenAI organization header (if provided)
        if let Some(org_id) = config.extra.get("organization_id") {
            if let Ok(value) = HeaderValue::from_str(org_id) {
                headers.insert("OpenAI-Organization", value);
            }
        }

        Ok(headers)
    }
}

impl Default for OpenAiCompatibleBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// OpenAI's `/v1/chat/completions` rejects the combination of function tools
/// and `reasoning_effort` for reasoning models (gpt-5.5, o1, o3, …) with:
///
/// > "Function tools with reasoning_effort are not supported for gpt-5.5 in
/// > /v1/chat/completions. Please use /v1/responses instead."
///
/// The proper fix is to route tools+reasoning_effort requests through OpenAI's
/// Responses API; that is a larger effort. As an interim, when we detect this
/// combination on an OpenAI upstream we drop `reasoning_effort` from the
/// request so the call succeeds. The model still reasons by default — only
/// the user's effort selector is lost. We log a warn so the degradation is
/// auditable.
///
/// Scoped to OpenAI proper (`api.openai.com`, `api.openai.azure.com` family)
/// because we cannot assume the same restriction applies to other
/// openai-compatible providers (Together, Groq, Fireworks, etc.).
fn strip_reasoning_effort_if_unsupported(
    params: &mut ChatCompletionParams,
    base_url: &str,
    model: &str,
) {
    if params.tools.as_ref().is_none_or(|t| t.is_empty()) {
        return;
    }
    if !base_url.contains("api.openai.com") && !base_url.contains(".openai.azure.com") {
        return;
    }
    if params.extra.remove("reasoning_effort").is_some() {
        tracing::warn!(
            model = %model,
            base_url = %base_url,
            "Stripped `reasoning_effort` from OpenAI request: combination with `tools` is rejected by /v1/chat/completions"
        );
    }
}

#[async_trait]
impl ExternalBackend for OpenAiCompatibleBackend {
    fn backend_type(&self) -> &'static str {
        "openai_compatible"
    }

    async fn chat_completion_stream(
        &self,
        config: &BackendConfig,
        model: &str,
        params: ChatCompletionParams,
    ) -> Result<StreamingResult, CompletionError> {
        let url = format!("{}/chat/completions", config.base_url);

        // Ensure streaming and usage are enabled
        let mut streaming_params = params;
        streaming_params.model = model.to_string();
        streaming_params.stream = Some(true);
        streaming_params.stream_options = Some(StreamOptions {
            include_usage: Some(true),
            continuous_usage_stats: None, // Not all providers support this
        });

        // Convert max_tokens to max_completion_tokens for newer OpenAI models
        // Some models (e.g., gpt-5.2) require max_completion_tokens instead of max_tokens
        // If max_completion_tokens is not set but max_tokens is, convert it
        // Always clear max_tokens to avoid sending unsupported parameter to newer models
        if streaming_params.max_completion_tokens.is_none() && streaming_params.max_tokens.is_some()
        {
            streaming_params.max_completion_tokens = streaming_params.max_tokens;
        }
        streaming_params.max_tokens = None;

        strip_reasoning_effort_if_unsupported(&mut streaming_params, &config.base_url, model);

        let headers = self
            .build_headers(config)
            .map_err(CompletionError::CompletionError)?;

        let client = self.client.clone();
        let timeout = std::time::Duration::from_secs(config.timeout_seconds as u64);

        let response = client
            .post(&url)
            .headers(headers)
            .timeout(timeout)
            .json(&streaming_params)
            .send()
            .await
            .map_err(|e| CompletionError::CompletionError(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let status_code = status.as_u16();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|e| format!("Failed to read error response body: {e}"));
            return Err(CompletionError::HttpError {
                status_code,
                message: crate::extract_error_message(&error_text),
                is_external: true,
            });
        }

        // Use the SSE parser to handle the stream. External upstream → any
        // in-stream `{"error":{...}}` frame surfaces as
        // `HttpError { is_external: true }` so `map_provider_error` applies
        // the third-party taxonomy (e.g. 404 → `ProviderError 502`).
        let sse_stream = new_external_sse_parser(response.bytes_stream(), true);
        Ok(Box::pin(sse_stream))
    }

    async fn chat_completion(
        &self,
        config: &BackendConfig,
        model: &str,
        params: ChatCompletionParams,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        let url = format!("{}/chat/completions", config.base_url);

        // Ensure non-streaming
        let mut non_streaming_params = params;
        non_streaming_params.model = model.to_string();
        non_streaming_params.stream = Some(false);

        // Convert max_tokens to max_completion_tokens for newer OpenAI models
        // Some models (e.g., gpt-5.2) require max_completion_tokens instead of max_tokens
        // If max_completion_tokens is not set but max_tokens is, convert it
        // Always clear max_tokens to avoid sending unsupported parameter to newer models
        if non_streaming_params.max_completion_tokens.is_none()
            && non_streaming_params.max_tokens.is_some()
        {
            non_streaming_params.max_completion_tokens = non_streaming_params.max_tokens;
        }
        non_streaming_params.max_tokens = None;

        strip_reasoning_effort_if_unsupported(&mut non_streaming_params, &config.base_url, model);

        let headers = self
            .build_headers(config)
            .map_err(CompletionError::CompletionError)?;

        let client = self.client.clone();
        let timeout = std::time::Duration::from_secs(config.timeout_seconds as u64);

        let response = client
            .post(&url)
            .headers(headers)
            .timeout(timeout)
            .json(&non_streaming_params)
            .send()
            .await
            .map_err(|e| CompletionError::CompletionError(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let status_code = status.as_u16();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|e| format!("Failed to read error response body: {e}"));
            return Err(CompletionError::HttpError {
                status_code,
                message: crate::extract_error_message(&error_text),
                is_external: true,
            });
        }

        let body_bytes = response
            .bytes()
            .await
            .map_err(|e| CompletionError::CompletionError(e.to_string()))?
            .to_vec();

        let parsed: ChatCompletionResponse = serde_json::from_slice(&body_bytes).map_err(|e| {
            CompletionError::CompletionError(format!("Failed to parse response: {e}"))
        })?;

        // Serialize our normalized response. We intentionally overwrite fields
        // like `usage` (and any future cost-related fields derived from it) instead of passing
        // through native payload directly, to avoid inconsistencies between what we
        // bill on and what we expose on the wire.
        let raw_bytes = serde_json::to_vec(&parsed).map_err(|e| {
            CompletionError::CompletionError(format!("Failed to serialize response: {e}"))
        })?;

        Ok(ChatCompletionResponseWithBytes {
            response: parsed,
            raw_bytes,
        })
    }

    async fn image_generation(
        &self,
        config: &BackendConfig,
        model: &str,
        params: ImageGenerationParams,
    ) -> Result<ImageGenerationResponseWithBytes, ImageGenerationError> {
        let url = format!("{}/images/generations", config.base_url);

        // Override model in params
        let mut generation_params = params;
        generation_params.model = model.to_string();

        let headers = self
            .build_headers(config)
            .map_err(ImageGenerationError::GenerationError)?;

        let timeout = std::time::Duration::from_secs(config.timeout_seconds as u64);

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .timeout(timeout)
            .json(&generation_params)
            .send()
            .await
            .map_err(|e| ImageGenerationError::GenerationError(e.to_string()))?;

        if !response.status().is_success() {
            let status_code = response.status().as_u16();
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(ImageGenerationError::HttpError {
                status_code,
                message,
            });
        }

        // Get raw bytes first for exact hash verification
        let raw_bytes = response
            .bytes()
            .await
            .map_err(|e| ImageGenerationError::GenerationError(e.to_string()))?
            .to_vec();

        // Parse the response from the raw bytes
        let image_response: ImageGenerationResponse =
            serde_json::from_slice(&raw_bytes).map_err(|e| {
                ImageGenerationError::GenerationError(format!("Failed to parse response: {e}"))
            })?;

        Ok(ImageGenerationResponseWithBytes {
            response: image_response,
            raw_bytes,
        })
    }

    async fn audio_transcription(
        &self,
        config: &BackendConfig,
        model: &str,
        params: AudioTranscriptionParams,
    ) -> Result<AudioTranscriptionResponse, AudioTranscriptionError> {
        let url = format!("{}/audio/transcriptions", config.base_url);

        // Detect content type
        let content_type = crate::models::detect_audio_content_type(&params.filename);

        let file_part = reqwest::multipart::Part::bytes(params.file_bytes)
            .file_name(params.filename.clone())
            .mime_str(&content_type)
            .map_err(|e| AudioTranscriptionError::TranscriptionError(e.to_string()))?;

        let mut form = reqwest::multipart::Form::new()
            .part("file", file_part)
            .text("model", model.to_string());

        if let Some(language) = params.language {
            form = form.text("language", language);
        }

        if let Some(response_format) = params.response_format {
            form = form.text("response_format", response_format);
        }

        if let Some(temperature) = params.temperature {
            form = form.text("temperature", temperature.to_string());
        }

        let mut headers = self
            .build_headers(config)
            .map_err(AudioTranscriptionError::TranscriptionError)?;

        // Remove Content-Type header if set - reqwest will set it automatically for multipart
        headers.remove("Content-Type");

        // Add OpenAI-Organization header if provided
        if let Some(org_id) = config.extra.get("organization_id") {
            if let Ok(value) = HeaderValue::from_str(org_id) {
                headers.insert("OpenAI-Organization", value);
            }
        }

        let timeout = std::time::Duration::from_secs(config.timeout_seconds as u64);

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .multipart(form)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| AudioTranscriptionError::TranscriptionError(e.to_string()))?;

        if !response.status().is_success() {
            let status_code = response.status().as_u16();
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(AudioTranscriptionError::HttpError {
                status_code,
                message,
            });
        }

        let transcription_response: AudioTranscriptionResponse = response
            .json()
            .await
            .map_err(|e| AudioTranscriptionError::TranscriptionError(e.to_string()))?;

        Ok(transcription_response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // ==================== Header Building Tests ====================

    #[test]
    fn test_build_headers_basic() {
        let backend = OpenAiCompatibleBackend::new();
        let config = BackendConfig {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: "sk-test-key-123".to_string(),
            timeout_seconds: 30,
            extra: HashMap::new(),
        };

        let headers = backend.build_headers(&config).unwrap();

        assert_eq!(
            headers.get("Authorization").unwrap().to_str().unwrap(),
            "Bearer sk-test-key-123"
        );
        assert_eq!(
            headers.get("Content-Type").unwrap().to_str().unwrap(),
            "application/json"
        );
    }

    #[test]
    fn test_build_headers_with_organization() {
        let backend = OpenAiCompatibleBackend::new();
        let mut extra = HashMap::new();
        extra.insert("organization_id".to_string(), "org-abc123".to_string());

        let config = BackendConfig {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: "sk-test-key".to_string(),
            timeout_seconds: 30,
            extra,
        };

        let headers = backend.build_headers(&config).unwrap();

        assert_eq!(
            headers
                .get("OpenAI-Organization")
                .unwrap()
                .to_str()
                .unwrap(),
            "org-abc123"
        );
    }

    #[test]
    fn test_build_headers_no_organization() {
        let backend = OpenAiCompatibleBackend::new();
        let config = BackendConfig {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: "sk-test-key".to_string(),
            timeout_seconds: 30,
            extra: HashMap::new(),
        };

        let headers = backend.build_headers(&config).unwrap();

        assert!(headers.get("OpenAI-Organization").is_none());
    }

    // ==================== URL Building Tests ====================

    #[test]
    fn test_chat_completion_url() {
        let base_url = "https://api.openai.com/v1";
        let url = format!("{}/chat/completions", base_url);

        assert_eq!(url, "https://api.openai.com/v1/chat/completions");
    }

    #[test]
    fn test_chat_completion_url_different_providers() {
        let providers = vec![
            (
                "https://api.openai.com/v1",
                "https://api.openai.com/v1/chat/completions",
            ),
            (
                "https://api.together.xyz/v1",
                "https://api.together.xyz/v1/chat/completions",
            ),
            (
                "https://api.groq.com/openai/v1",
                "https://api.groq.com/openai/v1/chat/completions",
            ),
            (
                "https://api.fireworks.ai/inference/v1",
                "https://api.fireworks.ai/inference/v1/chat/completions",
            ),
        ];

        for (base_url, expected) in providers {
            let url = format!("{}/chat/completions", base_url);
            assert_eq!(url, expected);
        }
    }

    // ==================== Backend Type Tests ====================

    #[test]
    fn test_backend_type() {
        let backend = OpenAiCompatibleBackend::new();
        assert_eq!(backend.backend_type(), "openai_compatible");
    }

    // ==================== Default Implementation Tests ====================

    #[test]
    fn test_default_implementation() {
        let backend = OpenAiCompatibleBackend::default();
        assert_eq!(backend.backend_type(), "openai_compatible");
    }

    // ==================== Stream Options Tests ====================

    #[test]
    fn test_stream_options_serialization() {
        let options = StreamOptions {
            include_usage: Some(true),
            continuous_usage_stats: None,
        };

        let json = serde_json::to_string(&options).unwrap();
        assert!(json.contains("\"include_usage\":true"));
    }

    #[test]
    fn test_stream_options_with_all_fields() {
        let options = StreamOptions {
            include_usage: Some(true),
            continuous_usage_stats: Some(true),
        };

        let json = serde_json::to_string(&options).unwrap();
        assert!(json.contains("\"include_usage\":true"));
        assert!(json.contains("\"continuous_usage_stats\":true"));
    }

    // ==================== Image Generation URL Tests ====================

    #[test]
    fn test_image_generation_url() {
        let base_url = "https://api.openai.com/v1";
        let url = format!("{}/images/generations", base_url);

        assert_eq!(url, "https://api.openai.com/v1/images/generations");
    }

    #[test]
    fn test_image_generation_url_different_providers() {
        let providers = vec![
            (
                "https://api.openai.com/v1",
                "https://api.openai.com/v1/images/generations",
            ),
            (
                "https://api.together.xyz/v1",
                "https://api.together.xyz/v1/images/generations",
            ),
            (
                "https://api.fireworks.ai/inference/v1",
                "https://api.fireworks.ai/inference/v1/images/generations",
            ),
        ];

        for (base_url, expected) in providers {
            let url = format!("{}/images/generations", base_url);
            assert_eq!(url, expected);
        }
    }

    #[test]
    fn test_image_generation_params_serialization() {
        let params = ImageGenerationParams {
            model: "dall-e-3".to_string(),
            prompt: "A beautiful sunset".to_string(),
            n: Some(1),
            size: Some("1024x1024".to_string()),
            response_format: Some("b64_json".to_string()),
            quality: Some("hd".to_string()),
            style: Some("vivid".to_string()),
            extra: std::collections::HashMap::new(),
        };

        let json = serde_json::to_string(&params).unwrap();
        assert!(json.contains("\"model\":\"dall-e-3\""));
        assert!(json.contains("\"prompt\":\"A beautiful sunset\""));
        assert!(json.contains("\"n\":1"));
        assert!(json.contains("\"size\":\"1024x1024\""));
        assert!(json.contains("\"quality\":\"hd\""));
        assert!(json.contains("\"style\":\"vivid\""));
    }

    // ============ reasoning_effort + tools mitigation tests ============

    fn make_chat_params(
        tools: Option<Vec<crate::models::ToolDefinition>>,
        reasoning_effort: Option<&str>,
    ) -> ChatCompletionParams {
        let mut extra: HashMap<String, serde_json::Value> = HashMap::new();
        if let Some(re) = reasoning_effort {
            extra.insert(
                "reasoning_effort".to_string(),
                serde_json::Value::String(re.to_string()),
            );
        }
        ChatCompletionParams {
            model: "gpt-5.5".to_string(),
            messages: vec![],
            max_completion_tokens: None,
            max_tokens: None,
            temperature: None,
            top_p: None,
            n: None,
            stream: None,
            stop: None,
            frequency_penalty: None,
            presence_penalty: None,
            logit_bias: None,
            logprobs: None,
            top_logprobs: None,
            user: None,
            seed: None,
            tools,
            tool_choice: None,
            parallel_tool_calls: None,
            metadata: None,
            store: None,
            stream_options: None,
            modalities: None,
            extra,
        }
    }

    fn bash_tool() -> Vec<crate::models::ToolDefinition> {
        vec![crate::models::ToolDefinition {
            type_: "function".to_string(),
            function: crate::models::FunctionDefinition {
                name: "bash".to_string(),
                description: Some("run bash".to_string()),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            },
        }]
    }

    /// Reproduces the exact failure mode: OpenAI gpt-5.5 with tools +
    /// `reasoning_effort` is rejected by `/v1/chat/completions`. The strip
    /// must drop the field so the request succeeds.
    #[test]
    fn test_strip_reasoning_effort_openai_with_tools() {
        let mut params = make_chat_params(Some(bash_tool()), Some("low"));
        strip_reasoning_effort_if_unsupported(&mut params, "https://api.openai.com/v1", "gpt-5.5");
        assert!(!params.extra.contains_key("reasoning_effort"));
    }

    /// `reasoning_effort` on OpenAI without tools is fine — model still
    /// honors the effort selector. Don't strip.
    #[test]
    fn test_strip_reasoning_effort_openai_no_tools_keeps_field() {
        let mut params = make_chat_params(None, Some("low"));
        strip_reasoning_effort_if_unsupported(&mut params, "https://api.openai.com/v1", "gpt-5.5");
        assert_eq!(
            params.extra.get("reasoning_effort"),
            Some(&serde_json::Value::String("low".to_string()))
        );
    }

    /// Empty tools array is treated as no tools — keep `reasoning_effort`.
    /// Guards against a client that always sends `tools: []`.
    #[test]
    fn test_strip_reasoning_effort_empty_tools_keeps_field() {
        let mut params = make_chat_params(Some(vec![]), Some("low"));
        strip_reasoning_effort_if_unsupported(&mut params, "https://api.openai.com/v1", "gpt-5.5");
        assert_eq!(
            params.extra.get("reasoning_effort"),
            Some(&serde_json::Value::String("low".to_string()))
        );
    }

    /// Non-OpenAI providers (Together, Groq, Fireworks, Anyscale, …) may
    /// accept the combo. Scope of the workaround is OpenAI proper only.
    #[test]
    fn test_strip_reasoning_effort_non_openai_provider_keeps_field() {
        for base_url in &[
            "https://api.together.xyz/v1",
            "https://api.groq.com/openai/v1",
            "https://api.fireworks.ai/inference/v1",
            "https://api.anyscale.com/v1",
        ] {
            let mut params = make_chat_params(Some(bash_tool()), Some("low"));
            strip_reasoning_effort_if_unsupported(&mut params, base_url, "gpt-5.5");
            assert_eq!(
                params.extra.get("reasoning_effort"),
                Some(&serde_json::Value::String("low".to_string())),
                "should not strip for non-OpenAI provider {}",
                base_url
            );
        }
    }

    /// Azure OpenAI hosts the same models with the same restriction.
    #[test]
    fn test_strip_reasoning_effort_azure_openai_strips() {
        let mut params = make_chat_params(Some(bash_tool()), Some("medium"));
        strip_reasoning_effort_if_unsupported(
            &mut params,
            "https://my-resource.openai.azure.com",
            "gpt-5.5",
        );
        assert!(!params.extra.contains_key("reasoning_effort"));
    }

    /// No `reasoning_effort` in the request → no-op, no log noise.
    #[test]
    fn test_strip_reasoning_effort_no_field_present() {
        let mut params = make_chat_params(Some(bash_tool()), None);
        strip_reasoning_effort_if_unsupported(&mut params, "https://api.openai.com/v1", "gpt-5.5");
        assert!(!params.extra.contains_key("reasoning_effort"));
    }

    /// Other extras (e.g. `verbosity`, `parallel_tool_calls`) must survive
    /// when `reasoning_effort` is stripped.
    #[test]
    fn test_strip_reasoning_effort_preserves_other_extras() {
        let mut params = make_chat_params(Some(bash_tool()), Some("low"));
        params.extra.insert(
            "verbosity".to_string(),
            serde_json::Value::String("high".to_string()),
        );
        strip_reasoning_effort_if_unsupported(&mut params, "https://api.openai.com/v1", "gpt-5.5");
        assert!(!params.extra.contains_key("reasoning_effort"));
        assert_eq!(
            params.extra.get("verbosity"),
            Some(&serde_json::Value::String("high".to_string()))
        );
    }
}
