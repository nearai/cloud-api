pub mod manager;
pub mod ports;

use crate::organization::ports::OrganizationId;
use async_trait::async_trait;
pub use manager::McpClientManager;
use std::sync::Arc;

pub use ports::*;

/// MCP Service implementation following the responses pattern
pub struct McpServiceImpl {
    pub mcp_repository: Arc<dyn McpConnectorRepository>,
    pub client_manager: Arc<McpClientManager>,
}

impl McpServiceImpl {
    pub fn new(
        mcp_repository: Arc<dyn McpConnectorRepository>,
        client_manager: Arc<McpClientManager>,
    ) -> Self {
        Self {
            mcp_repository,
            client_manager,
        }
    }

    /// Get connector by ID, validating organization access
    #[allow(dead_code)]
    async fn get_connector(
        &self,
        connector_id: &McpConnectorId,
        organization_id: &OrganizationId,
    ) -> Result<McpConnector, McpError> {
        self.mcp_repository
            .get_by_id(connector_id.clone(), organization_id.clone())
            .await
            .map_err(|e| McpError::InternalError(format!("Repository error: {e}")))?
            .ok_or(McpError::ConnectorNotFound)
    }
}

#[async_trait]
impl McpService for McpServiceImpl {
    async fn connect_connector(&self, connector: &McpConnector) -> Result<(), McpError> {
        // Validate connector is active
        if !connector.is_active {
            return Err(McpError::InvalidConfiguration(
                "Connector is not active".to_string(),
            ));
        }

        // Connect using the client manager
        self.client_manager.connect(connector).await?;

        tracing::info!(
            "Successfully connected to MCP connector: {}",
            connector.name
        );
        Ok(())
    }

    async fn disconnect_connector(&self, connector_id: &McpConnectorId) -> Result<(), McpError> {
        self.client_manager.disconnect(connector_id).await?;

        tracing::info!(
            "Successfully disconnected from MCP connector: {}",
            connector_id
        );
        Ok(())
    }

    async fn get_connector_tools(
        &self,
        connector_id: &McpConnectorId,
    ) -> Result<Vec<Tool>, McpError> {
        // Ensure connector is connected
        if !self.client_manager.is_connected(connector_id).await {
            return Err(McpError::ConnectorNotFound);
        }

        let tools = self.client_manager.get_tools(connector_id).await?;

        tracing::debug!(
            "Retrieved {} tools from connector {}",
            tools.len(),
            connector_id
        );
        Ok(tools)
    }

