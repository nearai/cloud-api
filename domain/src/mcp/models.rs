// Re-export essential MCP model types from rmcp crate
pub use rmcp::model::{
    // Core types
    Content,
    CallToolResult,
    CallToolRequest,
    CallToolRequestParam,
    Tool,
    Resource,
    ResourceTemplate,
    Prompt,
    Implementation,
    ServerCapabilities,
    ClientCapabilities,
    ReadResourceResult,
    ReadResourceRequest,
    ReadResourceRequestParam,
    GetPromptResult,
    GetPromptRequest,
    GetPromptRequestParam,
    ListToolsRequest,
    ListResourcesRequest,
    ListPromptsRequest,
    ProtocolVersion,
};

// Additional types for compatibility
use serde::{Deserialize, Serialize};

/// Server info type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

/// Initialize Result wrapper for compatibility
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    pub protocol_version: String,
    pub server_info: Implementation,
    pub capabilities: ServerCapabilities,
    pub instructions: Option<String>,
}

/// Helper functions for working with Content
pub trait ContentHelpers {
    fn as_text(&self) -> Option<String>;
    fn to_string_representation(&self) -> String;
}

impl ContentHelpers for Content {
    fn as_text(&self) -> Option<String> {
        // Since Content is an opaque type from rmcp, we'll need to serialize and check
        // This is a workaround since we can't pattern match on the enum directly
        if let Ok(json) = serde_json::to_value(self) {
            if let Some(text_obj) = json.as_object() {
                if let Some(text_val) = text_obj.get("text") {
                    if let Some(text_str) = text_val.as_str() {
                        return Some(text_str.to_string());
                    }
                }
            }
        }
        None
    }
    
    fn to_string_representation(&self) -> String {
        // Serialize to JSON and extract a representation
        if let Ok(json) = serde_json::to_value(self) {
            if let Some(obj) = json.as_object() {
                // Check if it's text content
                if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
                    return text.to_string();
                }
                // Check if it's image content
                if obj.contains_key("data") && obj.contains_key("mimeType") {
                    return "[Image content]".to_string();
                }
                // Check if it's resource content
                if let Some(resource) = obj.get("resource") {
                    if let Some(uri) = resource.get("uri").and_then(|v| v.as_str()) {
                        return format!("[Resource: {}]", uri);
                    }
                }
            }
        }
        // Fallback to debug representation
        format!("{:?}", self)
    }
}