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
//! // Get all items for a conversation (useful for context building)
//! let items = repo.list_by_conversation(conversation_id).await?;
//!
//! // Get all items for a specific response
//! let items = repo.list_by_response(response_id).await?;
//! ```

use crate::pool::DbPool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
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
    fn row_to_item(&self, row: tokio_postgres::Row) -> Result<ResponseOutputItem> {
        let item_json: serde_json::Value = row.try_get("item")?;
        let item: ResponseOutputItem = serde_json::from_value(item_json)
            .context("Failed to deserialize response item from database")?;
        Ok(item)
    }

    /// Helper method to extract UUID from item ID string (e.g., "msg_abc123" -> "abc123")
    /// If the item_id is already a valid UUID, use it directly.
    /// Otherwise, try to extract the UUID portion after an underscore prefix.
    fn extract_uuid_from_item_id(item_id: &str) -> Result<Uuid> {
        // First try parsing as a UUID directly
        if let Ok(uuid) = Uuid::parse_str(item_id) {
            return Ok(uuid);
        }

        // Item IDs may be in format like "msg_abc123", "web_search_xyz789", etc.
        let parts: Vec<&str> = item_id.split('_').collect();

        let uuid = Uuid::parse_str(parts[parts.len() - 1])
            .with_context(|| format!("Failed to parse UUID from item ID: {item_id}"))?;
        Ok(uuid)
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
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        // Extract UUID from the item's ID string
        let item_id = item.id();
        let id = Self::extract_uuid_from_item_id(item_id)?;
        let now = Utc::now();

        // Serialize the item to JSON for storage
        let item_json = serde_json::to_value(&item).context("Failed to serialize response item")?;

        let conversation_uuid = conversation_id.map(|cid| cid.0);

        let row = client
            .query_one(
                r#"
                INSERT INTO response_items (id, response_id, api_key_id, conversation_id, item, created_at, updated_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7)
                RETURNING *
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
            .context("Failed to insert response item")?;

        debug!(
            "Created response item: {} for response: {} api_key: {}",
            item_id, response_id, api_key_id
        );

        self.row_to_item(row)
    }

    /// Get a response item by its ID
    async fn get_by_id(&self, id: ResponseItemId) -> Result<Option<ResponseOutputItem>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_opt("SELECT * FROM response_items WHERE id = $1", &[&id.0])
            .await
            .context("Failed to query response item")?;

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
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let now = Utc::now();

        // Serialize the updated item to JSON
        let item_json = serde_json::to_value(&item).context("Failed to serialize response item")?;

        let row = client
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
            .context("Failed to update response item")?;

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
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let result = client
            .execute("DELETE FROM response_items WHERE id = $1", &[&id.0])
            .await
            .context("Failed to delete response item")?;

        if result > 0 {
            debug!("Deleted response item: {}", id.0);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// List all items for a specific response
    async fn list_by_response(&self, response_id: ResponseId) -> Result<Vec<ResponseOutputItem>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
                SELECT * FROM response_items
                WHERE response_id = $1
                ORDER BY created_at ASC
                "#,
                &[&response_id.0],
            )
            .await
            .context("Failed to query response items by response")?;

        rows.into_iter().map(|row| self.row_to_item(row)).collect()
    }

    /// List all items for a specific API key
    async fn list_by_api_key(&self, api_key_id: uuid::Uuid) -> Result<Vec<ResponseOutputItem>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
                SELECT * FROM response_items
                WHERE api_key_id = $1
                ORDER BY created_at DESC
                "#,
                &[&api_key_id],
            )
            .await
            .context("Failed to query response items by API key")?;

        rows.into_iter().map(|row| self.row_to_item(row)).collect()
    }

    /// List all items for a specific conversation (useful for building context)
    /// Supports cursor-based pagination using the `after` parameter
    async fn list_by_conversation(
        &self,
        conversation_id: ConversationId,
        after: Option<String>,
        limit: i64,
    ) -> Result<Vec<ResponseOutputItem>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = if let Some(after_id) = after {
            // Extract UUID from the after item ID
            let after_uuid = Self::extract_uuid_from_item_id(&after_id)?;

            // Query items after the reference item using composite (created_at, id) comparison
            // This handles cases where multiple items have the same created_at timestamp
            // We fetch limit + 1 to determine if there are more items
            client
                .query(
                    r#"
                    SELECT * FROM response_items
                    WHERE conversation_id = $1
                      AND (created_at, id) > (
                          SELECT created_at, id FROM response_items WHERE id = $2
                      )
                    ORDER BY created_at ASC, id ASC
                    LIMIT $3
                    "#,
                    &[&conversation_id.0, &after_uuid, &limit],
                )
                .await
                .context("Failed to query response items by conversation with pagination")?
        } else {
            // No pagination cursor, fetch from the beginning
            client
                .query(
                    r#"
                    SELECT * FROM response_items
                    WHERE conversation_id = $1
                    ORDER BY created_at ASC, id ASC
                    LIMIT $2
                    "#,
                    &[&conversation_id.0, &limit],
                )
                .await
                .context("Failed to query response items by conversation")?
        };

        rows.into_iter().map(|row| self.row_to_item(row)).collect()
    }
}
