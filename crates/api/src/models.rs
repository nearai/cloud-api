use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use utoipa::ToSchema;

// Re-export ResponseImageUrl from services to avoid duplication
pub use services::responses::models::ResponseImageUrl;

// Streaming response models
#[derive(Debug, Serialize, Deserialize)]
pub struct StreamChunkResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<StreamChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StreamChoice {
    pub index: i64,
    pub delta: Delta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Delta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub max_tokens: Option<i64>,
    #[serde(default = "default_temperature")]
    pub temperature: Option<f32>,
    #[serde(default = "default_top_p")]
    pub top_p: Option<f32>,
    #[serde(default = "default_n")]
    pub n: Option<i64>,
    pub stream: Option<bool>,
    pub stop: Option<Vec<String>>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,

    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Content can be text or array of content parts
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<MessageContentPart>),
}

/// Content part (text, image, audio, file)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum MessageContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl {
        image_url: MessageImageUrl,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    #[serde(rename = "audio")]
    Audio { audio: MessageAudio },
    #[serde(rename = "file")]
    File { file_id: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum MessageImageUrl {
    String(String),
    Object { url: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct MessageAudio {
    pub data: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Message {
    pub role: String, // "system", "user", "assistant"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
    pub name: Option<String>,
}

#[derive(Debug, Serialize, ToSchema, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String, // "chat.completion"
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChatChoice>,
    pub usage: Usage,
}

#[derive(Debug, Serialize, ToSchema, Deserialize)]
pub struct ChatChoice {
    pub index: i64,
    pub message: Message,
    pub finish_reason: Option<String>, // "stop", "length", "content_filter"
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CompletionRequest {
    pub model: String,
    pub prompt: String,
    pub max_tokens: Option<i64>,
    #[serde(default = "default_temperature")]
    pub temperature: Option<f32>,
    #[serde(default = "default_top_p")]
    pub top_p: Option<f32>,
    #[serde(default = "default_n")]
    pub n: Option<i64>,
    pub stream: Option<bool>,
    pub logprobs: Option<i64>,
    pub echo: Option<bool>,
    pub stop: Option<Vec<String>>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub best_of: Option<i64>,

    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct CompletionResponse {
    pub id: String,
    pub object: String, // "text_completion"
    pub created: i64,
    pub model: String,
    pub choices: Vec<CompletionChoice>,
    pub usage: Usage,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ModelsResponse {
    pub object: String,
    pub data: Vec<ModelInfo>,
}

/// Model pricing information (HuggingFace compatible format)
/// Price is in US dollars per million tokens
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ModelPricing {
    /// Price per million input tokens in USD
    pub input: f64,
    /// Price per million output tokens in USD
    pub output: f64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ModelInfo {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub owned_by: String,
    /// Pricing information (HuggingFace compatible)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pricing: Option<ModelPricing>,
    /// Context length in tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_length: Option<i32>,
}

#[derive(Debug, Serialize)]
pub struct CompletionChoice {
    pub index: i64,
    pub text: String,
    pub logprobs: Option<serde_json::Value>,
    pub finish_reason: Option<String>, // "stop", "length", "content_filter"
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Usage {
    #[serde(alias = "prompt_tokens")]
    pub input_tokens: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens_details: Option<InputTokensDetails>,
    #[serde(alias = "completion_tokens")]
    pub output_tokens: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens_details: Option<OutputTokensDetails>,
    pub total_tokens: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InputTokensDetails {
    pub cached_tokens: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OutputTokensDetails {
    pub reasoning_tokens: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ErrorResponse {
    pub error: ErrorDetail,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ErrorDetail {
    pub message: String,
    pub r#type: String,
    pub param: Option<String>,
    pub code: Option<String>,
}

fn default_temperature() -> Option<f32> {
    Some(1.0)
}

fn default_top_p() -> Option<f32> {
    Some(1.0)
}

fn default_n() -> Option<i64> {
    Some(1)
}

// ============================================
// Common validation helpers (re-exported)
// ============================================

use crate::consts::{
    MAX_DESCRIPTION_LENGTH, MAX_EMAIL_LENGTH, MAX_INVITATIONS_PER_REQUEST, MAX_METADATA_SIZE_BYTES,
    MAX_NAME_LENGTH, MAX_SETTINGS_SIZE_BYTES, MAX_SYSTEM_PROMPT_LENGTH,
};
use crate::routes::common::{validate_max_length, validate_non_empty_field};

impl ChatCompletionRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.model.is_empty() {
            return Err("model is required".to_string());
        }

        if self.messages.is_empty() {
            return Err("messages cannot be empty".to_string());
        }

        for message in &self.messages {
            if message.role.is_empty() {
                return Err("message role is required".to_string());
            }
            if !["system", "user", "assistant", "tool"].contains(&message.role.as_str()) {
                return Err(format!("invalid message role: {}", message.role));
            }

            // Validate content: if it's an array, check that all parts are text-only
            if let Some(MessageContent::Parts(parts)) = &message.content {
                for part in parts {
                    match part {
                        MessageContentPart::Text { .. } => {
                            // Text parts are allowed
                        }
                        MessageContentPart::ImageUrl { .. }
                        | MessageContentPart::Audio { .. }
                        | MessageContentPart::File { .. } => {
                            return Err(
                                "Content array contains non-text parts (image, audio, or file). Only text content is currently supported.".to_string()
                            );
                        }
                    }
                }
            }
        }

        if let Some(temp) = self.temperature {
            if !(0.0..=2.0).contains(&temp) {
                return Err("temperature must be between 0 and 2".to_string());
            }
        }

        if let Some(top_p) = self.top_p {
            if !(0.0..=1.0).contains(&top_p) {
                return Err("top_p must be between 0 and 1".to_string());
            }
        }

        Ok(())
    }
}

impl CompletionRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.model.is_empty() {
            return Err("model is required".to_string());
        }

        if self.prompt.is_empty() {
            return Err("prompt is required".to_string());
        }

        if let Some(temp) = self.temperature {
            if !(0.0..=2.0).contains(&temp) {
                return Err("temperature must be between 0 and 2".to_string());
            }
        }

        if let Some(top_p) = self.top_p {
            if !(0.0..=1.0).contains(&top_p) {
                return Err("top_p must be between 0 and 1".to_string());
            }
        }

        Ok(())
    }
}

impl Usage {
    pub fn new(input_tokens: i32, output_tokens: i32) -> Self {
        Self {
            input_tokens,
            input_tokens_details: Some(InputTokensDetails { cached_tokens: 0 }),
            output_tokens,
            output_tokens_details: Some(OutputTokensDetails {
                reasoning_tokens: 0,
            }),
            total_tokens: input_tokens + output_tokens,
        }
    }
}

impl ErrorResponse {
    pub fn new(message: String, error_type: String) -> Self {
        Self {
            error: ErrorDetail {
                message,
                r#type: error_type,
                param: None,
                code: None,
            },
        }
    }

    pub fn with_param(message: String, error_type: String, param: String) -> Self {
        Self {
            error: ErrorDetail {
                message,
                r#type: error_type,
                param: Some(param),
                code: None,
            },
        }
    }
}

// ============================================
// Response API Models
// ============================================

/// Request to create a response
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateResponseRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<ResponseInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation: Option<ConversationReference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ResponseTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ResponseToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<ResponseTextConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ResponseReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety_identifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signing_algo: Option<String>,
}

/// Input for a response - can be text, array of items, or single item
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum ResponseInput {
    Text(String),
    Items(Vec<ResponseInputItem>),
}

/// Single input item
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResponseInputItem {
    pub role: String,
    pub content: ResponseContent,
}

/// Content can be text or array of content parts
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum ResponseContent {
    Text(String),
    Parts(Vec<ResponseContentPart>),
}

/// Content part from user inputs (input-only variants).
///
/// This type is used for type-safe operations on user inputs only.
/// It cannot contain output variants, providing compile-time safety.
///
/// Used in:
/// - ResponseContent::Parts (for input listing)
/// - list_input_items endpoint
/// - Input validation operations
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum ResponseContentPart {
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(rename = "input_image")]
    InputImage {
        image_url: ResponseImageUrl,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    #[serde(rename = "input_file")]
    InputFile {
        file_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

/// Conversation reference
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum ConversationReference {
    Id(String),
    Object {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
}

/// Tool configuration for responses
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum ResponseTool {
    #[serde(rename = "function")]
    Function {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        parameters: Option<serde_json::Value>,
    },
    #[serde(rename = "web_search")]
    WebSearch {},
    #[serde(rename = "file_search")]
    FileSearch {},
    #[serde(rename = "code_interpreter")]
    CodeInterpreter {},
    #[serde(rename = "computer")]
    Computer {},
    #[serde(rename = "mcp")]
    Mcp {
        server_label: String,
        server_url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        server_description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        authorization: Option<String>,
        #[serde(default)]
        require_approval: McpApprovalRequirement,
        #[serde(skip_serializing_if = "Option::is_none")]
        allowed_tools: Option<Vec<String>>,
    },
}

/// Approval requirement for MCP tool calls
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum McpApprovalRequirement {
    Simple(McpApprovalMode),
    Granular { never: McpToolNameFilter },
}

impl Default for McpApprovalRequirement {
    fn default() -> Self {
        Self::Simple(McpApprovalMode::Always)
    }
}

/// Simple MCP approval mode
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum McpApprovalMode {
    #[default]
    Always,
    Never,
}

/// Filter for tool names that don't require approval
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct McpToolNameFilter {
    pub tool_names: std::collections::HashSet<String>,
}

/// Tool choice configuration
#[derive(Debug, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum ResponseToolChoice {
    Auto(String), // "auto", "none", "required"
    Specific {
        #[serde(rename = "type")]
        type_: String,
        function: ResponseToolChoiceFunction,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResponseToolChoiceFunction {
    pub name: String,
}

/// Text format configuration
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResponseTextConfig {
    pub format: ResponseTextFormat,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum ResponseTextFormat {
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "json_object")]
    JsonObject,
    #[serde(rename = "json_schema")]
    JsonSchema { json_schema: serde_json::Value },
}

/// Reasoning configuration
#[derive(Debug, Deserialize, ToSchema)]
pub struct ResponseReasoningConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// Complete response object
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResponseObject {
    pub id: String,
    pub object: String, // "response"
    pub created_at: i64,
    pub status: ResponseStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incomplete_details: Option<ResponseIncompleteDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<i64>,
    pub model: String,
    pub output: Vec<ResponseOutputItem>,
    pub parallel_tool_calls: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>, // Previous response ID (parent in thread)
    #[serde(default)]
    pub next_response_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ResponseReasoningOutput>,
    pub store: bool,
    pub temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<ResponseTextConfig>,
    pub tool_choice: ResponseToolChoiceOutput,
    pub tools: Vec<ResponseTool>,
    pub top_p: f32,
    pub truncation: String,
    pub usage: Usage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ResponseStatus {
    Completed,
    Failed,
    InProgress,
    Cancelled,
    Queued,
    Incomplete,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResponseError {
    pub message: String,
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResponseIncompleteDetails {
    pub reason: String, // "length", "content_filter", "max_tool_calls"
}

/// Output item from response
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum ResponseOutputItem {
    #[serde(rename = "message")]
    Message {
        id: String,
        status: ResponseItemStatus,
        role: String,
        content: Vec<ResponseOutputContent>,
    },
    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        status: ResponseItemStatus,
        tool_type: String,
        function: ResponseOutputFunction,
    },
    #[serde(rename = "reasoning")]
    Reasoning {
        id: String,
        status: ResponseItemStatus,
        summary: String,
        content: String,
    },
    #[serde(rename = "mcp_list_tools")]
    McpListTools {
        id: String,
        server_label: String,
        tools: Vec<McpDiscoveredTool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ResponseItemStatus {
    Completed,
    Failed,
    InProgress,
    Cancelled,
}

/// Output content from assistant (output-only variants).
///
/// This type is used for type-safe operations on assistant outputs only.
/// It cannot contain input variants, providing compile-time safety.
/// Used in streaming events and response output items in the API layer.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum ResponseOutputContent {
    #[serde(rename = "output_text")]
    OutputText {
        text: String,
        annotations: Vec<serde_json::Value>,
    },
    #[serde(rename = "tool_calls")]
    ToolCalls {
        tool_calls: Vec<ResponseOutputToolCall>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResponseOutputFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResponseOutputToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub function: ResponseOutputFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResponseReasoningOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum ResponseToolChoiceOutput {
    Auto(String),
    Object {
        #[serde(rename = "type")]
        type_: String,
        function: ResponseToolChoiceFunction,
    },
}

/// Response deletion result
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResponseDeleteResult {
    pub id: String,
    pub object: String, // "response"
    pub deleted: bool,
}

// ============================================
// Response Streaming Event Types
// ============================================

/// Response streaming event wrapper
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResponseStreamEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseObject>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<ResponseOutputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part: Option<ResponseOutputContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Input item list for responses
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResponseInputItemList {
    pub object: String, // "list"
    pub data: Vec<ResponseInputItem>,
    pub first_id: String,
    pub last_id: String,
    pub has_more: bool,
}

// ============================================
// Conversation API Models
// ============================================

/// Request to create a conversation
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateConversationRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Request to update a conversation
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateConversationRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Request to create items in a conversation
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateConversationItemsRequest {
    pub items: Vec<ConversationInputItem>,
}

/// Conversation object (follows OpenAI spec)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ConversationObject {
    pub id: String,
    pub object: String, // "conversation"
    pub created_at: i64,
    pub metadata: serde_json::Value,
}

/// Deleted conversation result
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ConversationDeleteResult {
    pub id: String,
    pub object: String, // "conversation.deleted"
    pub deleted: bool,
}

/// Input item for conversations
#[derive(Debug, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum ConversationInputItem {
    #[serde(rename = "message")]
    Message {
        role: String,
        content: ConversationContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
}

/// Content for conversation items
#[derive(Debug, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum ConversationContent {
    Text(String),
    Parts(Vec<ConversationContentPart>),
}

/// Content part for conversations
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum ConversationContentPart {
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(rename = "input_image")]
    InputImage {
        image_url: ResponseImageUrl,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    #[serde(rename = "input_file")]
    InputFile {
        file_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    #[serde(rename = "output_text")]
    OutputText {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<Vec<serde_json::Value>>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct McpDiscoveredTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,
}

/// Conversation item (for responses)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum ConversationItem {
    #[serde(rename = "message")]
    Message {
        id: String,
        response_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_response_id: Option<String>,
        #[serde(default)]
        next_response_ids: Vec<String>,
        created_at: i64,
        status: ResponseItemStatus,
        role: String,
        content: Vec<ConversationContentPart>,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
        model: String,
    },
    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        response_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_response_id: Option<String>,
        #[serde(default)]
        next_response_ids: Vec<String>,
        created_at: i64,
        status: ResponseItemStatus,
        tool_type: String,
        function: ConversationItemFunction,
        model: String,
    },
    #[serde(rename = "web_search_call")]
    WebSearchCall {
        id: String,
        response_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_response_id: Option<String>,
        #[serde(default)]
        next_response_ids: Vec<String>,
        created_at: i64,
        status: ResponseItemStatus,
        action: ConversationItemWebSearchAction,
        model: String,
    },
    #[serde(rename = "reasoning")]
    Reasoning {
        id: String,
        response_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_response_id: Option<String>,
        #[serde(default)]
        next_response_ids: Vec<String>,
        created_at: i64,
        status: ResponseItemStatus,
        summary: String,
        content: String,
        model: String,
    },
    #[serde(rename = "mcp_list_tools")]
    McpListTools {
        id: String,
        server_label: String,
        tools: Vec<McpDiscoveredTool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    #[serde(rename = "mcp_call")]
    McpCall {
        id: String,
        response_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_response_id: Option<String>,
        #[serde(default)]
        next_response_ids: Vec<String>,
        created_at: i64,
        server_label: String,
        name: String,
        arguments: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        approval_request_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        model: String,
    },
    #[serde(rename = "mcp_approval_request")]
    McpApprovalRequest {
        id: String,
        response_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_response_id: Option<String>,
        #[serde(default)]
        next_response_ids: Vec<String>,
        created_at: i64,
        server_label: String,
        name: String,
        arguments: String,
        model: String,
    },
}

impl ConversationItem {
    /// Get the ID of the conversation item
    pub fn id(&self) -> &str {
        match self {
            ConversationItem::Message { id, .. } => id,
            ConversationItem::ToolCall { id, .. } => id,
            ConversationItem::WebSearchCall { id, .. } => id,
            ConversationItem::Reasoning { id, .. } => id,
            ConversationItem::McpListTools { id, .. } => id,
            ConversationItem::McpCall { id, .. } => id,
            ConversationItem::McpApprovalRequest { id, .. } => id,
        }
    }

    /// Get the role of the conversation item (only for Message items)
    pub fn role(&self) -> &str {
        match self {
            ConversationItem::Message { role, .. } => role,
            _ => "",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ConversationItemFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase", tag = "type")]
pub enum ConversationItemWebSearchAction {
    #[serde(rename = "search")]
    Search { query: String },
}

/// List of conversation items
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ConversationItemList {
    pub object: String, // "list"
    pub data: Vec<ConversationItem>,
    pub first_id: String,
    pub last_id: String,
    pub has_more: bool,
}

/// Request for batch retrieving conversations
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct BatchConversationsRequest {
    /// Array of conversation IDs to retrieve (supports both prefixed "conv_" and raw UUID formats)
    pub ids: Vec<String>,
}

/// Response for batch conversation retrieval
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ConversationBatchResponse {
    /// Object type identifier: "list"
    pub object: String,
    /// Array of retrieved conversations
    pub data: Vec<ConversationObject>,
    /// Array of requested conversation IDs that were not found (always present, may be empty)
    pub missing_ids: Vec<String>,
}

// ============================================
// Validation implementations
// ============================================

impl CreateResponseRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.model.trim().is_empty() {
            return Err("Model cannot be empty".to_string());
        }

        if let Some(max_tokens) = self.max_output_tokens {
            if max_tokens == 0 {
                return Err("max_output_tokens must be greater than 0".to_string());
            }
        }

        if let Some(max_calls) = self.max_tool_calls {
            if max_calls == 0 {
                return Err("max_tool_calls must be greater than 0".to_string());
            }
        }

        if let Some(temp) = self.temperature {
            if !(0.0..=2.0).contains(&temp) {
                return Err("temperature must be between 0.0 and 2.0".to_string());
            }
        }

        if let Some(top_p) = self.top_p {
            if top_p <= 0.0 || top_p > 1.0 {
                return Err("top_p must be between 0.0 and 1.0".to_string());
            }
        }

        // Validate mutual exclusivity
        if self.conversation.is_some() && self.previous_response_id.is_some() {
            return Err("Cannot specify both conversation and previous_response_id".to_string());
        }

        Ok(())
    }
}

impl CreateConversationRequest {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(metadata) = &self.metadata {
            // Prevent extremely large metadata blobs from being stored
            let serialized =
                serde_json::to_string(metadata).map_err(|_| "Invalid metadata".to_string())?;
            // Allow reasonably large metadata but cap to protect the database
            if serialized.len() > MAX_METADATA_SIZE_BYTES {
                return Err(format!(
                    "metadata is too large (max {} bytes when serialized)",
                    MAX_METADATA_SIZE_BYTES
                ));
            }
        }

        Ok(())
    }
}

impl UpdateConversationRequest {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(metadata) = &self.metadata {
            // Prevent extremely large metadata blobs from being stored
            let serialized =
                serde_json::to_string(metadata).map_err(|_| "Invalid metadata".to_string())?;
            // Allow reasonably large metadata but cap to protect the database
            if serialized.len() > MAX_METADATA_SIZE_BYTES {
                return Err(format!(
                    "metadata is too large (max {} bytes when serialized)",
                    MAX_METADATA_SIZE_BYTES
                ));
            }
        }

        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreateApiKeyRequest {
    pub name: String,
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "spendLimit")]
    pub spend_limit: Option<DecimalPriceRequest>,
}

impl CreateApiKeyRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_non_empty_field(&self.name, "name")?;
        validate_max_length(&self.name, "name", MAX_NAME_LENGTH)?;

        if let Some(limit) = &self.spend_limit {
            limit.validate().map_err(|e| format!("spend_limit: {e}"))?;
        }

        Ok(())
    }
}

// ============================================
// Organization API Models
// ============================================

/// Request to create a new organization
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateOrganizationRequest {
    pub name: String,
    pub description: Option<String>,
}

impl CreateOrganizationRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_non_empty_field(&self.name, "name")?;
        validate_max_length(&self.name, "name", MAX_NAME_LENGTH)?;

        if let Some(desc) = &self.description {
            validate_max_length(desc, "description", MAX_DESCRIPTION_LENGTH)?;
        }

        Ok(())
    }
}

/// Request to update an organization
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateOrganizationRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub rate_limit: Option<i32>,
    pub settings: Option<serde_json::Value>,
}

impl UpdateOrganizationRequest {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(name) = &self.name {
            validate_non_empty_field(name, "name")?;
            validate_max_length(name, "name", MAX_NAME_LENGTH)?;
        }

        if let Some(desc) = &self.description {
            validate_max_length(desc, "description", MAX_DESCRIPTION_LENGTH)?;
        }

        if let Some(rate) = self.rate_limit {
            if rate <= 0 {
                return Err("rate_limit must be positive".to_string());
            }
        }

        if let Some(settings) = &self.settings {
            // Cap settings size to protect DB from extremely large blobs
            let serialized =
                serde_json::to_string(settings).map_err(|_| "Invalid settings JSON".to_string())?;
            if serialized.len() > MAX_SETTINGS_SIZE_BYTES {
                return Err(format!(
                    "settings is too large (max {} bytes when serialized)",
                    MAX_SETTINGS_SIZE_BYTES
                ));
            }
        }

        Ok(())
    }
}

/// Organization response model
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OrganizationResponse {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub owner_id: String,
    pub settings: serde_json::Value,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Paginated organizations list response
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ListOrganizationsResponse {
    pub organizations: Vec<OrganizationResponse>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// Member role enum for API
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum MemberRole {
    Owner,
    Admin,
    Member,
}

/// Request to add an organization member
#[derive(Debug, Deserialize, ToSchema)]
pub struct AddOrganizationMemberRequest {
    pub user_id: String,
    pub role: MemberRole,
}

/// Individual invitation entry with email and role
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct InvitationEntry {
    pub email: String,
    pub role: MemberRole,
}

fn is_basic_valid_email(email: &str) -> bool {
    // Reject spaces outright
    if email.contains(' ') {
        return false;
    }

    // Require exactly one '@' and non-empty local/domain parts
    let (local, domain) = match email.split_once('@') {
        Some(parts) => parts,
        None => return false,
    };
    if local.is_empty() || domain.is_empty() {
        return false;
    }

    // Ensure there are no additional '@' characters in the domain part
    if domain.contains('@') {
        return false;
    }

    // Require at least one dot in the domain, not at start or end
    if !domain.contains('.') {
        return false;
    }
    if domain.starts_with('.') || domain.ends_with('.') {
        return false;
    }

    true
}

impl InvitationEntry {
    pub fn validate(&self) -> Result<(), String> {
        validate_non_empty_field(&self.email, "email")?;
        validate_max_length(&self.email, "email", MAX_EMAIL_LENGTH)?;
        if !is_basic_valid_email(&self.email) {
            return Err("email is not a valid email address".to_string());
        }
        Ok(())
    }
}

/// Request to invite organization members by email
#[derive(Debug, Deserialize, ToSchema)]
pub struct InviteOrganizationMemberByEmailRequest {
    pub invitations: Vec<InvitationEntry>,
}

impl InviteOrganizationMemberByEmailRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.invitations.is_empty() {
            return Err("invitations cannot be empty".to_string());
        }

        // Prevent abuse with very large batches
        if self.invitations.len() > MAX_INVITATIONS_PER_REQUEST {
            return Err(format!(
                "Maximum {} invitations per request",
                MAX_INVITATIONS_PER_REQUEST
            ));
        }

        for (idx, inv) in self.invitations.iter().enumerate() {
            if let Err(e) = inv.validate() {
                return Err(format!("invitations[{idx}]: {e}"));
            }
        }

        Ok(())
    }
}

/// Request to update an organization member
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateOrganizationMemberRequest {
    pub role: MemberRole,
}

/// Organization settings structure
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OrganizationSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

/// Wrapper type to distinguish between "field not provided" and "field explicitly set to null"
/// - Outer None = field not in request body (don't modify)
/// - Some(None) = field explicitly set to null (delete/clear)
/// - Some(Some(value)) = field set to a value (update)
pub type Nullable<T> = Option<Option<T>>;

/// Deserialize a field that can be absent, null, or a value
fn deserialize_nullable<'de, D, T>(deserializer: D) -> Result<Nullable<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    // If the field is present, deserialize it as Option<T>
    Ok(Some(Option::deserialize(deserializer)?))
}

/// Request to patch organization settings (PATCH endpoint)
/// Supports three states per field:
/// - Omit field: no change
/// - Set to null: delete/clear the field
/// - Set to value: update the field
#[derive(Debug, Deserialize, ToSchema)]
pub struct PatchOrganizationSettingsRequest {
    /// System prompt for the organization
    #[serde(default, deserialize_with = "deserialize_nullable")]
    #[schema(value_type = Option<String>)]
    pub system_prompt: Nullable<String>,
}

impl PatchOrganizationSettingsRequest {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(Some(prompt)) = &self.system_prompt {
            validate_max_length(prompt, "system_prompt", MAX_SYSTEM_PROMPT_LENGTH)?;
        }
        Ok(())
    }
}

/// Response containing organization settings
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OrganizationSettingsResponse {
    pub settings: OrganizationSettings,
}

/// Result of a single invitation attempt
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InvitationResult {
    pub email: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member: Option<OrganizationMemberResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response for batch invitation requests
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct InviteOrganizationMemberByEmailResponse {
    pub results: Vec<InvitationResult>,
    pub total: usize,
    pub successful: usize,
    pub failed: usize,
}

/// Public organization member response (for regular members)
/// Contains member info with limited user details
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublicOrganizationMemberResponse {
    pub id: String,
    pub organization_id: String,
    pub role: MemberRole,
    pub joined_at: DateTime<Utc>,
    pub user: PublicUserResponse,
}

/// List organization members response with pagination
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ListOrganizationMembersResponse {
    pub members: Vec<PublicOrganizationMemberResponse>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// Admin organization member response (for owners/admins)
/// Contains member info with full user details including sensitive data
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AdminOrganizationMemberResponse {
    pub id: String,
    pub organization_id: String,
    pub role: MemberRole,
    pub joined_at: DateTime<Utc>,
    pub invited_by: Option<String>,
    pub user: AdminUserResponse,
}

/// Public user response model (for regular members)
/// Only contains non-sensitive information visible to all organization members
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublicUserResponse {
    pub id: String,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Organization usage information (for admin user listing)
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OrganizationUsage {
    /// Total amount spent in nano-dollars (scale 9)
    pub total_spent: i64,
    /// Human readable total spent, e.g., "$12.50"
    pub total_spent_display: String,
    /// Total number of API requests
    pub total_requests: i64,
    /// Total number of tokens used
    pub total_tokens: i64,
}

/// Organization details with spend limit and usage (for admin user listing)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AdminUserOrganizationDetails {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "spendLimit", skip_serializing_if = "Option::is_none")]
    pub spend_limit: Option<SpendLimit>,
    #[serde(rename = "currentUsage", skip_serializing_if = "Option::is_none")]
    pub current_usage: Option<OrganizationUsage>,
}

/// Admin user response model (for owners/admins)
/// Contains sensitive information only visible to organization owners/admins
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AdminUserResponse {
    pub id: String,
    pub email: String,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
    pub is_active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organizations: Option<Vec<AdminUserOrganizationDetails>>,
}

/// User response model (full user profile)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserResponse {
    pub id: String,
    pub email: String,
    pub username: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
    pub is_active: bool,
    pub auth_provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organizations: Option<Vec<UserOrganizationResponse>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspaces: Option<Vec<UserWorkspaceResponse>>,
}

/// User's organization with role (subset of OrganizationResponse)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserOrganizationResponse {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub role: MemberRole,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
}

/// User's workspace (subset of WorkspaceResponse)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserWorkspaceResponse {
    pub id: String,
    pub name: String,
    pub organization_id: String,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
}

/// Refresh token response model
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RefreshTokenResponse {
    pub id: String,
    pub user_id: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub ip_address: Option<String>,
    pub user_agent: String,
}

/// Access token + refresh token response model
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AccessAndRefreshTokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub refresh_token_expiration: DateTime<Utc>,
}

/// Organization member response model (non-sensitive)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OrganizationMemberResponse {
    pub id: String,
    pub organization_id: String,
    pub user_id: String,
    pub role: MemberRole,
    pub joined_at: DateTime<Utc>,
    pub invited_by: Option<String>,
}

/// List users response model (admin only)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ListUsersResponse {
    pub users: Vec<AdminUserResponse>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// Admin access token request model
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateAdminAccessTokenRequest {
    /// Number of hours until the token expires (required)
    pub expires_in_hours: i64,
    /// Name for the token (required)
    pub name: String,
    /// Reason for creating the token (required)
    pub reason: String,
}

/// Admin access token response model
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AdminAccessTokenResponse {
    pub id: String,
    pub access_token: String,
    pub created_by_user_id: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub name: String,
    pub reason: String,
}

/// Delete admin access token request model
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DeleteAdminAccessTokenRequest {
    /// Reason for revoking the token (required)
    pub reason: String,
}

/// API Key response model
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ApiKeyResponse {
    pub id: String,
    pub name: Option<String>,
    pub key: Option<String>,
    pub key_prefix: String,
    pub workspace_id: String,
    pub created_by_user_id: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spend_limit: Option<DecimalPrice>,
    pub is_active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<DecimalPrice>,
}

/// Paginated API keys list response
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ListApiKeysResponse {
    pub api_keys: Vec<ApiKeyResponse>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// Request to update API key spend limit
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateApiKeySpendLimitRequest {
    #[serde(rename = "spendLimit")]
    pub spend_limit: Option<DecimalPriceRequest>,
}

impl UpdateApiKeySpendLimitRequest {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(limit) = &self.spend_limit {
            limit.validate()?;
        }
        Ok(())
    }
}

/// Request to update API key (general update for name, expires_at, and/or spend_limit)
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UpdateApiKeyRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "spendLimit")]
    pub spend_limit: Option<DecimalPriceRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_active: Option<bool>,
}

