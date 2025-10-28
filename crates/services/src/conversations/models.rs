use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::UserId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationId(pub Uuid);

impl From<Uuid> for ConversationId {
    fn from(uuid: Uuid) -> Self {
        ConversationId(uuid)
    }
}

impl std::fmt::Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "conv_{}", self.0)
    }
}

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
    pub id: crate::responses::models::ResponseId,
    pub role: String,
    pub content: String,
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}
