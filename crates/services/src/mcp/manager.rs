use super::ports::{McpAuthConfig, McpConnector, McpConnectorId, McpError, McpServerInfo};
use anyhow::Result;
use rmcp::{
    model::*,
    service::{RoleClient, RunningService},
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    },
    ServiceExt,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, RwLock};
use tokio::time::{timeout, Duration};
use tracing::{debug, error, info, warn};

/// Cached information about an MCP connector
#[derive(Clone)]
struct ConnectorCache {
    tools: Vec<Tool>,
    cached_at: Instant,
    server_info: McpServerInfo,
}

impl ConnectorCache {
    fn is_expired(&self, ttl: Duration) -> bool {
        self.cached_at.elapsed() > ttl
    }
}

/// Client information with connection and cache
struct ClientInfo {
    client: Arc<Mutex<RunningService<RoleClient, ()>>>,
    cache: Option<ConnectorCache>,
}

/// Statistics about the MCP client manager
#[derive(Debug, Clone)]
pub struct ManagerStats {
    pub total_clients: usize,
    pub cached_clients: usize,
    pub cache_ttl: Duration,
}

/// Manages multiple MCP client connections
pub struct McpClientManager {
    clients: Arc<RwLock<HashMap<McpConnectorId, ClientInfo>>>,
    cache_ttl: Duration,
    connection_timeout: Duration,
}

