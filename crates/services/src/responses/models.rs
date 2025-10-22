// ============================================
// Response Domain Models (Services Layer)
// ============================================

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseId(pub Uuid);

impl From<Uuid> for ResponseId {
    fn from(uuid: Uuid) -> Self {
        ResponseId(uuid)
    }
}

impl std::fmt::Display for ResponseId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "resp_{}", self.0)
    }
}

/// Request to create a response
#[derive(Debug)]
pub struct CreateResponseRequest {
    pub model: String,
    pub input: Option<ResponseInput>,
    pub instructions: Option<String>,
    pub conversation: Option<ConversationReference>,
    pub previous_response_id: Option<String>,
    pub max_output_tokens: Option<i64>,
    pub max_tool_calls: Option<i64>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub stream: Option<bool>,
    pub store: Option<bool>,
    pub background: Option<bool>,
    pub tools: Option<Vec<ResponseTool>>,
    pub tool_choice: Option<ResponseToolChoice>,
    pub parallel_tool_calls: Option<bool>,
    pub text: Option<ResponseTextConfig>,
    pub reasoning: Option<ResponseReasoningConfig>,
    pub include: Option<Vec<String>>,
    pub metadata: Option<serde_json::Value>,
    pub safety_identifier: Option<String>,
    pub prompt_cache_key: Option<String>,
}

/// Input for a response - can be text, array of items, or single item
#[derive(Debug, Clone)]
pub enum ResponseInput {
    Text(String),
    Items(Vec<ResponseInputItem>),
}

/// Single input item
#[derive(Debug, Clone)]
pub struct ResponseInputItem {
    pub role: String,
    pub content: ResponseContent,
}

/// Content can be text or array of content parts
#[derive(Debug, Clone)]
pub enum ResponseContent {
    Text(String),
    Parts(Vec<ResponseContentPart>),
}

