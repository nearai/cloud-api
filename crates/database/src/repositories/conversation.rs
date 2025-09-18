use crate::pool::DbPool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use services::conversations::ports::{Conversation, ConversationRepository};
use services::responses::ports::{ConversationId, UserId};
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
        let user_id: Uuid = row.try_get("user_id")?;

        Ok(Conversation {
            id: id.into(),
            user_id: user_id.into(),
            metadata: row.try_get("metadata")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

#[async_trait]
impl ConversationRepository for PgConversationRepository {
    /// Create a new conversation
    async fn create(&self, user_id: UserId, metadata: serde_json::Value) -> Result<Conversation> {
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
            INSERT INTO conversations (id, user_id, metadata, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5)
            RETURNING *
            "#,
                &[&id, &user_id.to_string(), &metadata, &now, &now],
            )
            .await
            .context("Failed to create conversation")?;

        debug!("Created conversation: {} for user: {}", id, user_id);
        self.row_to_conversation(row)
    }

    /// Get a conversation by ID
    async fn get_by_id(&self, id: ConversationId, user_id: UserId) -> Result<Option<Conversation>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_opt(
                "SELECT * FROM conversations WHERE id = $1 AND user_id = $2",
                &[&id.to_string(), &user_id.to_string()],
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
        user_id: UserId,
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
            WHERE id = $1 AND user_id = $2
            RETURNING *
            "#,
                &[&id.to_string(), &user_id.to_string(), &metadata, &now],
            )
            .await
            .context("Failed to update conversation")?;

        match row {
            Some(row) => {
                debug!("Updated conversation: {} for user: {}", id, user_id);
                Ok(Some(self.row_to_conversation(row)?))
            }
            None => Ok(None),
        }
    }

    /// Delete a conversation (will cascade delete associated responses)
    async fn delete(&self, id: ConversationId, user_id: UserId) -> Result<bool> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let result = client
            .execute(
                "DELETE FROM conversations WHERE id = $1 AND user_id = $2",
                &[&id.to_string(), &user_id.to_string()],
            )
            .await
            .context("Failed to delete conversation")?;

        if result > 0 {
            debug!("Deleted conversation: {} for user: {}", id, user_id);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// List conversations for a user
    async fn list_by_user(
        &self,
        user_id: UserId,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Conversation>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
            SELECT * FROM conversations 
            WHERE user_id = $1
            ORDER BY updated_at DESC
            LIMIT $2 OFFSET $3
            "#,
                &[&user_id.to_string(), &limit, &offset],
            )
            .await
            .context("Failed to list conversations")?;

        rows.into_iter()
            .map(|row| self.row_to_conversation(row))
            .collect()
    }

    /// Count total conversations for a user
    async fn count_by_user(&self, user_id: UserId) -> Result<i64> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_one(
                "SELECT COUNT(*) FROM conversations WHERE user_id = $1",
                &[&user_id.to_string()],
            )
            .await
            .context("Failed to count conversations")?;

        Ok(row.get(0))
    }
}
