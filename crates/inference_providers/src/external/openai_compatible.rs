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
    models::StreamOptions, sse_parser::SSEParser, ChatCompletionParams,
    ChatCompletionResponse, ChatCompletionResponseWithBytes, CompletionError, StreamingResult,
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
                message: error_text,
            });
        }

        // Use the SSE parser to handle the stream
        let sse_stream = SSEParser::new(response.bytes_stream(), true);
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
                message: error_text,
            });
        }

        // Get the raw bytes for hash verification
        let raw_bytes = response
            .bytes()
            .await
            .map_err(|e| CompletionError::CompletionError(e.to_string()))?
            .to_vec();

        // Parse the response
        let chat_response: ChatCompletionResponse = serde_json::from_slice(&raw_bytes)
            .map_err(|e| CompletionError::CompletionError(format!("Failed to parse response: {e}")))?;

        Ok(ChatCompletionResponseWithBytes {
            response: chat_response,
            raw_bytes,
        })
    }
}
