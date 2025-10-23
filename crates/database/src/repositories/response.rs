use crate::pool::DbPool;
use async_trait::async_trait;
use services::responses::models::*;
use services::{responses::ports::*, UserId};

pub struct PgResponseRepository {
    pool: DbPool,
}

impl PgResponseRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

// TODO: Implement ResponseRepositoryTrait methods properly
#[async_trait]
impl ResponseRepositoryTrait for PgResponseRepository {
    async fn create(
        &self,
        _user_id: UserId,
        _request: CreateResponseRequest,
    ) -> Result<ResponseObject, anyhow::Error> {
        unimplemented!("create not yet implemented")
    }

    async fn get_by_id(
        &self,
        _response_id: ResponseId,
        _user_id: UserId,
    ) -> Result<Option<ResponseObject>, anyhow::Error> {
        unimplemented!("get_by_id not yet implemented")
    }

    async fn update(
        &self,
        _response_id: ResponseId,
        _user_id: UserId,
        _output_message: Option<String>,
        _status: ResponseStatus,
        _usage: Option<serde_json::Value>,
    ) -> Result<Option<ResponseObject>, anyhow::Error> {
        unimplemented!("update not yet implemented")
    }

    async fn delete(
        &self,
        _response_id: ResponseId,
        _user_id: UserId,
    ) -> Result<bool, anyhow::Error> {
        unimplemented!("delete not yet implemented")
    }

    async fn cancel(
        &self,
        _response_id: ResponseId,
        _user_id: UserId,
    ) -> Result<Option<ResponseObject>, anyhow::Error> {
        unimplemented!("cancel not yet implemented")
    }

    async fn list_by_user(
        &self,
        _user_id: UserId,
        _limit: i64,
        _offset: i64,
    ) -> Result<Vec<ResponseObject>, anyhow::Error> {
        unimplemented!("list_by_user not yet implemented")
    }

    async fn list_by_conversation(
        &self,
        _conversation_id: services::conversations::models::ConversationId,
        _user_id: UserId,
        _limit: i64,
    ) -> Result<Vec<ResponseObject>, anyhow::Error> {
        unimplemented!("list_by_conversation not yet implemented")
    }

    async fn get_previous(
        &self,
        _response_id: ResponseId,
        _user_id: UserId,
    ) -> Result<Option<ResponseObject>, anyhow::Error> {
        unimplemented!("get_previous not yet implemented")
    }
}
