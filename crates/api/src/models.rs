use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// Streaming response models
#[derive(Debug, Serialize, Deserialize)]
pub struct StreamChunkResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<StreamChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StreamChoice {
    pub index: u32,
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
    #[serde(default = "default_max_tokens")]
    pub max_tokens: Option<u32>,
    #[serde(default = "default_temperature")]
    pub temperature: Option<f32>,
    #[serde(default = "default_top_p")]
    pub top_p: Option<f32>,
    #[serde(default = "default_n")]
    pub n: Option<u32>,
    pub stream: Option<bool>,
    pub stop: Option<Vec<String>>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Message {
    pub role: String, // "system", "user", "assistant"
    pub content: String,
    pub name: Option<String>,
}

#[derive(Debug, Serialize, ToSchema, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String, // "chat.completion"
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChatChoice>,
    pub usage: Usage,
}

#[derive(Debug, Serialize, ToSchema, Deserialize)]
pub struct ChatChoice {
    pub index: u32,
    pub message: Message,
    pub finish_reason: Option<String>, // "stop", "length", "content_filter"
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CompletionRequest {
    pub model: String,
    pub prompt: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: Option<u32>,
    #[serde(default = "default_temperature")]
    pub temperature: Option<f32>,
    #[serde(default = "default_top_p")]
    pub top_p: Option<f32>,
    #[serde(default = "default_n")]
    pub n: Option<u32>,
    pub stream: Option<bool>,
    pub logprobs: Option<u32>,
    pub echo: Option<bool>,
    pub stop: Option<Vec<String>>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub best_of: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct CompletionResponse {
    pub id: String,
    pub object: String, // "text_completion"
    pub created: u64,
    pub model: String,
    pub choices: Vec<CompletionChoice>,
    pub usage: Usage,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ModelsResponse {
    pub object: String,
    pub data: Vec<ModelInfo>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ModelInfo {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub owned_by: String,
}

#[derive(Debug, Serialize)]
pub struct CompletionChoice {
    pub index: u32,
    pub text: String,
    pub logprobs: Option<serde_json::Value>,
    pub finish_reason: Option<String>, // "stop", "length", "content_filter"
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Usage {
    pub input_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens_details: Option<InputTokensDetails>,
    pub output_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens_details: Option<OutputTokensDetails>,
    pub total_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InputTokensDetails {
    pub cached_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OutputTokensDetails {
    pub reasoning_tokens: u32,
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

fn default_max_tokens() -> Option<u32> {
    Some(100)
}

fn default_temperature() -> Option<f32> {
    Some(1.0)
}

fn default_top_p() -> Option<f32> {
    Some(1.0)
}

fn default_n() -> Option<u32> {
    Some(1)
}

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
            if !["system", "user", "assistant"].contains(&message.role.as_str()) {
                return Err(format!("invalid message role: {}", message.role));
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
    pub fn new(input_tokens: u32, output_tokens: u32) -> Self {
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
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
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
#[serde(tag = "type")]
pub enum ResponseInputItem {
    #[serde(rename = "message")]
    Message {
        role: String,
        content: ResponseContent,
    },
}

/// Content can be text or array of content parts
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum ResponseContent {
    Text(String),
    Parts(Vec<ResponseContentPart>),
}

/// Content part (text, image, etc.)
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

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResponseImageUrl {
    pub url: String,
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
    Function { function: ResponseFunction },
    #[serde(rename = "web_search")]
    WebSearch {},
    #[serde(rename = "file_search")]
    FileSearch {},
    #[serde(rename = "code_interpreter")]
    CodeInterpreter {},
    #[serde(rename = "computer")]
    Computer {},
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResponseFunction {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
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
    pub created_at: u64,
    pub status: ResponseStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incomplete_details: Option<ResponseIncompleteDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    pub model: String,
    pub output: Vec<ResponseOutputItem>,
    pub parallel_tool_calls: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ResponseItemStatus {
    Completed,
    Failed,
    InProgress,
    Cancelled,
}

/// Output content part
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

/// Conversation object
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ConversationObject {
    pub id: String,
    pub object: String, // "conversation"
    pub created_at: u64,
    pub metadata: serde_json::Value,
}

/// List of conversations
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ConversationList {
    pub object: String, // "list"
    pub data: Vec<ConversationObject>,
    pub first_id: String,
    pub last_id: String,
    pub has_more: bool,
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
    #[serde(rename = "output_text")]
    OutputText {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<Vec<serde_json::Value>>,
    },
}

/// Conversation item (for responses)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum ConversationItem {
    #[serde(rename = "message")]
    Message {
        id: String,
        status: ResponseItemStatus,
        role: String,
        content: Vec<ConversationContentPart>,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
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

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreateApiKeyRequest {
    pub name: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

// ============================================
// Organization API Models
// ============================================

/// Request to create a new organization
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreateOrganizationRequest {
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
}

/// Request to update an organization
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UpdateOrganizationRequest {
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub rate_limit: Option<i32>,
    pub settings: Option<serde_json::Value>,
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

/// Request to update an organization member
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateOrganizationMemberRequest {
    pub role: MemberRole,
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
}

/// Session response model
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SessionResponse {
    pub id: String,
    pub user_id: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
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
}

/// Request to update API key spend limit
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateApiKeySpendLimitRequest {
    #[serde(rename = "spendLimit")]
    pub spend_limit: Option<DecimalPrice>,
}

// ============================================
// Model Listing API Models
// ============================================

/// Response for model list endpoint
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ModelListResponse {
    pub models: Vec<ModelWithPricing>,
    #[serde(rename = "totalModels")]
    pub total_models: usize,
    pub page: usize,
    #[serde(rename = "pageSize")]
    pub page_size: usize,
    #[serde(rename = "totalPages")]
    pub total_pages: usize,
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

/// Decimal price representation using amount/scale/currency
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct DecimalPrice {
    pub amount: i64,
    pub scale: i32,
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
}

/// Request to update model pricing (admin endpoint)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpdateModelApiRequest {
    #[serde(rename = "inputCostPerToken")]
    pub input_cost_per_token: Option<DecimalPrice>,
    #[serde(rename = "outputCostPerToken")]
    pub output_cost_per_token: Option<DecimalPrice>,
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
}

/// Batch update request format - Array of model name to update data
pub type BatchUpdateModelApiRequest = std::collections::HashMap<String, UpdateModelApiRequest>;

/// Model pricing history entry
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ModelPricingHistoryEntry {
    pub id: String,
    #[serde(rename = "modelId")]
    pub model_id: String,
    #[serde(rename = "inputCostPerToken")]
    pub input_cost_per_token: DecimalPrice,
    #[serde(rename = "outputCostPerToken")]
    pub output_cost_per_token: DecimalPrice,
    #[serde(rename = "contextLength")]
    pub context_length: i32,
    #[serde(rename = "modelDisplayName")]
    pub model_display_name: String,
    #[serde(rename = "modelDescription")]
    pub model_description: String,
    #[serde(rename = "effectiveFrom")]
    pub effective_from: String,
    #[serde(rename = "effectiveUntil")]
    pub effective_until: Option<String>,
    #[serde(rename = "changedBy")]
    pub changed_by: Option<String>,
    #[serde(rename = "changeReason")]
    pub change_reason: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

/// Model pricing history response
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ModelPricingHistoryResponse {
    #[serde(rename = "modelName")]
    pub model_name: String,
    pub history: Vec<ModelPricingHistoryEntry>,
}

// ============================================
// Organization Limits API Models (Admin)
// ============================================

/// Request to update organization limits (Admin only)
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateOrganizationLimitsRequest {
    #[serde(rename = "spendLimit")]
    pub spend_limit: SpendLimit,
    #[serde(rename = "changedBy", skip_serializing_if = "Option::is_none")]
    pub changed_by: Option<String>,
    #[serde(rename = "changeReason", skip_serializing_if = "Option::is_none")]
    pub change_reason: Option<String>,
}

/// Spend limit with amount, scale, and currency (using decimal representation)
/// Examples:
///   $100.00 USD: amount=10000, scale=2, currency="USD"
///   0.0001 BTC: amount=1, scale=4, currency="BTC"
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SpendLimit {
    pub amount: i64,
    pub scale: i32,
    pub currency: String,
}

/// Response after updating organization limits
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UpdateOrganizationLimitsResponse {
    pub organization_id: String,
    #[serde(rename = "spendLimit")]
    pub spend_limit: SpendLimit,
    pub updated_at: String,
}
