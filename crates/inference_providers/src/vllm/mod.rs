use crate::{models::StreamOptions, sse_parser::SSEParser, *};
use async_trait::async_trait;
use reqwest::Client;

/// Configuration for vLLM provider
#[derive(Debug, Clone)]
pub struct VLlmConfig {
    /// Base URL of the vLLM server (e.g., "http://localhost:8000")
    pub base_url: String,
    /// Optional API key for authentication
    pub api_key: Option<String>,
    /// HTTP request timeout in seconds
    pub timeout_seconds: u64,
}

impl VLlmConfig {
    pub fn new(base_url: String, api_key: Option<String>, timeout_seconds: Option<u64>) -> Self {
        Self {
            base_url,
            api_key,
            timeout_seconds: timeout_seconds.unwrap_or(30),
        }
    }
}

impl Default for VLlmConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:8000".to_string(),
            api_key: None,
            timeout_seconds: 30,
        }
    }
}

/// vLLM provider implementation
///
/// Provides inference through vLLM's OpenAI-compatible API endpoints.
/// Supports both chat completions and text completions with streaming.
pub struct VLlmProvider {
    config: VLlmConfig,
    client: Client,
}

impl VLlmProvider {
    /// Create a new vLLM provider with the given configuration
    pub fn new(config: VLlmConfig) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_seconds))
            .build()
            .expect("Failed to create HTTP client");

        Self { config, client }
    }

    /// Build HTTP request headers
    fn build_headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("Content-Type", "application/json".parse().unwrap());

        if let Some(ref api_key) = self.config.api_key {
            headers.insert(
                "Authorization",
                format!("Bearer {}", api_key).parse().unwrap(),
            );
        }

        headers
    }
}

#[async_trait]
impl InferenceProvider for VLlmProvider {
    /// Lists all available models from the vLLM server
    async fn models(&self) -> Result<ModelsResponse, ListModelsError> {
        let url = format!("{}/v1/models", self.config.base_url);
        tracing::debug!("Listing models from vLLM server, url: {}", url);

        let response = self
            .client
            .get(&url)
            .headers(self.build_headers())
            .send()
            .await
            .map_err(|e| ListModelsError::FetchError(e.to_string()))?;

        if !response.status().is_success() {
            return Err(ListModelsError::FetchError(format!(
                "HTTP {}: {}",
                response.status(),
                response.status().canonical_reason().unwrap_or("Unknown")
            )));
        }

        let models_response = response
            .json()
            .await
            .map_err(|_| ListModelsError::InvalidResponse)?;

        Ok(models_response)
    }

    /// Performs a streaming chat completion request
    async fn chat_completion_stream(
        &self,
        params: ChatCompletionParams,
    ) -> Result<StreamingResult, CompletionError> {
        let url = format!("{}/v1/chat/completions", self.config.base_url);

        // Ensure streaming and token usage are enabled
        let mut streaming_params = params;
        streaming_params.stream = Some(true);
        streaming_params.stream_options = Some(StreamOptions {
            include_usage: Some(true),
        });

        let response = self
            .client
            .post(&url)
            .headers(self.build_headers())
            .json(&streaming_params)
            .send()
            .await
            .map_err(|e| CompletionError::CompletionError(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            return Err(CompletionError::CompletionError(format!(
                "HTTP {}: {}",
                status, error_text
            )));
        }

        // Use the SSE parser to handle the stream properly
        let sse_stream = SSEParser::new(response.bytes_stream(), true);
        Ok(Box::pin(sse_stream))
    }

    /// Performs a streaming text completion request
    async fn text_completion_stream(
        &self,
        params: CompletionParams,
    ) -> Result<StreamingResult, CompletionError> {
        let url = format!("{}/v1/completions", self.config.base_url);

        // Ensure streaming and token usage are enabled
        let mut streaming_params = params;
        streaming_params.stream = Some(true);
        streaming_params.stream_options = Some(StreamOptions {
            include_usage: Some(true),
        });

        let response = self
            .client
            .post(&url)
            .headers(self.build_headers())
            .json(&streaming_params)
            .send()
            .await
            .map_err(|e| CompletionError::CompletionError(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            return Err(CompletionError::CompletionError(format!(
                "HTTP {}: {}",
                status, error_text
            )));
        }

        // Use the SSE parser to handle the stream properly
        let sse_stream = SSEParser::new(response.bytes_stream(), false);
        Ok(Box::pin(sse_stream))
    }
}
