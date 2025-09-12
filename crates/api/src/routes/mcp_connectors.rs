use axum::{
    extract::{Path, State, Query},
    response::Json,
    http::StatusCode,
    Extension,
};
use database::{
    Database,
    models::{
        McpConnector, CreateMcpConnectorRequest, UpdateMcpConnectorRequest,
        McpConnectionStatus, McpConnectorUsage,
    },
};
use domain::mcp::{McpClientManager, manager::McpError};
use crate::{middleware::AuthenticatedUser, routes::api::AppState};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;
use tracing::{error, info};

/// List all MCP connectors for an organization
pub async fn list_mcp_connectors(
    Path(org_id): Path<Uuid>,
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<Vec<McpConnector>>, StatusCode> {
    // Check if user has access to the organization
    let member = app_state.db.organizations.get_member(org_id, user.0.id)
        .await
        .map_err(|e| {
            error!("Failed to check organization membership: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    
    if member.is_none() {
        return Err(StatusCode::FORBIDDEN);
    }
    
    let connectors = app_state.db.mcp_connectors.list_by_organization(org_id)
        .await
        .map_err(|e| {
            error!("Failed to list MCP connectors: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    
    Ok(Json(connectors))
}

/// Get a specific MCP connector
pub async fn get_mcp_connector(
    Path((org_id, connector_id)): Path<(Uuid, Uuid)>,
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<McpConnector>, StatusCode> {
    // Check if user has access to the organization
    let member = app_state.db.organizations.get_member(org_id, user.0.id)
        .await
        .map_err(|e| {
            error!("Failed to check organization membership: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    
    if member.is_none() {
        return Err(StatusCode::FORBIDDEN);
    }
    
    let connector = app_state.db.mcp_connectors.get_by_id(connector_id)
        .await
        .map_err(|e| {
            error!("Failed to get MCP connector: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
    
    // Verify the connector belongs to the organization
    if connector.organization_id != org_id {
        return Err(StatusCode::NOT_FOUND);
    }
    
    Ok(Json(connector))
}

/// Create a new MCP connector
pub async fn create_mcp_connector(
    Path(org_id): Path<Uuid>,
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(request): Json<CreateMcpConnectorRequest>,
) -> Result<(StatusCode, Json<McpConnector>), StatusCode> {
    // Check if user has permission to manage connectors
    let member = app_state.db.organizations.get_member(org_id, user.0.id)
        .await
        .map_err(|e| {
            error!("Failed to check organization membership: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::FORBIDDEN)?;
    
    if !member.role.can_manage_mcp_connectors() {
        return Err(StatusCode::FORBIDDEN);
    }
    
    let connector = app_state.db.mcp_connectors.create(org_id, user.0.id, request)
        .await
        .map_err(|e| {
            error!("Failed to create MCP connector: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    
    info!("Created MCP connector {} for organization {}", connector.id, org_id);
    
    // Optionally test the connection in the background
    let app_state_clone = app_state.clone();
    let connector_clone = connector.clone();
    tokio::spawn(async move {
        info!("Starting background MCP connection test for connector {}", connector_clone.id);
        match test_mcp_connection(app_state_clone.db.clone(), app_state_clone.mcp_manager, connector_clone.clone()).await {
            Ok(capabilities) => {
                info!("MCP connection test successful for connector {}: {:?}", connector_clone.id, capabilities);
            }
            Err(e) => {
                error!("Failed to test MCP connection for connector {}: {:?}", connector_clone.id, e);
                error!("Error chain: {:#}", e);
            }
        }
    });
    
    Ok((StatusCode::CREATED, Json(connector)))
}

/// Update an MCP connector
pub async fn update_mcp_connector(
    Path((org_id, connector_id)): Path<(Uuid, Uuid)>,
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(request): Json<UpdateMcpConnectorRequest>,
) -> Result<Json<McpConnector>, StatusCode> {
    // Check if user has permission to manage connectors
    let member = app_state.db.organizations.get_member(org_id, user.0.id)
        .await
        .map_err(|e| {
            error!("Failed to check organization membership: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::FORBIDDEN)?;
    
    if !member.role.can_manage_mcp_connectors() {
        return Err(StatusCode::FORBIDDEN);
    }
    
    // Verify the connector belongs to the organization
    let existing = app_state.db.mcp_connectors.get_by_id(connector_id)
        .await
        .map_err(|e| {
            error!("Failed to get MCP connector: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
    
    if existing.organization_id != org_id {
        return Err(StatusCode::NOT_FOUND);
    }
    
    let connector = app_state.db.mcp_connectors.update(connector_id, request)
        .await
        .map_err(|e| {
            error!("Failed to update MCP connector: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    
    info!("Updated MCP connector {} for organization {}", connector_id, org_id);
    
    // Test the connection if URL or auth changed
    let app_state_clone = app_state.clone();
    let connector_clone = connector.clone();
    tokio::spawn(async move {
        if let Err(e) = test_mcp_connection(app_state_clone.db.clone(), app_state_clone.mcp_manager, connector_clone).await {
            error!("Failed to test MCP connection: {}", e);
        }
    });
    
    Ok(Json(connector))
}

/// Delete an MCP connector
pub async fn delete_mcp_connector(
    Path((org_id, connector_id)): Path<(Uuid, Uuid)>,
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<StatusCode, StatusCode> {
    // Check if user has permission to manage connectors
    let member = app_state.db.organizations.get_member(org_id, user.0.id)
        .await
        .map_err(|e| {
            error!("Failed to check organization membership: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::FORBIDDEN)?;
    
    if !member.role.can_manage_mcp_connectors() {
        return Err(StatusCode::FORBIDDEN);
    }
    
    // Verify the connector belongs to the organization
    let existing = app_state.db.mcp_connectors.get_by_id(connector_id)
        .await
        .map_err(|e| {
            error!("Failed to get MCP connector: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
    
    if existing.organization_id != org_id {
        return Err(StatusCode::NOT_FOUND);
    }
    
    app_state.db.mcp_connectors.delete(connector_id)
        .await
        .map_err(|e| {
            error!("Failed to delete MCP connector: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    
    info!("Deleted MCP connector {} for organization {}", connector_id, org_id);
    
    Ok(StatusCode::NO_CONTENT)
}

/// Test an MCP connector connection
pub async fn test_mcp_connector(
    Path((org_id, connector_id)): Path<(Uuid, Uuid)>,
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<TestConnectionResponse>, StatusCode> {
    // Check if user has access to the organization
    let _member = app_state.db.organizations.get_member(org_id, user.0.id)
        .await
        .map_err(|e| {
            error!("Failed to check organization membership: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::FORBIDDEN)?;
    
    let connector = app_state.db.mcp_connectors.get_by_id(connector_id)
        .await
        .map_err(|e| {
            error!("Failed to get MCP connector: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
    
    // Verify the connector belongs to the organization
    if connector.organization_id != org_id {
        return Err(StatusCode::NOT_FOUND);
    }
    
    // Test the connection
    let result = test_mcp_connection(app_state.db.clone(), app_state.mcp_manager.clone(), connector).await;
    
    match result {
        Ok(capabilities) => {
            Ok(Json(TestConnectionResponse {
                success: true,
                message: Some("Connection successful".to_string()),
                capabilities: Some(capabilities),
                error: None,
            }))
        }
        Err(e) => {
            Ok(Json(TestConnectionResponse {
                success: false,
                message: Some("Connection failed".to_string()),
                capabilities: None,
                error: Some(e.to_string()),
            }))
        }
    }
}

/// Response for testing MCP connection
#[derive(Debug, Serialize)]
pub struct TestConnectionResponse {
    pub success: bool,
    pub message: Option<String>,
    pub capabilities: Option<serde_json::Value>,
    pub error: Option<String>,
}

/// Get available tools from an MCP connector
pub async fn list_mcp_tools(
    Path((org_id, connector_id)): Path<(Uuid, Uuid)>,
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Check if user has access to the organization
    let _member = app_state.db.organizations.get_member(org_id, user.0.id)
        .await
        .map_err(|e| {
            error!("Failed to check organization membership: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::FORBIDDEN)?;
    
    let connector = app_state.db.mcp_connectors.get_by_id(connector_id)
        .await
        .map_err(|e| {
            error!("Failed to get MCP connector: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
    
    // Verify the connector belongs to the organization
    if connector.organization_id != org_id {
        return Err(StatusCode::NOT_FOUND);
    }
    
    // Use shared MCP manager to list tools  
    let tools = app_state.mcp_manager.list_tools(&connector)
        .await
        .map_err(|e| {
            error!("Failed to list MCP tools: {}", e);
            StatusCode::BAD_GATEWAY
        })?;
    
    Ok(Json(serde_json::json!({ "tools": tools })))
}

/// Call a tool on an MCP connector
#[derive(Debug, Deserialize)]
pub struct CallToolRequest {
    pub tool_name: String,
    pub arguments: Option<serde_json::Value>,
}

pub async fn call_mcp_tool(
    Path((org_id, connector_id)): Path<(Uuid, Uuid)>,
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(request): Json<CallToolRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Check if user has access to the organization
    let _member = app_state.db.organizations.get_member(org_id, user.0.id)
        .await
        .map_err(|e| {
            error!("Failed to check organization membership: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::FORBIDDEN)?;
    
    let connector = app_state.db.mcp_connectors.get_by_id(connector_id)
        .await
        .map_err(|e| {
            error!("Failed to get MCP connector: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
    
    // Verify the connector belongs to the organization
    if connector.organization_id != org_id {
        return Err(StatusCode::NOT_FOUND);
    }
    
    // Use shared MCP manager to call tool
    let start = std::time::Instant::now();
    let result = app_state.mcp_manager.call_tool(&connector, request.tool_name.clone(), request.arguments.clone()).await;
    let duration_ms = start.elapsed().as_millis() as i32;
    
    // Log the usage
    let (response_payload, status_code, error_message) = match &result {
        Ok(res) => (Some(serde_json::to_value(res).unwrap_or(serde_json::json!({}))), Some(200), None),
        Err(e) => (None, Some(500), Some(e.to_string())),
    };
    
    let _ = app_state.db.mcp_connectors.log_usage(
        connector_id,
        user.0.id,
        format!("tools/call:{}", request.tool_name),
        request.arguments,
        response_payload,
        status_code,
        error_message.clone(),
        Some(duration_ms),
    ).await;
    
    result
        .map(|r| Json(serde_json::to_value(r).unwrap_or(serde_json::json!({}))))
        .map_err(|e| {
            error!("Failed to call MCP tool: {}", e);
            StatusCode::BAD_GATEWAY
        })
}

/// Get usage logs for an MCP connector
#[derive(Debug, Deserialize)]
pub struct UsageLogsQuery {
    pub limit: Option<i64>,
}

pub async fn get_mcp_usage_logs(
    Path((org_id, connector_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<UsageLogsQuery>,
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<Vec<McpConnectorUsage>>, StatusCode> {
    // Check if user has permission to view logs
    let member = app_state.db.organizations.get_member(org_id, user.0.id)
        .await
        .map_err(|e| {
            error!("Failed to check organization membership: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::FORBIDDEN)?;
    
    if !member.role.can_manage_mcp_connectors() {
        return Err(StatusCode::FORBIDDEN);
    }
    
    // Verify the connector belongs to the organization
    let connector = app_state.db.mcp_connectors.get_by_id(connector_id)
        .await
        .map_err(|e| {
            error!("Failed to get MCP connector: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
    
    if connector.organization_id != org_id {
        return Err(StatusCode::NOT_FOUND);
    }
    
    let limit = query.limit.unwrap_or(100).min(1000);
    
    let logs = app_state.db.mcp_connectors.get_usage_logs(connector_id, limit)
        .await
        .map_err(|e| {
            error!("Failed to get usage logs: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    
    Ok(Json(logs))
}

/// Helper function to test MCP connection
async fn test_mcp_connection(
    db: Arc<Database>,
    manager: Arc<McpClientManager>,
    connector: McpConnector,
) -> Result<serde_json::Value, McpError> {
    let connector_id = connector.id;
    
    info!("Testing MCP connection for connector {} ({})", connector_id, connector.name);
    info!("MCP Server URL: {}", connector.mcp_server_url);
    info!("Auth Type: {:?}", connector.auth_type);
    
    // Try to test the connection and get server info + tools in one call
    info!("Attempting to test connection to MCP server...");
    match manager.test_connection(&connector).await {
        Ok((server_info, tools)) => {
            info!("Connection test passed for connector {}, server: {} v{}", 
                  connector_id, server_info.name, server_info.version);
            info!("Successfully listed {} tools from connector {}", tools.len(), connector_id);
            
            // Create a combined capabilities response
            let capabilities = serde_json::json!({
                "server": {
                    "name": server_info.name,
                    "version": server_info.version,
                },
                "tools_available": tools.len(),
            });
            
            // Update connection status to connected
            info!("Updating connection status to Connected for connector {}", connector_id);
            match db.mcp_connectors.update_connection_status(
                connector_id,
                McpConnectionStatus::Connected,
                None,
                Some(capabilities.clone()),
            ).await {
                Ok(_) => {
                    info!("Successfully updated connection status to Connected for connector {}", connector_id);
                    Ok(capabilities)
                }
                    Err(e) => {
                        error!("Failed to update connection status for connector {}: {:?}", connector_id, e);
                        error!("Error details: {:#}", e);
                        Err(McpError::ServerError(format!("Failed to update connection status: {}", e)))
                    }
            }
        }
        Err(e) => {
            error!("Connection test failed for connector {}: {:?}", connector_id, e);
            error!("Error chain: {:#}", e);
            
            // Update connection status to failed
            info!("Updating connection status to Failed for connector {}", connector_id);
            match db.mcp_connectors.update_connection_status(
                connector_id,
                McpConnectionStatus::Failed,
                Some(e.to_string()),
                None,
            ).await {
                Ok(_) => {
                    info!("Successfully updated connection status to Failed for connector {}", connector_id);
                }
                Err(update_err) => {
                    error!("Failed to update connection status for connector {}: {:?}", connector_id, update_err);
                    error!("Update error details: {:#}", update_err);
                }
            }
            
            Err(e)
        }
    }
}
