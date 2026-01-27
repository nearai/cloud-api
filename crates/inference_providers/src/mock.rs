//! Mock implementation of InferenceProvider for testing
//!
//! This module provides a mock provider that generates realistic responses
//! without requiring external dependencies like VLLM.

use crate::{
    AttestationError, AudioTranscriptionError, AudioTranscriptionParams,
    AudioTranscriptionResponse, ChatChoice, ChatCompletionChunk, ChatCompletionParams,
    ChatCompletionResponse, ChatCompletionResponseChoice, ChatCompletionResponseWithBytes,
    ChatDelta, ChatResponseMessage, ChatSignature, CompletionChunk, CompletionError,
    CompletionParams, FinishReason, FunctionCallDelta, ImageData, ImageGenerationError,
    ImageGenerationParams, ImageGenerationResponse, ImageGenerationResponseWithBytes,
    ListModelsError, MessageRole, ModelInfo, ModelsResponse, SSEEvent, StreamChunk,
    StreamingResult, TokenUsage, ToolCallDelta, TranscriptionSegment, TranscriptionWord,
};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock};

fn compute_sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// Hash pair for signature generation
#[derive(Clone, Debug)]
struct SignatureHashes {
    request_hash: String,
    response_hash: String,
}

/// Request matcher for conditional responses
#[derive(Clone)]
pub enum RequestMatcher {
    /// Match any request
    Any,
    /// Match requests with exact prompt text (checks all text content in messages)
    /// Timestamps are automatically normalized to [TIME] for matching across test runs
    ExactPrompt(String),
    /// Match requests with exact prompt text and specific tool names
    /// Checks that all specified tool names are present in the request tools
    PromptWithTools {
        prompt: String,
        tool_names: Vec<String>,
    },
}

impl RequestMatcher {
    /// Check if this matcher matches the given parameters
    pub fn matches(&self, params: &ChatCompletionParams) -> bool {
        match self {
            Self::Any => true,
            Self::ExactPrompt(prompt) => {
                // Extract all text from messages and check if it equals the prompt
                let all_text = Self::extract_text_from_messages(&params.messages);
                // Normalize timestamps in both to allow matching regardless of when request was made
                let normalized_text = Self::normalize_timestamps(&all_text);
                let normalized_prompt = Self::normalize_timestamps(prompt);
                normalized_text == normalized_prompt
            }
            Self::PromptWithTools { prompt, tool_names } => {
                // Check prompt matches
                let all_text = Self::extract_text_from_messages(&params.messages);
                let normalized_text = Self::normalize_timestamps(&all_text);
                let normalized_prompt = Self::normalize_timestamps(prompt);
                if normalized_text != normalized_prompt {
                    return false;
                }

                // Check all expected tools are present
                let request_tool_names: Vec<&str> = params
                    .tools
                    .as_ref()
                    .map(|tools| tools.iter().map(|t| t.function.name.as_str()).collect())
                    .unwrap_or_default();

                tool_names
                    .iter()
                    .all(|name| request_tool_names.contains(&name.as_str()))
            }
        }
    }

