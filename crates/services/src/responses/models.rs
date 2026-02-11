// ============================================
// Response Domain Models (Services Layer)
// ============================================

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseId(pub Uuid);

impl From<Uuid> for ResponseId {
    fn from(uuid: Uuid) -> Self {
        ResponseId(uuid)
    }
}

impl ResponseId {
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl std::fmt::Display for ResponseId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "resp_{}", self.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseItemId(pub Uuid);

impl From<Uuid> for ResponseItemId {
    fn from(uuid: Uuid) -> Self {
        ResponseItemId(uuid)
    }
}

/// Request to create a response
#[derive(Debug, Clone, Deserialize, ToSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum ResponseInput {
    Text(String),
    Items(Vec<ResponseInputItem>),
}

/// Single input item
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum ResponseInputItem {
    McpApprovalResponse {
        #[serde(rename = "type")]
        type_: McpApprovalResponseType,
        approval_request_id: String,
        approve: bool,
    },
    McpListTools {
        #[serde(rename = "type")]
        type_: McpListToolsType,
        id: String,
        server_label: String,
        tools: Vec<McpDiscoveredTool>,
    },
    Message {
        role: String,
        content: ResponseContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
}

/// Type marker for MCP list tools input
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub enum McpListToolsType {
    #[serde(rename = "mcp_list_tools")]
    McpListTools,
}

impl ResponseInputItem {
    pub fn role(&self) -> Option<&str> {
        match self {
            ResponseInputItem::Message { role, .. } => Some(role),
            ResponseInputItem::McpApprovalResponse { .. } => None,
            ResponseInputItem::McpListTools { .. } => None,
        }
    }

    pub fn content(&self) -> Option<&ResponseContent> {
        match self {
            ResponseInputItem::Message { content, .. } => Some(content),
            ResponseInputItem::McpApprovalResponse { .. } => None,
            ResponseInputItem::McpListTools { .. } => None,
        }
    }

    pub fn metadata(&self) -> Option<&serde_json::Value> {
        match self {
            ResponseInputItem::Message { metadata, .. } => metadata.as_ref(),
            ResponseInputItem::McpApprovalResponse { .. } => None,
            ResponseInputItem::McpListTools { .. } => None,
        }
    }

    pub fn is_mcp_approval(&self) -> bool {
        matches!(self, ResponseInputItem::McpApprovalResponse { .. })
    }

    pub fn as_mcp_approval(&self) -> Option<(&str, bool)> {
        match self {
            ResponseInputItem::McpApprovalResponse {
                approval_request_id,
                approve,
                ..
            } => Some((approval_request_id, *approve)),
            ResponseInputItem::Message { .. } => None,
            ResponseInputItem::McpListTools { .. } => None,
        }
    }
}

/// Type marker for MCP approval response input
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub enum McpApprovalResponseType {
    #[serde(rename = "mcp_approval_response")]
    McpApprovalResponse,
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
#[serde(untagged)]
pub enum ResponseImageUrl {
    String(String),
    Object { url: String },
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
    WebSearch {
        #[serde(skip_serializing_if = "Option::is_none")]
        filters: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        search_context_size: Option<String>, // "small", "medium", "large"
        #[serde(skip_serializing_if = "Option::is_none")]
        user_location: Option<UserLocation>,
    },
    #[serde(rename = "file_search")]
    FileSearch {},
    #[serde(rename = "code_interpreter")]
    CodeInterpreter {},
    #[serde(rename = "computer")]
    Computer {},
    /// Remote MCP server tool
    #[serde(rename = "mcp")]
    Mcp {
        server_label: String,
        /// HTTPS endpoint for the remote MCP server
        server_url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        server_description: Option<String>,
        /// Authorization header for the MCP server (never serialized in responses for security)
        #[serde(skip_serializing)]
        authorization: Option<String>,
        /// Tool approval requirement (default: "always")
        #[serde(default)]
        require_approval: McpApprovalRequirement,
        #[serde(skip_serializing_if = "Option::is_none")]
        allowed_tools: Option<Vec<String>>,
    },
}

/// User location for web search
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserLocation {
    #[serde(rename = "type")]
    pub type_: String, // "approximate", "exact"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

// ============================================
// MCP (Model Context Protocol) Types
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum McpApprovalRequirement {
    Simple(McpApprovalMode),
    Granular { never: McpToolNameFilter },
}

impl Default for McpApprovalRequirement {
    fn default() -> Self {
        McpApprovalRequirement::Simple(McpApprovalMode::Always)
    }
}

impl McpApprovalRequirement {
    /// Check if a specific tool requires approval
    pub fn requires_approval(&self, tool_name: &str) -> bool {
        match self {
            McpApprovalRequirement::Simple(mode) => matches!(mode, McpApprovalMode::Always),
            McpApprovalRequirement::Granular { never } => !never.tool_names.contains(tool_name),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum McpApprovalMode {
    #[default]
    Always,
    Never,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct McpToolNameFilter {
    pub tool_names: HashSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct McpDiscoveredTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<serde_json::Value>,
}

/// Tool choice configuration
#[derive(Debug, Clone, Deserialize, ToSchema)]
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

/// Reasoning configuration
#[derive(Debug, Clone, Deserialize, ToSchema)]
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
    #[serde(default)]
    pub background: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation: Option<ConversationResponseReference>,
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
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ResponseReasoningOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety_identifier: Option<String>,
    #[serde(default = "default_service_tier")]
    pub service_tier: String,
    pub store: bool,
    pub temperature: f32,
    pub tool_choice: ResponseToolChoiceOutput,
    pub tools: Vec<ResponseTool>,
    #[serde(default)]
    pub top_logprobs: i32,
    pub top_p: f32,
    #[serde(default = "default_truncation")]
    pub truncation: String,
    pub usage: Usage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

fn default_service_tier() -> String {
    "default".to_string()
}

fn default_truncation() -> String {
    "disabled".to_string()
}

/// Conversation reference in response object
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ConversationResponseReference {
    pub id: String,
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
        #[serde(default)]
        response_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_response_id: Option<String>,
        #[serde(default)]
        next_response_ids: Vec<String>,
        #[serde(default)]
        created_at: i64,
        status: ResponseItemStatus,
        role: String,
        content: Vec<ResponseContentItem>,
        model: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        #[serde(default)]
        response_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_response_id: Option<String>,
        #[serde(default)]
        next_response_ids: Vec<String>,
        #[serde(default)]
        created_at: i64,
        status: ResponseItemStatus,
        tool_type: String,
        function: ResponseOutputFunction,
        model: String,
    },
    #[serde(rename = "web_search_call")]
    WebSearchCall {
        id: String,
        #[serde(default)]
        response_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_response_id: Option<String>,
        #[serde(default)]
        next_response_ids: Vec<String>,
        #[serde(default)]
        created_at: i64,
        status: ResponseItemStatus,
        action: WebSearchAction,
        model: String,
    },
    #[serde(rename = "reasoning")]
    Reasoning {
        id: String,
        #[serde(default)]
        response_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_response_id: Option<String>,
        #[serde(default)]
        next_response_ids: Vec<String>,
        #[serde(default)]
        created_at: i64,
        status: ResponseItemStatus,
        summary: String,
        content: String,
        model: String,
    },
    /// MCP tool list - emitted after connecting to an MCP server.
    /// Clients can include this in subsequent requests to skip tool discovery.
    #[serde(rename = "mcp_list_tools")]
    McpListTools {
        id: String,
        server_label: String,
        tools: Vec<McpDiscoveredTool>,
        /// Error message if the server could not list tools
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// MCP tool call - emitted after executing a tool on an MCP server
    #[serde(rename = "mcp_call")]
    McpCall {
        id: String,
        #[serde(default)]
        response_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_response_id: Option<String>,
        #[serde(default)]
        next_response_ids: Vec<String>,
        #[serde(default)]
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
        /// Status of the tool call: in_progress, completed, incomplete, calling, or failed
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        model: String,
    },
    /// MCP approval request - emitted when a tool requires approval
    #[serde(rename = "mcp_approval_request")]
    McpApprovalRequest {
        id: String,
        #[serde(default)]
        response_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_response_id: Option<String>,
        #[serde(default)]
        next_response_ids: Vec<String>,
        #[serde(default)]
        created_at: i64,
        server_label: String,
        name: String,
        arguments: String,
        model: String,
    },
}

impl ResponseOutputItem {
    /// Get the ID of the output item
    pub fn id(&self) -> &str {
        match self {
            ResponseOutputItem::Message { id, .. } => id,
            ResponseOutputItem::ToolCall { id, .. } => id,
            ResponseOutputItem::WebSearchCall { id, .. } => id,
            ResponseOutputItem::Reasoning { id, .. } => id,
            ResponseOutputItem::McpListTools { id, .. } => id,
            ResponseOutputItem::McpCall { id, .. } => id,
            ResponseOutputItem::McpApprovalRequest { id, .. } => id,
        }
    }

    /// Get the status of the output item
    pub fn status(&self) -> ResponseItemStatus {
        match self {
            ResponseOutputItem::Message { status, .. } => status.clone(),
            ResponseOutputItem::ToolCall { status, .. } => status.clone(),
            ResponseOutputItem::WebSearchCall { status, .. } => status.clone(),
            ResponseOutputItem::Reasoning { status, .. } => status.clone(),
            ResponseOutputItem::McpListTools { .. } => ResponseItemStatus::Completed,
            ResponseOutputItem::McpCall { .. } => ResponseItemStatus::Completed,
            ResponseOutputItem::McpApprovalRequest { .. } => ResponseItemStatus::InProgress,
        }
    }

    /// Get the model of the output item
    pub fn model(&self) -> Option<&str> {
        match self {
            ResponseOutputItem::Message { model, .. } => Some(model),
            ResponseOutputItem::ToolCall { model, .. } => Some(model),
            ResponseOutputItem::WebSearchCall { model, .. } => Some(model),
            ResponseOutputItem::Reasoning { model, .. } => Some(model),
            ResponseOutputItem::McpListTools { .. } => None,
            ResponseOutputItem::McpCall { model, .. } => Some(model),
            ResponseOutputItem::McpApprovalRequest { model, .. } => Some(model),
        }
    }

    /// Get the response_id of the output item
    pub fn response_id(&self) -> Option<&str> {
        match self {
            ResponseOutputItem::Message { response_id, .. } => Some(response_id),
            ResponseOutputItem::ToolCall { response_id, .. } => Some(response_id),
            ResponseOutputItem::WebSearchCall { response_id, .. } => Some(response_id),
            ResponseOutputItem::Reasoning { response_id, .. } => Some(response_id),
            ResponseOutputItem::McpListTools { .. } => None,
            ResponseOutputItem::McpCall { response_id, .. } => Some(response_id),
            ResponseOutputItem::McpApprovalRequest { response_id, .. } => Some(response_id),
        }
    }

    /// Get the previous_response_id of the output item
    pub fn previous_response_id(&self) -> Option<&str> {
        match self {
            ResponseOutputItem::Message {
                previous_response_id,
                ..
            } => previous_response_id.as_deref(),
            ResponseOutputItem::ToolCall {
                previous_response_id,
                ..
            } => previous_response_id.as_deref(),
            ResponseOutputItem::WebSearchCall {
                previous_response_id,
                ..
            } => previous_response_id.as_deref(),
            ResponseOutputItem::Reasoning {
                previous_response_id,
                ..
            } => previous_response_id.as_deref(),
            ResponseOutputItem::McpListTools { .. } => None,
            ResponseOutputItem::McpCall {
                previous_response_id,
                ..
            } => previous_response_id.as_deref(),
            ResponseOutputItem::McpApprovalRequest {
                previous_response_id,
                ..
            } => previous_response_id.as_deref(),
        }
    }
}

/// Web search action details
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum WebSearchAction {
    #[serde(rename = "search")]
    Search { query: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ResponseItemStatus {
    Completed,
    Failed,
    InProgress,
    Cancelled,
}

/// Registry to track web search sources during response generation (request-scoped)
/// Stores WebSearchResult from provider.search() for citation resolution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRegistry {
    pub web_sources: Vec<crate::responses::tools::WebSearchResult>,
}

impl SourceRegistry {
    pub fn with_results(results: Vec<crate::responses::tools::WebSearchResult>) -> Self {
        Self {
            web_sources: results,
        }
    }
}

/// Annotation for output text (citations, etc.)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum TextAnnotation {
    #[serde(rename = "url_citation")]
    UrlCitation {
        start_index: usize,
        end_index: usize,
        title: String,
        url: String,
    },
}

/// Unified content item that can represent both user inputs and assistant outputs
/// This replaces ResponseOutputContent and correctly represents semantic types
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum ResponseContentItem {
    // ===== INPUT VARIANTS (from user) =====
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

    // ===== OUTPUT VARIANTS (from assistant) =====
    #[serde(rename = "output_text")]
    OutputText {
        text: String,
        annotations: Vec<TextAnnotation>,
        #[serde(default)]
        logprobs: Vec<serde_json::Value>,
    },

    #[serde(rename = "tool_calls")]
    ToolCalls {
        tool_calls: Vec<ResponseOutputToolCall>,
    },

    #[serde(rename = "output_image")]
    OutputImage {
        /// Image data array (matches OpenAI format)
        data: Vec<ImageOutputData>,
        /// Optional URL (future support)
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ImageOutputData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub b64_json: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revised_prompt: Option<String>,
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

// ============================================
// ResponseContentItem Implementations
// ============================================

impl ResponseContentItem {
    /// Check if this content item is an input (from user)
    pub fn is_input(&self) -> bool {
        matches!(
            self,
            ResponseContentItem::InputText { .. }
                | ResponseContentItem::InputImage { .. }
                | ResponseContentItem::InputFile { .. }
        )
    }

    /// Check if this content item is an output (from assistant)
    pub fn is_output(&self) -> bool {
        matches!(
            self,
            ResponseContentItem::OutputText { .. }
                | ResponseContentItem::ToolCalls { .. }
                | ResponseContentItem::OutputImage { .. }
        )
    }

    /// Get text content if available
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ResponseContentItem::InputText { text } => Some(text),
            ResponseContentItem::OutputText { text, .. } => Some(text),
            _ => None,
        }
    }
}

/// Output content from assistant (output-only variants).
///
/// This type is used for type-safe operations on assistant outputs only.
/// It cannot contain input variants, providing compile-time safety.
/// Used in streaming events and response output items.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum ResponseOutputContent {
    #[serde(rename = "output_text")]
    OutputText {
        text: String,
        annotations: Vec<TextAnnotation>,
        #[serde(default)]
        logprobs: Vec<serde_json::Value>,
    },
    #[serde(rename = "tool_calls")]
    ToolCalls {
        tool_calls: Vec<ResponseOutputToolCall>,
    },
    #[serde(rename = "output_image")]
    OutputImage {
        data: Vec<ImageOutputData>,
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
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
    pub sequence_number: Option<u64>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub obfuscation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotation_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotation: Option<TextAnnotation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_title: Option<String>,
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
// Conversation Domain Models
// ============================================

/// Request to create a conversation
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateConversationRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Request to update a conversation
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateConversationRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Conversation object
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
#[derive(Debug, Clone, Deserialize, ToSchema)]
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
#[derive(Debug, Clone, Deserialize, ToSchema)]
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
        annotations: Option<Vec<TextAnnotation>>,
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
// Usage Models
// ============================================

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

// ============================================
// Validation implementations
// ============================================

impl CreateResponseRequest {
    pub fn validate(&self) -> Result<(), String> {
        use crate::common::MAX_METADATA_SIZE_BYTES;

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

        if let Some(metadata) = &self.metadata {
            let serialized =
                serde_json::to_string(metadata).map_err(|_| "Invalid metadata".to_string())?;
            if serialized.len() > MAX_METADATA_SIZE_BYTES {
                return Err(format!(
                    "metadata is too large (max {} bytes when serialized)",
                    MAX_METADATA_SIZE_BYTES
                ));
            }
        }

        // Validate input message metadata sizes
        if let Some(ResponseInput::Items(items)) = &self.input {
            for item in items {
                if let ResponseInputItem::Message {
                    metadata: Some(meta),
                    ..
                } = item
                {
                    let serialized = serde_json::to_string(meta)
                        .map_err(|_| "Invalid message metadata".to_string())?;
                    if serialized.len() > MAX_METADATA_SIZE_BYTES {
                        return Err(format!(
                            "message metadata is too large (max {} bytes when serialized)",
                            MAX_METADATA_SIZE_BYTES
                        ));
                    }
                }
            }
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

    pub fn new_with_reasoning(
        input_tokens: i32,
        output_tokens: i32,
        reasoning_tokens: i32,
    ) -> Self {
        Self {
            input_tokens,
            input_tokens_details: Some(InputTokensDetails { cached_tokens: 0 }),
            output_tokens,
            output_tokens_details: Some(OutputTokensDetails {
                reasoning_tokens: reasoning_tokens as i64,
            }),
            total_tokens: input_tokens + output_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_deserialize_old_response_item_message_without_new_fields() {
        // Simulate old JSON data that doesn't have response_id, created_at fields
        // This represents data stored in the database before the new fields were added
        let old_json = json!({
            "type": "message",
            "id": "msg_123",
            "status": "completed",
            "role": "assistant",
            "content": [],
            "model": "gpt-4"
        });

        // This should not panic and should deserialize with default values
        let result: Result<ResponseOutputItem, _> = serde_json::from_value(old_json);

        assert!(
            result.is_ok(),
            "Failed to deserialize old format: {:?}",
            result.err()
        );

        let item = result.unwrap();
        match item {
            ResponseOutputItem::Message {
                response_id,
                created_at,
                next_response_ids,
                previous_response_id,
                ..
            } => {
                assert_eq!(
                    response_id, "",
                    "response_id should default to empty string"
                );
                assert_eq!(created_at, 0, "created_at should default to 0");
                assert_eq!(
                    next_response_ids.len(),
                    0,
                    "next_response_ids should default to empty vec"
                );
                assert_eq!(
                    previous_response_id, None,
                    "previous_response_id should be None"
                );
            }
            _ => panic!("Expected Message variant"),
        }
    }

    #[test]
    fn test_deserialize_old_response_item_tool_call_without_new_fields() {
        let old_json = json!({
            "type": "tool_call",
            "id": "tool_456",
            "status": "completed",
            "tool_type": "function",
            "function": {
                "name": "test_function",
                "arguments": "{}"
            },
            "model": "gpt-4"
        });

        let result: Result<ResponseOutputItem, _> = serde_json::from_value(old_json);

        assert!(
            result.is_ok(),
            "Failed to deserialize old tool_call format: {:?}",
            result.err()
        );

        let item = result.unwrap();
        match item {
            ResponseOutputItem::ToolCall {
                response_id,
                created_at,
                next_response_ids,
                ..
            } => {
                assert_eq!(response_id, "");
                assert_eq!(created_at, 0);
                assert_eq!(next_response_ids.len(), 0);
            }
            _ => panic!("Expected ToolCall variant"),
        }
    }

    #[test]
    fn test_deserialize_response_item_with_new_fields() {
        // Test that new format still works
        let new_json = json!({
            "type": "message",
            "id": "msg_123",
            "response_id": "resp_abc",
            "previous_response_id": "resp_xyz",
            "next_response_ids": ["resp_def", "resp_ghi"],
            "created_at": 1234567890,
            "status": "completed",
            "role": "assistant",
            "content": [],
            "model": "gpt-4"
        });

        let result: Result<ResponseOutputItem, _> = serde_json::from_value(new_json);

        assert!(result.is_ok());

        let item = result.unwrap();
        match item {
            ResponseOutputItem::Message {
                response_id,
                created_at,
                next_response_ids,
                previous_response_id,
                ..
            } => {
                assert_eq!(response_id, "resp_abc");
                assert_eq!(created_at, 1234567890);
                assert_eq!(next_response_ids, vec!["resp_def", "resp_ghi"]);
                assert_eq!(previous_response_id, Some("resp_xyz".to_string()));
            }
            _ => panic!("Expected Message variant"),
        }
    }

    #[test]
    fn test_response_input_item_backward_compatibility() {
        // Test that old format {role, content} still deserializes correctly
        // This ensures backward compatibility when upgrading from struct to enum
        let old_format_json = r#"{"role": "user", "content": "Hello world"}"#;

        let result: Result<ResponseInputItem, _> = serde_json::from_str(old_format_json);

        assert!(
            result.is_ok(),
            "Old format (struct) should deserialize to Message variant: {:?}",
            result.err()
        );

        match result.unwrap() {
            ResponseInputItem::Message { role, content, .. } => {
                assert_eq!(role, "user");
                match content {
                    ResponseContent::Text(text) => assert_eq!(text, "Hello world"),
                    _ => panic!("Expected Text content"),
                }
            }
            _ => panic!("Expected Message variant for old format"),
        }
    }

    #[test]
    fn test_deserialize_old_response_item_message_without_metadata_field() {
        // Simulate old JSON data that doesn't have metadata field at all
        // This represents data stored in the database before the metadata field was added
        let old_json = json!({
            "type": "message",
            "id": "msg_123",
            "response_id": "resp_abc",
            "created_at": 1234567890,
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": "Hello!",
                "annotations": []
            }],
            "model": "gpt-4"
            // Note: no "metadata" field at all
        });

        let result: Result<ResponseOutputItem, _> = serde_json::from_value(old_json);

        assert!(
            result.is_ok(),
            "Failed to deserialize old format without metadata: {:?}",
            result.err()
        );

        let item = result.unwrap();
        match item {
            ResponseOutputItem::Message {
                id,
                metadata,
                content,
                ..
            } => {
                assert_eq!(id, "msg_123");
                assert!(
                    metadata.is_none(),
                    "metadata should be None when field is missing from JSON"
                );
                assert_eq!(content.len(), 1);
            }
            _ => panic!("Expected Message variant"),
        }
    }

    #[test]
    fn test_deserialize_response_item_message_with_metadata_field() {
        // Test that new format with metadata field works correctly
        let new_json = json!({
            "type": "message",
            "id": "msg_456",
            "response_id": "resp_def",
            "created_at": 1234567890,
            "status": "completed",
            "role": "assistant",
            "content": [],
            "model": "gpt-4",
            "metadata": {
                "custom_key": "custom_value",
                "nested": {"foo": "bar"}
            }
        });

        let result: Result<ResponseOutputItem, _> = serde_json::from_value(new_json);

        assert!(result.is_ok());

        let item = result.unwrap();
        match item {
            ResponseOutputItem::Message { id, metadata, .. } => {
                assert_eq!(id, "msg_456");
                assert!(metadata.is_some(), "metadata should be present");
                let meta = metadata.unwrap();
                assert_eq!(meta["custom_key"], "custom_value");
                assert_eq!(meta["nested"]["foo"], "bar");
            }
            _ => panic!("Expected Message variant"),
        }
    }

    #[test]
    fn test_deserialize_response_item_message_with_null_metadata() {
        // Test that explicit null metadata also deserializes correctly
        let json_with_null = json!({
            "type": "message",
            "id": "msg_789",
            "response_id": "resp_ghi",
            "created_at": 1234567890,
            "status": "completed",
            "role": "assistant",
            "content": [],
            "model": "gpt-4",
            "metadata": null
        });

        let result: Result<ResponseOutputItem, _> = serde_json::from_value(json_with_null);

        assert!(result.is_ok());

        let item = result.unwrap();
        match item {
            ResponseOutputItem::Message { id, metadata, .. } => {
                assert_eq!(id, "msg_789");
                assert!(
                    metadata.is_none(),
                    "metadata should be None when explicitly set to null"
                );
            }
            _ => panic!("Expected Message variant"),
        }
    }

    #[test]
    fn test_create_response_request_validates_metadata_size() {
        use crate::common::MAX_METADATA_SIZE_BYTES;

        // Test that valid metadata passes validation
        let request_with_small_metadata = CreateResponseRequest {
            model: "gpt-4".to_string(),
            input: None,
            instructions: None,
            conversation: None,
            previous_response_id: None,
            max_output_tokens: None,
            max_tool_calls: None,
            temperature: None,
            top_p: None,
            stream: None,
            store: None,
            background: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            reasoning: None,
            include: None,
            metadata: Some(json!({"key": "value"})),
            safety_identifier: None,
            prompt_cache_key: None,
        };

        assert!(
            request_with_small_metadata.validate().is_ok(),
            "Small metadata should pass validation"
        );

        // Test that metadata exceeding the limit fails validation
        let large_string = "x".repeat(MAX_METADATA_SIZE_BYTES + 1);
        let request_with_large_metadata = CreateResponseRequest {
            model: "gpt-4".to_string(),
            input: None,
            instructions: None,
            conversation: None,
            previous_response_id: None,
            max_output_tokens: None,
            max_tool_calls: None,
            temperature: None,
            top_p: None,
            stream: None,
            store: None,
            background: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            reasoning: None,
            include: None,
            metadata: Some(json!({"large_field": large_string})),
            safety_identifier: None,
            prompt_cache_key: None,
        };

        let result = request_with_large_metadata.validate();
        assert!(result.is_err(), "Large metadata should fail validation");
        assert!(
            result.unwrap_err().contains("metadata is too large"),
            "Error message should mention metadata size"
        );
    }

    #[test]
    fn test_create_response_request_validates_without_metadata() {
        // Test that request without metadata passes validation
        let request_without_metadata = CreateResponseRequest {
            model: "gpt-4".to_string(),
            input: None,
            instructions: None,
            conversation: None,
            previous_response_id: None,
            max_output_tokens: None,
            max_tool_calls: None,
            temperature: None,
            top_p: None,
            stream: None,
            store: None,
            background: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            reasoning: None,
            include: None,
            metadata: None,
            safety_identifier: None,
            prompt_cache_key: None,
        };

        assert!(
            request_without_metadata.validate().is_ok(),
            "Request without metadata should pass validation"
        );
    }

    #[test]
    fn test_deserialize_response_input_item_message_without_metadata() {
        // Test backward compatibility: old format without metadata field
        let old_json = json!({
            "role": "user",
            "content": "Hello"
        });

        let result: Result<ResponseInputItem, _> = serde_json::from_value(old_json);

        assert!(
            result.is_ok(),
            "Failed to deserialize old input format without metadata: {:?}",
            result.err()
        );

        let item = result.unwrap();
        match item {
            ResponseInputItem::Message {
                role,
                metadata,
                content,
            } => {
                assert_eq!(role, "user");
                assert!(
                    metadata.is_none(),
                    "metadata should be None when field is missing"
                );
                match content {
                    ResponseContent::Text(text) => assert_eq!(text, "Hello"),
                    _ => panic!("Expected Text content"),
                }
            }
            _ => panic!("Expected Message variant"),
        }
    }

    #[test]
    fn test_deserialize_response_input_item_message_with_metadata() {
        // Test new format with metadata field
        let new_json = json!({
            "role": "user",
            "content": "Hello",
            "metadata": {
                "custom_key": "custom_value",
                "source": "test"
            }
        });

        let result: Result<ResponseInputItem, _> = serde_json::from_value(new_json);

        assert!(result.is_ok());

        let item = result.unwrap();
        match item {
            ResponseInputItem::Message {
                role,
                metadata,
                content,
            } => {
                assert_eq!(role, "user");
                assert!(metadata.is_some(), "metadata should be present");
                let meta = metadata.unwrap();
                assert_eq!(meta["custom_key"], "custom_value");
                assert_eq!(meta["source"], "test");
                match content {
                    ResponseContent::Text(text) => assert_eq!(text, "Hello"),
                    _ => panic!("Expected Text content"),
                }
            }
            _ => panic!("Expected Message variant"),
        }
    }

    #[test]
    fn test_create_response_request_validates_input_message_metadata_size() {
        use crate::common::MAX_METADATA_SIZE_BYTES;

        // Test that valid input message metadata passes validation
        let request_with_small_input_metadata = CreateResponseRequest {
            model: "gpt-4".to_string(),
            input: Some(ResponseInput::Items(vec![ResponseInputItem::Message {
                role: "user".to_string(),
                content: ResponseContent::Text("Hello".to_string()),
                metadata: Some(json!({"key": "value"})),
            }])),
            instructions: None,
            conversation: None,
            previous_response_id: None,
            max_output_tokens: None,
            max_tool_calls: None,
            temperature: None,
            top_p: None,
            stream: None,
            store: None,
            background: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            reasoning: None,
            include: None,
            metadata: None,
            safety_identifier: None,
            prompt_cache_key: None,
        };

        assert!(
            request_with_small_input_metadata.validate().is_ok(),
            "Small input message metadata should pass validation"
        );

        // Test that input message metadata exceeding the limit fails validation
        let large_string = "x".repeat(MAX_METADATA_SIZE_BYTES + 1);
        let request_with_large_input_metadata = CreateResponseRequest {
            model: "gpt-4".to_string(),
            input: Some(ResponseInput::Items(vec![ResponseInputItem::Message {
                role: "user".to_string(),
                content: ResponseContent::Text("Hello".to_string()),
                metadata: Some(json!({"large_field": large_string})),
            }])),
            instructions: None,
            conversation: None,
            previous_response_id: None,
            max_output_tokens: None,
            max_tool_calls: None,
            temperature: None,
            top_p: None,
            stream: None,
            store: None,
            background: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            reasoning: None,
            include: None,
            metadata: None,
            safety_identifier: None,
            prompt_cache_key: None,
        };

        let result = request_with_large_input_metadata.validate();
        assert!(
            result.is_err(),
            "Large input message metadata should fail validation"
        );
        assert!(
            result
                .unwrap_err()
                .contains("message metadata is too large"),
            "Error message should mention message metadata size"
        );
    }

    #[test]
    fn test_response_input_item_metadata_accessor() {
        // Test the metadata() accessor method
        let item_with_metadata = ResponseInputItem::Message {
            role: "user".to_string(),
            content: ResponseContent::Text("Hello".to_string()),
            metadata: Some(json!({"key": "value"})),
        };

        assert!(item_with_metadata.metadata().is_some());
        assert_eq!(item_with_metadata.metadata().unwrap()["key"], "value");

        let item_without_metadata = ResponseInputItem::Message {
            role: "user".to_string(),
            content: ResponseContent::Text("Hello".to_string()),
            metadata: None,
        };

        assert!(item_without_metadata.metadata().is_none());

        // Test that non-message variants return None
        let mcp_item = ResponseInputItem::McpApprovalResponse {
            type_: McpApprovalResponseType::McpApprovalResponse,
            approval_request_id: "test".to_string(),
            approve: true,
        };

        assert!(mcp_item.metadata().is_none());
    }
}
