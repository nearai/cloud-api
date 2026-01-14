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
        let mut system_instruction = None;
        let mut contents = Vec::new();

        for msg in messages {
            match msg.role {
                MessageRole::System => {
                    // Gemini uses systemInstruction
                    if let Some(content) = &msg.content {
                        system_instruction = Some(GeminiSystemInstruction {
                            parts: vec![GeminiPart {
                                text: content.clone(),
                            }],
                        });
                    }
                }
                MessageRole::User => {
                    contents.push(GeminiContent {
                        role: "user".to_string(),
                        parts: vec![GeminiPart {
                            text: msg.content.clone().unwrap_or_default(),
                        }],
                    });
                }
                MessageRole::Assistant => {
                    // Gemini uses "model" role for assistant
                    contents.push(GeminiContent {
                        role: "model".to_string(),
                        parts: vec![GeminiPart {
                            text: msg.content.clone().unwrap_or_default(),
                        }],
                    });
                }
                MessageRole::Tool => {
                    // Tool results go as user messages
                    contents.push(GeminiContent {
                        role: "user".to_string(),
                        parts: vec![GeminiPart {
                            text: msg.content.clone().unwrap_or_default(),
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
    model: String,
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
            model,
            created: chrono::Utc::now().timestamp(),
            chunk_index: 0,
            accumulated_prompt_tokens: 0,
            accumulated_completion_tokens: 0,
        }
    }

    fn parse_response(&mut self, data: &str) -> Result<Option<StreamChunk>, CompletionError> {
        let response: GeminiResponse = serde_json::from_str(data)
            .map_err(|e| CompletionError::InvalidResponse(format!("Failed to parse Gemini response: {e}")))?;

        if response.candidates.is_empty() {
            return Ok(None);
        }

        let candidate = &response.candidates[0];
        let text = candidate
            .content
            .parts
            .iter()
            .filter_map(|p| Some(p.text.clone()))
            .collect::<Vec<_>>()
            .join("");

        // Update token counts
        self.accumulated_prompt_tokens = response.usage_metadata.prompt_token_count;
        self.accumulated_completion_tokens = response.usage_metadata.candidates_token_count;

        let finish_reason = candidate.finish_reason.as_ref().map(|r| match r.as_str() {
            "STOP" => crate::FinishReason::Stop,
            "MAX_TOKENS" => crate::FinishReason::Length,
            "SAFETY" => crate::FinishReason::ContentFilter,
            _ => crate::FinishReason::Stop,
        });

        let is_first = self.chunk_index == 0;
        self.chunk_index += 1;

        let chunk = ChatCompletionChunk {
            id: format!("gemini-{}", self.created),
            object: "chat.completion.chunk".to_string(),
            created: self.created,
            model: self.model.clone(),
            system_fingerprint: None,
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
        // Gemini API URL format: {base_url}/models/{model}:streamGenerateContent?key={api_key}
        let url = format!(
            "{}/models/{}:streamGenerateContent?key={}&alt=sse",
            config.base_url, model, config.api_key
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
        // Gemini API URL format: {base_url}/models/{model}:generateContent?key={api_key}
        let url = format!(
            "{}/models/{}:generateContent?key={}",
            config.base_url, model, config.api_key
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

        let gemini_response: GeminiResponse = serde_json::from_slice(&raw_bytes)
            .map_err(|e| CompletionError::CompletionError(format!("Failed to parse response: {e}")))?;

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
            .filter_map(|p| Some(p.text.clone()))
            .collect::<Vec<_>>()
            .join("");

        let openai_response = ChatCompletionResponse {
            id: format!("gemini-{}", chrono::Utc::now().timestamp()),
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
                finish_reason: candidate.finish_reason.clone(),
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
        let serialized_bytes = serde_json::to_vec(&openai_response)
            .map_err(|e| CompletionError::CompletionError(format!("Failed to serialize response: {e}")))?;

        Ok(ChatCompletionResponseWithBytes {
            response: openai_response,
            raw_bytes: serialized_bytes,
        })
    }
}