    /// Extract all text content from messages (handles serde_json::Value content)
    fn extract_text_from_messages(messages: &[crate::ChatMessage]) -> String {
        messages
            .iter()
            .filter_map(|msg| msg.content.as_ref())
            .filter_map(|c| match c {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Array(parts) => {
                    // Extract text from content parts array
                    let text: String = parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join(" ");
                    if text.is_empty() {
                        None
                    } else {
                        Some(text)
                    }
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Normalize timestamps in text by replacing ISO 8601 datetime patterns with [TIME]
    /// This allows exact matching of prompts regardless of when they were sent
    pub fn normalize_timestamps(text: &str) -> String {
        use regex::Regex;
        // Match ISO 8601 format: 2025-12-02T21:59:30.374311+00:00
        // Also match the human-readable part: (Tuesday, December 02, 2025 at 21:59:30 UTC)
        let iso_regex =
            Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+\+\d{2}:\d{2}").unwrap();
        let human_regex =
            Regex::new(r"\([A-Za-z]+, [A-Za-z]+ \d{2}, \d{4} at \d{2}:\d{2}:\d{2} UTC\)").unwrap();

        let text = iso_regex.replace_all(text, "[TIME]").to_string();
        let text = human_regex.replace_all(&text, "[TIME]").to_string();
        text
    }
}

/// A tool call to be included in the response
#[derive(Clone, Debug)]
pub struct ToolCall {
    /// Tool name (e.g., "server:tool_name" for MCP tools)
    pub name: String,
    /// JSON arguments for the tool
    pub arguments: String,
}

impl ToolCall {
    /// Create a new tool call
    pub fn new(name: impl Into<String>, arguments: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            arguments: arguments.into(),
        }
    }
}

/// Template for generating responses
#[derive(Clone)]
pub struct ResponseTemplate {
    content: String,
    reasoning_content: Option<String>,
    /// Simulate client disconnect after N chunks (stream ends without final usage chunk)
    disconnect_after_chunks: Option<usize>,
    /// Tool calls to include in the response
    tool_calls: Option<Vec<ToolCall>>,
}

impl ResponseTemplate {
    /// Create a new response template with the given content
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            reasoning_content: None,
            disconnect_after_chunks: None,
            tool_calls: None,
        }
    }

    /// Set reasoning content for this template
    pub fn with_reasoning(mut self, reasoning: impl Into<String>) -> Self {
        self.reasoning_content = Some(reasoning.into());
        self
    }

    /// Simulate client disconnect after N chunks
    /// The stream will be truncated and end without the final usage chunk
    pub fn with_disconnect_after(mut self, chunks: usize) -> Self {
        self.disconnect_after_chunks = Some(chunks);
        self
    }

    /// Add tool calls to this response
    pub fn with_tool_calls(mut self, tool_calls: Vec<ToolCall>) -> Self {
        self.tool_calls = Some(tool_calls);
        self
    }

    /// Generate a ChatCompletionResponse from this template
    fn generate_response(
        &self,
        id: String,
        created: i64,
        model: String,
        input_tokens: i32,
    ) -> ChatCompletionResponse {
        // Calculate output tokens as word count of content
        let output_tokens = self.content.split_whitespace().count() as i32;

        // Convert tool calls if present
        let tool_calls = self.tool_calls.as_ref().map(|calls| {
            calls
                .iter()
                .enumerate()
                .map(|(i, tc)| crate::ToolCall {
                    id: Some(format!("call_{}", i)),
                    type_: Some("function".to_string()),
                    index: None,
                    function: crate::FunctionCall {
                        name: Some(tc.name.clone()),
                        arguments: Some(tc.arguments.clone()),
                    },
                })
                .collect()
        });

        let finish_reason = if tool_calls.is_some() {
            "tool_calls"
        } else {
            "stop"
        };

        ChatCompletionResponse {
            id,
            object: "chat.completion".to_string(),
            created,
            model,
            choices: vec![ChatCompletionResponseChoice {
                index: 0,
                message: ChatResponseMessage {
                    role: MessageRole::Assistant,
                    content: if self.content.is_empty() {
                        None
                    } else {
                        Some(self.content.clone())
                    },
                    refusal: None,
                    annotations: None,
                    audio: None,
                    function_call: None,
                    tool_calls,
                    reasoning_content: self.reasoning_content.clone(),
                    reasoning: self.reasoning_content.clone(),
                },
                logprobs: None,
                finish_reason: Some(finish_reason.to_string()),
                token_ids: None,
            }],
            service_tier: None,
            system_fingerprint: None,
            usage: TokenUsage::new(input_tokens, output_tokens),
            prompt_logprobs: None,
            prompt_token_ids: None,
            kv_transfer_params: None,
        }
    }

    /// Generate streaming chunks from this template
    /// Streams word-by-word (split by spaces) for more realistic tokenization
    /// Includes cumulative usage in every chunk (simulates continuous_usage_stats: true)
    fn generate_chunks(
        &self,
        id: String,
        created: i64,
        model: String,
        input_tokens: i32,
    ) -> Vec<ChatCompletionChunk> {
        let mut chunks = Vec::new();
        let mut output_token_count = 0;

        // Stream reasoning content word by word if present
        if let Some(reasoning) = &self.reasoning_content {
            let words: Vec<&str> = reasoning.split(' ').collect();
            for (i, word) in words.iter().enumerate() {
                output_token_count += 1;
                let word_with_space = if i == 0 {
                    word.to_string()
                } else {
                    format!(" {}", word)
                };
                chunks.push(ChatCompletionChunk {
                    id: id.clone(),
                    object: "chat.completion.chunk".to_string(),
                    created,
                    model: model.clone(),
                    system_fingerprint: None,
                    choices: vec![ChatChoice {
                        index: 0,
                        delta: Some(ChatDelta {
                            role: None,
                            content: None,
                            name: None,
                            tool_call_id: None,
                            tool_calls: None,
                            reasoning_content: Some(word_with_space.clone()),
                            reasoning: Some(word_with_space),
                        }),
                        logprobs: None,
                        finish_reason: None,
                        token_ids: None,
                    }],
                    usage: Some(TokenUsage::new(input_tokens, output_token_count)),
                    prompt_token_ids: None,
                    modality: None,
                });
            }
        }

        // Stream content if present (not empty)
        if !self.content.is_empty() {
            let words: Vec<&str> = self.content.split(' ').collect();
            for (i, word) in words.iter().enumerate() {
                output_token_count += 1;
                let word_with_space = if i == 0 {
                    word.to_string()
                } else {
                    format!(" {}", word)
                };
                let is_last_content = i == words.len() - 1;
                let finish_reason = if is_last_content && self.tool_calls.is_none() {
                    Some(FinishReason::Stop)
                } else {
                    None
                };
                chunks.push(ChatCompletionChunk {
                    id: id.clone(),
                    object: "chat.completion.chunk".to_string(),
                    created,
                    model: model.clone(),
                    system_fingerprint: None,
                    choices: vec![ChatChoice {
                        index: 0,
                        delta: Some(ChatDelta {
                            role: None,
                            content: Some(word_with_space),
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
                    usage: Some(TokenUsage::new(input_tokens, output_token_count)),
                    prompt_token_ids: None,
                    modality: None,
                });
            }
        }

        // Stream tool calls if present
        if let Some(tool_calls) = &self.tool_calls {
            for (idx, tc) in tool_calls.iter().enumerate() {
                let tool_call_id = format!("call_{}", idx);
                let is_last_tool = idx == tool_calls.len() - 1;

                // First chunk: tool call start with name
                output_token_count += 1;
                chunks.push(ChatCompletionChunk {
                    id: id.clone(),
                    object: "chat.completion.chunk".to_string(),
                    created,
                    model: model.clone(),
                    system_fingerprint: None,
                    choices: vec![ChatChoice {
                        index: 0,
                        delta: Some(ChatDelta {
                            role: None,
                            content: None,
                            name: None,
                            tool_call_id: None,
                            tool_calls: Some(vec![ToolCallDelta {
                                id: Some(tool_call_id),
                                type_: Some("function".to_string()),
                                index: Some(idx as i64),
                                function: Some(FunctionCallDelta {
                                    name: Some(tc.name.clone()),
                                    arguments: None,
                                }),
                            }]),
                            reasoning_content: None,
                            reasoning: None,
                        }),
                        logprobs: None,
                        finish_reason: None,
                        token_ids: None,
                    }],
                    usage: Some(TokenUsage::new(input_tokens, output_token_count)),
                    prompt_token_ids: None,
                    modality: None,
                });

                // Stream arguments split by spaces (like content)
                let arg_parts: Vec<&str> = tc.arguments.split(' ').collect();
                for (i, part) in arg_parts.iter().enumerate() {
                    output_token_count += 1;
                    let part_with_space = if i == 0 {
                        part.to_string()
                    } else {
                        format!(" {}", part)
                    };
                    let is_last_part = i == arg_parts.len() - 1;
                    let finish_reason = if is_last_part && is_last_tool {
                        Some(FinishReason::ToolCalls)
                    } else {
                        None
                    };
                    chunks.push(ChatCompletionChunk {
                        id: id.clone(),
                        object: "chat.completion.chunk".to_string(),
                        created,
                        model: model.clone(),
                        system_fingerprint: None,
                        choices: vec![ChatChoice {
                            index: 0,
                            delta: Some(ChatDelta {
                                role: None,
                                content: None,
                                name: None,
                                tool_call_id: None,
                                tool_calls: Some(vec![ToolCallDelta {
                                    id: None,
                                    type_: None,
                                    index: Some(idx as i64),
                                    function: Some(FunctionCallDelta {
                                        name: None,
                                        arguments: Some(part_with_space),
                                    }),
                                }]),
                                reasoning_content: None,
                                reasoning: None,
                            }),
                            logprobs: None,
                            finish_reason,
                            token_ids: None,
                        }],
                        usage: Some(TokenUsage::new(input_tokens, output_token_count)),
                        prompt_token_ids: None,
                        modality: None,
                    });
                }
            }
        }

        // Final chunk with final usage
        chunks.push(ChatCompletionChunk {
            id,
            object: "chat.completion.chunk".to_string(),
            created,
            model,
            system_fingerprint: None,
            choices: vec![],
            usage: Some(TokenUsage::new(input_tokens, output_token_count)),
            prompt_token_ids: None,
            modality: None,
        });

        chunks
    }
}

/// Configuration for a single expectation
struct MockExpectation {
    matcher: RequestMatcher,
    response: ResponseTemplate,
}

/// Configuration for the mock provider
struct MockConfig {
    expectations: Vec<MockExpectation>,
    default_response: ResponseTemplate,
}

/// Builder for configuring a single expectation
pub struct MockExpectationBuilder {
    config: Arc<Mutex<MockConfig>>,
    matcher: RequestMatcher,
}

impl MockExpectationBuilder {
    /// Set the response for this expectation
    pub async fn respond_with(self, response: ResponseTemplate) {
        let mut config = self.config.lock().await;
        config.expectations.push(MockExpectation {
            matcher: self.matcher,
            response,
        });
    }
}

/// Mock provider that implements InferenceProvider for testing
pub struct MockProvider {
    /// List of available mock models
    models: Vec<ModelInfo>,
    /// Map of chat_id to (request_hash, response_hash) for signature generation
    signature_hashes: Arc<RwLock<std::collections::HashMap<String, SignatureHashes>>>,
    /// Configuration for conditional responses (thread-safe)
    config: Arc<Mutex<MockConfig>>,
}

impl MockProvider {
    /// Create a new mock provider with default models
    pub fn new() -> Self {
        let models = vec![ModelInfo {
            id: "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string(),
            object: "model".to_string(),
            created: 1762544256,
            owned_by: "vllm".to_string(),
        }];
        Self {
            models,
            signature_hashes: Arc::new(RwLock::new(std::collections::HashMap::new())),
            config: Arc::new(Mutex::new(MockConfig {
                expectations: Vec::new(),
                default_response: ResponseTemplate::new("1. 2. 3."),
            })),
        }
    }

    /// Create a new mock provider that accepts any model (useful for tests)
    /// This bypasses model validation to accept any model name
    pub fn new_accept_all() -> Self {
        // Return empty models list - we'll override is_valid_model to always return true
        Self {
            models: vec![],
            signature_hashes: Arc::new(RwLock::new(std::collections::HashMap::new())),
            config: Arc::new(Mutex::new(MockConfig {
                expectations: Vec::new(),
                default_response: ResponseTemplate::new("1. 2. 3."),
            })),
        }
    }

    /// Create a mock provider with custom model list
    pub fn with_models(models: Vec<ModelInfo>) -> Self {
        Self {
            models,
            signature_hashes: Arc::new(RwLock::new(std::collections::HashMap::new())),
            config: Arc::new(Mutex::new(MockConfig {
                expectations: Vec::new(),
                default_response: ResponseTemplate::new("1. 2. 3."),
            })),
        }
    }

    /// Register request and response hashes for a chat_id
    /// This allows MockProvider to return signatures in the correct format "request_hash:response_hash"
    pub async fn register_signature_hashes(
        &self,
        chat_id: String,
        request_hash: String,
        response_hash: String,
    ) {
        let mut hashes = self.signature_hashes.write().await;
        hashes.insert(
            chat_id,
            SignatureHashes {
                request_hash,
                response_hash,
            },
        );
    }

    /// Add a conditional response for a specific matcher
    pub fn when(&self, matcher: RequestMatcher) -> MockExpectationBuilder {
        MockExpectationBuilder {
            config: self.config.clone(),
            matcher,
        }
    }

    /// Set the default response for requests that don't match any expectation
    pub async fn set_default_response(&self, response: ResponseTemplate) {
        let mut config = self.config.lock().await;
        config.default_response = response;
    }

    /// Generate a completion ID
    fn generate_id(&self) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        std::time::SystemTime::now().hash(&mut hasher);
        format!("cmpl-{:x}", hasher.finish())
    }

    /// Generate a chat completion ID
    fn generate_chat_id(&self) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        std::time::SystemTime::now().hash(&mut hasher);
        format!("chatcmpl-{:x}", hasher.finish())
    }

    /// Get current timestamp
    fn current_timestamp(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// Check if a model is valid
    fn is_valid_model(&self, model: &str) -> bool {
        // If models list is empty, accept all models (for accept_all mode)
        if self.models.is_empty() {
            return true;
        }
        self.models.iter().any(|m| m.id == model)
    }

    /// Generate streaming text completion chunks
    fn generate_text_chunks(&self, params: &CompletionParams) -> Vec<CompletionChunk> {
        let id = self.generate_id();
        let created = self.current_timestamp();
        let model = params.model.clone();

        let content_parts = vec![
            " Paris", ".", " The", " capital", " of", " Italy", " is", " Rome", ".", " The",
            " capital", " of", " Spain", " is", " Madrid", ".", " The", " capital", " of",
            " Germany",
        ];

        let mut chunks = Vec::new();

        for (i, part) in content_parts.iter().enumerate() {
            chunks.push(CompletionChunk {
                id: id.clone(),
                object: "text_completion".to_string(),
                created,
                model: model.clone(),
                system_fingerprint: None,
                choices: vec![crate::TextChoice {
                    index: 0,
                    text: part.to_string(),
                    logprobs: None,
                    finish_reason: if i == content_parts.len() - 1 {
                        Some(FinishReason::Length)
                    } else {
                        None
                    },
                }],
                usage: None,
            });
        }

        // Final chunk with usage
        chunks.push(CompletionChunk {
            id: id.clone(),
            object: "text_completion".to_string(),
            created,
            model: model.clone(),
            system_fingerprint: None,
            choices: vec![],
            usage: Some(TokenUsage::new(6, 20)),
        });

        chunks
    }
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl crate::InferenceProvider for MockProvider {
    async fn models(&self) -> Result<ModelsResponse, ListModelsError> {
        Ok(ModelsResponse {
            object: "list".to_string(),
            data: self.models.clone(),
        })
    }

    async fn chat_completion_stream(
        &self,
        params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<StreamingResult, CompletionError> {
        // Check for invalid model
        if !self.is_valid_model(&params.model) {
            return Err(CompletionError::CompletionError(format!(
                "HTTP 404 Not Found: {{\"error\":{{\"message\":\"The model `{}` does not exist.\",\"type\":\"NotFoundError\",\"param\":null,\"code\":404}}}}",
                params.model
            )));
        }

        // Check for matching expectation
        let response_template = {
            let config = self.config.lock().await;
            config
                .expectations
                .iter()
                .find(|exp| exp.matcher.matches(&params))
                .map(|exp| exp.response.clone())
                .unwrap_or_else(|| config.default_response.clone())
        };

        // Calculate input tokens from messages (rough estimate: 1 word ≈ 1 token)
        let input_tokens: i32 = params
            .messages
            .iter()
            .filter_map(|m| m.content.as_ref())
            .map(Self::count_tokens_in_content)
            .sum();
        // Ensure at least some input tokens for very short messages
        let input_tokens = input_tokens.max(6);

        // Generate chunks from the matched response template
        // Always use the template - it now supports tool calls
        let id = self.generate_chat_id();
        let created = self.current_timestamp();
        let model = params.model.clone();
        let mut chunks = response_template.generate_chunks(id, created, model, input_tokens);

        // If disconnect simulation is enabled, truncate chunks (simulates client disconnect)
        // The stream will end abruptly without the final usage chunk
        if let Some(disconnect_at) = response_template.disconnect_after_chunks {
            chunks.truncate(disconnect_at);
        }

        // Register signature hashes for this chat_id.
        // response_hash is computed over SSE bytes in the same format as returned by the API:
        // concatenated "data: {json}\n\n" lines plus the final "data: [DONE]\n\n" terminator.
        let chat_id_opt = chunks.first().map(|c| c.id.clone());
        if let Some(chat_id) = chat_id_opt {
            let mut accumulated: Vec<u8> = Vec::new();
            for chunk in &chunks {
                let json = serde_json::to_value(chunk).map_err(|e| {
                    CompletionError::CompletionError(format!(
                        "Failed to serialize mock chunk to JSON for hashing: {e}"
                    ))
                })?;
                let raw_bytes = Self::sse_data_static(&json);
                accumulated.extend_from_slice(&raw_bytes);
            }
            if response_template.disconnect_after_chunks.is_none() {
                accumulated.extend_from_slice(b"data: [DONE]\n\n");
            }
            let response_hash = compute_sha256_hex(&accumulated);
            self.register_signature_hashes(chat_id, request_hash, response_hash)
                .await;
        }

        // Convert chunks to SSE stream
        let stream = stream::iter(chunks.into_iter().map(move |chunk| {
            let json = serde_json::to_value(&chunk).unwrap();
            let raw_bytes = Self::sse_data_static(&json);
            Ok(SSEEvent {
                raw_bytes,
                chunk: StreamChunk::Chat(chunk),
            })
        }));

        Ok(Box::pin(stream))
    }

    async fn chat_completion(
        &self,
        params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        // Check for invalid model
        if !self.is_valid_model(&params.model) {
            return Err(CompletionError::CompletionError(format!(
                "HTTP 404 Not Found: {{\"error\":{{\"message\":\"The model `{}` does not exist.\",\"type\":\"NotFoundError\",\"param\":null,\"code\":404}}}}",
                params.model
            )));
        }

        let id = self.generate_chat_id();
        let created = self.current_timestamp();
        let model = params.model.clone();

        // Find matching expectation in config
        let response_template = {
            let config = self.config.lock().await;
            config
                .expectations
                .iter()
                .find(|exp| exp.matcher.matches(&params))
                .map(|exp| exp.response.clone())
                .unwrap_or_else(|| config.default_response.clone())
        };

        // Calculate input tokens from messages (rough estimate: 1 word ≈ 1 token)
        let input_tokens: i32 = params
            .messages
            .iter()
            .filter_map(|m| m.content.as_ref())
            .map(Self::count_tokens_in_content)
            .sum();
        // Ensure at least some input tokens for very short messages
        let input_tokens = input_tokens.max(6);

        // Keep a stable chat_id for both the response and signature registration.
        let response =
            response_template.generate_response(id.clone(), created, model, input_tokens);

        let raw_bytes = serde_json::to_vec(&response)
            .map_err(|e| CompletionError::CompletionError(format!("Failed to serialize: {e}")))?;

        // Register signature hashes for non-streaming chat completions (hash of exact JSON bytes).
        let response_hash = compute_sha256_hex(&raw_bytes);
        self.register_signature_hashes(id, request_hash, response_hash)
            .await;

        Ok(ChatCompletionResponseWithBytes {
            response,
            raw_bytes,
        })
    }

    async fn text_completion_stream(
        &self,
        params: CompletionParams,
    ) -> Result<StreamingResult, CompletionError> {
        // Check for invalid model
        if !self.is_valid_model(&params.model) {
            return Err(CompletionError::CompletionError(format!(
                "HTTP 404 Not Found: {{\"error\":{{\"message\":\"The model `{}` does not exist.\",\"type\":\"NotFoundError\",\"param\":null,\"code\":404}}}}",
                params.model
            )));
        }

        let chunks = self.generate_text_chunks(&params);

        // Convert chunks to SSE stream
        let stream = stream::iter(chunks.into_iter().map(move |chunk| {
            let json = serde_json::to_value(&chunk).unwrap();
            let raw_bytes = Self::sse_data_static(&json);
            Ok(SSEEvent {
                raw_bytes,
                chunk: StreamChunk::Text(chunk),
            })
        }));

        Ok(Box::pin(stream))
    }

    async fn image_generation(
        &self,
        params: ImageGenerationParams,
        _request_hash: String,
    ) -> Result<ImageGenerationResponseWithBytes, ImageGenerationError> {
        // Check for invalid model
        if !self.is_valid_model(&params.model) {
            return Err(ImageGenerationError::GenerationError(format!(
                "The model `{}` does not exist.",
                params.model
            )));
        }

        let n = params.n.unwrap_or(1);
        let created = self.current_timestamp();

        // Generate mock image data
        let data: Vec<ImageData> = (0..n)
            .map(|i| ImageData {
                b64_json: Some(format!("mock_base64_image_data_{}", i)),
                url: None,
                revised_prompt: Some(params.prompt.clone()),
            })
            .collect();

        let response = ImageGenerationResponse {
            id: format!("img-{}", self.generate_id()),
            created,
            data,
        };

        // Serialize to raw bytes for TEE verification consistency
        let raw_bytes = serde_json::to_vec(&response)
            .map_err(|e| ImageGenerationError::GenerationError(e.to_string()))?;

        Ok(ImageGenerationResponseWithBytes {
            response,
            raw_bytes,
        })
    }

    async fn get_signature(
        &self,
        chat_id: &str,
        signing_algo: Option<String>,
    ) -> Result<ChatSignature, CompletionError> {
        let signing_algo = signing_algo.unwrap_or_else(|| "ecdsa".to_string());

        // Check if we have registered hashes for this chat_id
        let hashes = self.signature_hashes.read().await;
        if let Some(sig_hashes) = hashes.get(chat_id) {
            // Return signature in the correct format "request_hash:response_hash"
            let signature_text =
                format!("{}:{}", sig_hashes.request_hash, sig_hashes.response_hash);
            // Generate a deterministic mock signature based on the hashes and algorithm
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            signature_text.hash(&mut hasher);
            signing_algo.hash(&mut hasher);
            let sig_hash = format!("{:x}", hasher.finish());

            Ok(ChatSignature {
                text: signature_text,
                signature: format!("0x{sig_hash}"),
                signing_address: "mock-address".to_string(),
                signing_algo,
            })
        } else {
            // Fallback to old mock signature format if hashes not registered
            Ok(ChatSignature {
                text: format!("mock-signature-text-{chat_id}"),
                signature: format!("mock-signature-{chat_id}"),
                signing_address: "mock-address".to_string(),
                signing_algo,
            })
        }
    }

    async fn get_attestation_report(
        &self,
        model: String,
        signing_algo: Option<String>,
        _nonce: Option<String>,
        _signing_address: Option<String>,
    ) -> Result<serde_json::Map<String, serde_json::Value>, AttestationError> {
        let mut report = serde_json::Map::new();
        report.insert("model".to_string(), serde_json::Value::String(model));
        report.insert(
            "attestation".to_string(),
            serde_json::Value::String("mock-attestation".to_string()),
        );

        // Include signing_public_key for encryption tests
        // ECDSA: 128 hex chars (64 bytes) - uncompressed point (x and y coordinates, 32 bytes each)
        // Ed25519: 64 hex chars (32 bytes) - public key bytes
        let mock_signing_public_key = match signing_algo.as_deref() {
            Some("ecdsa") => "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            Some("ed25519") => "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
            _ => "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", // default to ecdsa format (128 hex chars)
        };
        report.insert(
            "signing_public_key".to_string(),
            serde_json::Value::String(mock_signing_public_key.to_string()),
        );

        Ok(report)
    }

    async fn audio_transcription(
        &self,
        params: AudioTranscriptionParams,
        _request_hash: String,
    ) -> Result<AudioTranscriptionResponse, AudioTranscriptionError> {
        // Mock implementation returns simple transcription with mock timing
        let file_size_kb = params.file_bytes.len() / 1024;
        let mock_duration = (file_size_kb as f64) * 0.1; // Assume ~0.1s per KB

        Ok(AudioTranscriptionResponse {
            text: format!("Mock transcription for file: {}", params.filename),
            duration: Some(mock_duration),
            language: params.language.or(Some("en".to_string())),
            segments: Some(vec![TranscriptionSegment {
                id: 0,
                seek: 0,
                start: 0.0,
                end: mock_duration,
                text: format!("Mock transcription for file: {}", params.filename),
                tokens: vec![50364, 15947],
                temperature: 0.0,
                avg_logprob: Some(-0.5),
                compression_ratio: Some(1.0),
                no_speech_prob: Some(0.0),
            }]),
            words: Some(vec![
                TranscriptionWord {
                    word: "Mock".to_string(),
                    start: 0.0,
                    end: 0.5,
                },
                TranscriptionWord {
                    word: "transcription".to_string(),
                    start: 0.5,
                    end: 1.5,
                },
            ]),
            id: None,
        })
    }
}

impl MockProvider {
    /// Generate SSE bytes from a JSON value (static method for use in closures)
    fn sse_data_static(json: &serde_json::Value) -> Bytes {
        let json_str = json.to_string();
        let sse_line = format!("data: {json_str}\n\n");
        Bytes::from(sse_line)
    }

    /// Count tokens in content (handles serde_json::Value)
    fn count_tokens_in_content(content: &serde_json::Value) -> i32 {
        match content {
            serde_json::Value::String(s) => s.split_whitespace().count() as i32,
            serde_json::Value::Array(parts) => {
                // Sum token counts from text parts
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .map(|t| t.split_whitespace().count() as i32)
                    .sum()
            }
            _ => 0,
        }
    }
}
