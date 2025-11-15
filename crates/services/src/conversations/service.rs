use crate::{
    conversations::{errors, models},
    responses::ports::{ResponseItemRepositoryTrait, ResponseRepositoryTrait},
    workspace::WorkspaceId,
};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use uuid::Uuid;

use crate::conversations::ports;

// Helper functions for ID parsing
pub fn parse_uuid(id: &str) -> Result<Uuid, errors::ConversationError> {
    Uuid::parse_str(id)
        .map_err(|_| errors::ConversationError::InvalidParams(format!("Invalid UUID: {id}")))
}

pub fn parse_uuid_from_prefixed(id: &str, prefix: &str) -> Result<Uuid, errors::ConversationError> {
    let uuid_str = id.strip_prefix(prefix).ok_or_else(|| {
        errors::ConversationError::InvalidParams(format!(
            "Invalid {} ID format: {}",
            prefix.trim_end_matches('_'),
            id
        ))
    })?;

    Uuid::parse_str(uuid_str).map_err(|_| {
        errors::ConversationError::InvalidParams(format!(
            "Invalid {} UUID: {}",
            prefix.trim_end_matches('_'),
            id
        ))
    })
}

/// Conversation service for managing conversations
pub struct ConversationServiceImpl {
    pub conv_repo: Arc<dyn ports::ConversationRepository>,
    pub resp_repo: Arc<dyn ResponseRepositoryTrait>,
    pub response_items_repo: Arc<dyn ResponseItemRepositoryTrait>,
}

impl ConversationServiceImpl {
    pub fn new(
        conv_repo: Arc<dyn ports::ConversationRepository>,
        resp_repo: Arc<dyn ResponseRepositoryTrait>,
        response_items_repo: Arc<dyn ResponseItemRepositoryTrait>,
    ) -> Self {
        Self {
            conv_repo,
            resp_repo,
            response_items_repo,
        }
    }
}

#[async_trait]
impl ports::ConversationServiceTrait for ConversationServiceImpl {
    /// Create a new conversation
    async fn create_conversation(
        &self,
        request: models::ConversationRequest,
    ) -> Result<models::Conversation, errors::ConversationError> {
        let metadata = request.metadata.unwrap_or_else(|| serde_json::json!({}));

        tracing::info!(
            "Creating conversation for workspace: {}",
            request.workspace_id.0
        );

        let db_conversation = self
            .conv_repo
            .create(request.workspace_id.clone(), request.api_key_id, metadata)
            .await
            .map_err(|e| {
                errors::ConversationError::InternalError(format!(
                    "Failed to create conversation: {e}"
                ))
            })?;

        tracing::info!("Created conversation: {}", db_conversation.id);

        let conversation = models::Conversation {
            id: db_conversation.id,
            workspace_id: db_conversation.workspace_id,
            api_key_id: db_conversation.api_key_id,
            metadata: db_conversation.metadata,
            created_at: db_conversation.created_at,
            updated_at: db_conversation.updated_at,
        };

        Ok(conversation)
    }

    /// Get a conversation by ID
    async fn get_conversation(
        &self,
        conversation_id: models::ConversationId,
        workspace_id: WorkspaceId,
    ) -> Result<Option<models::Conversation>, errors::ConversationError> {
        let db_conversation = self
            .conv_repo
            .get_by_id(conversation_id, workspace_id.clone())
            .await
            .map_err(|e| {
                errors::ConversationError::InternalError(format!("Failed to get conversation: {e}"))
            })?;

        Ok(db_conversation.map(|c| models::Conversation {
            id: c.id,
            workspace_id: c.workspace_id,
            api_key_id: c.api_key_id,
            metadata: c.metadata,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }))
    }

