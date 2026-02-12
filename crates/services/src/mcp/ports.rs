use super::super::organization::ports::OrganizationId;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// Re-export essential MCP model types from rmcp crate
pub use rmcp::model::{
    CallToolRequestParams,
    CallToolResult,
    ClientCapabilities,
    // Core types
    Content,
    GetPromptRequestParams,
    GetPromptResult,
    Implementation,
    Prompt,
    ProtocolVersion,
    ReadResourceRequestParams,
    ReadResourceResult,
    Resource,
    ResourceTemplate,
    ServerCapabilities,
    Tool,
};

// Domain ID types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct McpConnectorId(pub Uuid);

impl From<Uuid> for McpConnectorId {
    fn from(uuid: Uuid) -> Self {
        McpConnectorId(uuid)
    }
}

impl std::fmt::Display for McpConnectorId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// Domain models
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConnector {
    pub id: McpConnectorId,
    pub organization_id: OrganizationId,
    pub name: String,
    pub description: Option<String>,
    pub server_url: String,
    pub auth: McpAuthConfig,
    pub is_active: bool,
    pub settings: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpAuthConfig {
    None,
    Bearer(McpBearerConfig),
    ApiKey(McpApiKeyConfig),
    Custom(serde_json::Value),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpBearerConfig {
    pub token: String,
    pub header_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpApiKeyConfig {
    pub key: String,
    pub header_name: String,
}

/// Server info type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerInfo {
    pub name: String,
    pub version: String,
}

/// Initialize Result wrapper for compatibility
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpInitializeResult {
    pub protocol_version: String,
    pub server_info: Implementation,
    pub capabilities: ServerCapabilities,
    pub instructions: Option<String>,
}

// Error types
#[derive(Debug, Clone, thiserror::Error)]
pub enum McpError {
    #[error("Connection timeout after {seconds}s")]
    ConnectionTimeout { seconds: i64 },

    #[error("Authentication failed: {reason}")]
    AuthenticationFailed { reason: String },

    #[error("Tool '{tool}' not found")]
    ToolNotFound { tool: String },

    #[error("Network error: {0}")]
    NetworkError(String),

    #[error("Protocol error: {0}")]
    ProtocolError(String),

    #[error("Invalid configuration: {0}")]
    InvalidConfiguration(String),

    #[error("Internal error: {0}")]
    InternalError(String),

    #[error("Connector not found")]
    ConnectorNotFound,
}

// Repository traits
#[async_trait]
pub trait McpConnectorRepository: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn create(
        &self,
        organization_id: OrganizationId,
        name: String,
        description: Option<String>,
        server_url: String,
        auth: McpAuthConfig,
        settings: serde_json::Value,
    ) -> anyhow::Result<McpConnector>;

    async fn get_by_id(
        &self,
        id: McpConnectorId,
        organization_id: OrganizationId,
    ) -> anyhow::Result<Option<McpConnector>>;

    #[allow(clippy::too_many_arguments)]
    async fn update(
        &self,
        id: McpConnectorId,
        organization_id: OrganizationId,
        name: Option<String>,
        description: Option<String>,
        server_url: Option<String>,
        auth: Option<McpAuthConfig>,
        settings: Option<serde_json::Value>,
        is_active: Option<bool>,
    ) -> anyhow::Result<Option<McpConnector>>;

    async fn delete(
        &self,
        id: McpConnectorId,
        organization_id: OrganizationId,
    ) -> anyhow::Result<bool>;

    async fn list_by_organization(
        &self,
        organization_id: OrganizationId,
        include_inactive: bool,
    ) -> anyhow::Result<Vec<McpConnector>>;

    async fn count_by_organization(&self, organization_id: OrganizationId) -> anyhow::Result<i64>;
}

/// Helper functions for working with Content
pub trait ContentHelpers {
    fn as_text(&self) -> Option<String>;
    fn to_string_representation(&self) -> String;
}

impl ContentHelpers for Content {
    fn as_text(&self) -> Option<String> {
        // Content is an opaque type, we'll extract text based on its serialized form
        if let Ok(value) = serde_json::to_value(self) {
            if let Some(text) = value.get("text").and_then(|v| v.as_str()) {
                return Some(text.to_string());
            }
        }
        None
    }

    fn to_string_representation(&self) -> String {
        // Serialize to get a string representation
        serde_json::to_string(self).unwrap_or("[Unknown content]".to_string())
    }
}

impl ContentHelpers for Vec<Content> {
    fn as_text(&self) -> Option<String> {
        let texts: Vec<String> = self.iter().filter_map(|c| c.as_text()).collect();

        if texts.is_empty() {
            None
        } else {
            Some(texts.join("\n"))
        }
    }

    fn to_string_representation(&self) -> String {
        self.iter()
            .map(|c| c.to_string_representation())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// Service traits following the responses pattern

/// MCP Service for managing MCP operations
#[async_trait]
pub trait McpService: Send + Sync {
    /// Connect to an MCP connector
    async fn connect_connector(&self, connector: &McpConnector) -> Result<(), McpError>;

    /// Disconnect from an MCP connector
    async fn disconnect_connector(&self, connector_id: &McpConnectorId) -> Result<(), McpError>;

    /// Get available tools from a connector
    async fn get_connector_tools(
        &self,
        connector_id: &McpConnectorId,
    ) -> Result<Vec<Tool>, McpError>;

    /// Call a tool on an MCP connector
    async fn call_connector_tool(
        &self,
        connector_id: &McpConnectorId,
        tool_name: String,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult, McpError>;

    /// List resources from an MCP connector
    async fn list_connector_resources(
        &self,
        connector_id: &McpConnectorId,
    ) -> Result<Vec<Resource>, McpError>;

    /// Read a resource from an MCP connector
    async fn read_connector_resource(
        &self,
        connector_id: &McpConnectorId,
        uri: String,
    ) -> Result<ReadResourceResult, McpError>;

    /// List prompts from an MCP connector
    async fn list_connector_prompts(
        &self,
        connector_id: &McpConnectorId,
    ) -> Result<Vec<Prompt>, McpError>;

    /// Get a prompt from an MCP connector
    async fn get_connector_prompt(
        &self,
        connector_id: &McpConnectorId,
        name: String,
        arguments: Option<serde_json::Value>,
    ) -> Result<GetPromptResult, McpError>;

    /// Check if a connector is connected
    async fn is_connector_connected(&self, connector_id: &McpConnectorId) -> bool;

    /// Get server info for a connected connector
    async fn get_connector_server_info(
        &self,
        connector_id: &McpConnectorId,
    ) -> Result<McpServerInfo, McpError>;

    // CRUD Operations for connector management

    /// Create a new MCP connector
    async fn create_connector(
        &self,
        organization_id: OrganizationId,
        name: String,
        description: Option<String>,
        server_url: String,
        auth: McpAuthConfig,
        settings: serde_json::Value,
    ) -> Result<McpConnector, McpError>;

    /// Update an existing MCP connector
    #[allow(clippy::too_many_arguments)]
    async fn update_connector(
        &self,
        connector_id: McpConnectorId,
        organization_id: OrganizationId,
        name: Option<String>,
        description: Option<String>,
        server_url: Option<String>,
        auth: Option<McpAuthConfig>,
        settings: Option<serde_json::Value>,
        is_active: Option<bool>,
    ) -> Result<Option<McpConnector>, McpError>;

    /// Delete an MCP connector
    async fn delete_connector(
        &self,
        connector_id: McpConnectorId,
        organization_id: OrganizationId,
    ) -> Result<bool, McpError>;

    /// List all connectors for an organization
    async fn list_connectors(
        &self,
        organization_id: OrganizationId,
        include_inactive: bool,
    ) -> Result<Vec<McpConnector>, McpError>;

    /// Get a connector by ID
    async fn get_connector_by_id(
        &self,
        connector_id: McpConnectorId,
        organization_id: OrganizationId,
    ) -> Result<Option<McpConnector>, McpError>;
}
