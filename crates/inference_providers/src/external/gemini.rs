//! Gemini backend implementation
//!
//! This backend handles Google's Gemini API, translating between our
//! OpenAI-compatible format and Gemini's native format.

use super::backend::{BackendConfig, ExternalBackend};
use crate::{
    ChatChoice, ChatCompletionChunk, ChatCompletionParams, ChatCompletionResponse,
    ChatCompletionResponseChoice, ChatCompletionResponseWithBytes, ChatDelta, ChatResponseMessage,
    CompletionError, MessageRole, SSEEvent, StreamChunk, StreamingResult, TokenUsage,
};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::Stream;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::task::{Context, Poll};
use uuid::Uuid;

/// Gemini backend
///
/// Translates between OpenAI-compatible format and Google Gemini's API.
pub struct GeminiBackend {
    client: Client,
}

impl GeminiBackend {
    pub fn new() -> Self {
        let client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()
            .expect("Failed to create HTTP client");

        Self { client }
    }

    /// Convert OpenAI messages to Gemini format
    fn convert_messages(
        messages: &[crate::ChatMessage],
    ) -> (Option<GeminiSystemInstruction>, Vec<GeminiContent>) {
        // Helper to extract string content from serde_json::Value
        let extract_content = |value: &serde_json::Value| -> String {
            match value {
                serde_json::Value::String(s) => s.clone(),
                _ => value.to_string(),
            }
        };

        let mut system_instruction = None;
        let mut contents = Vec::new();

        for msg in messages {
            match msg.role {
                MessageRole::System => {
                    // Gemini uses systemInstruction
                    if let Some(content) = &msg.content {
                        system_instruction = Some(GeminiSystemInstruction {
                            parts: vec![GeminiPart {
                                text: extract_content(content),
                            }],
                        });
                    }
                }
                MessageRole::User => {
                    contents.push(GeminiContent {
                        role: "user".to_string(),
                        parts: vec![GeminiPart {
                            text: msg
                                .content
                                .as_ref()
                                .map(&extract_content)
                                .unwrap_or_default(),
                        }],
                    });
                }
                MessageRole::Assistant => {
                    // Gemini uses "model" role for assistant
                    contents.push(GeminiContent {
                        role: "model".to_string(),
                        parts: vec![GeminiPart {
                            text: msg
                                .content
                                .as_ref()
                                .map(&extract_content)
                                .unwrap_or_default(),
                        }],
                    });
                }
                MessageRole::Tool => {
                    // Tool results go as user messages
                    contents.push(GeminiContent {
                        role: "user".to_string(),
                        parts: vec![GeminiPart {
                            text: msg
                                .content
                                .as_ref()
                                .map(&extract_content)
                                .unwrap_or_default(),
                        }],
                    });
                }
            }
        }

        (system_instruction, contents)
    }
}

impl Default for GeminiBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Map Gemini's finishReason to OpenAI-compatible finish_reason (enum for streaming)
///
/// Gemini uses: "STOP", "MAX_TOKENS", "SAFETY", "RECITATION", "OTHER"
/// OpenAI uses: "stop", "length", "content_filter", "tool_calls"
fn map_finish_reason(finish_reason: Option<&String>) -> Option<crate::FinishReason> {
    finish_reason.map(|r| match r.as_str() {
        "STOP" => crate::FinishReason::Stop,
        "MAX_TOKENS" => crate::FinishReason::Length,
        "SAFETY" => crate::FinishReason::ContentFilter,
        _ => crate::FinishReason::Stop,
    })
}

/// Map Gemini's finishReason to OpenAI-compatible finish_reason (string for non-streaming)
fn map_finish_reason_string(finish_reason: Option<&String>) -> Option<String> {
    finish_reason.map(|r| match r.as_str() {
        "STOP" => "stop".to_string(),
        "MAX_TOKENS" => "length".to_string(),
        "SAFETY" => "content_filter".to_string(),
        _ => "stop".to_string(),
    })
}

/// Gemini part format
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeminiPart {
    text: String,
}

/// Gemini content format
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

/// Gemini system instruction
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiSystemInstruction {
    parts: Vec<GeminiPart>,
}

/// Gemini generation config
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<Vec<String>>,
}

/// Gemini request format
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiSystemInstruction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
}

/// Gemini response candidate
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct GeminiCandidate {
    content: GeminiContent,
    finish_reason: Option<String>,
    #[serde(default)]
    safety_ratings: Vec<serde_json::Value>,
}

