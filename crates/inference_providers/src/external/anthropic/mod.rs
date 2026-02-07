//! Anthropic backend implementation
//!
//! This backend handles HTTP communication with Anthropic's Messages API.
//! Format conversion is handled by the `anthropic_converter` module.

mod converter;

use super::backend::{BackendConfig, ExternalBackend};
use crate::{
    BufferedSSEParser, ChatCompletionParams, ChatCompletionResponse, ChatCompletionResponseChoice,
    ChatCompletionResponseWithBytes, ChatResponseMessage, CompletionError, MessageRole,
    StreamingResult, TokenUsage,
};
use async_trait::async_trait;
use bytes::Bytes;
use converter::{
    convert_messages, convert_tool_choice, convert_tools, extract_response_content,
    map_finish_reason_string, AnthropicEventParser, AnthropicParserState, AnthropicRequest,
    AnthropicResponse,
};
use futures_util::Stream;
use reqwest::{header::HeaderValue, Client};

const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic backend - handles HTTP communication with Anthropic's API
pub struct AnthropicBackend {
    client: Client,
}

impl AnthropicBackend {
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

        let header_value = HeaderValue::from_str(&config.api_key)
            .map_err(|e| format!("Invalid API key format: {e}"))?;
        headers.insert("x-api-key", header_value);

        let version = config
            .extra
            .get("version")
            .map(|s| s.as_str())
            .unwrap_or(DEFAULT_ANTHROPIC_VERSION);
        if let Ok(value) = HeaderValue::from_str(version) {
            headers.insert("anthropic-version", value);
        }

        Ok(headers)
    }

    fn build_request(
        &self,
        model: &str,
        params: &ChatCompletionParams,
        stream: bool,
    ) -> AnthropicRequest {
        let (system, messages) = convert_messages(&params.messages);
        let max_tokens = params
            .max_completion_tokens
            .or(params.max_tokens)
            .unwrap_or(4096);

        // Convert tools if provided
        let tools = params.tools.as_ref().map(|t| convert_tools(t));
        let tool_choice = params.tool_choice.as_ref().and_then(convert_tool_choice);

        // Anthropic doesn't allow both temperature and top_p - prefer temperature if both are set.
        // Also clamp temperature to Anthropic's valid range [0.0, 1.0] (OpenAI allows up to 2.0).
        let (temperature, top_p) = if let Some(temp) = params.temperature {
            (Some(temp.clamp(0.0, 1.0)), None)
        } else {
            (None, params.top_p)
        };

        AnthropicRequest {
            model: model.to_string(),
            messages,
            max_tokens,
            system,
            temperature,
            top_p,
            stop_sequences: params.stop.clone(),
            tools,
            tool_choice,
            stream,
        }
    }
}

impl Default for AnthropicBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// SSE parser type alias for Anthropic
pub type AnthropicSSEParser<S> = BufferedSSEParser<S, AnthropicEventParser>;

/// Create a new Anthropic SSE parser
pub fn new_anthropic_sse_parser<S>(stream: S, model: String) -> AnthropicSSEParser<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    BufferedSSEParser::new(stream, AnthropicParserState::new(model))
}

