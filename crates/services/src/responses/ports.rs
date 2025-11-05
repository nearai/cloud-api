use crate::responses::{errors, models};
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

use crate::conversations::models::ConversationId;
use crate::UserId;

#[async_trait]
pub trait ResponseRepositoryTrait: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn create(
        &self,
        user_id: UserId,
        request: models::CreateResponseRequest,
    ) -> anyhow::Result<models::ResponseObject>;

    async fn get_by_id(
        &self,
        id: models::ResponseId,
        user_id: UserId,
    ) -> anyhow::Result<Option<models::ResponseObject>>;

    async fn update(
        &self,
        id: models::ResponseId,
        user_id: UserId,
        output_message: Option<String>,
        status: models::ResponseStatus,
        usage: Option<serde_json::Value>,
    ) -> anyhow::Result<Option<models::ResponseObject>>;

    async fn delete(&self, id: models::ResponseId, user_id: UserId) -> anyhow::Result<bool>;

    async fn cancel(
        &self,
        id: models::ResponseId,
        user_id: UserId,
    ) -> anyhow::Result<Option<models::ResponseObject>>;

    async fn list_by_user(
        &self,
        user_id: UserId,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<models::ResponseObject>>;

    async fn list_by_conversation(
        &self,
        conversation_id: ConversationId,
        user_id: UserId,
        limit: i64,
    ) -> anyhow::Result<Vec<models::ResponseObject>>;

    async fn get_previous(
        &self,
        response_id: models::ResponseId,
        user_id: UserId,
    ) -> anyhow::Result<Option<models::ResponseObject>>;
}

#[async_trait]
pub trait ResponseItemRepositoryTrait: Send + Sync {
    async fn create(
        &self,
        response_id: models::ResponseId,
        user_id: UserId,
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
    async fn list_by_user(
        &self,
        user_id: UserId,
    ) -> anyhow::Result<Vec<models::ResponseOutputItem>>;
    async fn list_by_conversation(
        &self,
        conversation_id: ConversationId,
    ) -> anyhow::Result<Vec<models::ResponseOutputItem>>;
}

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
    ) -> Result<
        Pin<Box<dyn Stream<Item = models::ResponseStreamEvent> + Send>>,
        errors::ResponseError,
    >;
}
