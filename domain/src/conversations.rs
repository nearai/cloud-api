use crate::errors::CompletionError;
use crate::models::{UserId, ConversationId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::{DateTime, Utc};
use std::sync::Arc;

// Helper functions for ID parsing
fn parse_uuid(id: &str) -> Result<Uuid, CompletionError> {
    Uuid::parse_str(id)
        .map_err(|_| CompletionError::InvalidParams(format!("Invalid UUID: {}", id)))
}

fn parse_uuid_from_prefixed(id: &str, prefix: &str) -> Result<Uuid, CompletionError> {
    let uuid_str = id.strip_prefix(prefix)
        .ok_or_else(|| CompletionError::InvalidParams(format!("Invalid {} ID format: {}", prefix.trim_end_matches('_'), id)))?;
    
    Uuid::parse_str(uuid_str)
        .map_err(|_| CompletionError::InvalidParams(format!("Invalid {} UUID: {}", prefix.trim_end_matches('_'), id)))
}

/// Domain model for a conversation request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationRequest {
    pub user_id: String,
    pub metadata: Option<serde_json::Value>,
}

/// Domain model for a stored conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: ConversationId,
    pub user_id: UserId,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Domain model for a conversation message (extracted from responses)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub id: String, // Keep as String since it's a composite ID like "msg_{response_id}_{index}"
    pub role: String,
    pub content: String,
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

/// Conversation service for managing conversations
pub struct ConversationService;

impl ConversationService {
    pub fn new() -> Self {
        Self
    }

    /// Create a new conversation (mock implementation)
    pub async fn create_conversation(&self, request: ConversationRequest) -> Result<Conversation, CompletionError> {
        let user_uuid = parse_uuid(&request.user_id)?;
        let now = Utc::now();
        let conversation = Conversation {
            id: Uuid::new_v4().into(),
            user_id: user_uuid.into(),
            metadata: request.metadata.unwrap_or_else(|| serde_json::json!({})),
            created_at: now,
            updated_at: now,
        };
        Ok(conversation)
    }

    /// Get a conversation by ID (mock implementation)
    pub async fn get_conversation(&self, _conversation_id: &str, _user_id: &str) -> Result<Option<Conversation>, CompletionError> {
        // Mock implementation - returns None (not found)
        Ok(None)
    }

    /// Update a conversation (mock implementation)
    pub async fn update_conversation(&self, _conversation_id: &str, user_id: &str, metadata: serde_json::Value) -> Result<Option<Conversation>, CompletionError> {
        // Mock implementation - returns a new conversation
        let user_uuid = parse_uuid(user_id)?;
        let now = Utc::now();
        let conversation = Conversation {
            id: Uuid::new_v4().into(),
            user_id: user_uuid.into(),
            metadata,
            created_at: now,
            updated_at: now,
        };
        Ok(Some(conversation))
    }

    /// Delete a conversation (mock implementation)
    pub async fn delete_conversation(&self, _conversation_id: &str, _user_id: &str) -> Result<bool, CompletionError> {
        // Mock implementation - always succeeds
        Ok(true)
    }

    /// List conversations for a user (mock implementation)
    pub async fn list_conversations(&self, _user_id: &str, _limit: Option<i32>, _offset: Option<i32>) -> Result<Vec<Conversation>, CompletionError> {
        // Mock implementation - returns empty list
        Ok(vec![])
    }

    /// Get conversation messages by extracting from responses (mock implementation)
    pub async fn get_conversation_messages(&self, _conversation_id: &str, _user_id: &str, _limit: Option<i32>) -> Result<Vec<ConversationMessage>, CompletionError> {
        // Mock implementation - returns empty list
        Ok(vec![])
    }
}