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

        Ok(Conversation {
            id: id.into(),
            workspace_id: workspace_id.into(),
            api_key_id,
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

    /// Get a conversation by ID
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
                "SELECT * FROM conversations WHERE id = $1 AND workspace_id = $2",
                &[&id.0, &workspace_id.0],
            )
            .await
            .context("Failed to query conversation")?;

        match row {
            Some(row) => Ok(Some(self.row_to_conversation(row)?)),
            None => Ok(None),
        }
    }

    /// Update a conversation's metadata
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
            WHERE id = $1 AND workspace_id = $2
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

    /// Delete a conversation (will cascade delete associated responses)
    async fn delete(&self, id: ConversationId, workspace_id: WorkspaceId) -> Result<bool> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let result = client
            .execute(
                "DELETE FROM conversations WHERE id = $1 AND workspace_id = $2",
                &[&id.0, &workspace_id.0],
            )
            .await
            .context("Failed to delete conversation")?;

        if result > 0 {
            debug!(
                "Deleted conversation: {} for workspace: {}",
                id, workspace_id.0
            );
            Ok(true)
        } else {
            Ok(false)
        }
    }
}