impl UpdateApiKeyRequest {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(name) = &self.name {
            validate_non_empty_field(name, "name")?;
            validate_max_length(name, "name", MAX_NAME_LENGTH)?;
        }

        if let Some(limit) = &self.spend_limit {
            limit.validate()?;
        }

        Ok(())
    }
}

// ============================================
// Organization Invitations API Models
// ============================================

/// Organization invitation status
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum InvitationStatus {
    Pending,
    Accepted,
    Declined,
    Expired,
}

/// Organization invitation response
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OrganizationInvitationResponse {
    pub id: String,
    pub organization_id: String,
    pub email: String,
    pub role: MemberRole,
    pub invited_by_user_id: String,
    pub status: InvitationStatus,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub responded_at: Option<DateTime<Utc>>,
}

/// Organization invitation with organization details
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OrganizationInvitationWithOrgResponse {
    #[serde(flatten)]
    pub invitation: OrganizationInvitationResponse,
    pub organization_name: String,
    pub invited_by_display_name: Option<String>,
}

/// Accept invitation response
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AcceptInvitationResponse {
    pub organization_member: OrganizationMemberResponse,
    pub message: String,
}

// ============================================
// Model Listing API Models
// ============================================

/// Response for model list endpoint
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ModelListResponse {
    pub models: Vec<ModelWithPricing>,
    pub limit: i64,
    pub offset: i64,
    pub total: i64,
}

