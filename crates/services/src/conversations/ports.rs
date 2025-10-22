use crate::conversations;
use crate::UserId;
use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait ConversationRepository: Send + Sync {
    /// Create a new conversation
    async fn create(
        &self,
        user_id: UserId,
        metadata: serde_json::Value,
    ) -> Result<conversations::models::Conversation>;

    /// Get a conversation by ID
    async fn get_by_id(
        &self,
        id: conversations::models::ConversationId,
        user_id: UserId,
    ) -> Result<Option<conversations::models::Conversation>>;

    /// Update a conversation's metadata
    async fn update(
        &self,
        id: conversations::models::ConversationId,
        user_id: UserId,
        metadata: serde_json::Value,
    ) -> Result<Option<conversations::models::Conversation>>;

    /// Delete a conversation (will cascade delete associated responses)
    async fn delete(
        &self,
        id: conversations::models::ConversationId,
        user_id: UserId,
    ) -> Result<bool>;
}

#[async_trait]
pub trait ConversationServiceTrait: Send + Sync {
    async fn create_conversation(
        &self,
        request: conversations::models::ConversationRequest,
    ) -> Result<conversations::models::Conversation, conversations::errors::ConversationError>;
    async fn get_conversation(
        &self,
        conversation_id: conversations::models::ConversationId,
        user_id: UserId,
    ) -> Result<Option<conversations::models::Conversation>, conversations::errors::ConversationError>;
    async fn update_conversation(
        &self,
        conversation_id: conversations::models::ConversationId,
        user_id: UserId,
        metadata: serde_json::Value,
    ) -> Result<Option<conversations::models::Conversation>, conversations::errors::ConversationError>;
    async fn delete_conversation(
        &self,
        conversation_id: conversations::models::ConversationId,
        user_id: UserId,
    ) -> Result<bool, conversations::errors::ConversationError>;
    async fn get_conversation_messages(
        &self,
        conversation_id: conversations::models::ConversationId,
        user_id: UserId,
        limit: i64,
    ) -> Result<
        Vec<conversations::models::ConversationMessage>,
        conversations::errors::ConversationError,
    >;
}
