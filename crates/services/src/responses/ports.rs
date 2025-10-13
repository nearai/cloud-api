use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::UserId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseId(pub Uuid);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationId(pub Uuid);

impl From<Uuid> for ResponseId {
    fn from(uuid: Uuid) -> Self {
        ResponseId(uuid)
    }
}

impl From<Uuid> for ConversationId {
    fn from(uuid: Uuid) -> Self {
        ConversationId(uuid)
    }
}

impl std::fmt::Display for ResponseId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "resp_{}", self.0)
    }
}

impl std::fmt::Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "conv_{}", self.0)
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub id: ResponseId,
    pub user_id: UserId,
    pub model: String,
    pub input_messages: serde_json::Value, // JSONB storing input messages
    pub output_message: Option<String>,
    pub status: ResponseStatus,
    pub instructions: Option<String>,
    pub conversation_id: Option<ConversationId>,
    pub previous_response_id: Option<ResponseId>,
    pub usage: Option<serde_json::Value>, // JSONB storing token usage
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Response status enum
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseStatus {
    InProgress,
    Completed,
    Failed,
    Cancelled,
}

impl std::fmt::Display for ResponseStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResponseStatus::InProgress => write!(f, "in_progress"),
            ResponseStatus::Completed => write!(f, "completed"),
            ResponseStatus::Failed => write!(f, "failed"),
            ResponseStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResponseError {
    #[error("Internal error: {0}")]
    InternalError(String),
    #[error("Invalid parameters: {0}")]
    InvalidParams(String),
}

/// Domain model for a response request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseRequest {
    pub model: String,
    pub input: Option<ResponseInput>,
    pub instructions: Option<String>,
    pub conversation_id: Option<ConversationId>,
    pub previous_response_id: Option<ResponseId>,
    pub max_output_tokens: Option<i64>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub user_id: UserId,
    pub metadata: Option<serde_json::Value>,
}

/// Input for a response - can be text or messages
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseInput {
    Text(String),
    Messages(Vec<ResponseMessage>),
}

/// A message in response input
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseMessage {
    pub role: String,
    pub content: String,
}

/// API Spec-compliant streaming event for response API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseStreamEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<serde_json::Value>, // Full response object
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<serde_json::Value>, // ResponseOutputItem
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part: Option<serde_json::Value>, // ResponseOutputContent
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[async_trait]
pub trait ResponseRepository: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn create(
        &self,
        user_id: UserId,
        model: String,
        input_messages: serde_json::Value,
        instructions: Option<String>,
        conversation_id: Option<ConversationId>,
        previous_response_id: Option<ResponseId>,
        metadata: Option<serde_json::Value>,
    ) -> anyhow::Result<Response>;

    async fn update(
        &self,
        id: ResponseId,
        user_id: UserId,
        output_message: Option<String>,
        status: ResponseStatus,
        usage: Option<serde_json::Value>,
    ) -> anyhow::Result<Option<Response>>;

    async fn get_by_id(&self, id: ResponseId, user_id: UserId) -> anyhow::Result<Option<Response>>;

    async fn delete(&self, id: ResponseId, user_id: UserId) -> anyhow::Result<bool>;

    async fn cancel(&self, id: ResponseId, user_id: UserId) -> anyhow::Result<Option<Response>>;

    async fn list_by_user(
        &self,
        user_id: UserId,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<Response>>;

    async fn list_by_conversation(
        &self,
        conversation_id: ConversationId,
        user_id: UserId,
        limit: i64,
    ) -> anyhow::Result<Vec<Response>>;

    async fn get_previous(
        &self,
        response_id: ResponseId,
        user_id: UserId,
    ) -> anyhow::Result<Option<Response>>;
}