#[async_trait]
impl ExternalBackend for AnthropicBackend {
    fn backend_type(&self) -> &'static str {
        "anthropic"
    }

    async fn chat_completion_stream(
        &self,
        config: &BackendConfig,
        model: &str,
        params: ChatCompletionParams,
    ) -> Result<StreamingResult, CompletionError> {
        let url = format!("{}/messages", config.base_url);
        let request = self.build_request(model, &params, true);

        let headers = self
            .build_headers(config)
            .map_err(CompletionError::CompletionError)?;
        let timeout = std::time::Duration::from_secs(config.timeout_seconds as u64);

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .timeout(timeout)
            .json(&request)
            .send()
            .await
            .map_err(|e| CompletionError::CompletionError(e.to_string()))?;

        if !response.status().is_success() {
            let status_code = response.status().as_u16();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|e| format!("Failed to read error response body: {e}"));
            return Err(CompletionError::HttpError {
                status_code,
                message: error_text,
            });
        }

        let sse_stream = new_anthropic_sse_parser(response.bytes_stream(), model.to_string());
        Ok(Box::pin(sse_stream))
    }

    async fn chat_completion(
        &self,
        config: &BackendConfig,
        model: &str,
        params: ChatCompletionParams,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        let url = format!("{}/messages", config.base_url);
        let request = self.build_request(model, &params, false);

        let headers = self
            .build_headers(config)
            .map_err(CompletionError::CompletionError)?;
        let timeout = std::time::Duration::from_secs(config.timeout_seconds as u64);

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .timeout(timeout)
            .json(&request)
            .send()
            .await
            .map_err(|e| CompletionError::CompletionError(e.to_string()))?;

        if !response.status().is_success() {
            let status_code = response.status().as_u16();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|e| format!("Failed to read error response body: {e}"));
            return Err(CompletionError::HttpError {
                status_code,
                message: error_text,
            });
        }

        let raw_bytes = response
            .bytes()
            .await
            .map_err(|e| CompletionError::CompletionError(e.to_string()))?
            .to_vec();

        let anthropic_response: AnthropicResponse =
            serde_json::from_slice(&raw_bytes).map_err(|e| {
                CompletionError::CompletionError(format!("Failed to parse response: {e}"))
            })?;

        // Convert to OpenAI format using the converter
        let (content, tool_calls) = extract_response_content(&anthropic_response.content);

        let openai_response = ChatCompletionResponse {
            id: anthropic_response.id,
            object: "chat.completion".to_string(),
            created: chrono::Utc::now().timestamp(),
            model: anthropic_response.model,
            choices: vec![ChatCompletionResponseChoice {
                index: 0,
                message: ChatResponseMessage {
                    role: MessageRole::Assistant,
                    content,
                    refusal: None,
                    annotations: None,
                    audio: None,
                    function_call: None,
                    tool_calls,
                    reasoning_content: None,
                    reasoning: None,
                },
                logprobs: None,
                finish_reason: map_finish_reason_string(anthropic_response.stop_reason),
                token_ids: None,
            }],
            service_tier: None,
            system_fingerprint: None,
            usage: TokenUsage {
                prompt_tokens: anthropic_response.usage.input_tokens,
                completion_tokens: anthropic_response.usage.output_tokens,
                total_tokens: anthropic_response.usage.input_tokens
                    + anthropic_response.usage.output_tokens,
                prompt_tokens_details: None,
            },
            prompt_logprobs: None,
            prompt_token_ids: None,
            kv_transfer_params: None,
        };

        let serialized_bytes = serde_json::to_vec(&openai_response).map_err(|e| {
            CompletionError::CompletionError(format!("Failed to serialize response: {e}"))
        })?;

        Ok(ChatCompletionResponseWithBytes {
            response: openai_response,
            raw_bytes: serialized_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_headers_default_version() {
        let backend = AnthropicBackend::new();
        let config = BackendConfig {
            base_url: "https://api.anthropic.com".to_string(),
            api_key: "test-key".to_string(),
            timeout_seconds: 30,
            extra: std::collections::HashMap::new(),
        };

        let headers = backend.build_headers(&config).unwrap();

        assert_eq!(
            headers.get("x-api-key").unwrap().to_str().unwrap(),
            "test-key"
        );
        assert_eq!(
            headers.get("anthropic-version").unwrap().to_str().unwrap(),
            DEFAULT_ANTHROPIC_VERSION
        );
    }

    #[test]
    fn test_build_headers_custom_version() {
        let backend = AnthropicBackend::new();
        let mut extra = std::collections::HashMap::new();
        extra.insert("version".to_string(), "2024-01-01".to_string());

        let config = BackendConfig {
            base_url: "https://api.anthropic.com".to_string(),
            api_key: "test-key".to_string(),
            timeout_seconds: 30,
            extra,
        };

        let headers = backend.build_headers(&config).unwrap();

        assert_eq!(
            headers.get("anthropic-version").unwrap().to_str().unwrap(),
            "2024-01-01"
        );
    }

    fn make_params(temperature: Option<f32>, top_p: Option<f32>) -> ChatCompletionParams {
        ChatCompletionParams {
            model: "claude-sonnet-4-5-20250514".to_string(),
            messages: vec![crate::ChatMessage {
                role: MessageRole::User,
                content: Some(serde_json::Value::String("Hello".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_completion_tokens: None,
            max_tokens: None,
            temperature,
            top_p,
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
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            metadata: None,
            store: None,
            stream_options: None,
            modalities: None,
            extra: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn test_build_request_temperature_only() {
        let backend = AnthropicBackend::new();
        let params = make_params(Some(0.7), None);
        let request = backend.build_request("claude-sonnet-4-5-20250514", &params, false);

        assert_eq!(request.temperature, Some(0.7));
        assert_eq!(request.top_p, None);
    }

    #[test]
    fn test_build_request_top_p_only() {
        let backend = AnthropicBackend::new();
        let params = make_params(None, Some(0.9));
        let request = backend.build_request("claude-sonnet-4-5-20250514", &params, false);

        assert_eq!(request.temperature, None);
        assert_eq!(request.top_p, Some(0.9));
    }

    #[test]
    fn test_build_request_both_temperature_and_top_p_prefers_temperature() {
        let backend = AnthropicBackend::new();
        let params = make_params(Some(0.5), Some(0.9));
        let request = backend.build_request("claude-sonnet-4-5-20250514", &params, false);

        // Anthropic doesn't allow both; temperature takes precedence
        assert_eq!(request.temperature, Some(0.5));
        assert_eq!(request.top_p, None);
    }

    #[test]
    fn test_build_request_neither_temperature_nor_top_p() {
        let backend = AnthropicBackend::new();
        let params = make_params(None, None);
        let request = backend.build_request("claude-sonnet-4-5-20250514", &params, false);

        assert_eq!(request.temperature, None);
        assert_eq!(request.top_p, None);
    }

    #[test]
    fn test_build_request_clamps_temperature_to_anthropic_range() {
        let backend = AnthropicBackend::new();
        // OpenAI allows temperature up to 2.0, Anthropic only allows up to 1.0
        let params = make_params(Some(1.5), None);
        let request = backend.build_request("claude-sonnet-4-5-20250514", &params, false);

        assert_eq!(request.temperature, Some(1.0));
    }

    #[test]
    fn test_build_request_default_max_tokens() {
        let backend = AnthropicBackend::new();
        let params = make_params(None, None);
        let request = backend.build_request("claude-sonnet-4-5-20250514", &params, false);

        assert_eq!(request.max_tokens, 4096);
    }

    #[tokio::test]
    async fn test_image_generation_returns_error() {
        let backend = AnthropicBackend::new();
        let config = BackendConfig {
            base_url: "https://api.anthropic.com".to_string(),
            api_key: "test-key".to_string(),
            timeout_seconds: 30,
            extra: std::collections::HashMap::new(),
        };

        let params = crate::ImageGenerationParams {
            model: "claude-3-opus".to_string(),
            prompt: "A cat".to_string(),
            n: None,
            size: None,
            response_format: None,
            quality: None,
            style: None,
            extra: std::collections::HashMap::new(),
        };

        let result = backend
            .image_generation(&config, "claude-3-opus", params)
            .await;

        assert!(result.is_err());
    }
}
