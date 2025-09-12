use database::models::{McpConnector, McpAuthType, McpBearerConfig};
use rmcp::{
    ServiceExt,
    service::{RoleClient, RunningService},
    transport::{StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig},
    model::*,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{RwLock, Mutex};
use tokio::time::{timeout, Duration};
use anyhow::{Result, Context};
use tracing::{debug, info, warn, error};
use uuid::Uuid;

/// Cached information about an MCP connector
#[derive(Clone)]
struct ConnectorCache {
    tools: Vec<Tool>,
    cached_at: Instant,
    server_info: crate::mcp::ServerInfo,
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

/// MCP-specific error types
#[derive(Debug, Clone, thiserror::Error)]
pub enum McpError {
    #[error("Connection timeout after {seconds}s")]
    ConnectionTimeout { seconds: u64 },
    
    #[error("Authentication failed: {reason}")]
    AuthenticationFailed { reason: String },
    
    #[error("Tool '{tool}' not found")]
    ToolNotFound { tool: String },
    
    #[error("Network error: {0}")]
    NetworkError(String),
    
    #[error("Server error: {0}")]
    ServerError(String),
    
    #[error("Configuration error: {0}")]
    ConfigError(String),
}

impl McpError {
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            McpError::NetworkError(_) | McpError::ConnectionTimeout { .. }
        )
    }
}