/// Response for admin model list endpoint
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AdminModelListResponse {
    pub models: Vec<AdminModelWithPricing>,
    pub limit: i64,
    pub offset: i64,
    pub total: i64,
}

/// Model with pricing information for admin listing
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AdminModelWithPricing {
    #[serde(rename = "modelId")]
    pub model_id: String,
    #[serde(rename = "inputCostPerToken")]
    pub input_cost_per_token: DecimalPrice,
    #[serde(rename = "outputCostPerToken")]
    pub output_cost_per_token: DecimalPrice,
    pub metadata: ModelMetadata,
    #[serde(rename = "isActive")]
    pub is_active: bool,
    #[serde(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
    #[serde(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,
}

/// Model with pricing information
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ModelWithPricing {
    #[serde(rename = "modelId")]
    pub model_id: String,
    #[serde(rename = "inputCostPerToken")]
    pub input_cost_per_token: DecimalPrice,
    #[serde(rename = "outputCostPerToken")]
    pub output_cost_per_token: DecimalPrice,
    pub metadata: ModelMetadata,
}

/// Decimal price for API requests
///
/// The system internally uses a fixed scale of 9 (nano-dollars = 1 billionth of a dollar).
/// Clients must provide amounts in nano-dollars.
///
/// Examples:
///   $100.00 USD: amount=100000000000, currency="USD"
///   $1.00 USD: amount=1000000000, currency="USD"
///   $0.01 USD: amount=10000000, currency="USD"
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct DecimalPriceRequest {
    /// Amount in nano-dollars (scale 9). For example, $1.00 = 1000000000 nano-dollars.
    pub amount: i64,
    pub currency: String,
}

