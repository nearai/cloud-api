use crate::{models::StreamOptions, sse_parser::SSEParser, ImageGenerationError, *};
use async_trait::async_trait;
use reqwest::{header::HeaderValue, Client};
use serde::Serialize;

/// Convert any displayable error to ImageGenerationError::GenerationError
fn to_image_gen_error<E: std::fmt::Display>(e: E) -> ImageGenerationError {
    ImageGenerationError::GenerationError(e.to_string())
}

/// Encryption header keys used in params.extra for passing encryption information
mod encryption_headers {
    /// Key for signing algorithm (x-signing-algo header)
    pub const SIGNING_ALGO: &str = "x_signing_algo";
    /// Key for client public key (x-client-pub-key header)
    pub const CLIENT_PUB_KEY: &str = "x_client_pub_key";
    /// Key for model public key (x-model-pub-key header)
    /// Note: This is not forwarded to vllm-proxy (vllm-proxy doesn't accept it),
    /// but kept here for consistency with other encryption header constants
    #[allow(dead_code)]
    pub const MODEL_PUB_KEY: &str = "x_model_pub_key";
}

/// Configuration for vLLM provider
#[derive(Debug, Clone)]
pub struct VLlmConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub timeout_seconds: i64,
}

impl VLlmConfig {
    pub fn new(base_url: String, api_key: Option<String>, timeout_seconds: Option<i64>) -> Self {
        Self {
            base_url,
            api_key,
            timeout_seconds: timeout_seconds.unwrap_or(30),
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
            .connect_timeout(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .read_timeout(std::time::Duration::from_secs(
                config.timeout_seconds as u64,
            ))
            .build()
            .expect("Failed to create HTTP client");

        Self { config, client }
    }

    /// Build HTTP request headers
    fn build_headers(&self) -> Result<reqwest::header::HeaderMap, String> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));

        if let Some(ref api_key) = self.config.api_key {
            let auth_value = format!("Bearer {api_key}");
            let header_value = HeaderValue::from_str(&auth_value)
                .map_err(|e| format!("Invalid API key format: {e}"))?;
            headers.insert("Authorization", header_value);
        }

        Ok(headers)
    }

    /// Prepare encryption headers by extracting them from `extra` and forwarding as HTTP headers.
    /// Also removes encryption-related keys from `extra` to prevent them from leaking into the JSON body.
    ///
    /// NOTE: `x_model_pub_key` is intentionally not forwarded to vllm-proxy. It is consumed by the
    /// cloud API layer for provider routing and is not needed by the downstream vllm-proxy, so it
    /// is stripped from `extra` without being added as an HTTP header.
    fn prepare_encryption_headers(
        &self,
        headers: &mut reqwest::header::HeaderMap,
        extra: &mut std::collections::HashMap<String, serde_json::Value>,
    ) {
        // Extract and forward x_signing_algo as HTTP header, then remove from extra
        if let Some(algo) = extra
            .remove(encryption_headers::SIGNING_ALGO)
            .as_ref()
            .and_then(|v| v.as_str())
        {
            if let Ok(value) = HeaderValue::from_str(algo) {
                headers.insert("X-Signing-Algo", value);
            }
        }

        // Extract and forward x_client_pub_key as HTTP header, then remove from extra
        if let Some(pub_key) = extra
            .remove(encryption_headers::CLIENT_PUB_KEY)
            .as_ref()
            .and_then(|v| v.as_str())
        {
            if let Ok(value) = HeaderValue::from_str(pub_key) {
                headers.insert("X-Client-Pub-Key", value);
            }
        }

        // Remove x_model_pub_key from extra (not forwarded to vllm-proxy, used only for routing)
        extra.remove(encryption_headers::MODEL_PUB_KEY);
    }
}

#[async_trait]
impl InferenceProvider for VLlmProvider {
    async fn get_signature(
        &self,
        chat_id: &str,
        signing_algo: Option<String>,
    ) -> Result<ChatSignature, CompletionError> {
        let url = format!(
            "{}/v1/signature/{}?signing_algo={}",
            self.config.base_url,
            chat_id,
            signing_algo.unwrap_or_else(|| "ecdsa".to_string())
        );
        let headers = self
            .build_headers()
            .map_err(CompletionError::CompletionError)?;
        let response = self
            .client
            .get(&url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| CompletionError::CompletionError(e.to_string()))?;
        let signature = response
            .json()
            .await
            .map_err(|e| CompletionError::CompletionError(e.to_string()))?;
        Ok(signature)
    }

