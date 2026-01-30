use crate::responses::{errors, models};
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

use crate::conversations::models::ConversationId;
use crate::workspace::WorkspaceId;
use crate::UserId;

#[async_trait]
pub trait ResponseRepositoryTrait: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn create(
        &self,
        workspace_id: WorkspaceId,
        api_key_id: uuid::Uuid,
        request: models::CreateResponseRequest,
    ) -> anyhow::Result<models::ResponseObject>;

    async fn get_by_id(
        &self,
        id: models::ResponseId,
        workspace_id: WorkspaceId,
    ) -> anyhow::Result<Option<models::ResponseObject>>;

    async fn update(
        &self,
        id: models::ResponseId,
        workspace_id: WorkspaceId,
        output_message: Option<String>,
        status: models::ResponseStatus,
        usage: Option<serde_json::Value>,
    ) -> anyhow::Result<Option<models::ResponseObject>>;

    async fn delete(
        &self,
        id: models::ResponseId,
        workspace_id: WorkspaceId,
    ) -> anyhow::Result<bool>;

    async fn cancel(
        &self,
        id: models::ResponseId,
        workspace_id: WorkspaceId,
    ) -> anyhow::Result<Option<models::ResponseObject>>;

    async fn list_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<models::ResponseObject>>;

    async fn list_by_conversation(
        &self,
        conversation_id: ConversationId,
        workspace_id: WorkspaceId,
        limit: i64,
    ) -> anyhow::Result<Vec<models::ResponseObject>>;

    async fn get_previous(
        &self,
        response_id: models::ResponseId,
        workspace_id: WorkspaceId,
    ) -> anyhow::Result<Option<models::ResponseObject>>;

    async fn get_latest_in_conversation(
        &self,
        conversation_id: ConversationId,
        workspace_id: WorkspaceId,
    ) -> anyhow::Result<Option<models::ResponseObject>>;

    /// Ensure the structural "root_response" exists for a conversation and return its ID.
    ///
    /// This is used to support first-turn parallel responses (multiple models starting from
    /// the same parent) without racing on implicit "latest response" selection.
    async fn ensure_root_response(
        &self,
        conversation_id: ConversationId,
        workspace_id: WorkspaceId,
        api_key_id: uuid::Uuid,
    ) -> anyhow::Result<String>;
}

#[async_trait]
pub trait ResponseItemRepositoryTrait: Send + Sync {
    async fn create(
        &self,
        response_id: models::ResponseId,
        api_key_id: uuid::Uuid,
        conversation_id: Option<ConversationId>,
        item: models::ResponseOutputItem,
    ) -> anyhow::Result<models::ResponseOutputItem>;
    async fn get_by_id(
        &self,
        id: models::ResponseItemId,
    ) -> anyhow::Result<Option<models::ResponseOutputItem>>;
    async fn update(
        &self,
        id: models::ResponseItemId,
        item: models::ResponseOutputItem,
    ) -> anyhow::Result<models::ResponseOutputItem>;
    async fn delete(&self, id: models::ResponseItemId) -> anyhow::Result<bool>;
    async fn list_by_response(
        &self,
        response_id: models::ResponseId,
    ) -> anyhow::Result<Vec<models::ResponseOutputItem>>;
    async fn list_by_api_key(
        &self,
        api_key_id: uuid::Uuid,
    ) -> anyhow::Result<Vec<models::ResponseOutputItem>>;
    async fn list_by_conversation(
        &self,
        conversation_id: ConversationId,
        after: Option<String>,
        limit: i64,
    ) -> anyhow::Result<Vec<models::ResponseOutputItem>>;
}

#[allow(clippy::too_many_arguments)]
#[async_trait]
pub trait ResponseServiceTrait: Send + Sync {
    async fn create_response_stream(
        &self,
        request: models::CreateResponseRequest,
        user_id: UserId,
        api_key_id: String,
        organization_id: uuid::Uuid,
        workspace_id: uuid::Uuid,
        body_hash: String,
        signing_algo: Option<String>,
        client_pub_key: Option<String>,
        model_pub_key: Option<String>,
    ) -> Result<
        Pin<Box<dyn Stream<Item = models::ResponseStreamEvent> + Send>>,
        errors::ResponseError,
    >;
}