impl DecimalPriceRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.amount < 0 {
            return Err("amount must be non-negative".to_string());
        }
        validate_non_empty_field(&self.currency, "currency")?;
        // Currencies are typically short, e.g. "USD"
        validate_max_length(&self.currency, "currency", 16)?;
        Ok(())
    }
}

/// Decimal price for API responses
///
/// The system uses a fixed scale of 9 (nano-dollars = 1 billionth of a dollar).
/// The scale field is included in responses for client convenience.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct DecimalPrice {
    pub amount: i64,
    pub scale: i64,
    pub currency: String,
}

/// Model metadata
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ModelMetadata {
    pub verifiable: bool,
    #[serde(rename = "contextLength")]
    pub context_length: i32,
    #[serde(rename = "modelDisplayName")]
    pub model_display_name: String,
    #[serde(rename = "modelDescription")]
    pub model_description: String,
    #[serde(rename = "modelIcon", skip_serializing_if = "Option::is_none")]
    pub model_icon: Option<String>,
    #[serde(rename = "ownedBy")]
    pub owned_by: String,

    #[serde(rename = "aliases", skip_serializing_if = "Vec::is_empty", default)]
    pub aliases: Vec<String>,
}

/// Request to update model pricing (admin endpoint)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpdateModelApiRequest {
    #[serde(rename = "inputCostPerToken")]
    pub input_cost_per_token: Option<DecimalPriceRequest>,
    #[serde(rename = "outputCostPerToken")]
    pub output_cost_per_token: Option<DecimalPriceRequest>,
    #[serde(rename = "modelDisplayName")]
    pub model_display_name: Option<String>,
    #[serde(rename = "modelDescription")]
    pub model_description: Option<String>,
    #[serde(rename = "modelIcon")]
    pub model_icon: Option<String>,
    #[serde(rename = "contextLength")]
    pub context_length: Option<i32>,
    pub verifiable: Option<bool>,
    #[serde(rename = "isActive")]
    pub is_active: Option<bool>,
    pub aliases: Option<Vec<String>>,
    #[serde(rename = "ownedBy")]
    pub owned_by: Option<String>,
    #[serde(rename = "changeReason", skip_serializing_if = "Option::is_none")]
    pub change_reason: Option<String>,
}

