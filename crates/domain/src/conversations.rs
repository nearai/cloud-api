use crate::errors::CompletionError;
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
    pub id: String,
    pub user_id: String,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Domain model for a conversation message (extracted from responses)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub id: String,
    pub role: String,
    pub content: String,
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

/// Conversation service for managing conversations
pub struct ConversationService {
    database: Option<Arc<database::Database>>,
}

impl ConversationService {
    pub fn new(database: Option<Arc<database::Database>>) -> Self {
        Self { database }
    }

    /// Create a new conversation
    pub async fn create_conversation(&self, request: ConversationRequest) -> Result<Conversation, CompletionError> {
        if let Some(ref db) = self.database {
            let user_id = parse_uuid(&request.user_id)?;
            let metadata = request.metadata.unwrap_or_else(|| serde_json::json!({}));
            
            let db_conversation = db.conversations.create(user_id, metadata).await
                .map_err(|e| CompletionError::InternalError(format!("Failed to create conversation: {}", e)))?;
            
            let conversation = Conversation {
                id: format!("conv_{}", db_conversation.id),
                user_id: db_conversation.user_id.to_string(),
                metadata: db_conversation.metadata,
                created_at: db_conversation.created_at,
                updated_at: db_conversation.updated_at,
            };
            
            Ok(conversation)
        } else {
            // Fallback to mock implementation
            let now = Utc::now();
            let conversation = Conversation {
                id: format!("conv_{}", Uuid::new_v4()),
                user_id: request.user_id.clone(),
                metadata: request.metadata.unwrap_or_else(|| serde_json::json!({})),
                created_at: now,
                updated_at: now,
            };
            Ok(conversation)
        }
    }

    /// Get a conversation by ID
    pub async fn get_conversation(&self, conversation_id: &str, user_id: &str) -> Result<Option<Conversation>, CompletionError> {
        if let Some(ref db) = self.database {
            let conv_uuid = parse_uuid_from_prefixed(conversation_id, "conv_")?;
            let user_uuid = parse_uuid(user_id)?;
            
            let db_conversation = db.conversations.get_by_id(conv_uuid, user_uuid).await
                .map_err(|e| CompletionError::InternalError(format!("Failed to get conversation: {}", e)))?;
            
            Ok(db_conversation.map(|c| Conversation {
                id: format!("conv_{}", c.id),
                user_id: c.user_id.to_string(),
                metadata: c.metadata,
                created_at: c.created_at,
                updated_at: c.updated_at,
            }))
        } else {
            // Mock implementation without database
            Ok(None)
        }
    }

    /// Update a conversation
    pub async fn update_conversation(&self, conversation_id: &str, user_id: &str, metadata: serde_json::Value) -> Result<Option<Conversation>, CompletionError> {
        if let Some(ref db) = self.database {
            let conv_uuid = parse_uuid_from_prefixed(conversation_id, "conv_")?;
            let user_uuid = parse_uuid(user_id)?;
            
            let db_conversation = db.conversations.update(conv_uuid, user_uuid, metadata).await
                .map_err(|e| CompletionError::InternalError(format!("Failed to update conversation: {}", e)))?;
            
            Ok(db_conversation.map(|c| Conversation {
                id: format!("conv_{}", c.id),
                user_id: c.user_id.to_string(),
                metadata: c.metadata,
                created_at: c.created_at,
                updated_at: c.updated_at,
            }))
        } else {
            // Mock implementation without database
            let now = Utc::now();
            let conversation = Conversation {
                id: format!("conv_{}", Uuid::new_v4()),
                user_id: "mock_user".to_string(),
                metadata,
                created_at: now,
                updated_at: now,
            };
            Ok(Some(conversation))
        }
    }

    /// Delete a conversation
    pub async fn delete_conversation(&self, conversation_id: &str, user_id: &str) -> Result<bool, CompletionError> {
        if let Some(ref db) = self.database {
            let conv_uuid = parse_uuid_from_prefixed(conversation_id, "conv_")?;
            let user_uuid = parse_uuid(user_id)?;
            
            db.conversations.delete(conv_uuid, user_uuid).await
                .map_err(|e| CompletionError::InternalError(format!("Failed to delete conversation: {}", e)))
        } else {
            // Mock implementation
            Ok(true)
        }
    }