/// Gemini usage metadata
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GeminiUsageMetadata {
    #[serde(default)]
    prompt_token_count: i32,
    #[serde(default)]
    candidates_token_count: i32,
    #[serde(default)]
    total_token_count: i32,
}

/// Gemini response format
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct GeminiResponse {
    candidates: Vec<GeminiCandidate>,
    #[serde(default)]
    usage_metadata: GeminiUsageMetadata,
    model_version: Option<String>,
}

/// SSE parser for Gemini's streaming format
struct GeminiSSEParser<S> {
    inner: S,
    buffer: String,
    bytes_buffer: Vec<u8>,
    /// Pending results from previous process_buffer() calls
    /// Multiple SSE events can arrive in a single network packet
    pending_results: std::collections::VecDeque<Result<SSEEvent, CompletionError>>,
    model: String,
    /// Unique request ID (UUID-based to ensure uniqueness across concurrent requests)
    request_id: String,
    created: i64,
    chunk_index: i64,
    accumulated_prompt_tokens: i32,
    accumulated_completion_tokens: i32,
}

impl<S> GeminiSSEParser<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    fn new(stream: S, model: String) -> Self {
        Self {
            inner: stream,
            buffer: String::new(),
            bytes_buffer: Vec::new(),
            pending_results: std::collections::VecDeque::new(),
            model,
            request_id: format!("gemini-{}", Uuid::new_v4()),
            created: chrono::Utc::now().timestamp(),
            chunk_index: 0,
            accumulated_prompt_tokens: 0,
            accumulated_completion_tokens: 0,
        }
    }

    fn parse_response(&mut self, data: &str) -> Result<Option<StreamChunk>, CompletionError> {
        let response: GeminiResponse = serde_json::from_str(data).map_err(|e| {
            CompletionError::InvalidResponse(format!("Failed to parse Gemini response: {e}"))
        })?;

        if response.candidates.is_empty() {
            return Ok(None);
        }

        let candidate = &response.candidates[0];
        let text = candidate
            .content
            .parts
            .iter()
            .map(|p| p.text.clone())
            .collect::<Vec<_>>()
            .join("");

        // Update token counts
        self.accumulated_prompt_tokens = response.usage_metadata.prompt_token_count;
        self.accumulated_completion_tokens = response.usage_metadata.candidates_token_count;

        let finish_reason = map_finish_reason(candidate.finish_reason.as_ref());

        let is_first = self.chunk_index == 0;
        self.chunk_index += 1;

        let chunk = ChatCompletionChunk {
            id: self.request_id.clone(),
            object: "chat.completion.chunk".to_string(),
            created: self.created,
            model: self.model.clone(),
            system_fingerprint: None,
            modality: None,
            choices: vec![ChatChoice {
                index: 0,
                delta: Some(ChatDelta {
                    role: if is_first {
                        Some(MessageRole::Assistant)
                    } else {
                        None
                    },
                    content: if text.is_empty() { None } else { Some(text) },
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    reasoning_content: None,
                    reasoning: None,
                }),
                logprobs: None,
                finish_reason,
                token_ids: None,
            }],
            usage: Some(TokenUsage {
                prompt_tokens: self.accumulated_prompt_tokens,
                completion_tokens: self.accumulated_completion_tokens,
                total_tokens: self.accumulated_prompt_tokens + self.accumulated_completion_tokens,
                prompt_tokens_details: None,
            }),
            prompt_token_ids: None,
        };

        Ok(Some(StreamChunk::Chat(chunk)))
    }

    fn process_buffer(&mut self) -> Vec<Result<SSEEvent, CompletionError>> {
        let mut results = Vec::new();

        // Gemini streaming returns JSON lines, not SSE format
        while let Some(newline_pos) = self.buffer.find('\n') {
            let line_len = newline_pos + 1;
            let raw_bytes = Bytes::copy_from_slice(&self.bytes_buffer[..line_len]);
            self.bytes_buffer.drain(..line_len);

            let line = self.buffer.drain(..=newline_pos).collect::<String>();
            let line = line.trim();

            if line.is_empty() {
                continue;
            }

            // Handle SSE format (data: prefix) or raw JSON
            let data = if let Some(d) = line.strip_prefix("data: ") {
                d
            } else {
                line
            };

            match self.parse_response(data) {
                Ok(Some(chunk)) => {
                    results.push(Ok(SSEEvent { raw_bytes, chunk }));
                }
                Ok(None) => {}
                Err(e) => results.push(Err(e)),
            }
        }

        results
    }
}

