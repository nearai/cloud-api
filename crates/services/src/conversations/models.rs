use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{conversations::errors, workspace::WorkspaceId};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ConversationId(pub Uuid);

impl std::str::FromStr for ConversationId {
    type Err = errors::ConversationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.strip_prefix("conv_").unwrap_or(value);
        Uuid::parse_str(value).map(ConversationId).map_err(|e| {
            errors::ConversationError::InvalidParams(format!(
                "Invalid conversation ID: {value}, error: {e}"
            ))
        })
    }
}

impl From<Uuid> for ConversationId {
    fn from(uuid: Uuid) -> Self {
        ConversationId(uuid)
    }
}

impl std::fmt::Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "conv_{}", self.0.simple())
    }
}

/// Domain model for a conversation request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationRequest {
    pub workspace_id: WorkspaceId,
    pub api_key_id: uuid::Uuid,
    pub metadata: Option<serde_json::Value>,
}

/// Conversation model - stores conversation metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: ConversationId,
    pub workspace_id: WorkspaceId,
    pub api_key_id: uuid::Uuid,
    pub pinned_at: Option<DateTime<Utc>>, // Timestamp when pinned, NULL if not pinned
    pub archived_at: Option<DateTime<Utc>>, // Timestamp when archived, NULL if not archived
    pub deleted_at: Option<DateTime<Utc>>, // Timestamp when soft-deleted, NULL if not deleted
    pub cloned_from_id: Option<ConversationId>, // ID of conversation this was cloned from
    pub metadata: serde_json::Value, // JSONB storing conversation metadata (includes title/name)
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Domain model for a conversation message (extracted from responses)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub id: crate::responses::models::ResponseId,
    pub role: String,
    pub content: String,
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

/// Conversation item
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ConversationItem {
    #[serde(rename = "message")]
    Message {
        #[serde(rename = "public_id")]
        id: String,
        status: crate::responses::models::ResponseItemStatus,
        role: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
    #[serde(rename = "web_search_call")]
    WebSearchCall {
        id: String,
        status: crate::responses::models::ResponseItemStatus,
        action: crate::responses::models::WebSearchAction,
    },
}
