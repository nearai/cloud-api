//! Response Items Repository
//!
//! This repository provides persistent storage for individual ResponseOutputItem entries.
//! Each item (message, tool call, web search call, reasoning) is stored separately in the
//! database, allowing for:
//!
//! 1. **Granular retrieval**: Get specific items without loading entire responses
//! 2. **Conversation context**: Build context for long-running conversations efficiently
//! 3. **Historical tracking**: Track the evolution of responses over time
//! 4. **Item-level queries**: Search and filter by item type, status, or content
//!
//! ## Usage Example
//!
//! ```ignore
//! use database::repositories::PgResponseItemsRepository;
//! use services::responses::models::{ResponseOutputItem, ResponseId, ResponseItemStatus};
//!
//! // Create repository
//! let repo = PgResponseItemsRepository::new(pool);
//!
//! // Store a message item
//! let item = ResponseOutputItem::Message {
//!     id: "msg_123".to_string(),
//!     status: ResponseItemStatus::Completed,
//!     role: "assistant".to_string(),
//!     content: vec![],
//! };
//! repo.create(response_id, user_id, Some(conversation_id), item).await?;
//!
//! // Get all items for a conversation (useful for context building).
//! // Queries are always constrained to the owning workspace.
//! let items = repo.list_by_conversation(conversation_id, workspace_id, None, 100).await?;
//!
//! // Get all items for a specific response
//! let items = repo.list_by_response(response_id).await?;
//! ```

use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use services::common::RepositoryError;
use services::conversations::models::ConversationId;
use services::responses::models::*;
use services::responses::ports::*;
use tracing::debug;
use uuid::Uuid;

pub struct PgResponseItemsRepository {
    pool: DbPool,
}

