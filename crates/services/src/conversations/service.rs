use crate::{
    conversations::{errors, models},
    responses::ports::ResponseRepositoryTrait,
    UserId,
};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use uuid::Uuid;

use crate::conversations::ports;

// Helper functions for ID parsing
pub fn parse_uuid(id: &str) -> Result<Uuid, errors::ConversationError> {
    Uuid::parse_str(id)
        .map_err(|_| errors::ConversationError::InvalidParams(format!("Invalid UUID: {id}")))
}

pub fn parse_uuid_from_prefixed(id: &str, prefix: &str) -> Result<Uuid, errors::ConversationError> {
    let uuid_str = id.strip_prefix(prefix).ok_or_else(|| {
        errors::ConversationError::InvalidParams(format!(
            "Invalid {} ID format: {}",
            prefix.trim_end_matches('_'),
            id
        ))
    })?;

    Uuid::parse_str(uuid_str).map_err(|_| {
        errors::ConversationError::InvalidParams(format!(
            "Invalid {} UUID: {}",
            prefix.trim_end_matches('_'),
            id
        ))
    })
}

/// Conversation service for managing conversations
pub struct ConversationServiceImpl {
    pub conv_repo: Arc<dyn ports::ConversationRepository>,
    pub resp_repo: Arc<dyn ResponseRepositoryTrait>,
}

impl ConversationServiceImpl {
    pub fn new(
        conv_repo: Arc<dyn ports::ConversationRepository>,
        resp_repo: Arc<dyn ResponseRepositoryTrait>,
    ) -> Self {
        Self {
            conv_repo,
            resp_repo,
        }
    }
}

#[async_trait]
impl ports::ConversationServiceTrait for ConversationServiceImpl {
    /// Create a new conversation
    async fn create_conversation(
        &self,
        request: models::ConversationRequest,
    ) -> Result<models::Conversation, errors::ConversationError> {
        let metadata = request.metadata.unwrap_or_else(|| serde_json::json!({}));

        tracing::info!("Creating conversation for user: {}", request.user_id.0);

        let db_conversation = self
            .conv_repo
            .create(request.user_id, metadata)
            .await
            .map_err(|e| {
                errors::ConversationError::InternalError(format!(
                    "Failed to create conversation: {e}"
                ))
            })?;

        tracing::info!("Created conversation: {}", db_conversation.id);

        let conversation = models::Conversation {
            id: db_conversation.id,
            user_id: db_conversation.user_id,
            metadata: db_conversation.metadata,
            created_at: db_conversation.created_at,
            updated_at: db_conversation.updated_at,
        };

        Ok(conversation)
    }

    /// Get a conversation by ID
    async fn get_conversation(
        &self,
        conversation_id: models::ConversationId,
        user_id: UserId,
    ) -> Result<Option<models::Conversation>, errors::ConversationError> {
        let db_conversation = self
            .conv_repo
            .get_by_id(conversation_id.into(), user_id)
            .await
            .map_err(|e| {
                errors::ConversationError::InternalError(format!("Failed to get conversation: {e}"))
            })?;

        Ok(db_conversation.map(|c| models::Conversation {
            id: c.id,
            user_id: c.user_id,
            metadata: c.metadata,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }))
    }

    /// Update a conversation
    async fn update_conversation(
        &self,
        conversation_id: models::ConversationId,
        user_id: UserId,
        metadata: serde_json::Value,
    ) -> Result<Option<models::Conversation>, errors::ConversationError> {
        let db_conversation = self
            .conv_repo
            .update(conversation_id, user_id, metadata)
            .await
            .map_err(|e| {
                errors::ConversationError::InternalError(format!(
                    "Failed to update conversation: {e}"
                ))
            })?;

        Ok(db_conversation.map(|c| models::Conversation {
            id: c.id,
            user_id: c.user_id,
            metadata: c.metadata,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }))
    }

    /// Delete a conversation
    async fn delete_conversation(
        &self,
        conversation_id: models::ConversationId,
        user_id: UserId,
    ) -> Result<bool, errors::ConversationError> {
        self.conv_repo
            .delete(conversation_id, user_id)
            .await
            .map_err(|e| {
                errors::ConversationError::InternalError(format!(
                    "Failed to delete conversation: {e}"
                ))
            })
    }

    /// Get conversation messages by extracting from responses
    async fn get_conversation_messages(
        &self,
        conversation_id: models::ConversationId,
        user_id: UserId,
        limit: i64,
    ) -> Result<Vec<models::ConversationMessage>, errors::ConversationError> {
        // Get responses for this conversation
        let responses = self
            .resp_repo
            .list_by_conversation(conversation_id, user_id, limit)
            .await
            .map_err(|e| {
                errors::ConversationError::InternalError(format!(
                    "Failed to get conversation messages: {e}"
                ))
            })?;

        // Extract messages from responses with deduplication
        let mut messages = Vec::new();
        let mut seen_content = std::collections::HashSet::new();

        for response in responses {
            // Parse input_messages JSONB to extract individual messages
            if let Some(input_array) = response.input_messages.as_array() {
                for msg_value in input_array.iter() {
                    if let Some(msg_obj) = msg_value.as_object() {
                        let role = msg_obj
                            .get("role")
                            .and_then(|r| r.as_str())
                            .unwrap_or("user")
                            .to_string();

                        let content = msg_obj
                            .get("content")
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_string();

                        let metadata = msg_obj.get("metadata").cloned();

                        // Create a deduplication key based on role + content + rough timestamp
                        let dedup_key = format!(
                            "{}:{}:{}",
                            role,
                            content,
                            response.created_at.timestamp() / 60
                        ); // Group by minute

                        // Only add if we haven't seen this content recently
                        if !seen_content.contains(&dedup_key) {
                            seen_content.insert(dedup_key);
                            messages.push(models::ConversationMessage {
                                id: response.id.clone(),
                                role,
                                content,
                                metadata,
                                created_at: response.created_at,
                            });
                        }
                    }
                }
            }

            // Add the output message if present (these are usually unique)
            if let Some(output) = response.output_message {
                let dedup_key = format!(
                    "assistant:{}:{}",
                    output,
                    response.updated_at.timestamp() / 60
                );

                if !seen_content.contains(&dedup_key) {
                    seen_content.insert(dedup_key);
                    messages.push(models::ConversationMessage {
                        id: response.id.clone(),
                        role: "assistant".to_string(),
                        content: output,
                        metadata: None,
                        created_at: response.updated_at,
                    });
                }
            }
        }

        // Sort by creation time to maintain conversation flow
        messages.sort_by(|a, b| a.created_at.cmp(&b.created_at));

        Ok(messages)
    }
}