impl<S> Stream for GeminiSSEParser<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    type Item = Result<SSEEvent, CompletionError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // First, return any pending results from previous process_buffer() calls
        if let Some(result) = self.pending_results.pop_front() {
            return Poll::Ready(Some(result));
        }

        // Try to get more results from the current buffer
        let buffered_results = self.process_buffer();
        if !buffered_results.is_empty() {
            // Store all results in pending queue
            self.pending_results.extend(buffered_results);
            if let Some(result) = self.pending_results.pop_front() {
                return Poll::Ready(Some(result));
            }
        }

        // Poll for more data from the underlying stream
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                self.bytes_buffer.extend_from_slice(&bytes);
                let text = String::from_utf8_lossy(&bytes);
                self.buffer.push_str(&text);

                let results = self.process_buffer();
                if !results.is_empty() {
                    // Store all results in pending queue
                    self.pending_results.extend(results);
                    if let Some(result) = self.pending_results.pop_front() {
                        return Poll::Ready(Some(result));
                    }
                }
                // No complete events yet, wake and try again
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Ready(Some(Err(e))) => {
                Poll::Ready(Some(Err(CompletionError::CompletionError(e.to_string()))))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[async_trait]
impl ExternalBackend for GeminiBackend {
    fn backend_type(&self) -> &'static str {
        "gemini"
    }

    async fn chat_completion_stream(
        &self,
        config: &BackendConfig,
        model: &str,
        params: ChatCompletionParams,
    ) -> Result<StreamingResult, CompletionError> {
        // Gemini API URL format: {base_url}/models/{model}:streamGenerateContent?alt=sse
        // API key is passed via x-goog-api-key header for security
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            config.base_url, model
        );

        let (system_instruction, contents) = Self::convert_messages(&params.messages);

        let max_tokens = params.max_completion_tokens.or(params.max_tokens);

        let generation_config = if params.temperature.is_some()
            || params.top_p.is_some()
            || max_tokens.is_some()
            || params.stop.is_some()
        {
            Some(GeminiGenerationConfig {
                temperature: params.temperature,
                top_p: params.top_p,
                max_output_tokens: max_tokens,
                stop_sequences: params.stop,
            })
        } else {
            None
        };

        let request = GeminiRequest {
            contents,
            system_instruction,
            generation_config,
        };

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "Content-Type",
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            "x-goog-api-key",
            reqwest::header::HeaderValue::from_str(&config.api_key)
                .map_err(|e| CompletionError::CompletionError(format!("Invalid API key: {e}")))?,
        );

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

        let sse_stream = GeminiSSEParser::new(response.bytes_stream(), model.to_string());
        Ok(Box::pin(sse_stream))
    }

    async fn chat_completion(
        &self,
        config: &BackendConfig,
        model: &str,
        params: ChatCompletionParams,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        // Gemini API URL format: {base_url}/models/{model}:generateContent
        // API key is passed via x-goog-api-key header for security
        let url = format!("{}/models/{}:generateContent", config.base_url, model);

        let (system_instruction, contents) = Self::convert_messages(&params.messages);

        let max_tokens = params.max_completion_tokens.or(params.max_tokens);

        let generation_config = if params.temperature.is_some()
            || params.top_p.is_some()
            || max_tokens.is_some()
            || params.stop.is_some()
        {
            Some(GeminiGenerationConfig {
                temperature: params.temperature,
                top_p: params.top_p,
                max_output_tokens: max_tokens,
                stop_sequences: params.stop,
            })
        } else {
            None
        };

        let request = GeminiRequest {
            contents,
            system_instruction,
            generation_config,
        };

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "Content-Type",
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            "x-goog-api-key",
            reqwest::header::HeaderValue::from_str(&config.api_key)
                .map_err(|e| CompletionError::CompletionError(format!("Invalid API key: {e}")))?,
        );

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

        let raw_bytes = response
            .bytes()
            .await
            .map_err(|e| CompletionError::CompletionError(e.to_string()))?
            .to_vec();

        let gemini_response: GeminiResponse = serde_json::from_slice(&raw_bytes).map_err(|e| {
            CompletionError::CompletionError(format!("Failed to parse response: {e}"))
        })?;

        if gemini_response.candidates.is_empty() {
            return Err(CompletionError::CompletionError(
                "No candidates in Gemini response".to_string(),
            ));
        }

        let candidate = &gemini_response.candidates[0];
        let content = candidate
            .content
            .parts
            .iter()
            .map(|p| p.text.clone())
            .collect::<Vec<_>>()
            .join("");

        let openai_response = ChatCompletionResponse {
            id: format!("gemini-{}", Uuid::new_v4()),
            object: "chat.completion".to_string(),
            created: chrono::Utc::now().timestamp(),
            model: model.to_string(),
            choices: vec![ChatCompletionResponseChoice {
                index: 0,
                message: ChatResponseMessage {
                    role: MessageRole::Assistant,
                    content: Some(content),
                    refusal: None,
                    annotations: None,
                    audio: None,
                    function_call: None,
                    tool_calls: None,
                    reasoning_content: None,
                    reasoning: None,
                },
                logprobs: None,
                finish_reason: map_finish_reason_string(candidate.finish_reason.as_ref()),
                token_ids: None,
            }],
            service_tier: None,
            system_fingerprint: None,
            usage: TokenUsage {
                prompt_tokens: gemini_response.usage_metadata.prompt_token_count,
                completion_tokens: gemini_response.usage_metadata.candidates_token_count,
                total_tokens: gemini_response.usage_metadata.total_token_count,
                prompt_tokens_details: None,
            },
            prompt_logprobs: None,
            prompt_token_ids: None,
            kv_transfer_params: None,
        };

        // Re-serialize for consistent raw bytes
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
    use crate::ChatMessage;

    /// Helper to create a string content value for tests
    fn str_content(s: &str) -> serde_json::Value {
        serde_json::Value::String(s.to_string())
    }

    // ==================== Message Translation Tests ====================

    #[test]
    fn test_convert_messages_extracts_system_instruction() {
        let messages = vec![
            ChatMessage {
                role: MessageRole::System,
                content: Some(str_content("You are a helpful assistant.")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: Some(str_content("Hello")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let (system_instruction, contents) = GeminiBackend::convert_messages(&messages);

        assert!(system_instruction.is_some());
        let sys = system_instruction.unwrap();
        assert_eq!(sys.parts.len(), 1);
        assert_eq!(sys.parts[0].text, "You are a helpful assistant.");

        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role, "user");
        assert_eq!(contents[0].parts[0].text, "Hello");
    }

    #[test]
    fn test_convert_messages_assistant_becomes_model() {
        let messages = vec![
            ChatMessage {
                role: MessageRole::User,
                content: Some(str_content("Hello")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::Assistant,
                content: Some(str_content("Hi there!")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let (system_instruction, contents) = GeminiBackend::convert_messages(&messages);

        assert!(system_instruction.is_none());
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0].role, "user");
        assert_eq!(contents[1].role, "model"); // assistant -> model
        assert_eq!(contents[1].parts[0].text, "Hi there!");
    }

    #[test]
    fn test_convert_messages_empty() {
        let messages: Vec<ChatMessage> = vec![];
        let (system_instruction, contents) = GeminiBackend::convert_messages(&messages);

        assert!(system_instruction.is_none());
        assert!(contents.is_empty());
    }

    #[test]
    fn test_convert_messages_only_system() {
        let messages = vec![ChatMessage {
            role: MessageRole::System,
            content: Some(str_content("You are a bot.")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let (system_instruction, contents) = GeminiBackend::convert_messages(&messages);

        assert!(system_instruction.is_some());
        assert!(contents.is_empty());
    }

    #[test]
    fn test_convert_messages_tool_becomes_user() {
        let messages = vec![ChatMessage {
            role: MessageRole::Tool,
            content: Some(str_content("Tool result here")),
            name: None,
            tool_call_id: Some("call_123".to_string()),
            tool_calls: None,
        }];

        let (system_instruction, contents) = GeminiBackend::convert_messages(&messages);

        assert!(system_instruction.is_none());
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role, "user");
        assert_eq!(contents[0].parts[0].text, "Tool result here");
    }

    #[test]
    fn test_convert_messages_none_content() {
        let messages = vec![ChatMessage {
            role: MessageRole::User,
            content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let (system_instruction, contents) = GeminiBackend::convert_messages(&messages);

        assert!(system_instruction.is_none());
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].parts[0].text, ""); // Empty string for None
    }

    #[test]
    fn test_convert_messages_multiple_system_uses_last() {
        let messages = vec![
            ChatMessage {
                role: MessageRole::System,
                content: Some(str_content("First system")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: Some(str_content("Hello")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::System,
                content: Some(str_content("Second system")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let (system_instruction, contents) = GeminiBackend::convert_messages(&messages);

        // Last system message should be used
        assert!(system_instruction.is_some());
        let sys = system_instruction.unwrap();
        assert_eq!(sys.parts[0].text, "Second system");
        assert_eq!(contents.len(), 1);
    }

    #[test]
    fn test_convert_messages_no_system() {
        let messages = vec![ChatMessage {
            role: MessageRole::User,
            content: Some(str_content("Hello")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let (system_instruction, contents) = GeminiBackend::convert_messages(&messages);

        assert!(system_instruction.is_none());
        assert_eq!(contents.len(), 1);
    }

    // ==================== Response Parsing Tests ====================

    #[test]
    fn test_parse_gemini_response() {
        let json = r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Hello! How can I help you?"}]
                },
                "finishReason": "STOP",
                "safetyRatings": []
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 8,
                "totalTokenCount": 18
            },
            "modelVersion": "gemini-1.5-pro"
        }"#;

        let response: GeminiResponse = serde_json::from_str(json).unwrap();

        assert_eq!(response.candidates.len(), 1);
        assert_eq!(response.candidates[0].content.role, "model");
        assert_eq!(
            response.candidates[0].content.parts[0].text,
            "Hello! How can I help you?"
        );
        assert_eq!(
            response.candidates[0].finish_reason,
            Some("STOP".to_string())
        );
        assert_eq!(response.usage_metadata.prompt_token_count, 10);
        assert_eq!(response.usage_metadata.candidates_token_count, 8);
        assert_eq!(response.usage_metadata.total_token_count, 18);
    }

    #[test]
    fn test_parse_gemini_response_empty_candidates() {
        let json = r#"{
            "candidates": [],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 0,
                "totalTokenCount": 10
            }
        }"#;

        let response: GeminiResponse = serde_json::from_str(json).unwrap();

        assert!(response.candidates.is_empty());
    }

    #[test]
    fn test_parse_gemini_response_multiple_parts() {
        let json = r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"text": "First part. "},
                        {"text": "Second part."}
                    ]
                },
                "finishReason": "STOP",
                "safetyRatings": []
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 8,
                "totalTokenCount": 18
            }
        }"#;

        let response: GeminiResponse = serde_json::from_str(json).unwrap();

        assert_eq!(response.candidates[0].content.parts.len(), 2);
    }

    #[test]
    fn test_parse_gemini_response_safety_finish_reason() {
        let json = r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": ""}]
                },
                "finishReason": "SAFETY",
                "safetyRatings": [{"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "probability": "HIGH"}]
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 0,
                "totalTokenCount": 10
            }
        }"#;

        let response: GeminiResponse = serde_json::from_str(json).unwrap();

        assert_eq!(
            response.candidates[0].finish_reason,
            Some("SAFETY".to_string())
        );
    }

    // ==================== Finish Reason Mapping Tests ====================

    #[test]
    fn test_finish_reason_mapping() {
        let test_cases = vec![
            ("STOP", crate::FinishReason::Stop),
            ("MAX_TOKENS", crate::FinishReason::Length),
            ("SAFETY", crate::FinishReason::ContentFilter),
            ("UNKNOWN", crate::FinishReason::Stop), // Default
        ];

        for (gemini_reason, expected) in test_cases {
            let reason_string = gemini_reason.to_string();
            let mapped = map_finish_reason(Some(&reason_string)).unwrap();
            assert_eq!(mapped, expected, "Failed for reason: {}", gemini_reason);
        }

        // Test None case
        assert_eq!(map_finish_reason(None), None);
    }

    // ==================== Request Building Tests ====================

    #[test]
    fn test_gemini_request_serialization() {
        let request = GeminiRequest {
            contents: vec![GeminiContent {
                role: "user".to_string(),
                parts: vec![GeminiPart {
                    text: "Hello".to_string(),
                }],
            }],
            system_instruction: Some(GeminiSystemInstruction {
                parts: vec![GeminiPart {
                    text: "You are helpful.".to_string(),
                }],
            }),
            generation_config: Some(GeminiGenerationConfig {
                temperature: Some(0.7),
                top_p: Some(0.9),
                max_output_tokens: Some(1024),
                stop_sequences: Some(vec!["STOP".to_string()]),
            }),
        };

        let json = serde_json::to_string(&request).unwrap();

        assert!(json.contains("\"contents\""));
        assert!(json.contains("\"systemInstruction\"")); // camelCase
        assert!(json.contains("\"generationConfig\"")); // camelCase
        assert!(json.contains("\"temperature\":0.7"));
        assert!(json.contains("\"maxOutputTokens\":1024")); // camelCase
    }

    #[test]
    fn test_gemini_request_skips_none_fields() {
        let request = GeminiRequest {
            contents: vec![],
            system_instruction: None,
            generation_config: None,
        };

        let json = serde_json::to_string(&request).unwrap();

        assert!(!json.contains("\"systemInstruction\""));
        assert!(!json.contains("\"generationConfig\""));
    }

    #[test]
    fn test_gemini_generation_config_skips_none_fields() {
        let config = GeminiGenerationConfig {
            temperature: Some(0.5),
            top_p: None,
            max_output_tokens: None,
            stop_sequences: None,
        };

        let json = serde_json::to_string(&config).unwrap();

        assert!(json.contains("\"temperature\":0.5"));
        assert!(!json.contains("\"topP\""));
        assert!(!json.contains("\"maxOutputTokens\""));
        assert!(!json.contains("\"stopSequences\""));
    }

    // ==================== Usage Metadata Tests ====================

    #[test]
    fn test_usage_metadata_defaults() {
        let json = r#"{}"#;

        let usage: GeminiUsageMetadata = serde_json::from_str(json).unwrap();

        assert_eq!(usage.prompt_token_count, 0);
        assert_eq!(usage.candidates_token_count, 0);
        assert_eq!(usage.total_token_count, 0);
    }

    #[test]
    fn test_usage_metadata_partial() {
        let json = r#"{"promptTokenCount": 10}"#;

        let usage: GeminiUsageMetadata = serde_json::from_str(json).unwrap();

        assert_eq!(usage.prompt_token_count, 10);
        assert_eq!(usage.candidates_token_count, 0);
        assert_eq!(usage.total_token_count, 0);
    }

    // ==================== URL Building Tests ====================

    #[test]
    fn test_streaming_url_format() {
        let base_url = "https://generativelanguage.googleapis.com/v1beta";
        let model = "gemini-1.5-pro";

        // API key is passed via x-goog-api-key header, not in URL
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            base_url, model
        );

        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-1.5-pro:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn test_non_streaming_url_format() {
        let base_url = "https://generativelanguage.googleapis.com/v1beta";
        let model = "gemini-1.5-pro";

        // API key is passed via x-goog-api-key header, not in URL
        let url = format!("{}/models/{}:generateContent", base_url, model);

        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-1.5-pro:generateContent"
        );
    }

    #[test]
    fn test_api_key_header() {
        // Verify that the x-goog-api-key header can be created
        let api_key = "test-api-key-123";
        let header_value = reqwest::header::HeaderValue::from_str(api_key);
        assert!(header_value.is_ok());
        assert_eq!(header_value.unwrap().to_str().unwrap(), api_key);
    }

    // ==================== Content Structure Tests ====================

    #[test]
    fn test_gemini_content_serialization() {
        let content = GeminiContent {
            role: "user".to_string(),
            parts: vec![GeminiPart {
                text: "Hello world".to_string(),
            }],
        };

        let json = serde_json::to_string(&content).unwrap();

        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"text\":\"Hello world\""));
    }

    #[test]
    fn test_gemini_system_instruction_serialization() {
        let instruction = GeminiSystemInstruction {
            parts: vec![GeminiPart {
                text: "Be helpful".to_string(),
            }],
        };

        let json = serde_json::to_string(&instruction).unwrap();

        assert!(json.contains("\"parts\""));
        assert!(json.contains("\"text\":\"Be helpful\""));
    }

    // ==================== SSE Parser Tests ====================

    #[tokio::test]
    async fn test_sse_parser_multiple_events_in_single_packet() {
        use futures_util::StreamExt;

        // Simulate multiple SSE events arriving in a single network packet
        // This tests that the parser doesn't lose events when process_buffer() returns multiple results
        let multi_event_packet = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}],\"role\":\"model\"},\"finishReason\":null,\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":1,\"totalTokenCount\":11}}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" World\"}],\"role\":\"model\"},\"finishReason\":null,\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":2,\"totalTokenCount\":12}}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"!\"}],\"role\":\"model\"},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":3,\"totalTokenCount\":13}}\n\n",
        );

        // Create a mock stream that returns all events in one packet
        let bytes = bytes::Bytes::from(multi_event_packet);
        let mock_stream = futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes)]);

        let parser = GeminiSSEParser::new(mock_stream, "gemini-1.5-pro".to_string());
        let events: Vec<_> = parser.collect().await;

        // Should have received all 3 events
        assert_eq!(events.len(), 3, "Expected 3 events, got {}", events.len());

        // Verify each event is Ok
        for (i, event) in events.iter().enumerate() {
            assert!(event.is_ok(), "Event {} should be Ok", i);
        }
    }

    #[tokio::test]
    async fn test_sse_parser_events_split_across_packets() {
        use futures_util::StreamExt;

        // Test events split across multiple network packets
        let packet1 = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}],\"role\":\"model\"},\"finishReason\":null,\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":1,\"totalTokenCount\":11}}\n\n";
        let packet2 = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" World\"}],\"role\":\"model\"},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":2,\"totalTokenCount\":12}}\n\n";

        let mock_stream = futures_util::stream::iter(vec![
            Ok::<_, reqwest::Error>(bytes::Bytes::from(packet1)),
            Ok(bytes::Bytes::from(packet2)),
        ]);

        let parser = GeminiSSEParser::new(mock_stream, "gemini-1.5-pro".to_string());
        let events: Vec<_> = parser.collect().await;

        assert_eq!(events.len(), 2, "Expected 2 events, got {}", events.len());

        for event in &events {
            assert!(event.is_ok());
        }
    }

    // ==================== ID Uniqueness Tests ====================

    #[tokio::test]
    async fn test_gemini_ids_are_unique_across_concurrent_requests() {
        use std::collections::HashSet;

        // Create multiple parsers simultaneously (simulating concurrent requests)
        // All should have unique IDs even if created within the same second
        let mut ids = HashSet::new();
        let num_requests = 100;

        for _ in 0..num_requests {
            let mock_stream =
                futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::new())]);
            let parser = GeminiSSEParser::new(mock_stream, "gemini-1.5-pro".to_string());

            // Access the request_id field
            let request_id = parser.request_id.clone();

            // Verify ID has correct format (gemini-<uuid>)
            assert!(
                request_id.starts_with("gemini-"),
                "ID should start with 'gemini-'"
            );
            assert!(
                request_id.len() > 7,
                "ID should have UUID component after prefix"
            );

            // Check for uniqueness
            let is_unique = ids.insert(request_id.clone());
            assert!(
                is_unique,
                "ID should be unique, but got duplicate: {}",
                request_id
            );
        }

        assert_eq!(
            ids.len(),
            num_requests,
            "All {} IDs should be unique",
            num_requests
        );
    }

    #[tokio::test]
    async fn test_gemini_streaming_response_has_consistent_id() {
        use futures_util::StreamExt;

        // Test that all chunks in a streaming response have the same ID
        let multi_event_packet = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}],\"role\":\"model\"},\"finishReason\":null,\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":1,\"totalTokenCount\":11}}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" World\"}],\"role\":\"model\"},\"finishReason\":null,\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":2,\"totalTokenCount\":12}}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"!\"}],\"role\":\"model\"},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":3,\"totalTokenCount\":13}}\n\n",
        );

        let bytes = bytes::Bytes::from(multi_event_packet);
        let mock_stream = futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes)]);

        let parser = GeminiSSEParser::new(mock_stream, "gemini-1.5-pro".to_string());
        let events: Vec<_> = parser.collect().await;

        // Extract IDs from all chunks
        let mut ids: Vec<String> = Vec::new();
        for event in events {
            if let Ok(sse_event) = event {
                if let StreamChunk::Chat(chat_chunk) = sse_event.chunk {
                    ids.push(chat_chunk.id.clone());
                }
            }
        }

        assert!(!ids.is_empty(), "Should have collected chunk IDs");

        // All IDs should be the same within a single request
        let first_id = &ids[0];
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(
                id, first_id,
                "Chunk {} has different ID. Expected: {}, Got: {}",
                i, first_id, id
            );
        }

        // ID should have the correct format
        assert!(
            first_id.starts_with("gemini-"),
            "ID should start with 'gemini-'"
        );
    }
}