    /// List conversations for a user
    pub async fn list_conversations(&self, user_id: &str, limit: Option<i32>, offset: Option<i32>) -> Result<Vec<Conversation>, CompletionError> {
        if let Some(ref db) = self.database {
            let user_uuid = parse_uuid(user_id)?;
            let limit = limit.unwrap_or(20).min(100) as i64;
            let offset = offset.unwrap_or(0) as i64;
            
            let db_conversations = db.conversations.list_by_user(user_uuid, limit, offset).await
                .map_err(|e| CompletionError::InternalError(format!("Failed to list conversations: {}", e)))?;
            
            // Convert to domain format
            Ok(db_conversations.into_iter().map(|c| Conversation {
                id: format!("conv_{}", c.id),
                user_id: c.user_id.to_string(),
                metadata: c.metadata,
                created_at: c.created_at,
                updated_at: c.updated_at,
            }).collect())
        } else {
            // Mock implementation
            Ok(vec![])
        }
    }

    /// Get conversation messages by extracting from responses
    pub async fn get_conversation_messages(&self, conversation_id: &str, user_id: &str, limit: Option<i32>) -> Result<Vec<ConversationMessage>, CompletionError> {
        if let Some(ref db) = self.database {
            let conv_uuid = parse_uuid_from_prefixed(conversation_id, "conv_")?;
            let user_uuid = parse_uuid(user_id)?;
            let limit = limit.unwrap_or(50).min(100) as i64;
            
            // Get responses for this conversation
            let responses = db.responses.list_by_conversation(conv_uuid, user_uuid, limit).await
                .map_err(|e| CompletionError::InternalError(format!("Failed to get conversation messages: {}", e)))?;
            
            // Extract messages from responses with deduplication
            let mut messages = Vec::new();
            let mut seen_content = std::collections::HashSet::new();
            
            for response in responses {
                // Parse input_messages JSONB to extract individual messages
                if let Some(input_array) = response.input_messages.as_array() {
                    for (index, msg_value) in input_array.iter().enumerate() {
                        if let Some(msg_obj) = msg_value.as_object() {
                            let role = msg_obj.get("role")
                                .and_then(|r| r.as_str())
                                .unwrap_or("user")
                                .to_string();
                            
                            let content = msg_obj.get("content")
                                .and_then(|c| c.as_str())
                                .unwrap_or("")
                                .to_string();
                            
                            let metadata = msg_obj.get("metadata")
                                .cloned();
                            
                            // Create a deduplication key based on role + content + rough timestamp
                            let dedup_key = format!("{}:{}:{}", role, content, response.created_at.timestamp() / 60); // Group by minute
                            
                            // Only add if we haven't seen this content recently
                            if !seen_content.contains(&dedup_key) {
                                seen_content.insert(dedup_key);
                                messages.push(ConversationMessage {
                                    id: format!("msg_{}_{}", response.id, index),
                                    role,
                                    content,
                                    metadata,
                                    created_at: response.created_at,
                                });
                            }
                        }
                    }
                }
                
                // Add the output message if present (these are usually unique)
                if let Some(output) = response.output_message {
                    let dedup_key = format!("assistant:{}:{}", output, response.updated_at.timestamp() / 60);
                    
                    if !seen_content.contains(&dedup_key) {
                        seen_content.insert(dedup_key);
                        messages.push(ConversationMessage {
                            id: format!("msg_{}_output", response.id),
                            role: "assistant".to_string(),
                            content: output,
                            metadata: None,
                            created_at: response.updated_at,
                        });
                    }
                }
            }
            
            // Sort by creation time to maintain conversation flow
            messages.sort_by(|a, b| a.created_at.cmp(&b.created_at));
            
            Ok(messages)
        } else {
            // Mock implementation
            Ok(vec![])
        }
    }
}