/// Batch update request format - Array of model name to update data
pub type BatchUpdateModelApiRequest = std::collections::HashMap<String, UpdateModelApiRequest>;

/// Delete model request - optional reason for deletion
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct DeleteModelRequest {
    #[serde(rename = "changeReason", skip_serializing_if = "Option::is_none")]
    pub change_reason: Option<String>,
}

/// Model history entry - includes pricing, context length, and other model attributes
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ModelHistoryEntry {
    pub id: String,
    #[serde(rename = "modelId")]
    pub model_id: String,
    #[serde(rename = "inputCostPerToken")]
    pub input_cost_per_token: DecimalPrice,
    #[serde(rename = "outputCostPerToken")]
    pub output_cost_per_token: DecimalPrice,
    #[serde(rename = "contextLength")]
    pub context_length: i32,
    #[serde(rename = "modelName")]
    pub model_name: String,
    #[serde(rename = "modelDisplayName")]
    pub model_display_name: String,
    #[serde(rename = "modelDescription")]
    pub model_description: String,
    #[serde(rename = "modelIcon")]
    pub model_icon: Option<String>,
    pub verifiable: bool,
    #[serde(rename = "isActive")]
    pub is_active: bool,
    #[serde(rename = "ownedBy")]
    pub owned_by: String,
    #[serde(rename = "effectiveFrom")]
    pub effective_from: String,
    #[serde(rename = "effectiveUntil")]
    pub effective_until: Option<String>,
    #[serde(rename = "changedByUserId")]
    pub changed_by_user_id: Option<String>,
    #[serde(rename = "changedByUserEmail")]
    pub changed_by_user_email: Option<String>,
    #[serde(rename = "changeReason")]
    pub change_reason: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

/// Model history response - complete history of model changes
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ModelHistoryResponse {
    #[serde(rename = "modelName")]
    pub model_name: String,
    pub history: Vec<ModelHistoryEntry>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

// ============================================
// Organization Limits API Models (Admin)
// ============================================

/// Request to update organization limits (Admin only)
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateOrganizationLimitsRequest {
    #[serde(rename = "type")]
    pub credit_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(rename = "spendLimit")]
    pub spend_limit: SpendLimitRequest,
    #[serde(rename = "changedBy", skip_serializing_if = "Option::is_none")]
    pub changed_by: Option<String>,
    #[serde(rename = "changeReason", skip_serializing_if = "Option::is_none")]
    pub change_reason: Option<String>,
}

/// Spend limit for API requests
///
/// The system internally uses a fixed scale of 9 (nano-dollars = 1 billionth of a dollar).
/// Clients must provide amounts in nano-dollars.
///
/// Examples:
///   $100.00 USD: amount=100000000000, currency="USD"
///   $1.00 USD: amount=1000000000, currency="USD"
///   $0.01 USD: amount=10000000, currency="USD"
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SpendLimitRequest {
    /// Amount in nano-dollars (scale 9). For example, $1.00 = 1000000000 nano-dollars.
    pub amount: i64,
    pub currency: String,
}

/// Spend limit for API responses
///
/// The system uses a fixed scale of 9 (nano-dollars = 1 billionth of a dollar).
/// The scale field is included in responses for client convenience.
///
/// Examples:
///   $100.00 USD: amount=100000000000, scale=9, currency="USD"
///   $0.01 USD: amount=10000000, scale=9, currency="USD"
#[derive(Debug, Serialize, Deserialize, ToSchema, Clone)]
pub struct SpendLimit {
    pub amount: i64,
    pub scale: i64,
    pub currency: String,
}

/// Response after updating organization limits
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UpdateOrganizationLimitsResponse {
    pub organization_id: String,
    #[serde(rename = "type")]
    pub credit_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(rename = "spendLimit")]
    pub spend_limit: SpendLimit,
    pub updated_at: String,
}

