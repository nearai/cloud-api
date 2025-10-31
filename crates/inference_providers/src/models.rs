use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// Delta message in streaming chat completions
/// All fields are optional as they may not be present in every chunk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<MessageRole>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

/// Tool call in a message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Tool call ID (optional in streaming mode where it may come in a later chunk)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Tool type (optional in streaming mode where it may come in a later chunk)
    #[serde(rename = "type")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    pub function: FunctionCall,
    /// Index of the tool call in streaming responses (for tracking multiple parallel tool calls)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<i64>,
}

/// Function call details
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    /// Function name (optional in streaming mode where it may come in a later chunk)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub arguments: Option<String>,
}

/// Tool definition for available tools
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub type_: String,
    pub function: FunctionDefinition,
}

/// Function definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: Option<String>,
    pub parameters: serde_json::Value,
}

/// Response format specification
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponseFormat {
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "json_object")]
    JsonObject,
    #[serde(rename = "json_schema")]
    JsonSchema { json_schema: JsonSchema },
}

/// JSON schema specification for structured outputs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonSchema {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// Tool choice specification
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolChoice {
    String(String), // "none", "auto", "required"
    Function {
        #[serde(rename = "type")]
        type_: String, // "function"
        function: FunctionChoice,
    },
}

/// Function choice specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionChoice {
    pub name: String,
}

/// Streaming options
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamOptions {
    /// Whether to include usage statistics in the final chunk
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_usage: Option<bool>,
}

/// Parameters for chat completion requests (matches OpenAI API)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionParams {
    /// Model ID to use for the completion
    pub model: String,

    /// List of messages comprising the conversation so far
    pub messages: Vec<ChatMessage>,

    /// Maximum number of completion tokens to generate
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<i64>,

    /// Legacy parameter - use max_completion_tokens instead
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,

    /// Sampling temperature between 0 and 2
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    /// Nucleus sampling parameter (0-1)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,

    /// Number of chat completion choices to generate
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<i64>,

    /// Whether to stream back partial progress
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,

    /// Stop sequences (up to 4)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,

    /// Frequency penalty (-2.0 to 2.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,

    /// Presence penalty (-2.0 to 2.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,

    /// Logit bias for specific tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logit_bias: Option<serde_json::Map<String, serde_json::Value>>,

    /// Whether to return log probabilities
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<bool>,

    /// Number of most likely tokens to return at each position
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<i64>,

    /// Unique identifier for the end-user
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,

    /// Response format specification
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,

    /// Random seed for deterministic sampling
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,

    /// Tools that the model may call
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,

    /// Controls which tool is called by the model
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,

    /// Whether to enable parallel function calling
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,

    /// Metadata for the request (up to 16 key-value pairs)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,

    /// Whether to store the output for model distillation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,

    /// Streaming options
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,

    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

/// Parameters for text completion requests (legacy OpenAI API)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionParams {
    /// Model ID to use for completion
    pub model: String,

    /// Text prompt to complete
    pub prompt: String,

    /// Maximum number of tokens to generate
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,

    /// Sampling temperature (0-2)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    /// Nucleus sampling (0-1)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,

    /// Number of completions to generate
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<i64>,

    /// Whether to stream partial progress
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,

    /// Stop sequences
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,

    /// Frequency penalty (-2.0 to 2.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,

    /// Presence penalty (-2.0 to 2.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,

    /// Logit bias for specific tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logit_bias: Option<serde_json::Map<String, serde_json::Value>>,

    /// Include log probabilities for N most likely tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<i64>,

    /// Echo the prompt in the completion
    #[serde(skip_serializing_if = "Option::is_none")]
    pub echo: Option<bool>,

    /// Generate best_of completions server-side
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_of: Option<i64>,

    /// Random seed for deterministic sampling
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,

    /// Unique identifier for end-user
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,

    /// Text suffix for completion
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,

    /// Streaming options
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ContentFilter,
    ToolCalls,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub total_tokens: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<serde_json::Value>,
}

impl TokenUsage {
    pub fn new(prompt_tokens: i32, completion_tokens: i32) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
            prompt_tokens_details: None,
        }
    }
}