impl PgResponseItemsRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Helper method to convert database row to ResponseOutputItem
    /// Enriches the item with response metadata (response_id, previous_response_id, next_response_ids, created_at)
    fn row_to_item(&self, row: tokio_postgres::Row) -> Result<ResponseOutputItem> {
        let item_json: serde_json::Value = row.try_get("item")?;
        let mut item: ResponseOutputItem = serde_json::from_value(item_json)
            .context("Failed to deserialize response item from database")?;

        // Get response metadata from joined responses table
        let response_id: Uuid = row.try_get("response_id")?;
        let response_id_str = format!("resp_{}", response_id.simple());

        let previous_response_id: Option<Uuid> = row.try_get("previous_response_id").ok().flatten();
        let previous_response_id_str =
            previous_response_id.map(|id| format!("resp_{}", id.simple()));

        let next_response_ids_json: Option<serde_json::Value> =
            row.try_get("next_response_ids").ok().flatten();
        let next_response_ids: Vec<String> = next_response_ids_json
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| {
                v.as_str().and_then(|s| {
                    Uuid::parse_str(s)
                        .ok()
                        .map(|uuid| format!("resp_{}", uuid.simple()))
                })
            })
            .collect();

        // Use the item's own created_at timestamp, not the response's created_at
        // This ensures each item has a unique timestamp reflecting when it was created
        let item_created_at: chrono::DateTime<chrono::Utc> = row.try_get("created_at")?;
        let created_at_timestamp = item_created_at.timestamp();

        let model = match item.model() {
            Some(m) if !m.is_empty() => m.to_string(),
            _ => row.try_get("model")?,
        };

        // Enrich the item with response metadata
        match &mut item {
            ResponseOutputItem::Message {
                response_id: ref mut rid,
                previous_response_id: ref mut prev,
                next_response_ids: ref mut next,
                created_at: ref mut ts,
                model: ref mut mdl,
                ..
            } => {
                *rid = response_id_str;
                *prev = previous_response_id_str;
                *next = next_response_ids;
                *ts = created_at_timestamp;
                *mdl = model;
            }
            ResponseOutputItem::ToolCall {
                response_id: ref mut rid,
                previous_response_id: ref mut prev,
                next_response_ids: ref mut next,
                created_at: ref mut ts,
                model: ref mut mdl,
                ..
            } => {
                *rid = response_id_str;
                *prev = previous_response_id_str;
                *next = next_response_ids;
                *ts = created_at_timestamp;
                *mdl = model;
            }
            ResponseOutputItem::WebSearchCall {
                response_id: ref mut rid,
                previous_response_id: ref mut prev,
                next_response_ids: ref mut next,
                created_at: ref mut ts,
                model: ref mut mdl,
                ..
            } => {
                *rid = response_id_str;
                *prev = previous_response_id_str;
                *next = next_response_ids;
                *ts = created_at_timestamp;
                *mdl = model;
            }
            ResponseOutputItem::Reasoning {
                response_id: ref mut rid,
                previous_response_id: ref mut prev,
                next_response_ids: ref mut next,
                created_at: ref mut ts,
                model: ref mut mdl,
                ..
            } => {
                *rid = response_id_str;
                *prev = previous_response_id_str;
                *next = next_response_ids;
                *ts = created_at_timestamp;
                *mdl = model;
            }
            // McpListTools is not stored in DB - it's only emitted for client-side caching
            ResponseOutputItem::McpListTools { .. } => {}
            ResponseOutputItem::McpCall {
                response_id: ref mut rid,
                previous_response_id: ref mut prev,
                next_response_ids: ref mut next,
                created_at: ref mut ts,
                model: ref mut mdl,
                ..
            } => {
                *rid = response_id_str;
                *prev = previous_response_id_str;
                *next = next_response_ids;
                *ts = created_at_timestamp;
                *mdl = model;
            }
            ResponseOutputItem::McpApprovalRequest {
                response_id: ref mut rid,
                previous_response_id: ref mut prev,
                next_response_ids: ref mut next,
                created_at: ref mut ts,
                model: ref mut mdl,
                ..
            } => {
                *rid = response_id_str;
                *prev = previous_response_id_str;
                *next = next_response_ids;
                *ts = created_at_timestamp;
                *mdl = model;
            }
            ResponseOutputItem::FunctionCall {
                response_id: ref mut rid,
                previous_response_id: ref mut prev,
                next_response_ids: ref mut next,
                created_at: ref mut ts,
                model: ref mut mdl,
                ..
            } => {
                *rid = response_id_str;
                *prev = previous_response_id_str;
                *next = next_response_ids;
                *ts = created_at_timestamp;
                *mdl = model;
            }
            ResponseOutputItem::FunctionCallOutput {
                response_id: ref mut rid,
                previous_response_id: ref mut prev,
                next_response_ids: ref mut next,
                created_at: ref mut ts,
                ..
            } => {
                *rid = response_id_str;
                *prev = previous_response_id_str;
                *next = next_response_ids;
                *ts = created_at_timestamp;
            }
        }

        Ok(item)
    }

    /// Helper method to extract or generate UUID from item ID string
    /// If the item_id is already a valid UUID or contains one (e.g., "msg_abc123"), use it.
    /// Otherwise, generate a new UUID (for external provider IDs like OpenAI's "call_xxx").
    fn extract_uuid_from_item_id(item_id: &str) -> Uuid {
        // First try parsing as a UUID directly
        if let Ok(uuid) = Uuid::parse_str(item_id) {
            return uuid;
        }

        // Item IDs may be in format like "msg_abc123", "web_search_xyz789", etc.
        let parts: Vec<&str> = item_id.split('_').collect();
        if let Ok(uuid) = Uuid::parse_str(parts[parts.len() - 1]) {
            return uuid;
        }

        // Generate a new UUID for external provider IDs
        Uuid::new_v4()
    }

    /// Helper to create a response item ID from a UUID
    pub fn create_item_id(uuid: Uuid, prefix: &str) -> String {
        format!("{prefix}_{uuid}")
    }
}

