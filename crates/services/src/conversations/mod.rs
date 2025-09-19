pub mod ports;

use crate::{
    conversations::ports::*,
    responses::ports::{ConversationId, ResponseRepository, UserId},
};
use ports::ConversationError;
use std::sync::Arc;
use uuid::Uuid;

// Helper functions for ID parsing
pub fn parse_uuid(id: &str) -> Result<Uuid, ConversationError> {
    Uuid::parse_str(id)
        .map_err(|_| ConversationError::InvalidParams(format!("Invalid UUID: {}", id)))
}

pub fn parse_uuid_from_prefixed(id: &str, prefix: &str) -> Result<Uuid, ConversationError> {
    let uuid_str = id.strip_prefix(prefix).ok_or_else(|| {
        ConversationError::InvalidParams(format!(
            "Invalid {} ID format: {}",
            prefix.trim_end_matches('_'),
            id
        ))
    })?;

    Uuid::parse_str(uuid_str).map_err(|_| {
        ConversationError::InvalidParams(format!(
            "Invalid {} UUID: {}",
            prefix.trim_end_matches('_'),
            id
        ))
    })
}

impl ConversationService {
    pub fn new(
        conv_repo: Arc<dyn ports::ConversationRepository>,
        resp_repo: Arc<dyn ResponseRepository>,
    ) -> Self {
        Self {
            conv_repo,
            resp_repo,
        }
    }

    /// Create a new conversation
    pub async fn create_conversation(
        &self,
        request: ConversationRequest,
    ) -> Result<Conversation, ConversationError> {
        let metadata = request.metadata.unwrap_or_else(|| serde_json::json!({}));

        tracing::info!("Creating conversation for user: {}", request.user_id.0);

        let db_conversation = self
            .conv_repo
            .create(request.user_id, metadata)
            .await
            .map_err(|e| {
                ConversationError::InternalError(format!("Failed to create conversation: {}", e))
            })?;

        tracing::info!("Created conversation: {}", db_conversation.id);

        let conversation = Conversation {
            id: db_conversation.id,
            user_id: db_conversation.user_id,
            metadata: db_conversation.metadata,
            created_at: db_conversation.created_at,
            updated_at: db_conversation.updated_at,
        };

        Ok(conversation)
    }

    /// Get a conversation by ID
    pub async fn get_conversation(
        &self,
        conversation_id: &ConversationId,
        user_id: &UserId,
    ) -> Result<Option<Conversation>, ConversationError> {
        let db_conversation = self
            .conv_repo
            .get_by_id(conversation_id.clone(), user_id.clone())
            .await
            .map_err(|e| {
                ConversationError::InternalError(format!("Failed to get conversation: {}", e))
            })?;

        Ok(db_conversation.map(|c| Conversation {
            id: c.id,
            user_id: c.user_id,
            metadata: c.metadata,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }))
    }

    /// Update a conversation
    pub async fn update_conversation(
        &self,
        conversation_id: &ConversationId,
        user_id: &UserId,
        metadata: serde_json::Value,
    ) -> Result<Option<Conversation>, ConversationError> {
        let db_conversation = self
            .conv_repo
            .update(conversation_id.clone(), user_id.clone(), metadata)
            .await
            .map_err(|e| {
                ConversationError::InternalError(format!("Failed to update conversation: {}", e))
            })?;

        Ok(db_conversation.map(|c| Conversation {
            id: c.id,
            user_id: c.user_id,
            metadata: c.metadata,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }))
    }

    /// Delete a conversation
    pub async fn delete_conversation(
        &self,
        conversation_id: &ConversationId,
        user_id: &UserId,
    ) -> Result<bool, ConversationError> {
        self.conv_repo
            .delete(conversation_id.clone(), user_id.clone())
            .await
            .map_err(|e| {
                ConversationError::InternalError(format!("Failed to delete conversation: {}", e))
            })
    }

    /// List conversations for a user
    pub async fn list_conversations(
        &self,
        user_id: &UserId,
        limit: Option<i32>,
        offset: Option<i32>,
    ) -> Result<Vec<Conversation>, ConversationError> {
        let limit = limit.unwrap_or(20).min(100) as i64;
        let offset = offset.unwrap_or(0) as i64;

        tracing::info!("Listing conversations for user: {}", user_id.0);

        let db_conversations = self
            .conv_repo
            .list_by_user(user_id.clone(), limit, offset)
            .await
            .map_err(|e| {
                ConversationError::InternalError(format!("Failed to list conversations: {}", e))
            })?;

        tracing::info!("Found {} conversations", db_conversations.len());

        Ok(db_conversations)
    }

    /// Get conversation messages by extracting from responses
    pub async fn get_conversation_messages(
        &self,
        conversation_id: &ConversationId,
        user_id: &UserId,
        limit: Option<i32>,
    ) -> Result<Vec<ConversationMessage>, ConversationError> {
        let limit = limit.unwrap_or(50).min(100) as i64;

        // Get responses for this conversation
        let responses = self
            .resp_repo
            .list_by_conversation(conversation_id.clone(), user_id.clone(), limit)
            .await
            .map_err(|e| {
                ConversationError::InternalError(format!(
                    "Failed to get conversation messages: {}",
                    e
                ))
            })?;

        // Extract messages from responses with deduplication
        let mut messages = Vec::new();
        let mut seen_content = std::collections::HashSet::new();

        for response in responses {
            // Parse input_messages JSONB to extract individual messages
            if let Some(input_array) = response.input_messages.as_array() {
                for (index, msg_value) in input_array.iter().enumerate() {
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
                            messages.push(ConversationMessage {
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
                    messages.push(ConversationMessage {
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

// Re-export the service and types
pub use ports::*;