    /// Update a conversation
    async fn update_conversation(
        &self,
        conversation_id: models::ConversationId,
        workspace_id: WorkspaceId,
        metadata: serde_json::Value,
    ) -> Result<Option<models::Conversation>, errors::ConversationError> {
        let db_conversation = self
            .conv_repo
            .update(conversation_id, workspace_id.clone(), metadata)
            .await
            .map_err(|e| {
                errors::ConversationError::InternalError(format!(
                    "Failed to update conversation: {e}"
                ))
            })?;

        Ok(db_conversation.map(|c| models::Conversation {
            id: c.id,
            workspace_id: c.workspace_id,
            api_key_id: c.api_key_id,
            metadata: c.metadata,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }))
    }

    /// Delete a conversation
    async fn delete_conversation(
        &self,
        conversation_id: models::ConversationId,
        workspace_id: WorkspaceId,
    ) -> Result<bool, errors::ConversationError> {
        self.conv_repo
            .delete(conversation_id, workspace_id)
            .await
            .map_err(|e| {
                errors::ConversationError::InternalError(format!(
                    "Failed to delete conversation: {e}"
                ))
            })
    }

    /// Get conversation messages by extracting from responses
    async fn get_conversation_messages(
        &self,
        _conversation_id: models::ConversationId,
        _workspace_id: WorkspaceId,
        _limit: i64,
        _offset: i64,
    ) -> Result<Vec<models::ConversationMessage>, errors::ConversationError> {
        unimplemented!()
        // Get responses for this conversation
        // let responses = self
        //     .resp_repo
        //     .list_by_conversation(conversation_id, user_id, limit)
        //     .await
        //     .map_err(|e| {
        //         errors::ConversationError::InternalError(format!(
        //             "Failed to get conversation messages: {e}"
        //         ))
        //     })?;

        // // Extract messages from responses with deduplication
        // let mut messages = Vec::new();
        // let mut seen_content = std::collections::HashSet::new();

        // for response in responses {
        //     // Parse input_messages JSONB to extract individual messages
        //     if let Some(input_array) = response.input_messages.as_array() {
        //         for msg_value in input_array.iter() {
        //             if let Some(msg_obj) = msg_value.as_object() {
        //                 let role = msg_obj
        //                     .get("role")
        //                     .and_then(|r| r.as_str())
        //                     .unwrap_or("user")
        //                     .to_string();

        //                 let content = msg_obj
        //                     .get("content")
        //                     .and_then(|c| c.as_str())
        //                     .unwrap_or("")
        //                     .to_string();

        //                 let metadata = msg_obj.get("metadata").cloned();

        //                 // Create a deduplication key based on role + content + rough timestamp
        //                 let dedup_key = format!(
        //                     "{}:{}:{}",
        //                     role,
        //                     content,
        //                     response.created_at.timestamp() / 60
        //                 ); // Group by minute

        //                 // Only add if we haven't seen this content recently
        //                 if !seen_content.contains(&dedup_key) {
        //                     seen_content.insert(dedup_key);
        //                     messages.push(models::ConversationMessage {
        //                         id: response.id.clone(),
        //                         role,
        //                         content,
        //                         metadata,
        //                         created_at: response.created_at,
        //                     });
        //                 }
        //             }
        //         }
        //     }

        //     // Add the output message if present (these are usually unique)
        //     if let Some(output) = response.output_message {
        //         let dedup_key = format!(
        //             "assistant:{}:{}",
        //             output,
        //             response.updated_at.timestamp() / 60
        //         );

        //         if !seen_content.contains(&dedup_key) {
        //             seen_content.insert(dedup_key);
        //             messages.push(models::ConversationMessage {
        //                 id: response.id.clone(),
        //                 role: "assistant".to_string(),
        //                 content: output,
        //                 metadata: None,
        //                 created_at: response.updated_at,
        //             });
        //         }
        //     }
        // }

        // // Sort by creation time to maintain conversation flow
        // messages.sort_by(|a, b| a.created_at.cmp(&b.created_at));

        // Ok(messages)
    }