    async fn get_attestation_report(
        &self,
        model: String,
        signing_algo: Option<String>,
        nonce: Option<String>,
        signing_address: Option<String>,
    ) -> Result<serde_json::Map<String, serde_json::Value>, AttestationError> {
        #[derive(Serialize)]
        struct Query {
            model: String,
            signing_algo: Option<String>,
            nonce: Option<String>,
            signing_address: Option<String>,
        }

        let query = Query {
            model,
            signing_algo,
            nonce,
            signing_address,
        };

        // Build URL with optional query parameters
        let url = format!(
            "{}/v1/attestation/report?{}",
            self.config.base_url,
            serde_urlencoded::to_string(&query).map_err(|_| AttestationError::Unknown(
                "Failed to serialize query string".to_string()
            ))?
        );

        let headers = self.build_headers().map_err(AttestationError::FetchError)?;

        let response = self
            .client
            .get(&url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| AttestationError::FetchError(e.to_string()))?;

        // Handle 404 responses (expected when signing_address doesn't match)
        if response.status() == 404 {
            return Err(AttestationError::SigningAddressNotFound(
                query.signing_address.unwrap_or_default().to_string(),
            ));
        }

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|e| format!("Failed to read error response body: {e}"));
            return Err(AttestationError::FetchError(format!(
                "HTTP {status}: {error_text}",
            )));
        }

        let attestation_report = response
            .json()
            .await
            .map_err(|e| AttestationError::InvalidResponse(e.to_string()))?;
        Ok(attestation_report)
    }

    /// Lists all available models from the vLLM server
    async fn models(&self) -> Result<ModelsResponse, ListModelsError> {
        let url = format!("{}/v1/models", self.config.base_url);
        tracing::debug!("Listing models from vLLM server, url: {}", url);

        let headers = self.build_headers().map_err(ListModelsError::FetchError)?;
        let response = self
            .client
            .get(&url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| ListModelsError::FetchError(format!("{e:?}")))?;

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
        request_hash: String,
    ) -> Result<StreamingResult, CompletionError> {
        let url = format!("{}/v1/chat/completions", self.config.base_url);

        // Ensure streaming and token usage are enabled
        let mut streaming_params = params;
        streaming_params.stream = Some(true);
        streaming_params.stream_options = Some(StreamOptions {
            include_usage: Some(true),
            continuous_usage_stats: Some(true),
        });

        let mut headers = self
            .build_headers()
            .map_err(CompletionError::CompletionError)?;
        let request_hash_value = HeaderValue::from_str(&request_hash)
            .map_err(|e| CompletionError::CompletionError(format!("Invalid request hash: {e}")))?;
        headers.insert("X-Request-Hash", request_hash_value);

        // Prepare encryption headers
        self.prepare_encryption_headers(&mut headers, &mut streaming_params.extra);

        let response = self
            .client
            .post(&url)
            .headers(headers)
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

        // Use the SSE parser to handle the stream properly
        let sse_stream = SSEParser::new(response.bytes_stream(), true);
        Ok(Box::pin(sse_stream))
    }

    /// Performs a chat completion request
    async fn chat_completion(
        &self,
        params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        let url = format!("{}/v1/chat/completions", self.config.base_url);

        let mut non_streaming_params = params;

        let mut headers = self
            .build_headers()
            .map_err(CompletionError::CompletionError)?;
        let request_hash_value = HeaderValue::from_str(&request_hash)
            .map_err(|e| CompletionError::CompletionError(format!("Invalid request hash: {e}")))?;
        headers.insert("X-Request-Hash", request_hash_value);

        // Prepare encryption headers
        self.prepare_encryption_headers(&mut headers, &mut non_streaming_params.extra);

        let response = self
            .client
            .post(&url)
            .headers(headers)
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

        // Get the raw bytes first for exact hash verification
        let raw_bytes = response
            .bytes()
            .await
            .map_err(|e| CompletionError::CompletionError(e.to_string()))?
            .to_vec();

        // Parse the response from the raw bytes
        let chat_completion_response: ChatCompletionResponse = serde_json::from_slice(&raw_bytes)
            .map_err(|e| {
            CompletionError::CompletionError(format!("Failed to parse response: {e}"))
        })?;

        Ok(ChatCompletionResponseWithBytes {
            response: chat_completion_response,
            raw_bytes,
        })
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
            continuous_usage_stats: Some(true),
        });

        let headers = self
            .build_headers()
            .map_err(CompletionError::CompletionError)?;
        let response = self
            .client
            .post(&url)
            .headers(headers)
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

        // Use the SSE parser to handle the stream properly
        let sse_stream = SSEParser::new(response.bytes_stream(), false);
        Ok(Box::pin(sse_stream))
    }

    /// Performs an image generation request
    async fn image_generation(
        &self,
        params: ImageGenerationParams,
        request_hash: String,
    ) -> Result<ImageGenerationResponse, ImageGenerationError> {
        let url = format!("{}/v1/images/generations", self.config.base_url);

        let mut headers = self.build_headers().map_err(to_image_gen_error)?;

        headers.insert(
            "X-Request-Hash",
            HeaderValue::from_str(&request_hash).map_err(to_image_gen_error)?,
        );

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .json(&params)
            .send()
            .await
            .map_err(to_image_gen_error)?;

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

        response.json().await.map_err(to_image_gen_error)
    }
}
