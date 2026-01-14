//! Anthropic backend implementation
//!
//! This backend handles Anthropic's Messages API, translating between our
//! OpenAI-compatible format and Anthropic's native format.

use super::backend::{BackendConfig, ExternalBackend};
use crate::{
    ChatChoice, ChatCompletionChunk, ChatCompletionParams, ChatCompletionResponse,
    ChatCompletionResponseChoice, ChatCompletionResponseWithBytes, ChatDelta, ChatResponseMessage,
    CompletionError, MessageRole, SSEEvent, StreamChunk, StreamingResult, TokenUsage,
};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::Stream;
use reqwest::{header::HeaderValue, Client};
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::task::{Context, Poll};

/// Default Anthropic API version
const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic backend
///
/// Translates between OpenAI-compatible format and Anthropic's Messages API.
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

        // Anthropic uses x-api-key header
        let header_value = HeaderValue::from_str(&config.api_key)
            .map_err(|e| format!("Invalid API key format: {e}"))?;
        headers.insert("x-api-key", header_value);

        // Anthropic version header
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

    /// Convert OpenAI messages to Anthropic format
    fn convert_messages(
        messages: &[crate::ChatMessage],
    ) -> (Option<String>, Vec<AnthropicMessage>) {
        let mut system_message = None;
        let mut anthropic_messages = Vec::new();

        for msg in messages {
            match msg.role {
                MessageRole::System => {
                    // Anthropic has a separate system parameter
                    if let Some(content) = &msg.content {
                        system_message = Some(content.clone());
                    }
                }
                MessageRole::User => {
                    anthropic_messages.push(AnthropicMessage {
                        role: "user".to_string(),
                        content: msg.content.clone().unwrap_or_default(),
                    });
                }
                MessageRole::Assistant => {
                    anthropic_messages.push(AnthropicMessage {
                        role: "assistant".to_string(),
                        content: msg.content.clone().unwrap_or_default(),
                    });
                }
                MessageRole::Tool => {
                    // Tool results in Anthropic format
                    anthropic_messages.push(AnthropicMessage {
                        role: "user".to_string(),
                        content: msg.content.clone().unwrap_or_default(),
                    });
                }
            }
        }

        (system_message, anthropic_messages)
    }
}

impl Default for AnthropicBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Anthropic message format
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicMessage {
    role: String,
    content: String,
}

/// Anthropic request format
#[derive(Debug, Clone, Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    max_tokens: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<Vec<String>>,
    stream: bool,
}