/// Chat completion streaming chunk (matches OpenAI format)
///
/// Represents a single chunk in a streaming chat completion response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionChunk {
    /// Unique identifier for the completion
    pub id: String,

    /// Object type - always "chat.completion.chunk"
    pub object: String,

    /// Unix timestamp of when the chunk was created
    pub created: i64,

    /// Model used for the completion
    pub model: String,

    /// Backend configuration fingerprint
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<String>,

    /// List of completion choices
    pub choices: Vec<ChatChoice>,

    /// Usage statistics (typically only in final chunk)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,

    /// Token IDs for the prompt (typically only in first chunk)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_token_ids: Option<Vec<i64>>,
}

/// Text completion streaming chunk (matches OpenAI legacy format)
///
/// Represents a single chunk in a streaming text completion response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionChunk {
    /// Unique identifier for the completion
    pub id: String,

    /// Object type - always "text_completion"
    pub object: String,

    /// Unix timestamp of when the chunk was created
    pub created: i64,

    /// Model used for the completion
    pub model: String,

    /// Backend configuration fingerprint
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<String>,

    /// List of completion choices
    pub choices: Vec<TextChoice>,

    /// Usage statistics (typically only in final chunk)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
}

/// Choice in a chat completion response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatChoice {
    /// Choice index
    pub index: i64,

    /// Incremental message delta
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<ChatDelta>,

    /// Log probabilities for the choice tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<LogProbs>,

    /// Reason why generation finished
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,

    /// Token IDs generated in this chunk (streaming responses)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_ids: Option<Vec<i64>>,
}

/// Choice in a text completion response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextChoice {
    /// Choice index
    pub index: i64,

    /// Generated text content
    pub text: String,

    /// Log probabilities for the choice tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<TextLogProbs>,

    /// Reason why generation finished
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
}

/// Log probabilities for chat completion tokens
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogProbs {
    /// Log probabilities for each token
    pub content: Vec<TokenLogProb>,
}

/// Log probabilities for text completion tokens
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextLogProbs {
    /// Tokens generated
    pub tokens: Vec<String>,
    /// Log probabilities for each token
    pub token_logprobs: Vec<Option<f32>>,
    /// Top log probabilities for each position
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<Vec<serde_json::Map<String, serde_json::Value>>>,
    /// Text offsets for each token
    pub text_offset: Vec<i64>,
}

/// Log probability information for a single token
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenLogProb {
    /// The token
    pub token: String,
    /// Log probability of the token
    pub logprob: f32,
    /// UTF-8 bytes of the token
    pub bytes: Vec<u8>,
    /// Top alternative tokens at this position
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<Vec<TopLogProb>>,
}

/// Top alternative token with log probability
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopLogProb {
    /// The token
    pub token: String,
    /// Log probability of the token
    pub logprob: f32,
    /// UTF-8 bytes of the token
    pub bytes: Vec<u8>,
}

/// Generic streaming chunk that can represent either format
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StreamChunk {
    /// Chat completion chunk
    Chat(ChatCompletionChunk),
    /// Text completion chunk
    Text(CompletionChunk),
}

/// Complete (non-streaming) chat completion response (matches OpenAI format)
///
/// Represents the full response from a non-streaming chat completion request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    /// Unique identifier for the completion
    pub id: String,

    /// Object type - always "chat.completion"
    pub object: String,

    /// Unix timestamp of when the completion was created
    pub created: i64,

    /// Model used for the completion
    pub model: String,

    /// List of completion choices
    pub choices: Vec<ChatCompletionResponseChoice>,

    /// Service tier used for processing the request
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,

    /// Backend configuration fingerprint
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<String>,

    /// Usage statistics
    pub usage: TokenUsage,

    /// Log probabilities for the prompt tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_logprobs: Option<serde_json::Value>,

    /// Token IDs for the prompt
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_token_ids: Option<Vec<i64>>,

    /// KV cache transfer parameters
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kv_transfer_params: Option<serde_json::Value>,
}

/// Wrapper for chat completion response that includes raw bytes from provider
///
/// This allows returning the exact bytes from the provider for hash verification
/// while also providing the parsed response for internal processing (usage tracking, etc.)
#[derive(Debug, Clone)]
pub struct ChatCompletionResponseWithBytes {
    /// The parsed response
    pub response: ChatCompletionResponse,