/// Manages multiple MCP client connections
pub struct McpClientManager {
    clients: Arc<RwLock<HashMap<Uuid, ClientInfo>>>,
    cache_ttl: Duration,
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
        }
    }

    /// Create a new MCP client manager with custom cache TTL
    pub fn with_cache_ttl(cache_ttl: Duration) -> Self {
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
            cache_ttl,
        }
    }

    /// Create a new RMCP client for a connector
    async fn create_client(&self, connector: &McpConnector) -> Result<RunningService<RoleClient, ()>> {
        info!("Creating MCP client for connector: {} ({})", connector.name, connector.id);
        info!("MCP Server URL: {}", connector.mcp_server_url);
        
        // Configure the transport with authentication
        let mut config = StreamableHttpClientTransportConfig::with_uri(connector.mcp_server_url.as_str());
        
        // Add authentication if configured
        match connector.auth_type {
            McpAuthType::Bearer => {
                info!("Using Bearer authentication for connector {}", connector.name);
                if let Some(ref auth_config) = connector.auth_config {
                    let bearer_config: McpBearerConfig = serde_json::from_value(auth_config.clone())
                        .context("Failed to parse bearer auth config")?;
                    config = config.auth_header(&bearer_config.token);
                    debug!("Bearer token configured for connector {}", connector.name);
                } else {
                    warn!("Bearer auth type specified but no auth_config provided for connector {}", connector.name);
                }
            }
            McpAuthType::None => {
                info!("No authentication configured for connector {}", connector.name);
            }
        }
        
        // Create the transport and connect
        info!("Creating transport for MCP connector {}", connector.name);
        let transport = StreamableHttpClientTransport::from_config(config);
        
        // Connect with timeout
        info!("Attempting to connect to MCP server {} with 30s timeout...", connector.name);
        let client = timeout(
            Duration::from_secs(30),
            ().serve(transport)
        ).await
            .map_err(|e| {
                error!("Connection timeout for MCP server {}: {:?}", connector.name, e);
                anyhow::anyhow!("Connection timeout after 30 seconds")
            })?
            .map_err(|e| {
                error!("Failed to connect to MCP server {}: {:?}", connector.name, e);
                error!("Connection error details: {:#}", e);
                e
            })
            .context("Failed to connect to MCP server")?;
        
        info!("Successfully connected to MCP server: {} ({})", connector.name, connector.id);
        
        Ok(client)
    }

    /// Get or create a client for a connector - actually works now!
    pub async fn get_or_create_client(&self, connector: &McpConnector) -> Result<Arc<Mutex<RunningService<RoleClient, ()>>>> {
        let connector_id = connector.id;
        
        // Fast path: check if client exists
        {
            let clients = self.clients.read().await;
            if let Some(info) = clients.get(&connector_id) {
                debug!("Reusing existing MCP client for connector {}", connector.name);
                return Ok(Arc::clone(&info.client));
            }
        }
        
        // Slow path: create new client
        info!("Creating new MCP client for connector {}", connector.name);
        let client = self.create_client(connector).await?;
        let client_arc = Arc::new(Mutex::new(client));
        
        // Store the client with double-checked locking
        {
            let mut clients = self.clients.write().await;
            // Check again in case another task created it
            if let Some(info) = clients.get(&connector_id) {
                return Ok(Arc::clone(&info.client));
            }
            
            clients.insert(connector_id, ClientInfo {
                client: Arc::clone(&client_arc),
                cache: None,
            });
        }
        
        Ok(client_arc)
    }

    /// List tools from a connector with caching
    pub async fn list_tools(&self, connector: &McpConnector) -> Result<Vec<Tool>, McpError> {
        let connector_id = connector.id;
        
        // Check cache first
        {
            let clients = self.clients.read().await;
            if let Some(info) = clients.get(&connector_id) {
                if let Some(cache) = &info.cache {
                    if !cache.is_expired(self.cache_ttl) {
                        debug!("Using cached tools for connector {}", connector.name);
                        return Ok(cache.tools.clone());
                    }
                }
            }
        }
        
        info!("Listing tools from MCP connector {} ({})", connector.name, connector.id);
        let client_arc = self.get_or_create_client(connector).await
            .map_err(|e| McpError::NetworkError(format!("Failed to get client: {}", e)))?;
        
        info!("Client ready, calling list_all_tools for {}", connector.name);
        
        let tools = {
            let client = client_arc.lock().await;
            timeout(
                Duration::from_secs(30),
                client.list_all_tools()
            ).await
                .map_err(|_| McpError::ConnectionTimeout { seconds: 30 })?
                .map_err(|e| McpError::ServerError(format!("Failed to list tools: {}", e)))?
        };
        
        info!("Successfully listed {} tools from {}", tools.len(), connector.name);
        
        // Update cache
        self.update_tools_cache(connector_id, tools.clone()).await;
        
        Ok(tools)
    }
    
    /// Update the tools cache for a connector
    async fn update_tools_cache(&self, connector_id: Uuid, tools: Vec<Tool>) {
        let mut clients = self.clients.write().await;
        if let Some(info) = clients.get_mut(&connector_id) {
            let server_info = info.cache.as_ref()
                .map(|c| c.server_info.clone())
                .unwrap_or_else(|| crate::mcp::ServerInfo {
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
        connector: &McpConnector,
        name: String,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult, McpError> {
        let client_arc = self.get_or_create_client(connector).await
            .map_err(|e| McpError::NetworkError(format!("Failed to get client: {}", e)))?;
        
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
                client.call_tool(request_params)
            ).await
                .map_err(|_| McpError::ConnectionTimeout { seconds: 60 })?
                .map_err(|e| McpError::ServerError(format!("Failed to call tool '{}': {}", name, e)))?
        };
        
        debug!("Called tool '{}' on {}", name, connector.name);
        
        Ok(result)
    }

    /// Call a tool with smart retry logic
    pub async fn call_tool_with_retry(
        &self,
        connector: &McpConnector,
        name: String,
        arguments: Option<serde_json::Value>,
        max_retries: u32,
    ) -> Result<CallToolResult, McpError> {
        let mut last_error = None;
        
        for attempt in 0..=max_retries {
            match self.call_tool(connector, name.clone(), arguments.clone()).await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    last_error = Some(e.clone());
                    
                    // Only retry if error is retryable
                    if attempt < max_retries && e.is_retryable() {
                        let delay = Duration::from_millis(100 * (1 << attempt));
                        debug!("Retrying tool call '{}' after {:?} (attempt {}/{})", name, delay, attempt + 1, max_retries + 1);
                        tokio::time::sleep(delay).await;
                    } else if !e.is_retryable() {
                        // Don't retry non-retryable errors
                        debug!("Tool call '{}' failed with non-retryable error: {}", name, e);
                        break;
                    }
                }
            }
        }
        
        Err(last_error.unwrap())
    }

    /// List resources from a connector
    pub async fn list_resources(&self, connector: &McpConnector) -> Result<Vec<Resource>, McpError> {
        let client_arc = self.get_or_create_client(connector).await
            .map_err(|e| McpError::NetworkError(format!("Failed to get client: {}", e)))?;
        
        let resources = {
            let client = client_arc.lock().await;
            timeout(
                Duration::from_secs(30),
                client.list_all_resources()
            ).await
                .map_err(|_| McpError::ConnectionTimeout { seconds: 30 })?
                .map_err(|e| McpError::ServerError(format!("Failed to list resources: {}", e)))?
        };
        
        debug!("Listed {} resources from {}", resources.len(), connector.name);
        
        Ok(resources)
    }

    /// Read a resource from a connector
    pub async fn read_resource(&self, connector: &McpConnector, uri: String) -> Result<ReadResourceResult, McpError> {
        let client_arc = self.get_or_create_client(connector).await
            .map_err(|e| McpError::NetworkError(format!("Failed to get client: {}", e)))?;
        
        let request_params = ReadResourceRequestParam {
            uri: uri.clone().into(),
        };
        
        let result = {
            let client = client_arc.lock().await;
            timeout(
                Duration::from_secs(30),
                client.read_resource(request_params)
            ).await
                .map_err(|_| McpError::ConnectionTimeout { seconds: 30 })?
                .map_err(|e| McpError::ServerError(format!("Failed to read resource '{}': {}", uri, e)))?
        };
        
        debug!("Read resource '{}' from {}", uri, connector.name);
        
        Ok(result)
    }

    /// List prompts from a connector
    pub async fn list_prompts(&self, connector: &McpConnector) -> Result<Vec<Prompt>, McpError> {
        let client_arc = self.get_or_create_client(connector).await
            .map_err(|e| McpError::NetworkError(format!("Failed to get client: {}", e)))?;
        
        let prompts = {
            let client = client_arc.lock().await;
            timeout(
                Duration::from_secs(30),
                client.list_all_prompts()
            ).await
                .map_err(|_| McpError::ConnectionTimeout { seconds: 30 })?
                .map_err(|e| McpError::ServerError(format!("Failed to list prompts: {}", e)))?
        };
        
        debug!("Listed {} prompts from {}", prompts.len(), connector.name);
        
        Ok(prompts)
    }

    /// Get a prompt from a connector
    pub async fn get_prompt(
        &self,
        connector: &McpConnector,
        name: String,
        arguments: Option<serde_json::Value>,
    ) -> Result<GetPromptResult, McpError> {
        let client_arc = self.get_or_create_client(connector).await
            .map_err(|e| McpError::NetworkError(format!("Failed to get client: {}", e)))?;
        
        // Convert arguments to the expected format
        let args = arguments.and_then(|v| v.as_object().cloned());
        
        let request_params = GetPromptRequestParam {
            name: name.clone().into(),
            arguments: args,
        };
        
        let result = {
            let client = client_arc.lock().await;
            timeout(
                Duration::from_secs(30),
                client.get_prompt(request_params)
            ).await
                .map_err(|_| McpError::ConnectionTimeout { seconds: 30 })?
                .map_err(|e| McpError::ServerError(format!("Failed to get prompt '{}': {}", name, e)))?
        };
        
        debug!("Got prompt '{}' from {}", name, connector.name);
        
        Ok(result)
    }

    /// Test connection and discover all capabilities
    pub async fn test_connection(&self, connector: &McpConnector) -> Result<(crate::mcp::ServerInfo, Vec<Tool>), McpError> {
        info!("Starting connection test for MCP connector {} ({})", connector.name, connector.id);
        
        // This will create and cache the client
        let client_arc = self.get_or_create_client(connector).await
            .map_err(|e| McpError::NetworkError(format!("Failed to create client: {}", e)))?;
        
        info!("Client created successfully, attempting to list tools for {}", connector.name);
        
        // Try to list tools to verify connection works
        let tools = {
            let client = client_arc.lock().await;
            timeout(
                Duration::from_secs(30),
                client.list_all_tools()
            ).await
                .map_err(|_| McpError::ConnectionTimeout { seconds: 30 })?
                .map_err(|e| McpError::ServerError(format!("Failed to list tools: {}", e)))?
        };
        
        info!("Connection test successful for {} - found {} tools", connector.name, tools.len());
        
        // Create server info and cache it
        let server_info = crate::mcp::ServerInfo {
            name: connector.name.clone(),
            version: "1.0.0".to_string(),
        };
        
        // Update cache with both server info and tools
        self.update_full_cache(connector.id, tools.clone(), server_info.clone()).await;
        
        Ok((server_info, tools))
    }
    
    /// Update the full cache for a connector
    async fn update_full_cache(&self, connector_id: Uuid, tools: Vec<Tool>, server_info: crate::mcp::ServerInfo) {
        let mut clients = self.clients.write().await;
        if let Some(info) = clients.get_mut(&connector_id) {
            info.cache = Some(ConnectorCache {
                tools,
                cached_at: Instant::now(),
                server_info,
            });
        }
    }

    /// Get cached server info for a connector
    pub async fn get_server_info(&self, connector: &McpConnector) -> Option<crate::mcp::ServerInfo> {
        let clients = self.clients.read().await;
        clients.get(&connector.id)
            .and_then(|info| info.cache.as_ref())
            .map(|cache| cache.server_info.clone())
    }

    /// Remove a client from the manager
    pub async fn remove_client(&self, connector_id: Uuid) -> Result<(), McpError> {
        let mut clients = self.clients.write().await;
        
        if let Some(_info) = clients.remove(&connector_id) {
            // Client will be automatically dropped when the Arc<Mutex<...>> is dropped
            // We could try to call cancel() but it's tricky with the Mutex
            info!("Removed MCP client for connector {}", connector_id);
        }
        
        Ok(())
    }

    /// Shutdown all clients properly
    pub async fn shutdown(&self) -> Result<(), McpError> {
        let mut clients = self.clients.write().await;
        
        // Close all connections
        for (id, _info) in clients.drain() {
            // Client will be automatically dropped when the Arc<Mutex<...>> is dropped
            debug!("Shutting down client for connector {}", id);
        }
        
        info!("Shut down all MCP client connections");
        
        Ok(())
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

/// Statistics about the MCP client manager
#[derive(Debug)]
pub struct ManagerStats {
    pub total_clients: usize,
    pub cached_clients: usize,
    pub cache_ttl: Duration,
}
