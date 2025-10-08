use crate::{
    responses::ports::{ConversationId, ResponseId, ResponseRepository},
    UserId,
};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Domain model for a conversation request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationRequest {
    pub user_id: UserId,
    pub metadata: Option<serde_json::Value>,
}

/// Conversation model - stores conversation metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: ConversationId,
    pub user_id: UserId,
    pub metadata: serde_json::Value, // JSONB storing conversation metadata
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Domain model for a conversation message (extracted from responses)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub id: ResponseId,
    pub role: String,
    pub content: String,
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

/// Conversation service for managing conversations
pub struct ConversationService {
    pub conv_repo: Arc<dyn ConversationRepository>,
    pub resp_repo: Arc<dyn ResponseRepository>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConversationError {
    #[error("Internal error: {0}")]
    InternalError(String),
    #[error("Invalid parameters: {0}")]
    InvalidParams(String),
}

#[async_trait]
pub trait ConversationRepository: Send + Sync {
    /// Create a new conversation
    async fn create(&self, user_id: UserId, metadata: serde_json::Value) -> Result<Conversation>;

    /// Get a conversation by ID
    async fn get_by_id(&self, id: ConversationId, user_id: UserId) -> Result<Option<Conversation>>;

    /// Update a conversation's metadata
    async fn update(
        &self,
        id: ConversationId,
        user_id: UserId,
        metadata: serde_json::Value,
    ) -> Result<Option<Conversation>>;

    /// Delete a conversation (will cascade delete associated responses)
    async fn delete(&self, id: ConversationId, user_id: UserId) -> Result<bool>;
}