    /// List items in a conversation (messages, tool calls, etc.)
    async fn list_conversation_items(
        &self,
        conversation_id: models::ConversationId,
        _workspace_id: WorkspaceId,
        after: Option<String>,
        limit: i64,
    ) -> Result<Vec<crate::responses::models::ResponseOutputItem>, errors::ConversationError> {
        tracing::debug!(
            "Listing conversation items for conversation_id={}, after={:?}, limit={}",
            conversation_id,
            after,
            limit
        );

        // Get items from response_items repository
        self.response_items_repo
            .list_by_conversation(conversation_id, after, limit)
            .await
            .map_err(|e| {
                errors::ConversationError::InternalError(format!(
                    "Failed to list conversation items: {e}"
                ))
            })
    }

    /// Create items in a conversation (for backfilling)
    async fn create_conversation_items(
        &self,
        conversation_id: models::ConversationId,
        workspace_id: WorkspaceId,
        api_key_id: uuid::Uuid,
        items: Vec<crate::responses::models::ResponseOutputItem>,
    ) -> Result<Vec<crate::responses::models::ResponseOutputItem>, errors::ConversationError> {
        tracing::debug!(
            "Creating {} items in conversation {} for workspace {}",
            items.len(),
            conversation_id,
            workspace_id.0
        );

        // Verify conversation exists
        let conversation = self
            .conv_repo
            .get_by_id(conversation_id, workspace_id.clone())
            .await
            .map_err(|e| {
                errors::ConversationError::InternalError(format!(
                    "Failed to verify conversation: {e}"
                ))
            })?;

        if conversation.is_none() {
            return Err(errors::ConversationError::InvalidParams(format!(
                "Conversation not found: {conversation_id}"
            )));
        }

        // Create a minimal response for backfilled items
        // Response items require a response_id, so we create a placeholder response
        let backfill_response_request = crate::responses::models::CreateResponseRequest {
            model: "backfill".to_string(), // Special model name for backfilled items
            input: None,
            instructions: None,
            conversation: Some(crate::responses::models::ConversationReference::Id(
                conversation_id.to_string(),
            )),
            previous_response_id: None,
            max_output_tokens: None,
            max_tool_calls: None,
            temperature: None,
            top_p: None,
            stream: Some(false),
            store: Some(false),
            background: Some(false),
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            text: None,
            reasoning: None,
            include: None,
            metadata: Some(serde_json::json!({
                "backfill": true
            })),
            safety_identifier: None,
            prompt_cache_key: None,
            signing_algo: None,
        };

        let backfill_response = self
            .resp_repo
            .create(workspace_id.clone(), api_key_id, backfill_response_request)
            .await
            .map_err(|e| {
                errors::ConversationError::InternalError(format!(
                    "Failed to create backfill response: {e}"
                ))
            })?;

        // Extract response_id from the created response
        let response_id_str = backfill_response
            .id
            .strip_prefix("resp_")
            .unwrap_or(&backfill_response.id);
        let response_uuid = Uuid::parse_str(response_id_str).map_err(|e| {
            errors::ConversationError::InternalError(format!("Failed to parse response ID: {e}"))
        })?;
        let response_id = crate::responses::models::ResponseId(response_uuid);

        // Create each item in the response_items repository
        let mut created_items = Vec::new();
        for item in items {
            let created_item = self
                .response_items_repo
                .create(response_id.clone(), api_key_id, Some(conversation_id), item)
                .await
                .map_err(|e| {
                    errors::ConversationError::InternalError(format!(
                        "Failed to create conversation item: {e}"
                    ))
                })?;
            created_items.push(created_item);
        }

        // Update the response status to "completed" since all items have been backfilled
        self.resp_repo
            .update(
                response_id.clone(),
                workspace_id.clone(),
                None, // output_message not used - messages stored as response_items
                crate::responses::models::ResponseStatus::Completed,
                None, // usage not updated for backfilled responses
            )
            .await
            .map_err(|e| {
                errors::ConversationError::InternalError(format!(
                    "Failed to update backfill response status: {e}"
                ))
            })?;

        tracing::debug!(
            "Created {} items in conversation {} and marked response as completed",
            created_items.len(),
            conversation_id
        );

        Ok(created_items)
    }
}