    async fn call_connector_tool(
        &self,
        connector_id: &McpConnectorId,
        tool_name: String,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult, McpError> {
        // Ensure connector is connected
        if !self.client_manager.is_connected(connector_id).await {
            return Err(McpError::ConnectorNotFound);
        }

        let result = self
            .client_manager
            .call_tool(connector_id, tool_name.clone(), arguments)
            .await?;

        tracing::info!(
            "Successfully called tool {} on connector {}",
            tool_name,
            connector_id
        );
        Ok(result)
    }

    async fn list_connector_resources(
        &self,
        connector_id: &McpConnectorId,
    ) -> Result<Vec<Resource>, McpError> {
        // Ensure connector is connected
        if !self.client_manager.is_connected(connector_id).await {
            return Err(McpError::ConnectorNotFound);
        }

        let resources = self.client_manager.list_resources(connector_id).await?;

        tracing::debug!(
            "Retrieved {} resources from connector {}",
            resources.len(),
            connector_id
        );
        Ok(resources)
    }

    async fn read_connector_resource(
        &self,
        connector_id: &McpConnectorId,
        uri: String,
    ) -> Result<ReadResourceResult, McpError> {
        // Ensure connector is connected
        if !self.client_manager.is_connected(connector_id).await {
            return Err(McpError::ConnectorNotFound);
        }

        let result = self
            .client_manager
            .read_resource(connector_id, uri.clone())
            .await?;

        tracing::debug!(
            "Successfully read resource {} from connector {}",
            uri,
            connector_id
        );
        Ok(result)
    }

    async fn list_connector_prompts(
        &self,
        connector_id: &McpConnectorId,
    ) -> Result<Vec<Prompt>, McpError> {
        // Ensure connector is connected
        if !self.client_manager.is_connected(connector_id).await {
            return Err(McpError::ConnectorNotFound);
        }

        let prompts = self.client_manager.list_prompts(connector_id).await?;

        tracing::debug!(
            "Retrieved {} prompts from connector {}",
            prompts.len(),
            connector_id
        );
        Ok(prompts)
    }

    async fn get_connector_prompt(
        &self,
        connector_id: &McpConnectorId,
        name: String,
        arguments: Option<serde_json::Value>,
    ) -> Result<GetPromptResult, McpError> {
        // Ensure connector is connected
        if !self.client_manager.is_connected(connector_id).await {
            return Err(McpError::ConnectorNotFound);
        }

        let result = self
            .client_manager
            .get_prompt(connector_id, name.clone(), arguments)
            .await?;

        tracing::debug!(
            "Successfully retrieved prompt {} from connector {}",
            name,
            connector_id
        );
        Ok(result)
    }

    async fn is_connector_connected(&self, connector_id: &McpConnectorId) -> bool {
        self.client_manager.is_connected(connector_id).await
    }

    async fn get_connector_server_info(
        &self,
        connector_id: &McpConnectorId,
    ) -> Result<McpServerInfo, McpError> {
        // Ensure connector is connected
        if !self.client_manager.is_connected(connector_id).await {
            return Err(McpError::ConnectorNotFound);
        }

        self.client_manager.get_server_info(connector_id).await
    }

    /// Create a new MCP connector
    async fn create_connector(
        &self,
        organization_id: OrganizationId,
        name: String,
        description: Option<String>,
        server_url: String,
        auth: ports::McpAuthConfig,
        settings: serde_json::Value,
    ) -> Result<McpConnector, McpError> {
        let connector = self
            .mcp_repository
            .create(
                organization_id,
                name,
                description,
                server_url,
                auth,
                settings,
            )
            .await
            .map_err(|e| McpError::InternalError(format!("Failed to create connector: {e}")))?;

        tracing::info!("Created new MCP connector: {}", connector.name);
        Ok(connector)
    }

    /// Update an existing MCP connector
    async fn update_connector(
        &self,
        connector_id: McpConnectorId,
        organization_id: OrganizationId,
        name: Option<String>,
        description: Option<String>,
        server_url: Option<String>,
        auth: Option<ports::McpAuthConfig>,
        settings: Option<serde_json::Value>,
        is_active: Option<bool>,
    ) -> Result<Option<McpConnector>, McpError> {
        let connector = self
            .mcp_repository
            .update(
                connector_id.clone(),
                organization_id,
                name,
                description,
                server_url,
                auth,
                settings,
                is_active,
            )
            .await
            .map_err(|e| McpError::InternalError(format!("Failed to update connector: {e}")))?;

        if connector.is_some() {
            tracing::info!("Updated MCP connector: {}", connector_id);
        }

        Ok(connector)
    }

    /// Delete an MCP connector
    async fn delete_connector(
        &self,
        connector_id: McpConnectorId,
        organization_id: OrganizationId,
    ) -> Result<bool, McpError> {
        // Disconnect first if connected
        if self.client_manager.is_connected(&connector_id).await {
            self.client_manager.disconnect(&connector_id).await?;
        }

        let deleted = self
            .mcp_repository
            .delete(connector_id.clone(), organization_id)
            .await
            .map_err(|e| McpError::InternalError(format!("Failed to delete connector: {e}")))?;

        if deleted {
            tracing::info!("Deleted MCP connector: {}", connector_id);
        }

        Ok(deleted)
    }

    /// List all connectors for an organization
    async fn list_connectors(
        &self,
        organization_id: OrganizationId,
        include_inactive: bool,
    ) -> Result<Vec<McpConnector>, McpError> {
        self.mcp_repository
            .list_by_organization(organization_id, include_inactive)
            .await
            .map_err(|e| McpError::InternalError(format!("Failed to list connectors: {e}")))
    }

    /// Get a connector by ID
    async fn get_connector_by_id(
        &self,
        connector_id: McpConnectorId,
        organization_id: OrganizationId,
    ) -> Result<Option<McpConnector>, McpError> {
        self.mcp_repository
            .get_by_id(connector_id, organization_id)
            .await
            .map_err(|e| McpError::InternalError(format!("Failed to get connector: {e}")))
    }
}