#[async_trait]
impl ResponseItemRepositoryTrait for PgResponseItemsRepository {
    /// Create a new response item
    async fn create(
        &self,
        response_id: ResponseId,
        api_key_id: uuid::Uuid,
        conversation_id: Option<ConversationId>,
        item: ResponseOutputItem,
    ) -> Result<ResponseOutputItem> {
        // Extract or generate UUID from the item's ID string
        let item_id = item.id();
        let id = Self::extract_uuid_from_item_id(item_id);

        // Serialize the item to JSON for storage
        let item_json = serde_json::to_value(&item).context("Failed to serialize response item")?;

        let conversation_uuid = conversation_id.map(|cid| cid.0);

        let row = retry_db!("create_response_item", {
            let now = Utc::now();
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    r#"
                    INSERT INTO response_items (id, response_id, api_key_id, conversation_id, item, created_at, updated_at)
                    VALUES ($1, $2, $3, $4, $5, $6, $7)
                    RETURNING
                        response_items.*,
                        (SELECT previous_response_id FROM responses WHERE id = $2) as previous_response_id,
                        (SELECT next_response_ids FROM responses WHERE id = $2) as next_response_ids,
                        (SELECT created_at FROM responses WHERE id = $2) as response_created_at,
                        (SELECT model FROM responses WHERE id = $2) as model
                    "#,
                    &[
                        &id,
                        &response_id.0,
                        &api_key_id,
                        &conversation_uuid,
                        &item_json,
                        &now,
                        &now,
                    ],
                )
                .await
                .map_err(map_db_error)
        })?;

        debug!(
            "Created response item: {} for response: {} api_key: {}",
            item_id, response_id, api_key_id
        );

