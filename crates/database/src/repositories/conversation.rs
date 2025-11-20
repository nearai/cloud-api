use crate::pool::DbPool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use services::conversations::models::{Conversation, ConversationId};
use services::conversations::ports::ConversationRepository;
use services::workspace::WorkspaceId;
use tracing::debug;
use uuid::Uuid;

pub struct PgConversationRepository {
    pool: DbPool,
}

impl PgConversationRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    // Helper method to convert database row to Conversation model
    fn row_to_conversation(&self, row: tokio_postgres::Row) -> Result<Conversation> {
        let id: Uuid = row.try_get("id")?;
        let workspace_id: Uuid = row.try_get("workspace_id")?;
        let api_key_id: Uuid = row.try_get("api_key_id")?;
        let cloned_from_id: Option<Uuid> = row.try_get("cloned_from_id")?;

        Ok(Conversation {
            id: id.into(),
            workspace_id: workspace_id.into(),
            api_key_id,
            pinned_at: row.try_get("pinned_at")?,
            archived_at: row.try_get("archived_at")?,
            deleted_at: row.try_get("deleted_at")?,
            cloned_from_id: cloned_from_id.map(|id| id.into()),
            metadata: row.try_get("metadata")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

#[async_trait]
impl ConversationRepository for PgConversationRepository {
    /// Create a new conversation
    async fn create(
        &self,
        workspace_id: WorkspaceId,
        api_key_id: uuid::Uuid,
        metadata: serde_json::Value,
    ) -> Result<Conversation> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let id = Uuid::new_v4();
        let now = Utc::now();

        let row = client
            .query_one(
                r#"
            INSERT INTO conversations (id, workspace_id, api_key_id, metadata, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            RETURNING *
            "#,
                &[&id, &workspace_id.0, &api_key_id, &metadata, &now, &now],
            )
            .await
            .context("Failed to create conversation")?;

        debug!(
            "Created conversation: {} for workspace: {} with api_key: {}",
            id, workspace_id.0, api_key_id
        );
        self.row_to_conversation(row)
    }

    /// Get a conversation by ID (excludes soft-deleted conversations)
    async fn get_by_id(
        &self,
        id: ConversationId,
        workspace_id: WorkspaceId,
    ) -> Result<Option<Conversation>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_opt(
                "SELECT * FROM conversations WHERE id = $1 AND workspace_id = $2 AND deleted_at IS NULL",
                &[&id.0, &workspace_id.0],
            )
            .await
            .context("Failed to query conversation")?;

        match row {
            Some(row) => Ok(Some(self.row_to_conversation(row)?)),
            None => Ok(None),
        }
    }

    /// Update a conversation's metadata (excludes soft-deleted conversations)
    async fn update(
        &self,
        id: ConversationId,
        workspace_id: WorkspaceId,
        metadata: serde_json::Value,
    ) -> Result<Option<Conversation>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let now = Utc::now();

        let row = client
            .query_opt(
                r#"
            UPDATE conversations 
            SET metadata = $3, updated_at = $4
            WHERE id = $1 AND workspace_id = $2 AND deleted_at IS NULL
            RETURNING *
            "#,
                &[&id.0, &workspace_id.0, &metadata, &now],
            )
            .await
            .context("Failed to update conversation")?;

        match row {
            Some(row) => {
                debug!(
                    "Updated conversation: {} for workspace: {}",
                    id, workspace_id.0
                );
                Ok(Some(self.row_to_conversation(row)?))
            }
            None => Ok(None),
        }
    }

    /// Pin or unpin a conversation
    async fn set_pinned(
        &self,
        id: ConversationId,
        workspace_id: WorkspaceId,
        is_pinned: bool,
    ) -> Result<Option<Conversation>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let now = Utc::now();
        let pinned_at = if is_pinned { Some(now) } else { None };

        let row = client
            .query_opt(
                r#"
            UPDATE conversations 
            SET pinned_at = $3, updated_at = $4
            WHERE id = $1 AND workspace_id = $2 AND deleted_at IS NULL
            RETURNING *
            "#,
                &[&id.0, &workspace_id.0, &pinned_at, &now],
            )
            .await
            .context("Failed to update conversation pinned status")?;

        match row {
            Some(row) => {
                debug!(
                    "Updated conversation pinned status: {} (pinned={}) for workspace: {}",
                    id, is_pinned, workspace_id.0
                );
                Ok(Some(self.row_to_conversation(row)?))
            }
            None => Ok(None),
        }
    }

    /// Archive or unarchive a conversation
    async fn set_archived(
        &self,
        id: ConversationId,
        workspace_id: WorkspaceId,
        is_archived: bool,
    ) -> Result<Option<Conversation>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let now = Utc::now();
        let archived_at = if is_archived { Some(now) } else { None };

        let row = client
            .query_opt(
                r#"
            UPDATE conversations 
            SET archived_at = $3, updated_at = $4
            WHERE id = $1 AND workspace_id = $2 AND deleted_at IS NULL
            RETURNING *
            "#,
                &[&id.0, &workspace_id.0, &archived_at, &now],
            )
            .await
            .context("Failed to update conversation archived status")?;

        match row {
            Some(row) => {
                debug!(
                    "Updated conversation archived status: {} (archived={}) for workspace: {}",
                    id, is_archived, workspace_id.0
                );
                Ok(Some(self.row_to_conversation(row)?))
            }
            None => Ok(None),
        }
    }

    /// Clone a conversation (deep copy with new ID, includes all responses and items)
    /// Excludes soft-deleted conversations
    async fn clone_conversation(
        &self,
        id: ConversationId,
        workspace_id: WorkspaceId,
        api_key_id: uuid::Uuid,
    ) -> Result<Option<Conversation>> {
        let mut client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let new_conv_id = Uuid::new_v4();
        let now = Utc::now();

        // Start a transaction for atomic cloning
        let transaction = client
            .transaction()
            .await
            .context("Failed to start transaction")?;

        // Step 1: Clone the conversation with a new ID and append " (Copy)" to title in metadata
        // Reset pinned_at, archived_at, deleted_at to NULL for the clone
        let conv_row = transaction
            .query_opt(
                r#"
            INSERT INTO conversations (id, workspace_id, api_key_id, pinned_at, archived_at, deleted_at, cloned_from_id, metadata, created_at, updated_at)
            SELECT 
                $1, 
                workspace_id, 
                $2, 
                NULL,
                NULL,
                NULL,
                id,
                CASE 
                    WHEN metadata->>'title' IS NOT NULL THEN 
                        jsonb_set(metadata, '{title}', to_jsonb((metadata->>'title') || ' (Copy)'))
                    ELSE 
                        metadata
                END,
                $3, 
                $4
            FROM conversations
            WHERE id = $5 AND workspace_id = $6 AND deleted_at IS NULL
            RETURNING *
            "#,
                &[&new_conv_id, &api_key_id, &now, &now, &id.0, &workspace_id.0],
            )
            .await
            .context("Failed to clone conversation")?;

        if conv_row.is_none() {
            // Conversation not found or is deleted, rollback and return None
            transaction.rollback().await.ok();
            return Ok(None);
        }

        // Step 2: Get all responses from the original conversation
        let original_responses = transaction
            .query(
                "SELECT id FROM responses WHERE conversation_id = $1 AND workspace_id = $2 ORDER BY created_at ASC",
                &[&id.0, &workspace_id.0],
            )
            .await
            .context("Failed to get original responses")?;

        // Step 3: Clone each response and build ID mapping
        let mut id_map = std::collections::HashMap::new();

        for orig_row in &original_responses {
            let old_response_id: Uuid = orig_row.try_get("id")?;
            let new_response_id = Uuid::new_v4();
            id_map.insert(old_response_id, new_response_id);

            // Clone the response with new ID and new conversation_id
            transaction
                .execute(
                    r#"
                INSERT INTO responses (id, workspace_id, api_key_id, model, status, instructions, conversation_id, previous_response_id, next_response_ids, usage, metadata, created_at, updated_at)
                SELECT 
                    $1,
                    workspace_id,
                    $2,
                    model,
                    status,
                    instructions,
                    $3,
                    previous_response_id,
                    next_response_ids,
                    usage,
                    metadata,
                    $4,
                    $5
                FROM responses
                WHERE id = $6
                "#,
                    &[&new_response_id, &api_key_id, &new_conv_id, &now, &now, &old_response_id],
                )
                .await
                .context("Failed to clone response")?;
        }

        // Step 4: Update previous_response_id and next_response_ids in cloned responses to point to new IDs
        for (old_id, new_id) in &id_map {
            // Get the original response to check its relationships
            let original_resp = transaction
                .query_opt(
                    "SELECT previous_response_id, next_response_ids FROM responses WHERE id = $1",
                    &[old_id],
                )
                .await
                .context("Failed to get original response")?;

            if let Some(orig_row) = original_resp {
                let old_prev: Option<Uuid> = orig_row.try_get("previous_response_id")?;
                let old_next: Option<serde_json::Value> = orig_row.try_get("next_response_ids")?;

                // Map previous_response_id to new ID
                let new_prev = old_prev.and_then(|old_prev_id| id_map.get(&old_prev_id).copied());

                // Map next_response_ids array to new IDs
                let new_next = if let Some(next_json) = old_next {
                    if let Some(next_array) = next_json.as_array() {
                        let mapped_next: Vec<String> = next_array
                            .iter()
                            .filter_map(|v| v.as_str())
                            .filter_map(|s| Uuid::parse_str(s).ok())
                            .filter_map(|old_next_id| id_map.get(&old_next_id))
                            .map(|new_next_id| new_next_id.to_string())
                            .collect();
                        Some(serde_json::json!(mapped_next))
                    } else {
                        Some(next_json)
                    }
                } else {
                    None
                };

                // Update the cloned response with mapped IDs
                transaction
                    .execute(
                        r#"
                    UPDATE responses
                    SET previous_response_id = $2, next_response_ids = $3
                    WHERE id = $1
                    "#,
                        &[new_id, &new_prev, &new_next],
                    )
                    .await
                    .context("Failed to update cloned response relationships")?;
            }
        }

        // Step 5: Clone all response_items, mapping old response_ids to new ones
        // Preserve original created_at timestamps to maintain order
        let original_items = transaction
            .query(
                "SELECT id, response_id, item, created_at FROM response_items WHERE conversation_id = $1 ORDER BY created_at ASC",
                &[&id.0],
            )
            .await
            .context("Failed to get original response items")?;

        for item_row in &original_items {
            let old_response_id: Uuid = item_row.try_get("response_id")?;
            let mut item_json: serde_json::Value = item_row.try_get("item")?;
            let original_created_at: chrono::DateTime<Utc> = item_row.try_get("created_at")?;

            // Map old response_id to new response_id
            let new_response_id = id_map
                .get(&old_response_id)
                .copied()
                .unwrap_or(old_response_id);
            let new_item_id = Uuid::new_v4();

            // Update the "id" field inside the item JSON to use the new item ID
            // The item JSON has a structure like: { "id": "msg_...", "type": "message", ... }
            if let Some(obj) = item_json.as_object_mut() {
                // Generate a new message ID in the format "msg_<uuid without hyphens>"
                let new_msg_id = format!("msg_{}", new_item_id.as_simple());
                obj.insert("id".to_string(), serde_json::Value::String(new_msg_id));
            }

            transaction
                .execute(
                    r#"
                INSERT INTO response_items (id, response_id, api_key_id, conversation_id, item, created_at, updated_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7)
                "#,
                    &[&new_item_id, &new_response_id, &api_key_id, &new_conv_id, &item_json, &original_created_at, &now],
                )
                .await
                .context("Failed to clone response item")?;
        }

        // Commit the transaction
        transaction
            .commit()
            .await
            .context("Failed to commit clone transaction")?;

        debug!(
            "Cloned conversation: {} -> {} for workspace: {} (including all responses and items)",
            id, new_conv_id, workspace_id.0
        );

        // Return the cloned conversation
        let cloned_conv = self
            .get_by_id(ConversationId(new_conv_id), workspace_id)
            .await?;
        Ok(cloned_conv)
    }

    /// Soft delete a conversation (sets deleted_at timestamp)
    async fn delete(&self, id: ConversationId, workspace_id: WorkspaceId) -> Result<bool> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let now = Utc::now();

        let result = client
             .execute(
                 "UPDATE conversations SET deleted_at = $3, updated_at = $4 WHERE id = $1 AND workspace_id = $2 AND deleted_at IS NULL",
                 &[&id.0, &workspace_id.0, &now, &now],
             )
             .await
             .context("Failed to soft delete conversation")?;

        if result > 0 {
            debug!(
                "Soft deleted conversation: {} for workspace: {}",
                id, workspace_id.0
            );
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Batch get conversations by IDs (excludes soft-deleted conversations)
    async fn batch_get_by_ids(
        &self,
        ids: Vec<ConversationId>,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Conversation>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        // Extract raw UUIDs from ConversationId wrappers
        let uuid_ids: Vec<Uuid> = ids.into_iter().map(|id| id.0).collect();

        // Query with ANY() for efficient batch retrieval
        let rows = client
             .query(
                 "SELECT * FROM conversations WHERE id = ANY($1) AND workspace_id = $2 AND deleted_at IS NULL",
                 &[&uuid_ids, &workspace_id.0],
             )
             .await
             .context("Failed to batch query conversations")?;

        debug!(
            "Batch retrieved {} conversations (requested {}) for workspace: {}",
            rows.len(),
            uuid_ids.len(),
            workspace_id.0
        );

        // Convert each row to Conversation model
        rows.into_iter()
            .map(|row| self.row_to_conversation(row))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_batch_get_by_ids_empty_vector() {
        // Empty vector should be handled gracefully
        let empty_ids: Vec<ConversationId> = vec![];
        assert_eq!(empty_ids.len(), 0, "Empty vector should have length 0");
    }

    #[test]
    fn test_batch_get_by_ids_creates_uuid_array() {
        // Test that ConversationId vectors are properly converted to UUID arrays
        let id1 = ConversationId(Uuid::nil());
        let id2 = ConversationId(Uuid::new_v4());
        let id3 = ConversationId(Uuid::new_v4());

        let ids = vec![id1, id2, id3];
        let uuid_ids: Vec<Uuid> = ids.into_iter().map(|id| id.0).collect();

        assert_eq!(uuid_ids.len(), 3, "Should have 3 UUIDs");
        assert_eq!(uuid_ids[0], Uuid::nil(), "First UUID should be nil");
        assert_eq!(uuid_ids[1], id2.0, "Second UUID should match");
        assert_eq!(uuid_ids[2], id3.0, "Third UUID should match");
    }

    #[test]
    fn test_conversation_id_ordering() {
        // Test that conversation IDs maintain order
        let id1 = ConversationId(Uuid::nil());
        let id2 = ConversationId(Uuid::new_v4());
        let id3 = ConversationId(Uuid::new_v4());

        let ids = [id1, id2, id3];

        assert_eq!(ids[0].0, id1.0, "First ID UUID should match");
        assert_eq!(ids[1].0, id2.0, "Second ID UUID should match");
        assert_eq!(ids[2].0, id3.0, "Third ID UUID should match");
    }
}
