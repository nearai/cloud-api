use crate::models::{
    CreateMcpConnectorRequest, McpAuthType, McpBearerConfig, McpConnectionStatus, McpConnector,
    McpConnectorUsage, UpdateMcpConnectorRequest,
};
use crate::pool::DbPool;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

pub struct McpConnectorRepository {
    pool: DbPool,
}

impl McpConnectorRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Create a new MCP connector for an organization
    pub async fn create(
        &self,
        organization_id: Uuid,
        creator_user_id: Uuid,
        request: CreateMcpConnectorRequest,
    ) -> Result<McpConnector> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let id = Uuid::new_v4();
        let now = Utc::now();

        // Convert bearer token to auth_config if present
        let auth_config = request.bearer_token.as_ref().map(|token| {
            serde_json::to_value(McpBearerConfig {
                token: token.clone(),
            })
            .unwrap()
        });

        let row = client
            .query_one(
                r#"
            INSERT INTO mcp_connectors (
                id, organization_id, name, description,
                mcp_server_url, auth_type, auth_config,
                is_active, created_by, created_at, updated_at,
                connection_status, capabilities, metadata
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, true, $8, $9, $10, 'pending', $11, $12)
            RETURNING *
            "#,
                &[
                    &id,
                    &organization_id,
                    &request.name,
                    &request.description,
                    &request.mcp_server_url,
                    &request.auth_type.to_string(),
                    &auth_config,
                    &creator_user_id,
                    &now,
                    &now,
                    &None::<serde_json::Value>,
                    &None::<serde_json::Value>,
                ],
            )
            .await
            .context("Failed to create MCP connector")?;

        debug!(
            "Created MCP connector: {} for organization: {}",
            id, organization_id
        );
        self.row_to_connector(row)
    }

    /// Get an MCP connector by ID
    pub async fn get_by_id(&self, id: Uuid) -> Result<Option<McpConnector>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_opt("SELECT * FROM mcp_connectors WHERE id = $1", &[&id])
            .await
            .context("Failed to query MCP connector")?;

        match row {
            Some(row) => Ok(Some(self.row_to_connector(row)?)),
            None => Ok(None),
        }
    }

    /// Get all MCP connectors for an organization
    pub async fn list_by_organization(&self, organization_id: Uuid) -> Result<Vec<McpConnector>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
            SELECT * FROM mcp_connectors 
            WHERE organization_id = $1 
            ORDER BY created_at DESC
            "#,
                &[&organization_id],
            )
            .await
            .context("Failed to query MCP connectors")?;

        rows.into_iter()
            .map(|row| self.row_to_connector(row))
            .collect()
    }

    /// Get active MCP connectors for an organization
    pub async fn list_active_by_organization(
        &self,
        organization_id: Uuid,
    ) -> Result<Vec<McpConnector>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
            SELECT * FROM mcp_connectors 
            WHERE organization_id = $1 AND is_active = true
            ORDER BY created_at DESC
            "#,
                &[&organization_id],
            )
            .await
            .context("Failed to query active MCP connectors")?;

        rows.into_iter()
            .map(|row| self.row_to_connector(row))
            .collect()
    }

    /// Update an MCP connector
    pub async fn update(
        &self,
        id: Uuid,
        request: UpdateMcpConnectorRequest,
    ) -> Result<McpConnector> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let now = Utc::now();

        // Build dynamic update query
        let mut query = String::from("UPDATE mcp_connectors SET updated_at = $1");
        let mut params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = vec![&now];
        let mut param_idx = 2;

        if let Some(ref name) = request.name {
            query.push_str(&format!(", name = ${param_idx}"));
            params.push(name);
            param_idx += 1;
        }

        if let Some(ref description) = request.description {
            query.push_str(&format!(", description = ${param_idx}"));
            params.push(description);
            param_idx += 1;
        }

        if let Some(ref url) = request.mcp_server_url {
            query.push_str(&format!(", mcp_server_url = ${param_idx}"));
            params.push(url);
            param_idx += 1;
        }

        let auth_type_str;
        if let Some(ref auth_type) = request.auth_type {
            auth_type_str = auth_type.to_string();
            query.push_str(&format!(", auth_type = ${param_idx}"));
            params.push(&auth_type_str);
            param_idx += 1;
        }

        let auth_config;
        if let Some(ref bearer_token) = request.bearer_token {
            auth_config = serde_json::to_value(McpBearerConfig {
                token: bearer_token.clone(),
            })
            .unwrap();
            query.push_str(&format!(", auth_config = ${param_idx}"));
            params.push(&auth_config);
            param_idx += 1;
        }

        if let Some(ref is_active) = request.is_active {
            query.push_str(&format!(", is_active = ${param_idx}"));
            params.push(is_active);
            param_idx += 1;
        }

        query.push_str(&format!(" WHERE id = ${param_idx} RETURNING *"));
        params.push(&id);

        let row = client
            .query_one(&query, &params)
            .await
            .context("Failed to update MCP connector")?;

        debug!("Updated MCP connector: {}", id);
        self.row_to_connector(row)
    }

    /// Delete an MCP connector
    pub async fn delete(&self, id: Uuid) -> Result<()> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let result = client
            .execute("DELETE FROM mcp_connectors WHERE id = $1", &[&id])
            .await
            .context("Failed to delete MCP connector")?;

        if result == 0 {
            bail!("MCP connector not found");
        }

        debug!("Deleted MCP connector: {}", id);
        Ok(())
    }

    /// Update connection status for an MCP connector
    pub async fn update_connection_status(
        &self,
        id: Uuid,
        status: McpConnectionStatus,
        error_message: Option<String>,
        capabilities: Option<serde_json::Value>,
    ) -> Result<()> {
        debug!(
            "Attempting to update connection status for connector {}",
            id
        );
        debug!(
            "Status: {:?}, Error: {:?}, Has capabilities: {}",
            status,
            error_message,
            capabilities.is_some()
        );

        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        debug!("Got database connection for connector {} status update", id);

        let now = Utc::now();
        let status_str = match status {
            McpConnectionStatus::Pending => "pending".to_string(),
            McpConnectionStatus::Connected => "connected".to_string(),
            McpConnectionStatus::Failed => "failed".to_string(),
        };

        debug!("Executing UPDATE query for connector {}: status={}, error_message={:?}, capabilities={:?}, now={}, id={}",
               id, status_str, error_message, capabilities.as_ref().map(|_| "<present>"), now, id);

        // Use separate variables to avoid type ambiguity
        let update_last_connected = status == McpConnectionStatus::Connected;

        let rows_affected = if update_last_connected {
            client
                .execute(
                    r#"
                UPDATE mcp_connectors 
                SET connection_status = $1,
                    error_message = $2,
                    capabilities = $3,
                    last_connected_at = $4,
                    updated_at = $4
                WHERE id = $5
                "#,
                    &[&status_str, &error_message, &capabilities, &now, &id],
                )
                .await
        } else {
            client
                .execute(
                    r#"
                UPDATE mcp_connectors 
                SET connection_status = $1,
                    error_message = $2,
                    capabilities = $3,
                    updated_at = $4
                WHERE id = $5
                "#,
                    &[&status_str, &error_message, &capabilities, &now, &id],
                )
                .await
        }
        .map_err(|e| {
            error!("Database UPDATE failed for connector {}: {:?}", id, e);
            error!("SQL Error details: {:#}", e);
            e
        })
        .context("Failed to update connection status")?;

        info!(
            "Updated connection status for MCP connector {}: {} (rows affected: {})",
            id, status_str, rows_affected
        );

        if rows_affected == 0 {
            warn!(
                "No rows updated for connector {} - connector may not exist",
                id
            );
        }

        Ok(())
    }

    /// Log MCP connector usage
    #[allow(clippy::too_many_arguments)]
    pub async fn log_usage(
        &self,
        connector_id: Uuid,
        user_id: Uuid,
        method: String,
        request_payload: Option<serde_json::Value>,
        response_payload: Option<serde_json::Value>,
        status_code: Option<i32>,
        error_message: Option<String>,
        duration_ms: Option<i32>,
    ) -> Result<()> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let id = Uuid::new_v4();
        let now = Utc::now();

        client
            .execute(
                r#"
            INSERT INTO mcp_connector_usage (
                id, connector_id, user_id, method,
                request_payload, response_payload,
                status_code, error_message, duration_ms,
                created_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            "#,
                &[
                    &id,
                    &connector_id,
                    &user_id,
                    &method,
                    &request_payload,
                    &response_payload,
                    &status_code,
                    &error_message,
                    &duration_ms,
                    &now,
                ],
            )
            .await
            .context("Failed to log MCP connector usage")?;

        Ok(())
    }

    /// Get usage logs for an MCP connector
    pub async fn get_usage_logs(
        &self,
        connector_id: Uuid,
        limit: i64,
    ) -> Result<Vec<McpConnectorUsage>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
            SELECT * FROM mcp_connector_usage
            WHERE connector_id = $1
            ORDER BY created_at DESC
            LIMIT $2
            "#,
                &[&connector_id, &limit],
            )
            .await
            .context("Failed to query usage logs")?;

        rows.into_iter().map(|row| self.row_to_usage(row)).collect()
    }

    /// Convert a database row to McpConnector
    fn row_to_connector(&self, row: tokio_postgres::Row) -> Result<McpConnector> {
        let auth_type_str: String = row.get("auth_type");
        let auth_type = match auth_type_str.as_str() {
            "none" => McpAuthType::None,
            "bearer" => McpAuthType::Bearer,
            _ => bail!("Invalid auth type: {}", auth_type_str),
        };

        let status_str: String = row.get("connection_status");
        let connection_status = match status_str.as_str() {
            "pending" => McpConnectionStatus::Pending,
            "connected" => McpConnectionStatus::Connected,
            "failed" => McpConnectionStatus::Failed,
            _ => McpConnectionStatus::Pending,
        };

        Ok(McpConnector {
            id: row.get("id"),
            organization_id: row.get("organization_id"),
            name: row.get("name"),
            description: row.get("description"),
            mcp_server_url: row.get("mcp_server_url"),
            auth_type,
            auth_config: row.get("auth_config"),
            is_active: row.get("is_active"),
            created_by: row.get("created_by"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
            last_connected_at: row.get("last_connected_at"),
            connection_status,
            error_message: row.get("error_message"),
            capabilities: row.get("capabilities"),
            metadata: row.get("metadata"),
        })
    }

    /// Convert a database row to McpConnectorUsage
    fn row_to_usage(&self, row: tokio_postgres::Row) -> Result<McpConnectorUsage> {
        Ok(McpConnectorUsage {
            id: row.get("id"),
            connector_id: row.get("connector_id"),
            user_id: row.get("user_id"),
            method: row.get("method"),
            request_payload: row.get("request_payload"),
            response_payload: row.get("response_payload"),
            status_code: row.get("status_code"),
            error_message: row.get("error_message"),
            duration_ms: row.get("duration_ms"),
            created_at: row.get("created_at"),
        })
    }
}