    /// The raw bytes from the provider response
    pub raw_bytes: Vec<u8>,
}

/// Choice in a complete (non-streaming) chat completion response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponseChoice {
    /// Choice index
    pub index: i64,

    /// Complete message from the assistant
    pub message: ChatResponseMessage,

    /// Log probabilities for the choice tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<LogProbs>,

    /// Reason why generation finished
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,

    /// Alternative stop reason (provider-specific)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,

    /// Token IDs generated for this choice
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_ids: Option<Vec<i64>>,
}

/// Message in a complete chat completion response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponseMessage {
    /// Role of the message sender
    pub role: MessageRole,

    /// Text content of the message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,

    /// Refusal message if the model refused to respond
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,

    /// Annotations for the message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<serde_json::Value>,

    /// Audio content (if any)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<serde_json::Value>,

    /// Legacy function call (deprecated, use tool_calls)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<serde_json::Value>,

    /// Tool calls made by the model
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,

    /// Reasoning content for models that support chain-of-thought
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

/// Model object (matches OpenAI API)
/// Describes an OpenAI model offering that can be used with the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// The Unix timestamp (in seconds) when the model was created
    pub created: i64,
    /// The model identifier, which can be referenced in the API endpoints
    pub id: String,
    /// The object type, which is always "model"
    pub object: String,
    /// The organization that owns the model
    pub owned_by: String,
}

// vLLM returns OpenAI-compatible models response
#[derive(Deserialize)]
pub struct ModelsResponse {
    pub object: String,
    pub data: Vec<ModelInfo>,
}

#[derive(Debug, Error)]
pub enum ListModelsError {
    #[error("Failed to fetch models: {0}")]
    FetchError(String),
    #[error("Invalid response format")]
    InvalidResponse,
    #[error("Unknown error")]
    Unknown,
}

/// Chat signature for cryptographic verification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSignature {
    /// The text being signed (typically contains hashes)
    pub text: String,
    /// The cryptographic signature
    pub signature: String,
    /// The address that created the signature
    pub signing_address: String,
    /// The signing algorithm used (e.g., "ecdsa")
    pub signing_algo: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_completion_response_deserialization() {
        let json_response = r#"{
            "id":"chatcmpl-047346ea58694a589185856879eef398",
            "object":"chat.completion",
            "created":1760402549,
            "model":"Qwen/Qwen3-30B-A3B-Instruct-2507",
            "choices":[{
                "index":0,
                "message":{
                    "role":"assistant",
                    "content":"Hello world",
                    "refusal":null,
                    "annotations":null,
                    "audio":null,
                    "function_call":null,
                    "tool_calls":[],
                    "reasoning_content":null
                },
                "logprobs":null,
                "finish_reason":"stop",
                "stop_reason":null,
                "token_ids":null
            }],
            "service_tier":null,
            "system_fingerprint":null,
            "usage":{
                "prompt_tokens":14,
                "total_tokens":17,
                "completion_tokens":3,
                "prompt_tokens_details":null
            },
            "prompt_logprobs":null,
            "prompt_token_ids":null,
            "kv_transfer_params":null
        }"#;

        let response: ChatCompletionResponse = serde_json::from_str(json_response).unwrap();

        assert_eq!(response.id, "chatcmpl-047346ea58694a589185856879eef398");
        assert_eq!(response.object, "chat.completion");
        assert_eq!(response.created, 1760402549);
        assert_eq!(response.model, "Qwen/Qwen3-30B-A3B-Instruct-2507");
        assert_eq!(response.choices.len(), 1);

        let choice = &response.choices[0];
        assert_eq!(choice.index, 0);
        assert_eq!(choice.finish_reason, Some("stop".to_string()));
        assert_eq!(choice.message.content, Some("Hello world".to_string()));

        assert_eq!(response.usage.prompt_tokens, 14);
        assert_eq!(response.usage.completion_tokens, 3);
        assert_eq!(response.usage.total_tokens, 17);
    }
}

#[derive(Debug, Error, Clone, Serialize, Deserialize)]
pub enum CompletionError {
    #[error("Failed to perform completion: {0}")]
    CompletionError(String),
    #[error("Invalid response format")]
    InvalidResponse(String),
    #[error("Unknown error")]
    Unknown(String),
}
