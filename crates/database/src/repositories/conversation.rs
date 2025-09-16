use crate::models::Conversation;
use crate::pool::DbPool;
use anyhow::{Result, Context};
use uuid::Uuid;
use chrono::Utc;
use tracing::debug;

pub struct ConversationRepository {
    pool: DbPool,
}

impl ConversationRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Create a new conversation
    pub async fn create(&self, user_id: Uuid, metadata: serde_json::Value) -> Result<Conversation> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let id = Uuid::new_v4();
        let now = Utc::now();
        
        let row = client.query_one(
            r#"
            INSERT INTO conversations (id, user_id, metadata, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5)
            RETURNING *
            "#,
            &[&id, &user_id, &metadata, &now, &now],
        ).await.context("Failed to create conversation")?;
        
        debug!("Created conversation: {} for user: {}", id, user_id);
        self.row_to_conversation(row)
    }

    /// Get a conversation by ID
    pub async fn get_by_id(&self, id: Uuid, user_id: Uuid) -> Result<Option<Conversation>> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let row = client.query_opt(
            "SELECT * FROM conversations WHERE id = $1 AND user_id = $2",
            &[&id, &user_id],
        ).await.context("Failed to query conversation")?;
        
        match row {
            Some(row) => Ok(Some(self.row_to_conversation(row)?)),
            None => Ok(None),
        }
    }

    /// Update a conversation's metadata
    pub async fn update(&self, id: Uuid, user_id: Uuid, metadata: serde_json::Value) -> Result<Option<Conversation>> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let now = Utc::now();
        
        let row = client.query_opt(
            r#"
            UPDATE conversations 
            SET metadata = $3, updated_at = $4
            WHERE id = $1 AND user_id = $2
            RETURNING *
            "#,
            &[&id, &user_id, &metadata, &now],
        ).await.context("Failed to update conversation")?;
        
        match row {
            Some(row) => {
                debug!("Updated conversation: {} for user: {}", id, user_id);
                Ok(Some(self.row_to_conversation(row)?))
            }
            None => Ok(None),
        }
    }

    /// Delete a conversation (will cascade delete associated responses)
    pub async fn delete(&self, id: Uuid, user_id: Uuid) -> Result<bool> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let result = client.execute(
            "DELETE FROM conversations WHERE id = $1 AND user_id = $2",
            &[&id, &user_id],
        ).await.context("Failed to delete conversation")?;
        
        if result > 0 {
            debug!("Deleted conversation: {} for user: {}", id, user_id);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// List conversations for a user
    pub async fn list_by_user(&self, user_id: Uuid, limit: i64, offset: i64) -> Result<Vec<Conversation>> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let rows = client.query(
            r#"
            SELECT * FROM conversations 
            WHERE user_id = $1
            ORDER BY updated_at DESC
            LIMIT $2 OFFSET $3
            "#,
            &[&user_id, &limit, &offset],
        ).await.context("Failed to list conversations")?;
        
        rows.into_iter()
            .map(|row| self.row_to_conversation(row))
            .collect()
    }

    /// Count total conversations for a user
    pub async fn count_by_user(&self, user_id: Uuid) -> Result<i64> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let row = client.query_one(
            "SELECT COUNT(*) FROM conversations WHERE user_id = $1",
            &[&user_id],
        ).await.context("Failed to count conversations")?;
        
        Ok(row.get(0))
    }

    // Helper method to convert database row to Conversation model
    fn row_to_conversation(&self, row: tokio_postgres::Row) -> Result<Conversation> {
        Ok(Conversation {
            id: row.try_get("id")?,
            user_id: row.try_get("user_id")?,
            metadata: row.try_get("metadata")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}