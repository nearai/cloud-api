use crate::conversations;
use crate::workspace::WorkspaceId;
use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait ConversationRepository: Send + Sync {
    /// Create a new conversation
    async fn create(
        &self,
        workspace_id: WorkspaceId,
        api_key_id: uuid::Uuid,
        metadata: serde_json::Value,
    ) -> Result<conversations::models::Conversation>;

    /// Get a conversation by ID
    async fn get_by_id(
        &self,
        id: conversations::models::ConversationId,
        workspace_id: WorkspaceId,
    ) -> Result<Option<conversations::models::Conversation>>;

    /// Update a conversation's metadata
    async fn update(
        &self,
        id: conversations::models::ConversationId,
        workspace_id: WorkspaceId,
        metadata: serde_json::Value,
    ) -> Result<Option<conversations::models::Conversation>>;

    /// Delete a conversation (will cascade delete associated responses)
    async fn delete(
        &self,
        id: conversations::models::ConversationId,
        workspace_id: WorkspaceId,
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
        workspace_id: WorkspaceId,
    ) -> Result<Option<conversations::models::Conversation>, conversations::errors::ConversationError>;
    async fn update_conversation(
        &self,
        conversation_id: conversations::models::ConversationId,
        workspace_id: WorkspaceId,
        metadata: serde_json::Value,
    ) -> Result<Option<conversations::models::Conversation>, conversations::errors::ConversationError>;
    async fn delete_conversation(
        &self,
        conversation_id: conversations::models::ConversationId,
        workspace_id: WorkspaceId,
    ) -> Result<bool, conversations::errors::ConversationError>;
    async fn get_conversation_messages(
        &self,
        conversation_id: conversations::models::ConversationId,
        workspace_id: WorkspaceId,
        limit: i64,
        offset: i64,
    ) -> Result<
        Vec<conversations::models::ConversationMessage>,
        conversations::errors::ConversationError,
    >;
    async fn list_conversation_items(
        &self,
        conversation_id: conversations::models::ConversationId,
        workspace_id: WorkspaceId,
        after: Option<String>,
        limit: i64,
    ) -> Result<
        Vec<crate::responses::models::ResponseOutputItem>,
        conversations::errors::ConversationError,
    >;
}