/// Organization limits history entry
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OrgLimitsHistoryEntry {
    pub id: String,
    #[serde(rename = "organizationId")]
    pub organization_id: String,
    #[serde(rename = "type")]
    pub credit_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(rename = "spendLimit")]
    pub spend_limit: SpendLimit,
    #[serde(rename = "effectiveFrom")]
    pub effective_from: String,
    #[serde(rename = "effectiveUntil")]
    pub effective_until: Option<String>,
    #[serde(rename = "changedBy")]
    pub changed_by: Option<String>,
    #[serde(rename = "changeReason")]
    pub change_reason: Option<String>,
    #[serde(rename = "changedByUserId")]
    pub changed_by_user_id: Option<String>,
    #[serde(rename = "changedByUserEmail")]
    pub changed_by_user_email: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

/// Organization limits history response
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OrgLimitsHistoryResponse {
    pub history: Vec<OrgLimitsHistoryEntry>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

// ============================================
// Organization Concurrent Limit API Models (Admin)
// ============================================

/// Request to update organization concurrent request limit (Admin only)
///
/// The concurrent limit controls how many requests an organization can have
/// in-flight simultaneously per model. Set to null to use the default (64).
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateOrganizationConcurrentLimitRequest {
    /// Concurrent request limit per model. Set to null to use default (64).
    #[serde(rename = "concurrentLimit")]
    pub concurrent_limit: Option<u32>,
}

/// Response after updating organization concurrent limit
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UpdateOrganizationConcurrentLimitResponse {
    #[serde(rename = "organizationId")]
    pub organization_id: String,
    /// Current concurrent limit. Null means default (64) is used.
    #[serde(rename = "concurrentLimit")]
    pub concurrent_limit: Option<u32>,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
}

/// Response for getting organization concurrent limit
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct GetOrganizationConcurrentLimitResponse {
    #[serde(rename = "organizationId")]
    pub organization_id: String,
    /// Current concurrent limit. Null means default (64) is used.
    #[serde(rename = "concurrentLimit")]
    pub concurrent_limit: Option<u32>,
    /// The effective limit (either custom or default)
    #[serde(rename = "effectiveLimit")]
    pub effective_limit: u32,
}

// ============================================
// File Upload Models
// ============================================

/// File upload response
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct FileUploadResponse {
    pub id: String,
    pub object: String, // Always "file"
    pub bytes: i64,
    pub created_at: i64, // Unix timestamp
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>, // Unix timestamp
    pub filename: String,
    pub purpose: String,
}

/// Expires after configuration
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ExpiresAfter {
    pub anchor: String, // "created_at"
    pub seconds: i64,   // Max: 31536000 (1 year)
}

/// File list response
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct FileListResponse {
    pub object: String, // Always "list"
    pub data: Vec<FileUploadResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_id: Option<String>,
    pub has_more: bool,
}

/// File delete response
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct FileDeleteResponse {
    pub id: String,
    pub object: String, // Always "file"
    pub deleted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_response_request_simple_text_input() {
        let json = r#"{
            "model": "gpt-4.1",
            "input": "Tell me a three sentence bedtime story about a unicorn."
        }"#;

        let result: Result<CreateResponseRequest, _> = serde_json::from_str(json);
        assert!(result.is_ok(), "Failed to deserialize: {:?}", result.err());

        let request = result.unwrap();
        assert_eq!(request.model, "gpt-4.1");
        assert!(matches!(request.input, Some(ResponseInput::Text(_))));
        if let Some(ResponseInput::Text(text)) = request.input {
            assert_eq!(
                text,
                "Tell me a three sentence bedtime story about a unicorn."
            );
        }
    }

    #[test]
    fn test_create_response_request_with_optional_fields() {
        let json = r#"{
            "model": "gpt-4.1",
            "instructions": "You are a helpful assistant.",
            "input": "Hello!",
            "stream": true,
            "temperature": 0.7,
            "max_output_tokens": 1000
        }"#;

        let result: Result<CreateResponseRequest, _> = serde_json::from_str(json);
        assert!(result.is_ok(), "Failed to deserialize: {:?}", result.err());

        let request = result.unwrap();
        assert_eq!(request.model, "gpt-4.1");
        assert_eq!(
            request.instructions,
            Some("You are a helpful assistant.".to_string())
        );
        assert_eq!(request.stream, Some(true));
        assert_eq!(request.temperature, Some(0.7));
        assert_eq!(request.max_output_tokens, Some(1000));
    }

    #[test]
    fn test_create_response_request_array_input_with_multipart_content() {
        let json = r#"{
            "model": "gpt-4.1",
            "input": [
                {
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "what is in this image?"},
                        {
                            "type": "input_image",
                            "image_url": "https://example.com/image.jpg"
                        }
                    ]
                }
            ]
        }"#;

        let result: Result<CreateResponseRequest, _> = serde_json::from_str(json);
        assert!(result.is_ok(), "Failed to deserialize: {:?}", result.err());

        let request = result.unwrap();
        assert_eq!(request.model, "gpt-4.1");

        if let Some(ResponseInput::Items(items)) = request.input {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].role, "user");

            if let ResponseContent::Parts(parts) = &items[0].content {
                assert_eq!(parts.len(), 2);
                assert!(matches!(parts[0], ResponseContentPart::InputText { .. }));
                assert!(matches!(parts[1], ResponseContentPart::InputImage { .. }));

                if let ResponseContentPart::InputText { text } = &parts[0] {
                    assert_eq!(text, "what is in this image?");
                }

                if let ResponseContentPart::InputImage { image_url, .. } = &parts[1] {
                    match image_url {
                        ResponseImageUrl::String(url) => {
                            assert_eq!(url, "https://example.com/image.jpg");
                        }
                        ResponseImageUrl::Object { url } => {
                            assert_eq!(url, "https://example.com/image.jpg");
                        }
                    }
                }
            } else {
                panic!("Expected Parts content");
            }
        } else {
            panic!("Expected Items input");
        }
    }

    #[test]
    fn test_create_response_request_with_function_tools() {
        let json = r#"{
            "model": "gpt-4.1",
            "input": "What is the weather like in Boston today?",
            "tools": [
                {
                    "type": "function",
                    "name": "get_current_weather",
                    "description": "Get the current weather in a given location",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "location": {
                                "type": "string",
                                "description": "The city and state, e.g. San Francisco, CA"
                            },
                            "unit": {
                                "type": "string",
                                "enum": ["celsius", "fahrenheit"]
                            }
                        },
                        "required": ["location", "unit"]
                    }
                }
            ],
            "tool_choice": "auto"
        }"#;

        let result: Result<CreateResponseRequest, _> = serde_json::from_str(json);
        assert!(result.is_ok(), "Failed to deserialize: {:?}", result.err());

        let request = result.unwrap();
        assert_eq!(request.model, "gpt-4.1");

        assert!(request.tools.is_some());
        let tools = request.tools.unwrap();
        assert_eq!(tools.len(), 1);

        if let ResponseTool::Function {
            name,
            description,
            parameters,
        } = &tools[0]
        {
            assert_eq!(name, "get_current_weather");
            assert_eq!(
                description.as_ref().unwrap(),
                "Get the current weather in a given location"
            );
            assert!(parameters.is_some());
        } else {
            panic!("Expected Function tool");
        }

        assert!(request.tool_choice.is_some());
        if let Some(ResponseToolChoice::Auto(choice)) = request.tool_choice {
            assert_eq!(choice, "auto");
        } else {
            panic!("Expected Auto tool choice");
        }
    }

    #[test]
    fn test_image_url_string_format() {
        let json = r#""https://example.com/image.jpg""#;
        let result: Result<ResponseImageUrl, _> = serde_json::from_str(json);
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), ResponseImageUrl::String(_)));
    }

    #[test]
    fn test_image_url_object_format() {
        let json = r#"{"url": "https://example.com/image.jpg"}"#;
        let result: Result<ResponseImageUrl, _> = serde_json::from_str(json);
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), ResponseImageUrl::Object { .. }));
    }

    // ChatCompletionRequest validation tests
    #[test]
    fn test_chat_completion_request_with_text_content_array() {
        let request = ChatCompletionRequest {
            model: "gpt-4".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: Some(MessageContent::Parts(vec![
                    MessageContentPart::Text {
                        text: "Hello".to_string(),
                    },
                    MessageContentPart::Text {
                        text: "World".to_string(),
                    },
                ])),
                name: None,
            }],
            max_tokens: Some(100),
            temperature: None,
            top_p: None,
            n: None,
            stream: None,
            stop: None,
            presence_penalty: None,
            frequency_penalty: None,
            extra: std::collections::HashMap::new(),
        };

        // Text-only content array should pass validation
        assert!(request.validate().is_ok());
    }

    #[test]
    fn test_chat_completion_request_with_image_content_rejected() {
        let request = ChatCompletionRequest {
            model: "gpt-4".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: Some(MessageContent::Parts(vec![
                    MessageContentPart::Text {
                        text: "What's in this image?".to_string(),
                    },
                    MessageContentPart::ImageUrl {
                        image_url: MessageImageUrl::String(
                            "data:image/jpeg;base64,/9j/4AAQSkZJRg==".to_string(),
                        ),
                        detail: Some("low".to_string()),
                    },
                ])),
                name: None,
            }],
            max_tokens: Some(100),
            temperature: None,
            top_p: None,
            n: None,
            stream: None,
            stop: None,
            presence_penalty: None,
            frequency_penalty: None,
            extra: std::collections::HashMap::new(),
        };

        // Image content should be rejected
        let result = request.validate();
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "Content array contains non-text parts (image, audio, or file). Only text content is currently supported."
        );
    }

    #[test]
    fn test_chat_completion_request_with_audio_content_rejected() {
        let request = ChatCompletionRequest {
            model: "gpt-4".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: Some(MessageContent::Parts(vec![MessageContentPart::Audio {
                    audio: MessageAudio {
                        data: "base64_audio_data".to_string(),
                        format: Some("mp3".to_string()),
                    },
                }])),
                name: None,
            }],
            max_tokens: Some(100),
            temperature: None,
            top_p: None,
            n: None,
            stream: None,
            stop: None,
            presence_penalty: None,
            frequency_penalty: None,
            extra: std::collections::HashMap::new(),
        };

        // Audio content should be rejected
        let result = request.validate();
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "Content array contains non-text parts (image, audio, or file). Only text content is currently supported."
        );
    }

    #[test]
    fn test_chat_completion_request_with_file_content_rejected() {
        let request = ChatCompletionRequest {
            model: "gpt-4".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: Some(MessageContent::Parts(vec![MessageContentPart::File {
                    file_id: "file-abc123".to_string(),
                }])),
                name: None,
            }],
            max_tokens: Some(100),
            temperature: None,
            top_p: None,
            n: None,
            stream: None,
            stop: None,
            presence_penalty: None,
            frequency_penalty: None,
            extra: std::collections::HashMap::new(),
        };

        // File content should be rejected
        let result = request.validate();
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "Content array contains non-text parts (image, audio, or file). Only text content is currently supported."
        );
    }

    #[test]
    fn test_chat_completion_request_with_string_content_allowed() {
        let request = ChatCompletionRequest {
            model: "gpt-4".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: Some(MessageContent::Text("Hello, world!".to_string())),
                name: None,
            }],
            max_tokens: Some(100),
            temperature: None,
            top_p: None,
            n: None,
            stream: None,
            stop: None,
            presence_penalty: None,
            frequency_penalty: None,
            extra: std::collections::HashMap::new(),
        };

        // String content should pass validation
        assert!(request.validate().is_ok());
    }

    #[test]
    fn test_is_basic_valid_email_accepts_simple_email() {
        assert!(is_basic_valid_email("user@example.com"));
    }

    #[test]
    fn test_is_basic_valid_email_rejects_multiple_ats() {
        assert!(!is_basic_valid_email("user@domain@example.com"));
    }
}