        self.row_to_item(row)
    }

    /// Get a response item by its ID, constrained to the owning workspace.
    ///
    /// The workspace constraint goes through the owning response so a caller
    /// can never read another workspace's item, even with a known item ID.
    async fn get_by_id(
        &self,
        id: ResponseItemId,
        workspace_id: services::workspace::WorkspaceId,
    ) -> Result<Option<ResponseOutputItem>> {
        let row = retry_db!("get_response_item_by_id", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
                    SELECT
                        ri.*,
                        r.previous_response_id,
                        r.next_response_ids,
                        r.created_at as response_created_at,
                        r.model
                    FROM response_items ri
                    JOIN responses r ON ri.response_id = r.id
                    WHERE ri.id = $1
                      AND r.workspace_id = $2
                    "#,
                    &[&id.0, &workspace_id.0],
                )
                .await
                .map_err(map_db_error)
        })?;

        match row {
            Some(row) => Ok(Some(self.row_to_item(row)?)),
            None => Ok(None),
        }
    }

    /// Update a response item
    async fn update(
        &self,
        id: ResponseItemId,
        item: ResponseOutputItem,
    ) -> Result<ResponseOutputItem> {
        // Serialize the updated item to JSON
        let item_json = serde_json::to_value(&item).context("Failed to serialize response item")?;

        let row = retry_db!("update_response_item", {
            let now = Utc::now();
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
                    UPDATE response_items
                    SET item = $2, updated_at = $3
                    WHERE id = $1
                    RETURNING *
                    "#,
                    &[&id.0, &item_json, &now],
                )
                .await
                .map_err(map_db_error)
        })?;

        match row {
            Some(row) => {
                debug!("Updated response item: {}", id.0);
                self.row_to_item(row)
            }
            None => Err(anyhow::anyhow!("Response item not found: {}", id.0)),
        }
    }

    /// Delete a response item
    async fn delete(&self, id: ResponseItemId) -> Result<bool> {
        let result = retry_db!("delete_response_item", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute("DELETE FROM response_items WHERE id = $1", &[&id.0])
                .await
                .map_err(map_db_error)
        })?;

        if result > 0 {
            debug!("Deleted response item: {}", id.0);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// List all items for a specific response
    async fn list_by_response(&self, response_id: ResponseId) -> Result<Vec<ResponseOutputItem>> {
        let rows = retry_db!("list_response_items_by_response", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    r#"
                    SELECT
                        ri.*,
                        r.previous_response_id,
                        r.next_response_ids,
                        r.created_at as response_created_at,
                        r.model
                    FROM response_items ri
                    JOIN responses r ON ri.response_id = r.id
                    WHERE ri.response_id = $1
                    ORDER BY ri.created_at ASC
                    "#,
                    &[&response_id.0],
                )
                .await
                .map_err(map_db_error)
        })?;

        rows.into_iter().map(|row| self.row_to_item(row)).collect()
    }

    /// List all items for a specific API key
    async fn list_by_api_key(&self, api_key_id: uuid::Uuid) -> Result<Vec<ResponseOutputItem>> {
        let rows = retry_db!("list_response_items_by_api_key", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    r#"
                    SELECT
                        ri.*,
                        r.previous_response_id,
                        r.next_response_ids,
                        r.created_at as response_created_at,
                        r.model
                    FROM response_items ri
                    JOIN responses r ON ri.response_id = r.id
                    WHERE ri.api_key_id = $1
                    ORDER BY ri.created_at DESC
                    "#,
                    &[&api_key_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        rows.into_iter().map(|row| self.row_to_item(row)).collect()
    }

    /// List all items for a specific conversation (useful for building context)
    /// Supports cursor-based pagination using the `after` parameter
    ///
    /// Every query is constrained to the owning workspace through the joined
    /// response row (defense in depth on top of the service-level ownership
    /// check), and an `after` cursor is only accepted when it references an
    /// item of this exact conversation and workspace.
    async fn list_by_conversation(
        &self,
        conversation_id: ConversationId,
        workspace_id: services::workspace::WorkspaceId,
        after: Option<String>,
        limit: i64,
    ) -> Result<Vec<ResponseOutputItem>> {
        // Extract UUID from the after item ID if provided
        let after_uuid = after.as_ref().map(|id| Self::extract_uuid_from_item_id(id));

        // Validate the pagination cursor before using it: it must reference an
        // item that belongs to this conversation and this workspace. Unknown
        // and foreign cursors are rejected identically so they cannot be used
        // to probe other conversations or workspaces.
        let after_position = if let Some(after_uuid) = after_uuid {
            let cursor_row = retry_db!("validate_conversation_item_cursor", {
                let client = self
                    .pool
                    .get()
                    .await
                    .context("Failed to get database connection")
                    .map_err(RepositoryError::PoolError)?;

                client
                    .query_opt(
                        r#"
                        SELECT ri.created_at, ri.id
                        FROM response_items ri
                        JOIN responses r ON ri.response_id = r.id
                        WHERE ri.id = $1
                          AND ri.conversation_id = $2
                          AND r.workspace_id = $3
                        "#,
                        &[&after_uuid, &conversation_id.0, &workspace_id.0],
                    )
                    .await
                    .map_err(map_db_error)
            })?;

            let Some(cursor_row) = cursor_row else {
                return Err(anyhow::Error::new(RepositoryError::NotFound(
                    "pagination cursor".to_string(),
                )));
            };

            let cursor_created_at: chrono::DateTime<Utc> = cursor_row.try_get("created_at")?;
            let cursor_id: Uuid = cursor_row.try_get("id")?;
            Some((cursor_created_at, cursor_id))
        } else {
            None
        };

        let rows = retry_db!("list_response_items_by_conversation", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            if let Some((cursor_created_at, cursor_id)) = after_position {
                // Query items after the reference item using composite (created_at, id) comparison
                // This handles cases where multiple items have the same created_at timestamp
                // We fetch limit + 1 to determine if there are more items
                client
                    .query(
                        r#"
                        SELECT
                            ri.*,
                            r.previous_response_id,
                            r.next_response_ids,
                            r.created_at as response_created_at,
                            r.model
                        FROM response_items ri
                        JOIN responses r ON ri.response_id = r.id
                        WHERE ri.conversation_id = $1
                          AND r.workspace_id = $2
                          AND (ri.created_at, ri.id) > ($3, $4)
                        ORDER BY ri.created_at ASC, ri.id ASC
                        LIMIT $5
                        "#,
                        &[
                            &conversation_id.0,
                            &workspace_id.0,
                            &cursor_created_at,
                            &cursor_id,
                            &limit,
                        ],
                    )
                    .await
                    .map_err(map_db_error)
            } else {
                // No pagination cursor, fetch from the beginning
                client
                    .query(
                        r#"
                        SELECT
                            ri.*,
                            r.previous_response_id,
                            r.next_response_ids,
                            r.created_at as response_created_at,
                            r.model
                        FROM response_items ri
                        JOIN responses r ON ri.response_id = r.id
                        WHERE ri.conversation_id = $1
                          AND r.workspace_id = $2
                        ORDER BY ri.created_at ASC, ri.id ASC
                        LIMIT $3
                        "#,
                        &[&conversation_id.0, &workspace_id.0, &limit],
                    )
                    .await
                    .map_err(map_db_error)
            }
        })?;

        rows.into_iter().map(|row| self.row_to_item(row)).collect()
    }
}