/// Content part (text, image, etc.)
#[derive(Debug, Clone)]
pub enum ResponseContentPart {
    InputText {
        text: String,
    },
    InputImage {
        image_url: ResponseImageUrl,
        detail: Option<String>,
    },
    InputFile {
        file_id: String,
        detail: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub enum ResponseImageUrl {
    String(String),
    Object { url: String },
}

/// Conversation reference
#[derive(Debug, Clone)]
pub enum ConversationReference {
    Id(String),
    Object {
        id: String,
        metadata: Option<serde_json::Value>,
    },
}

/// Tool configuration for responses
#[derive(Debug, Clone)]
pub enum ResponseTool {
    Function {
        name: String,
        description: Option<String>,
        parameters: Option<serde_json::Value>,
    },
    WebSearch {},
    FileSearch {},
    CodeInterpreter {},
    Computer {},
}

/// Tool choice configuration
#[derive(Debug)]
pub enum ResponseToolChoice {
    Auto(String), // "auto", "none", "required"
    Specific {
        type_: String,
        function: ResponseToolChoiceFunction,
    },
}

#[derive(Debug, Clone)]
pub struct ResponseToolChoiceFunction {
    pub name: String,
}

/// Text format configuration
#[derive(Debug, Clone)]
pub struct ResponseTextConfig {
    pub format: ResponseTextFormat,
}

#[derive(Debug, Clone)]
pub enum ResponseTextFormat {
    Text,
    JsonObject,
    JsonSchema { json_schema: serde_json::Value },
}

/// Reasoning configuration
#[derive(Debug)]
pub struct ResponseReasoningConfig {
    pub effort: Option<String>,
}

/// Complete response object
#[derive(Debug, Clone)]
pub struct ResponseObject {
    pub id: String,
    pub object: String, // "response"
    pub created_at: i64,
    pub status: ResponseStatus,
    pub error: Option<ResponseError>,
    pub incomplete_details: Option<ResponseIncompleteDetails>,
    pub instructions: Option<String>,
    pub max_output_tokens: Option<i64>,
    pub max_tool_calls: Option<i64>,
    pub model: String,
    pub output: Vec<ResponseOutputItem>,
    pub parallel_tool_calls: bool,
    pub previous_response_id: Option<String>,
    pub reasoning: Option<ResponseReasoningOutput>,
    pub store: bool,
    pub temperature: f32,
    pub text: Option<ResponseTextConfig>,
    pub tool_choice: ResponseToolChoiceOutput,
    pub tools: Vec<ResponseTool>,
    pub top_p: f32,
    pub truncation: String,
    pub usage: Usage,
    pub user: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResponseStatus {
    Completed,
    Failed,
    InProgress,
    Cancelled,
    Queued,
    Incomplete,
}

#[derive(Debug, Clone)]
pub struct ResponseError {
    pub message: String,
    pub type_: String,
    pub code: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResponseIncompleteDetails {
    pub reason: String, // "length", "content_filter", "max_tool_calls"
}

/// Output item from response
#[derive(Debug, Clone)]
pub enum ResponseOutputItem {
    Message {
        id: String,
        status: ResponseItemStatus,
        role: String,
        content: Vec<ResponseOutputContent>,
    },
    ToolCall {
        id: String,
        status: ResponseItemStatus,
        tool_type: String,
        function: ResponseOutputFunction,
    },
    Reasoning {
        id: String,
        status: ResponseItemStatus,
        summary: String,
        content: String,
    },
}

#[derive(Debug, Clone)]
pub enum ResponseItemStatus {
    Completed,
    Failed,
    InProgress,
    Cancelled,
}

/// Output content part
#[derive(Debug, Clone)]
pub enum ResponseOutputContent {
    OutputText {
        text: String,
        annotations: Vec<serde_json::Value>,
    },
    ToolCalls {
        tool_calls: Vec<ResponseOutputToolCall>,
    },
}

#[derive(Debug, Clone)]
pub struct ResponseOutputFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone)]
pub struct ResponseOutputToolCall {
    pub id: String,
    pub type_: String,
    pub function: ResponseOutputFunction,
}

#[derive(Debug, Clone)]
pub struct ResponseReasoningOutput {
    pub effort: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ResponseToolChoiceOutput {
    Auto(String),
    Object {
        type_: String,
        function: ResponseToolChoiceFunction,
    },
}

/// Response deletion result
#[derive(Debug, Clone)]
pub struct ResponseDeleteResult {
    pub id: String,
    pub object: String, // "response"
    pub deleted: bool,
}

// ============================================
// Response Streaming Event Types
// ============================================

/// Response streaming event wrapper
#[derive(Debug, Clone)]
pub struct ResponseStreamEvent {
    pub event_type: String,
    pub response: Option<ResponseObject>,
    pub output_index: Option<usize>,
    pub content_index: Option<usize>,
    pub item: Option<ResponseOutputItem>,
    pub item_id: Option<String>,
    pub part: Option<ResponseOutputContent>,
    pub delta: Option<String>,
    pub text: Option<String>,
}

/// Input item list for responses
#[derive(Debug, Clone)]
pub struct ResponseInputItemList {
    pub object: String, // "list"
    pub data: Vec<ResponseInputItem>,
    pub first_id: String,
    pub last_id: String,
    pub has_more: bool,
}

// ============================================
// Conversation Domain Models
// ============================================

/// Request to create a conversation
#[derive(Debug)]
pub struct CreateConversationRequest {
    pub metadata: Option<serde_json::Value>,
}

/// Request to update a conversation
#[derive(Debug)]
pub struct UpdateConversationRequest {
    pub metadata: Option<serde_json::Value>,
}

/// Conversation object
#[derive(Debug, Clone)]
pub struct ConversationObject {
    pub id: String,
    pub object: String, // "conversation"
    pub created_at: i64,
    pub metadata: serde_json::Value,
}

/// Deleted conversation result
#[derive(Debug, Clone)]
pub struct ConversationDeleteResult {
    pub id: String,
    pub object: String, // "conversation.deleted"
    pub deleted: bool,
}

/// Input item for conversations
#[derive(Debug)]
pub enum ConversationInputItem {
    Message {
        role: String,
        content: ConversationContent,
        metadata: Option<serde_json::Value>,
    },
}

/// Content for conversation items
#[derive(Debug)]
pub enum ConversationContent {
    Text(String),
    Parts(Vec<ConversationContentPart>),
}

/// Content part for conversations
#[derive(Debug, Clone)]
pub enum ConversationContentPart {
    InputText {
        text: String,
    },
    InputImage {
        image_url: ResponseImageUrl,
        detail: Option<String>,
    },
    OutputText {
        text: String,
        annotations: Option<Vec<serde_json::Value>>,
    },
}

/// Conversation item (for responses)
#[derive(Debug, Clone)]
pub enum ConversationItem {
    Message {
        id: String,
        status: ResponseItemStatus,
        role: String,
        content: Vec<ConversationContentPart>,
        metadata: Option<serde_json::Value>,
    },
}

/// List of conversation items
#[derive(Debug, Clone)]
pub struct ConversationItemList {
    pub object: String, // "list"
    pub data: Vec<ConversationItem>,
    pub first_id: String,
    pub last_id: String,
    pub has_more: bool,
}

// ============================================
// Usage Models
// ============================================

#[derive(Debug, Clone)]
pub struct Usage {
    pub input_tokens: i32,
    pub input_tokens_details: Option<InputTokensDetails>,
    pub output_tokens: i32,
    pub output_tokens_details: Option<OutputTokensDetails>,
    pub total_tokens: i32,
}

#[derive(Debug, Clone)]
pub struct InputTokensDetails {
    pub cached_tokens: i64,
}

#[derive(Debug, Clone)]
pub struct OutputTokensDetails {
    pub reasoning_tokens: i64,
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
        // Basic validation - can be extended if needed
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