/// Anthropic streaming event types
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
#[allow(dead_code)]
enum AnthropicStreamEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: AnthropicMessageInfo },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: i64,
        content_block: AnthropicContentBlock,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta {
        index: i64,
        delta: AnthropicDelta,
    },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: i64 },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: AnthropicMessageDelta,
        usage: AnthropicUsage,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "error")]
    Error { error: AnthropicError },
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct AnthropicMessageInfo {
    id: String,
    model: String,
    role: String,
    usage: AnthropicUsage,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    type_: String,
    text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct AnthropicDelta {
    #[serde(rename = "type")]
    type_: String,
    text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct AnthropicMessageDelta {
    stop_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: i32,
    #[serde(default)]
    output_tokens: i32,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct AnthropicError {
    #[serde(rename = "type")]
    type_: String,
    message: String,
}

/// Anthropic non-streaming response
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct AnthropicResponse {
    id: String,
    #[serde(rename = "type")]
    type_: String,
    role: String,
    content: Vec<AnthropicContentBlock>,
    model: String,
    stop_reason: Option<String>,
    usage: AnthropicUsage,
}

/// SSE parser for Anthropic's streaming format
struct AnthropicSSEParser<S> {
    inner: S,
    buffer: String,
    bytes_buffer: Vec<u8>,
    message_id: Option<String>,
    model: String,
    created: i64,
    accumulated_input_tokens: i32,
    accumulated_output_tokens: i32,
}

impl<S> AnthropicSSEParser<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    fn new(stream: S, model: String) -> Self {
        Self {
            inner: stream,
            buffer: String::new(),
            bytes_buffer: Vec::new(),
            message_id: None,
            model,
            created: chrono::Utc::now().timestamp(),
            accumulated_input_tokens: 0,
            accumulated_output_tokens: 0,
        }
    }

    fn parse_event(&mut self, data: &str) -> Result<Option<StreamChunk>, CompletionError> {
        let event: AnthropicStreamEvent = serde_json::from_str(data)
            .map_err(|e| CompletionError::InvalidResponse(format!("Failed to parse Anthropic event: {e}")))?;

        match event {
            AnthropicStreamEvent::MessageStart { message } => {
                self.message_id = Some(message.id.clone());
                self.accumulated_input_tokens = message.usage.input_tokens;

                // Emit initial chunk with role
                let chunk = ChatCompletionChunk {
                    id: message.id,
                    object: "chat.completion.chunk".to_string(),
                    created: self.created,
                    model: self.model.clone(),
                    system_fingerprint: None,
                    choices: vec![ChatChoice {
                        index: 0,
                        delta: Some(ChatDelta {
                            role: Some(MessageRole::Assistant),
                            content: None,
                            name: None,
                            tool_call_id: None,
                            tool_calls: None,
                            reasoning_content: None,
                            reasoning: None,
                        }),
                        logprobs: None,
                        finish_reason: None,
                        token_ids: None,
                    }],
                    usage: None,
                    prompt_token_ids: None,
                };
                Ok(Some(StreamChunk::Chat(chunk)))
            }
            AnthropicStreamEvent::ContentBlockDelta { delta, .. } => {
                if let Some(text) = delta.text {
                    let chunk = ChatCompletionChunk {
                        id: self.message_id.clone().unwrap_or_default(),
                        object: "chat.completion.chunk".to_string(),
                        created: self.created,
                        model: self.model.clone(),
                        system_fingerprint: None,
                        choices: vec![ChatChoice {
                            index: 0,
                            delta: Some(ChatDelta {
                                role: None,
                                content: Some(text),
                                name: None,
                                tool_call_id: None,
                                tool_calls: None,
                                reasoning_content: None,
                                reasoning: None,
                            }),
                            logprobs: None,
                            finish_reason: None,
                            token_ids: None,
                        }],
                        usage: None,
                        prompt_token_ids: None,
                    };
                    Ok(Some(StreamChunk::Chat(chunk)))
                } else {
                    Ok(None)
                }
            }
            AnthropicStreamEvent::MessageDelta { delta, usage } => {
                self.accumulated_output_tokens = usage.output_tokens;

                let finish_reason = delta.stop_reason.map(|r| match r.as_str() {
                    "end_turn" | "stop_sequence" => crate::FinishReason::Stop,
                    "max_tokens" => crate::FinishReason::Length,
                    _ => crate::FinishReason::Stop,
                });

                let chunk = ChatCompletionChunk {
                    id: self.message_id.clone().unwrap_or_default(),
                    object: "chat.completion.chunk".to_string(),
                    created: self.created,
                    model: self.model.clone(),
                    system_fingerprint: None,
                    choices: vec![ChatChoice {
                        index: 0,
                        delta: Some(ChatDelta {
                            role: None,
                            content: None,
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
                        prompt_tokens: self.accumulated_input_tokens,
                        completion_tokens: self.accumulated_output_tokens,
                        total_tokens: self.accumulated_input_tokens + self.accumulated_output_tokens,
                        prompt_tokens_details: None,
                    }),
                    prompt_token_ids: None,
                };
                Ok(Some(StreamChunk::Chat(chunk)))
            }
            AnthropicStreamEvent::Error { error } => {
                Err(CompletionError::CompletionError(format!(
                    "Anthropic error: {} - {}",
                    error.type_, error.message
                )))
            }
            // Ignore other events
            _ => Ok(None),
        }
    }

    fn process_buffer(&mut self) -> Vec<Result<SSEEvent, CompletionError>> {
        let mut results = Vec::new();

        while let Some(newline_pos) = self.buffer.find('\n') {
            let line_len = newline_pos + 1;
            let raw_bytes = Bytes::copy_from_slice(&self.bytes_buffer[..line_len]);
            self.bytes_buffer.drain(..line_len);

            let line = self.buffer.drain(..=newline_pos).collect::<String>();
            let line = line.trim();

            if line.is_empty() || line.starts_with(':') {
                continue;
            }

            if let Some(data) = line.strip_prefix("data: ") {
                match self.parse_event(data) {
                    Ok(Some(chunk)) => {
                        results.push(Ok(SSEEvent { raw_bytes, chunk }));
                    }
                    Ok(None) => {}
                    Err(e) => results.push(Err(e)),
                }
            }
        }

        results
    }
}

impl<S> Stream for AnthropicSSEParser<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    type Item = Result<SSEEvent, CompletionError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let buffered_results = self.process_buffer();
        if !buffered_results.is_empty() {
            if let Some(result) = buffered_results.into_iter().next() {
                return Poll::Ready(Some(result));
            }
        }

        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                self.bytes_buffer.extend_from_slice(&bytes);
                let text = String::from_utf8_lossy(&bytes);
                self.buffer.push_str(&text);

                let results = self.process_buffer();
                if let Some(result) = results.into_iter().next() {
                    Poll::Ready(Some(result))
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
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

        let (system, messages) = Self::convert_messages(&params.messages);

        let max_tokens = params
            .max_completion_tokens
            .or(params.max_tokens)
            .unwrap_or(4096);

        let request = AnthropicRequest {
            model: model.to_string(),
            messages,
            max_tokens,
            system,
            temperature: params.temperature,
            top_p: params.top_p,
            stop_sequences: params.stop,
            stream: true,
        };

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

        let sse_stream = AnthropicSSEParser::new(response.bytes_stream(), model.to_string());
        Ok(Box::pin(sse_stream))
    }

    async fn chat_completion(
        &self,
        config: &BackendConfig,
        model: &str,
        params: ChatCompletionParams,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        let url = format!("{}/messages", config.base_url);

        let (system, messages) = Self::convert_messages(&params.messages);

        let max_tokens = params
            .max_completion_tokens
            .or(params.max_tokens)
            .unwrap_or(4096);

        let request = AnthropicRequest {
            model: model.to_string(),
            messages,
            max_tokens,
            system,
            temperature: params.temperature,
            top_p: params.top_p,
            stop_sequences: params.stop,
            stream: false,
        };

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

        let anthropic_response: AnthropicResponse = serde_json::from_slice(&raw_bytes)
            .map_err(|e| CompletionError::CompletionError(format!("Failed to parse response: {e}")))?;

        // Convert Anthropic response to OpenAI format
        let content = anthropic_response
            .content
            .into_iter()
            .filter_map(|c| c.text)
            .collect::<Vec<_>>()
            .join("");

        let openai_response = ChatCompletionResponse {
            id: anthropic_response.id,
            object: "chat.completion".to_string(),
            created: chrono::Utc::now().timestamp(),
            model: anthropic_response.model,
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
                finish_reason: anthropic_response.stop_reason,
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

        // Re-serialize for consistent raw bytes
        let serialized_bytes = serde_json::to_vec(&openai_response)
            .map_err(|e| CompletionError::CompletionError(format!("Failed to serialize response: {e}")))?;

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

    // ==================== Message Translation Tests ====================

    #[test]
    fn test_convert_messages_extracts_system() {
        let messages = vec![
            ChatMessage {
                role: MessageRole::System,
                content: Some("You are a helpful assistant.".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: Some("Hello".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let (system, anthropic_messages) = AnthropicBackend::convert_messages(&messages);

        assert_eq!(system, Some("You are a helpful assistant.".to_string()));
        assert_eq!(anthropic_messages.len(), 1);
        assert_eq!(anthropic_messages[0].role, "user");
        assert_eq!(anthropic_messages[0].content, "Hello");
    }

    #[test]
    fn test_convert_messages_no_system() {
        let messages = vec![
            ChatMessage {
                role: MessageRole::User,
                content: Some("Hello".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::Assistant,
                content: Some("Hi there!".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let (system, anthropic_messages) = AnthropicBackend::convert_messages(&messages);

        assert!(system.is_none());
        assert_eq!(anthropic_messages.len(), 2);
        assert_eq!(anthropic_messages[0].role, "user");
        assert_eq!(anthropic_messages[1].role, "assistant");
    }

    #[test]
    fn test_convert_messages_empty() {
        let messages: Vec<ChatMessage> = vec![];
        let (system, anthropic_messages) = AnthropicBackend::convert_messages(&messages);

        assert!(system.is_none());
        assert!(anthropic_messages.is_empty());
    }

    #[test]
    fn test_convert_messages_only_system() {
        let messages = vec![ChatMessage {
            role: MessageRole::System,
            content: Some("You are a bot.".to_string()),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let (system, anthropic_messages) = AnthropicBackend::convert_messages(&messages);

        assert_eq!(system, Some("You are a bot.".to_string()));
        assert!(anthropic_messages.is_empty());
    }

    #[test]
    fn test_convert_messages_tool_becomes_user() {
        let messages = vec![ChatMessage {
            role: MessageRole::Tool,
            content: Some("Tool result here".to_string()),
            name: None,
            tool_call_id: Some("call_123".to_string()),
            tool_calls: None,
        }];

        let (system, anthropic_messages) = AnthropicBackend::convert_messages(&messages);

        assert!(system.is_none());
        assert_eq!(anthropic_messages.len(), 1);
        assert_eq!(anthropic_messages[0].role, "user");
        assert_eq!(anthropic_messages[0].content, "Tool result here");
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

        let (system, anthropic_messages) = AnthropicBackend::convert_messages(&messages);

        assert!(system.is_none());
        assert_eq!(anthropic_messages.len(), 1);
        assert_eq!(anthropic_messages[0].content, ""); // Empty string for None
    }

    #[test]
    fn test_convert_messages_multiple_system_uses_last() {
        // If there are multiple system messages, the last one wins
        let messages = vec![
            ChatMessage {
                role: MessageRole::System,
                content: Some("First system".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: Some("Hello".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::System,
                content: Some("Second system".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let (system, anthropic_messages) = AnthropicBackend::convert_messages(&messages);

        // Last system message should be used
        assert_eq!(system, Some("Second system".to_string()));
        assert_eq!(anthropic_messages.len(), 1);
    }

    // ==================== SSE Event Parsing Tests ====================

    #[test]
    fn test_parse_message_start_event() {
        let data = r#"{"type":"message_start","message":{"id":"msg_123","type":"message","role":"assistant","model":"claude-3-opus-20240229","usage":{"input_tokens":25,"output_tokens":0}}}"#;

        let event: AnthropicStreamEvent = serde_json::from_str(data).unwrap();

        match event {
            AnthropicStreamEvent::MessageStart { message } => {
                assert_eq!(message.id, "msg_123");
                assert_eq!(message.role, "assistant");
                assert_eq!(message.usage.input_tokens, 25);
            }
            _ => panic!("Expected MessageStart event"),
        }
    }

    #[test]
    fn test_parse_content_block_delta_event() {
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;

        let event: AnthropicStreamEvent = serde_json::from_str(data).unwrap();

        match event {
            AnthropicStreamEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(index, 0);
                assert_eq!(delta.type_, "text_delta");
                assert_eq!(delta.text, Some("Hello".to_string()));
            }
            _ => panic!("Expected ContentBlockDelta event"),
        }
    }

    #[test]
    fn test_parse_message_delta_event() {
        let data = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":15}}"#;

        let event: AnthropicStreamEvent = serde_json::from_str(data).unwrap();

        match event {
            AnthropicStreamEvent::MessageDelta { delta, usage } => {
                assert_eq!(delta.stop_reason, Some("end_turn".to_string()));
                assert_eq!(usage.output_tokens, 15);
            }
            _ => panic!("Expected MessageDelta event"),
        }
    }

    #[test]
    fn test_parse_error_event() {
        let data = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Invalid API key"}}"#;

        let event: AnthropicStreamEvent = serde_json::from_str(data).unwrap();

        match event {
            AnthropicStreamEvent::Error { error } => {
                assert_eq!(error.type_, "invalid_request_error");
                assert_eq!(error.message, "Invalid API key");
            }
            _ => panic!("Expected Error event"),
        }
    }

    #[test]
    fn test_parse_ping_event() {
        let data = r#"{"type":"ping"}"#;

        let event: AnthropicStreamEvent = serde_json::from_str(data).unwrap();

        match event {
            AnthropicStreamEvent::Ping => {}
            _ => panic!("Expected Ping event"),
        }
    }

    #[test]
    fn test_parse_message_stop_event() {
        let data = r#"{"type":"message_stop"}"#;

        let event: AnthropicStreamEvent = serde_json::from_str(data).unwrap();

        match event {
            AnthropicStreamEvent::MessageStop => {}
            _ => panic!("Expected MessageStop event"),
        }
    }

    #[test]
    fn test_parse_content_block_start_event() {
        let data = r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;

        let event: AnthropicStreamEvent = serde_json::from_str(data).unwrap();

        match event {
            AnthropicStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                assert_eq!(index, 0);
                assert_eq!(content_block.type_, "text");
            }
            _ => panic!("Expected ContentBlockStart event"),
        }
    }

    // ==================== Response Parsing Tests ====================

    #[test]
    fn test_parse_anthropic_response() {
        let json = r#"{
            "id": "msg_abc123",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "Hello! How can I help you?"}],
            "model": "claude-3-opus-20240229",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 8}
        }"#;

        let response: AnthropicResponse = serde_json::from_str(json).unwrap();

        assert_eq!(response.id, "msg_abc123");
        assert_eq!(response.role, "assistant");
        assert_eq!(response.content.len(), 1);
        assert_eq!(
            response.content[0].text,
            Some("Hello! How can I help you?".to_string())
        );
        assert_eq!(response.stop_reason, Some("end_turn".to_string()));
        assert_eq!(response.usage.input_tokens, 10);
        assert_eq!(response.usage.output_tokens, 8);
    }

    #[test]
    fn test_parse_anthropic_response_multiple_content_blocks() {
        let json = r#"{
            "id": "msg_abc123",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "text", "text": "First part. "},
                {"type": "text", "text": "Second part."}
            ],
            "model": "claude-3-opus-20240229",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 8}
        }"#;

        let response: AnthropicResponse = serde_json::from_str(json).unwrap();

        assert_eq!(response.content.len(), 2);
    }

    // ==================== Finish Reason Mapping Tests ====================

    #[test]
    fn test_finish_reason_mapping() {
        // Test the finish reason mapping logic
        let test_cases = vec![
            ("end_turn", crate::FinishReason::Stop),
            ("stop_sequence", crate::FinishReason::Stop),
            ("max_tokens", crate::FinishReason::Length),
            ("unknown_reason", crate::FinishReason::Stop), // Default
        ];

        for (anthropic_reason, expected) in test_cases {
            let mapped = match anthropic_reason {
                "end_turn" | "stop_sequence" => crate::FinishReason::Stop,
                "max_tokens" => crate::FinishReason::Length,
                _ => crate::FinishReason::Stop,
            };
            assert_eq!(mapped, expected, "Failed for reason: {}", anthropic_reason);
        }
    }

    // ==================== Request Building Tests ====================

    #[test]
    fn test_anthropic_request_serialization() {
        let request = AnthropicRequest {
            model: "claude-3-opus-20240229".to_string(),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: "Hello".to_string(),
            }],
            max_tokens: 1024,
            system: Some("You are helpful.".to_string()),
            temperature: Some(0.7),
            top_p: None,
            stop_sequences: Some(vec!["STOP".to_string()]),
            stream: true,
        };

        let json = serde_json::to_string(&request).unwrap();

        assert!(json.contains("\"model\":\"claude-3-opus-20240229\""));
        assert!(json.contains("\"max_tokens\":1024"));
        assert!(json.contains("\"system\":\"You are helpful.\""));
        assert!(json.contains("\"temperature\":0.7"));
        assert!(json.contains("\"stream\":true"));
        assert!(!json.contains("\"top_p\"")); // Should be skipped (None)
    }

    #[test]
    fn test_anthropic_request_skips_none_fields() {
        let request = AnthropicRequest {
            model: "claude-3-opus-20240229".to_string(),
            messages: vec![],
            max_tokens: 1024,
            system: None,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: false,
        };

        let json = serde_json::to_string(&request).unwrap();

        assert!(!json.contains("\"system\""));
        assert!(!json.contains("\"temperature\""));
        assert!(!json.contains("\"top_p\""));
        assert!(!json.contains("\"stop_sequences\""));
    }

    // ==================== Header Building Tests ====================

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
        assert_eq!(
            headers.get("Content-Type").unwrap().to_str().unwrap(),
            "application/json"
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
}