impl Default for McpClientManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McpClientManager {
    /// Create a new MCP client manager
    pub fn new() -> Self {
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
            cache_ttl: Duration::from_secs(300), // 5 minute cache
            connection_timeout: Duration::from_secs(30),
        }
    }

    /// Create a new MCP client manager with custom settings
    pub fn with_config(cache_ttl: Duration, connection_timeout: Duration) -> Self {
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
            cache_ttl,
            connection_timeout,
        }
    }

    /// Create a new RMCP client for a connector
    async fn create_client(
        &self,
        connector: &McpConnector,
    ) -> Result<RunningService<RoleClient, ()>> {
        info!(
            "Creating MCP client for connector: {} ({})",
            connector.name, connector.id
        );
        info!("MCP Server URL: {}", connector.server_url);

        // Configure the transport with authentication
        let mut config =
            StreamableHttpClientTransportConfig::with_uri(connector.server_url.as_str());

        // Add authentication if configured
        match &connector.auth {
            McpAuthConfig::Bearer(bearer_config) => {
                info!(
                    "Using Bearer authentication for connector {}",
                    connector.name
                );
                let header_name = bearer_config
                    .header_name
                    .as_deref()
                    .unwrap_or("Authorization");
                let auth_value = if header_name.eq_ignore_ascii_case("authorization") {
                    format!("Bearer {}", bearer_config.token)
                } else {
                    bearer_config.token.clone()
                };
                config = config.auth_header(&auth_value);
                debug!("Bearer token configured for connector {}", connector.name);
            }
            McpAuthConfig::ApiKey(api_key_config) => {
                info!(
                    "Using API Key authentication for connector {}",
                    connector.name
                );
                // API key authentication would need custom header support in transport config
                // For now, we can use auth_header with custom header name if supported
                config = config.auth_header(&api_key_config.key);
                debug!("API key configured for connector {}", connector.name);
            }
            McpAuthConfig::Custom(custom_config) => {
                warn!(
                    "Custom auth type not yet implemented for connector {}: {:?}",
                    connector.name, custom_config
                );
                // TODO: Handle custom authentication
            }
            McpAuthConfig::None => {
                info!(
                    "No authentication configured for connector {}",
                    connector.name
                );
            }
        }

        // Create the transport and connect
        info!("Creating transport for MCP connector {}", connector.name);
        let transport = StreamableHttpClientTransport::from_config(config);

        // Connect with timeout
        info!(
            "Attempting to connect to MCP server {} with {}s timeout...",
            connector.name,
            self.connection_timeout.as_secs()
        );
        let client = timeout(self.connection_timeout, ().serve(transport))
            .await
            .map_err(|_| {
                error!(
                    "Connection timeout for MCP server {}: {}s",
                    connector.name,
                    self.connection_timeout.as_secs()
                );
                McpError::ConnectionTimeout {
                    seconds: self.connection_timeout.as_secs() as i64,
                }
            })?
            .map_err(|e| {
                error!(
                    "Failed to connect to MCP server {}: {:?}",
                    connector.name, e
                );
                error!("Connection error details: {:#}", e);
                McpError::NetworkError(format!("Failed to connect: {e}"))
            })?;

        info!(
            "Successfully connected to MCP server: {} ({})",
            connector.name, connector.id
        );

        Ok(client)
    }

    /// Connect to an MCP server using connector configuration  
    pub async fn connect(&self, connector: &McpConnector) -> Result<(), McpError> {
        // Validate connector configuration
        if !connector.is_active {
            return Err(McpError::InvalidConfiguration(
                "Connector is not active".to_string(),
            ));
        }

        let connector_id = connector.id.clone();

        // Check if already connected
        {
            let clients = self.clients.read().await;
            if clients.contains_key(&connector_id) {
                debug!("Client already connected for connector {}", connector.name);
                return Ok(());
            }
        }

        // Create new client
        info!("Creating new MCP client for connector {}", connector.name);
        let client = self
            .create_client(connector)
            .await
            .map_err(|e| McpError::NetworkError(format!("Failed to create client: {e}")))?;
        let client_arc = Arc::new(Mutex::new(client));

        // Store the client with server info
        let server_info = McpServerInfo {
            name: connector.name.clone(),
            version: "1.0.0".to_string(), // TODO: Get actual version from server
        };

        let client_info = ClientInfo {
            client: client_arc,
            cache: Some(ConnectorCache {
                tools: Vec::new(), // Will be populated on first tools request
                cached_at: Instant::now(),
                server_info,
            }),
        };

        let mut clients = self.clients.write().await;
        clients.insert(connector_id, client_info);

        info!("Successfully connected to MCP server: {}", connector.name);
        Ok(())
    }

    /// Get or create a client for a connector ID
    async fn get_or_create_client_arc(
        &self,
        connector_id: &McpConnectorId,
    ) -> Result<Arc<Mutex<RunningService<RoleClient, ()>>>, McpError> {
        let clients = self.clients.read().await;
        if let Some(info) = clients.get(connector_id) {
            Ok(Arc::clone(&info.client))
        } else {
            Err(McpError::ConnectorNotFound)
        }
    }

    /// Disconnect from an MCP server
    pub async fn disconnect(&self, connector_id: &McpConnectorId) -> Result<(), McpError> {
        let mut clients = self.clients.write().await;

        if let Some(_info) = clients.remove(connector_id) {
            // The client will be dropped when it goes out of scope
            // We can't call cancel() directly as it would move the value
            info!("Disconnected from MCP connector: {}", connector_id);
        }

        Ok(())
    }

    /// List tools from a connector with caching
    pub async fn get_tools(&self, connector_id: &McpConnectorId) -> Result<Vec<Tool>, McpError> {
        // Check cache first
        {
            let clients = self.clients.read().await;
            if let Some(info) = clients.get(connector_id) {
                if let Some(cache) = &info.cache {
                    if !cache.is_expired(self.cache_ttl) {
                        debug!("Using cached tools for connector {}", connector_id);
                        return Ok(cache.tools.clone());
                    }
                }
            }
        }

        info!("Listing tools from MCP connector {}", connector_id);
        let client_arc = self.get_or_create_client_arc(connector_id).await?;

        let tools = {
            let client = client_arc.lock().await;
            timeout(Duration::from_secs(30), client.list_all_tools())
                .await
                .map_err(|_| McpError::ConnectionTimeout { seconds: 30 })?
                .map_err(|e| McpError::NetworkError(format!("Failed to list tools: {e}")))?
        };

        info!(
            "Successfully listed {} tools from connector {}",
            tools.len(),
            connector_id
        );

        // Update cache
        self.update_tools_cache(connector_id.clone(), tools.clone())
            .await;

        Ok(tools)
    }

    /// Update the tools cache for a connector
    async fn update_tools_cache(&self, connector_id: McpConnectorId, tools: Vec<Tool>) {
        let mut clients = self.clients.write().await;
        if let Some(info) = clients.get_mut(&connector_id) {
            let server_info = info
                .cache
                .as_ref()
                .map(|c| c.server_info.clone())
                .unwrap_or_else(|| McpServerInfo {
                    name: "Unknown".to_string(),
                    version: "1.0.0".to_string(),
                });

            info.cache = Some(ConnectorCache {
                tools,
                cached_at: Instant::now(),
                server_info,
            });
        }
    }

    /// Call a tool on a connector
    pub async fn call_tool(
        &self,
        connector_id: &McpConnectorId,
        name: String,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult, McpError> {
        let client_arc = self.get_or_create_client_arc(connector_id).await?;

        // Convert arguments to the expected format
        let args = arguments.and_then(|v| v.as_object().cloned());

        let request_params = CallToolRequestParam {
            name: name.clone().into(),
            arguments: args,
        };

        let result = {
            let client = client_arc.lock().await;
            timeout(
                Duration::from_secs(60), // Longer timeout for tool calls
                client.call_tool(request_params),
            )
            .await
            .map_err(|_| McpError::ConnectionTimeout { seconds: 60 })?
            .map_err(|e| McpError::NetworkError(format!("Failed to call tool '{name}': {e}")))?
        };

        debug!("Called tool '{}' on connector {}", name, connector_id);

        Ok(result)
    }

    /// Call a tool with smart retry logic
    pub async fn call_tool_with_retry(
        &self,
        connector_id: &McpConnectorId,
        name: String,
        arguments: Option<serde_json::Value>,
        max_retries: i64,
    ) -> Result<CallToolResult, McpError> {
        let mut last_error = None;

        for attempt in 0..=max_retries {
            match self
                .call_tool(connector_id, name.clone(), arguments.clone())
                .await
            {
                Ok(result) => return Ok(result),
                Err(e) => {
                    last_error = Some(e.clone());

                    // Only retry if error is retryable
                    if attempt < max_retries && Self::is_retryable_error(&e) {
                        let delay = Duration::from_millis(100 * (1 << attempt));
                        debug!(
                            "Retrying tool call '{}' after {:?} (attempt {}/{})",
                            name,
                            delay,
                            attempt + 1,
                            max_retries + 1
                        );
                        tokio::time::sleep(delay).await;
                    } else if !Self::is_retryable_error(&e) {
                        // Don't retry non-retryable errors
                        debug!(
                            "Tool call '{}' failed with non-retryable error: {}",
                            name, e
                        );
                        break;
                    }
                }
            }
        }

        Err(last_error.unwrap())
    }

    /// Check if an error is retryable
    fn is_retryable_error(error: &McpError) -> bool {
        matches!(
            error,
            McpError::NetworkError(_) | McpError::ConnectionTimeout { .. }
        )
    }

    /// List resources from a connector
    pub async fn list_resources(
        &self,
        connector_id: &McpConnectorId,
    ) -> Result<Vec<Resource>, McpError> {
        let client_arc = self.get_or_create_client_arc(connector_id).await?;

        let resources = {
            let client = client_arc.lock().await;
            timeout(Duration::from_secs(30), client.list_all_resources())
                .await
                .map_err(|_| McpError::ConnectionTimeout { seconds: 30 })?
                .map_err(|e| McpError::NetworkError(format!("Failed to list resources: {e}")))?
        };

        debug!(
            "Listed {} resources from connector {}",
            resources.len(),
            connector_id
        );

        Ok(resources)
    }

    /// Read a resource from a connector
    pub async fn read_resource(
        &self,
        connector_id: &McpConnectorId,
        uri: String,
    ) -> Result<ReadResourceResult, McpError> {
        let client_arc = self.get_or_create_client_arc(connector_id).await?;

        let request_params = ReadResourceRequestParam { uri: uri.clone() };

        let result = {
            let client = client_arc.lock().await;
            timeout(
                Duration::from_secs(30),
                client.read_resource(request_params),
            )
            .await
            .map_err(|_| McpError::ConnectionTimeout { seconds: 30 })?
            .map_err(|e| McpError::NetworkError(format!("Failed to read resource '{uri}': {e}")))?
        };

        debug!("Read resource '{}' from connector {}", uri, connector_id);

        Ok(result)
    }

    /// List prompts from a connector
    pub async fn list_prompts(
        &self,
        connector_id: &McpConnectorId,
    ) -> Result<Vec<Prompt>, McpError> {
        let client_arc = self.get_or_create_client_arc(connector_id).await?;

        let prompts = {
            let client = client_arc.lock().await;
            timeout(Duration::from_secs(30), client.list_all_prompts())
                .await
                .map_err(|_| McpError::ConnectionTimeout { seconds: 30 })?
                .map_err(|e| McpError::NetworkError(format!("Failed to list prompts: {e}")))?
        };

        debug!(
            "Listed {} prompts from connector {}",
            prompts.len(),
            connector_id
        );

        Ok(prompts)
    }

    /// Get a prompt from a connector
    pub async fn get_prompt(
        &self,
        connector_id: &McpConnectorId,
        name: String,
        arguments: Option<serde_json::Value>,
    ) -> Result<GetPromptResult, McpError> {
        let client_arc = self.get_or_create_client_arc(connector_id).await?;

        // Convert arguments to the expected format
        let args = arguments.and_then(|v| v.as_object().cloned());

        let request_params = GetPromptRequestParam {
            name: name.clone(),
            arguments: args,
        };

        let result = {
            let client = client_arc.lock().await;
            timeout(Duration::from_secs(30), client.get_prompt(request_params))
                .await
                .map_err(|_| McpError::ConnectionTimeout { seconds: 30 })?
                .map_err(|e| {
                    McpError::NetworkError(format!("Failed to get prompt '{name}': {e}"))
                })?
        };

        debug!("Got prompt '{}' from connector {}", name, connector_id);

        Ok(result)
    }

    /// Test connection and discover all capabilities
    pub async fn test_connection(
        &self,
        connector: &McpConnector,
    ) -> Result<(McpServerInfo, Vec<Tool>), McpError> {
        info!(
            "Starting connection test for MCP connector {} ({})",
            connector.name, connector.id
        );

        // This will create and cache the client
        self.connect(connector).await?;

        info!(
            "Client created successfully, attempting to list tools for {}",
            connector.name
        );

        // Try to list tools to verify connection works
        let tools = self.get_tools(&connector.id).await?;

        info!(
            "Connection test successful for {} - found {} tools",
            connector.name,
            tools.len()
        );

        // Get server info from cache
        let server_info = self.get_server_info(&connector.id).await?;

        Ok((server_info, tools))
    }

    /// Check if a connector is connected
    pub async fn is_connected(&self, connector_id: &McpConnectorId) -> bool {
        let clients = self.clients.read().await;
        clients.contains_key(connector_id)
    }

    /// Get connected connector IDs
    pub async fn get_connected_connectors(&self) -> Vec<McpConnectorId> {
        let clients = self.clients.read().await;
        clients.keys().cloned().collect()
    }

    /// Get server info for a connected connector
    pub async fn get_server_info(
        &self,
        connector_id: &McpConnectorId,
    ) -> Result<McpServerInfo, McpError> {
        let clients = self.clients.read().await;
        let info = clients
            .get(connector_id)
            .ok_or(McpError::ConnectorNotFound)?;

        if let Some(ref cache) = info.cache {
            Ok(cache.server_info.clone())
        } else {
            Err(McpError::InternalError(
                "Server info not cached".to_string(),
            ))
        }
    }

    /// Remove a client from the manager
    pub async fn remove_client(&self, connector_id: &McpConnectorId) -> Result<(), McpError> {
        self.disconnect(connector_id).await
    }

    /// Shutdown all clients properly
    pub async fn shutdown(&self) -> Result<(), McpError> {
        let mut clients = self.clients.write().await;

        // Close all connections
        for (id, _info) in clients.drain() {
            // The clients will be dropped when they go out of scope
            debug!("Shutting down client for connector {}", id);
        }

        info!("Shut down all MCP client connections");

        Ok(())
    }

    /// Disconnect all clients (alias for shutdown)
    pub async fn disconnect_all(&self) {
        let _ = self.shutdown().await;
    }

    /// Get stats about the manager
    pub async fn get_stats(&self) -> ManagerStats {
        let clients = self.clients.read().await;
        let total_clients = clients.len();
        let cached_clients = clients.values().filter(|info| info.cache.is_some()).count();

        ManagerStats {
            total_clients,
            cached_clients,
            cache_ttl: self.cache_ttl,
        }
    }
}

impl Drop for McpClientManager {
    fn drop(&mut self) {
        // Connections will be automatically dropped
        // We could try to call cancel() but that requires async context
    }
}
