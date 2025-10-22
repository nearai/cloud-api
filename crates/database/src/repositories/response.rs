use crate::pool::DbPool;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use services::{responses::ports::*, UserId};
use tracing::debug;
use uuid::Uuid;

pub struct PgResponseRepository {
    pool: DbPool,
}

impl PgResponseRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    // Helper method to convert database row to domain model
    fn row_to_response(&self, row: tokio_postgres::Row) -> Result<Response> {
        let status_str: String = row.try_get("status")?;
        let status = match status_str.as_str() {
            "in_progress" => ResponseStatus::InProgress,
            "completed" => ResponseStatus::Completed,
            "failed" => ResponseStatus::Failed,
            "cancelled" => ResponseStatus::Cancelled,
            _ => bail!("Unknown response status: {status_str}"),
        };

        let id: Uuid = row.try_get("id")?;
        let user_id: Uuid = row.try_get("user_id")?;
        let conversation_id: Option<Uuid> = row.try_get("conversation_id")?;
        let previous_response_id: Option<Uuid> = row.try_get("previous_response_id")?;

        Ok(Response {
            id: id.into(),
            user_id: user_id.into(),
            model: row.try_get("model")?,
            input_messages: row.try_get("input_messages")?,
            output_message: row.try_get("output_message")?,
            status,
            instructions: row.try_get("instructions")?,
            conversation_id: conversation_id.map(|id| id.into()),
            previous_response_id: previous_response_id.map(|id| id.into()),
            usage: row.try_get("usage")?,
            metadata: row.try_get("metadata")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

#[async_trait]
impl ResponseRepository for PgResponseRepository {
    /// Create a new response
    async fn create(
        &self,
        user_id: UserId,
        model: String,
        input_messages: serde_json::Value,
        instructions: Option<String>,
        conversation_id: Option<ConversationId>,
        previous_response_id: Option<ResponseId>,
        metadata: Option<serde_json::Value>,
    ) -> Result<Response> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let id = Uuid::new_v4();
        let now = Utc::now();
        let status = ResponseStatus::InProgress;

        let row = client
            .query_one(
                r#"
            INSERT INTO responses (
                id, user_id, model, input_messages, output_message, 
                status, instructions, conversation_id, previous_response_id,
                usage, metadata, created_at, updated_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
            RETURNING *
            "#,
                &[
                    &id,
                    &user_id.0, // Store UUID directly
                    &model,
                    &input_messages,
                    &None::<String>,
                    &status.to_string(),
                    &instructions,
                    &conversation_id.map(|id| id.0), // Store UUID directly
                    &previous_response_id.map(|id| id.0), // Store UUID directly
                    &None::<serde_json::Value>,
                    &metadata,
                    &now,
                    &now,
                ],
            )
            .await
            .context("Failed to create response")?;

        debug!("Created response: {} for user: {}", id, user_id);
        self.row_to_response(row)
    }

    /// Update a response (for completion, cancellation, or failure)
    async fn update(
        &self,
        id: ResponseId,
        user_id: UserId,
        output_message: Option<String>,
        status: ResponseStatus,
        usage: Option<serde_json::Value>,
    ) -> Result<Option<Response>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let now = Utc::now();

        let row = client
            .query_opt(
                r#"
            UPDATE responses 
            SET output_message = $3, status = $4, usage = $5, updated_at = $6
            WHERE id = $1 AND user_id = $2
            RETURNING *
            "#,
                &[
                    &id.0,      // Store UUID directly
                    &user_id.0, // Store UUID directly
                    &output_message,
                    &status.to_string(),
                    &usage,
                    &now,
                ],
            )
            .await
            .context("Failed to update response")?;

        match row {
            Some(row) => {
                debug!("Updated response: {} to status: {}", id, status);
                Ok(Some(self.row_to_response(row)?))
            }
            None => Ok(None),
        }
    }

    /// Get a response by ID
    async fn get_by_id(&self, id: ResponseId, user_id: UserId) -> Result<Option<Response>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_opt(
                "SELECT * FROM responses WHERE id = $1 AND user_id = $2",
                &[&id.0, &user_id.0],
            )
            .await
            .context("Failed to query response")?;

        match row {
            Some(row) => Ok(Some(self.row_to_response(row)?)),
            None => Ok(None),
        }
    }

    /// Delete a response
    async fn delete(&self, id: ResponseId, user_id: UserId) -> Result<bool> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let result = client
            .execute(
                "DELETE FROM responses WHERE id = $1 AND user_id = $2",
                &[&id.0, &user_id.0],
            )
            .await
            .context("Failed to delete response")?;

        if result > 0 {
            debug!("Deleted response: {} for user: {}", id, user_id);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Cancel a response (set status to cancelled)
    async fn cancel(&self, id: ResponseId, user_id: UserId) -> Result<Option<Response>> {
        self.update(id, user_id, None, ResponseStatus::Cancelled, None)
            .await
    }

    /// List responses for a user
    async fn list_by_user(
        &self,
        user_id: UserId,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Response>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
            SELECT * FROM responses 
            WHERE user_id = $1
            ORDER BY created_at DESC
            LIMIT $2 OFFSET $3
            "#,
                &[&user_id.0, &limit, &offset],
            )
            .await
            .context("Failed to list responses")?;

        rows.into_iter()
            .map(|row| self.row_to_response(row))
            .collect()
    }

    /// List responses for a conversation
    async fn list_by_conversation(
        &self,
        conversation_id: ConversationId,
        user_id: UserId,
        limit: i64,
    ) -> Result<Vec<Response>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
            SELECT * FROM responses 
            WHERE conversation_id = $1 AND user_id = $2
            ORDER BY created_at DESC
            LIMIT $3
            "#,
                &[&conversation_id.0, &user_id.0, &limit],
            )
            .await
            .context("Failed to list responses by conversation")?;

        rows.into_iter()
            .map(|row| self.row_to_response(row))
            .collect()
    }

    /// Get the previous response in a chain
    async fn get_previous(
        &self,
        response_id: ResponseId,
        user_id: UserId,
    ) -> Result<Option<Response>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        // First get the current response to find its previous_response_id
        let current = client
            .query_opt(
                "SELECT previous_response_id FROM responses WHERE id = $1 AND user_id = $2",
                &[&response_id.0, &user_id.0],
            )
            .await
            .context("Failed to query current response")?;

        if let Some(current_row) = current {
            if let Ok(Some(prev_id)) =
                current_row.try_get::<_, Option<Uuid>>("previous_response_id")
            {
                return self.get_by_id(prev_id.into(), user_id).await;
            }
        }

        Ok(None)
    }
}
