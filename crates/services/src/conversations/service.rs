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

/// Returns true when a repository error indicates the pagination cursor was
/// rejected (it does not exist, or belongs to another conversation/workspace).
fn is_invalid_cursor_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<crate::common::RepositoryError>())
        .any(|e| matches!(e, crate::common::RepositoryError::NotFound(_)))
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

        // Create the hidden structural root response for this conversation so clients can
        // use root_response_id for first-turn parallel responses (multiple models).
        let root_response_id = self
            .resp_repo
            .get_or_create_root_response(
                db_conversation.id,
                request.workspace_id.clone(),
                request.api_key_id,
            )
            .await
            .map_err(|e| {
                errors::ConversationError::InternalError(format!(
                    "Failed to create root response for conversation: {e}"
                ))
            })?;

        let conversation = models::Conversation {
            id: db_conversation.id,
            workspace_id: db_conversation.workspace_id,
            api_key_id: db_conversation.api_key_id,
            pinned_at: db_conversation.pinned_at,
            archived_at: db_conversation.archived_at,
            deleted_at: db_conversation.deleted_at,
            cloned_from_id: db_conversation.cloned_from_id,
            root_response_id: Some(root_response_id),
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

        let Some(c) = db_conversation.map(|c| models::Conversation {
            id: c.id,
            workspace_id: c.workspace_id,
            api_key_id: c.api_key_id,
            pinned_at: c.pinned_at,
            archived_at: c.archived_at,
            deleted_at: c.deleted_at,
            cloned_from_id: c.cloned_from_id,
            root_response_id: None,
            metadata: c.metadata,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }) else {
            return Ok(None);
        };

        Ok(Some(c))
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

        let Some(c) = db_conversation.map(|c| models::Conversation {
            id: c.id,
            workspace_id: c.workspace_id,
            api_key_id: c.api_key_id,
            pinned_at: c.pinned_at,
            archived_at: c.archived_at,
            deleted_at: c.deleted_at,
            cloned_from_id: c.cloned_from_id,
            root_response_id: None,
            metadata: c.metadata,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }) else {
            return Ok(None);
        };

        Ok(Some(c))
    }

    /// Pin or unpin a conversation
    async fn pin_conversation(
        &self,
        conversation_id: models::ConversationId,
        workspace_id: WorkspaceId,
        is_pinned: bool,
    ) -> Result<Option<models::Conversation>, errors::ConversationError> {
        let db_conversation = self
            .conv_repo
            .set_pinned(conversation_id, workspace_id.clone(), is_pinned)
            .await
            .map_err(|e| {
                errors::ConversationError::InternalError(format!(
                    "Failed to pin/unpin conversation: {e}"
                ))
            })?;

        let Some(c) = db_conversation.map(|c| models::Conversation {
            id: c.id,
            workspace_id: c.workspace_id,
            api_key_id: c.api_key_id,
            pinned_at: c.pinned_at,
            archived_at: c.archived_at,
            deleted_at: c.deleted_at,
            cloned_from_id: c.cloned_from_id,
            root_response_id: None,
            metadata: c.metadata,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }) else {
            return Ok(None);
        };

        Ok(Some(c))
    }

    /// Archive or unarchive a conversation
    async fn archive_conversation(
        &self,
        conversation_id: models::ConversationId,
        workspace_id: WorkspaceId,
        is_archived: bool,
    ) -> Result<Option<models::Conversation>, errors::ConversationError> {
        let db_conversation = self
            .conv_repo
            .set_archived(conversation_id, workspace_id.clone(), is_archived)
            .await
            .map_err(|e| {
                errors::ConversationError::InternalError(format!(
                    "Failed to archive/unarchive conversation: {e}"
                ))
            })?;

        let Some(c) = db_conversation.map(|c| models::Conversation {
            id: c.id,
            workspace_id: c.workspace_id,
            api_key_id: c.api_key_id,
            pinned_at: c.pinned_at,
            archived_at: c.archived_at,
            deleted_at: c.deleted_at,
            cloned_from_id: c.cloned_from_id,
            root_response_id: None,
            metadata: c.metadata,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }) else {
            return Ok(None);
        };

        Ok(Some(c))
    }

    /// Clone a conversation
    async fn clone_conversation(
        &self,
        conversation_id: models::ConversationId,
        workspace_id: WorkspaceId,
        api_key_id: uuid::Uuid,
    ) -> Result<Option<models::Conversation>, errors::ConversationError> {
        let db_conversation = self
            .conv_repo
            .clone_conversation(conversation_id, workspace_id.clone(), api_key_id)
            .await
            .map_err(|e| {
                errors::ConversationError::InternalError(format!(
                    "Failed to clone conversation: {e}"
                ))
            })?;

        let Some(c) = db_conversation else {
            return Ok(None);
        };

        // Clone copies all responses including the root; fetch its ID so clients can use
        // root_response_id for first-turn parallel responses (same as create_conversation).
        let root_response_id = self
            .resp_repo
            .get_or_create_root_response(c.id, workspace_id.clone(), api_key_id)
            .await
            .map_err(|e| {
                errors::ConversationError::InternalError(format!(
                    "Failed to get root response for cloned conversation: {e}"
                ))
            })?;

        let conversation = models::Conversation {
            id: c.id,
            workspace_id: c.workspace_id,
            api_key_id: c.api_key_id,
            pinned_at: c.pinned_at,
            archived_at: c.archived_at,
            deleted_at: c.deleted_at,
            cloned_from_id: c.cloned_from_id,
            root_response_id: Some(root_response_id),
            metadata: c.metadata,
            created_at: c.created_at,
            updated_at: c.updated_at,
        };

        Ok(Some(conversation))
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
        workspace_id: WorkspaceId,
        after: Option<String>,
        limit: i64,
    ) -> Result<Vec<crate::responses::models::ResponseOutputItem>, errors::ConversationError> {
        tracing::debug!(
            "Listing conversation items for conversation_id={}, workspace_id={}, after={:?}, limit={}",
            conversation_id,
            workspace_id.0,
            after,
            limit
        );

        // Enforce workspace ownership before touching any items. Unknown and
        // foreign conversation IDs both surface as the same NotFound so callers
        // cannot enumerate other workspaces' conversation IDs.
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
            return Err(errors::ConversationError::NotFound);
        }

        // Get items from response_items repository. The repository additionally
        // constrains the query by workspace (defense in depth) and rejects
        // pagination cursors that do not belong to this conversation.
        self.response_items_repo
            .list_by_conversation(conversation_id, workspace_id, after, limit)
            .await
            .map_err(|e| {
                if is_invalid_cursor_error(&e) {
                    errors::ConversationError::InvalidParams(
                        "Invalid 'after' cursor for this conversation".to_string(),
                    )
                } else {
                    errors::ConversationError::InternalError(format!(
                        "Failed to list conversation items: {e}"
                    ))
                }
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
            // Same non-enumerating response for unknown and foreign conversations.
            return Err(errors::ConversationError::NotFound);
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
            reasoning: None,
            include: None,
            metadata: Some(serde_json::json!({
                "backfill": true
            })),
            safety_identifier: None,
            prompt_cache_key: None,
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
            .strip_prefix(crate::id_prefixes::PREFIX_RESP)
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

    /// Batch get conversations by IDs
    async fn batch_get_conversations(
        &self,
        conversation_ids: Vec<models::ConversationId>,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<models::Conversation>, errors::ConversationError> {
        tracing::debug!(
            "Batch getting {} conversations for workspace: {}",
            conversation_ids.len(),
            workspace_id.0
        );

        let db_conversations = self
            .conv_repo
            .batch_get_by_ids(conversation_ids, workspace_id.clone())
            .await
            .map_err(|e| {
                errors::ConversationError::InternalError(format!(
                    "Failed to batch get conversations: {e}"
                ))
            })?;

        let num_conversations = db_conversations.len();

        let mut conversations = Vec::with_capacity(db_conversations.len());
        for c in db_conversations {
            let conv = models::Conversation {
                id: c.id,
                workspace_id: c.workspace_id,
                api_key_id: c.api_key_id,
                pinned_at: c.pinned_at,
                archived_at: c.archived_at,
                deleted_at: c.deleted_at,
                cloned_from_id: c.cloned_from_id,
                root_response_id: None,
                metadata: c.metadata,
                created_at: c.created_at,
                updated_at: c.updated_at,
            };
            conversations.push(conv);
        }

        tracing::debug!(
            "Batch retrieved {} conversations for workspace: {}",
            num_conversations,
            workspace_id.0
        );

        Ok(conversations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn test_conversation_id_creation() {
        // Test that ConversationId can be created from UUID
        let uuid = Uuid::new_v4();
        let conv_id = models::ConversationId(uuid);
        assert_eq!(conv_id.0, uuid, "ConversationId should wrap UUID correctly");
    }

    #[test]
    fn test_conversation_id_vector_creation() {
        // Test that we can create vectors of ConversationId
        let id1 = models::ConversationId(Uuid::new_v4());
        let id2 = models::ConversationId(Uuid::new_v4());
        let id3 = models::ConversationId(Uuid::new_v4());

        let ids = [id1, id2, id3];
        assert_eq!(ids.len(), 3, "Should create vector with 3 IDs");
        assert_eq!(ids[0].0, id1.0, "First ID should match");
        assert_eq!(ids[1].0, id2.0, "Second ID should match");
        assert_eq!(ids[2].0, id3.0, "Third ID should match");
    }

    #[test]
    fn test_workspace_id_creation() {
        // Test that WorkspaceId can be created
        let uuid = Uuid::new_v4();
        let workspace_id = WorkspaceId(uuid);
        assert_eq!(
            workspace_id.0, uuid,
            "WorkspaceId should wrap UUID correctly"
        );
    }

    #[test]
    fn test_conversation_model_creation() {
        // Test that Conversation model can be created with all fields
        let id = models::ConversationId(Uuid::new_v4());
        let workspace_id = WorkspaceId(Uuid::new_v4());
        let api_key_id = Uuid::new_v4();
        let now = chrono::Utc::now();

        let conversation = models::Conversation {
            id,
            workspace_id: workspace_id.clone(),
            api_key_id,
            root_response_id: None,
            pinned_at: None,
            archived_at: None,
            deleted_at: None,
            cloned_from_id: None,
            metadata: serde_json::json!({}),
            created_at: now,
            updated_at: now,
        };

        assert_eq!(conversation.id.0, id.0, "ID should match");
        assert_eq!(
            conversation.workspace_id.0, workspace_id.0,
            "Workspace ID should match"
        );
        assert_eq!(
            conversation.api_key_id, api_key_id,
            "API key ID should match"
        );
        assert_eq!(conversation.pinned_at, None, "pinned_at should be None");
        assert_eq!(conversation.archived_at, None, "archived_at should be None");
        assert_eq!(conversation.deleted_at, None, "deleted_at should be None");
    }

    #[test]
    fn test_batch_conversation_ids_collection() {
        // Test collecting conversation IDs for batch operation
        let ids: Vec<models::ConversationId> = (0..10)
            .map(|_| models::ConversationId(Uuid::new_v4()))
            .collect();

        assert_eq!(ids.len(), 10, "Should collect 10 conversation IDs");

        // Verify all IDs are unique
        let mut id_strings: Vec<String> = ids.iter().map(|id| id.0.to_string()).collect();
        id_strings.sort();
        id_strings.dedup();
        assert_eq!(
            id_strings.len(),
            10,
            "All conversation IDs should be unique"
        );
    }

    #[test]
    fn test_empty_batch_ids() {
        // Test handling of empty batch
        let empty_ids: Vec<models::ConversationId> = Vec::new();
        assert_eq!(empty_ids.len(), 0, "Empty vector should have length 0");
        assert!(empty_ids.is_empty(), "Empty vector should report as empty");
    }

    // ============================================
    // Workspace ownership enforcement (issue nearai/infra#190)
    // ============================================

    use crate::conversations::ports::ConversationServiceTrait;
    use crate::responses::models as resp_models;
    use std::sync::Mutex;

    fn test_conversation(
        id: models::ConversationId,
        workspace_id: WorkspaceId,
    ) -> models::Conversation {
        let now = chrono::Utc::now();
        models::Conversation {
            id,
            workspace_id,
            api_key_id: Uuid::new_v4(),
            pinned_at: None,
            archived_at: None,
            deleted_at: None,
            cloned_from_id: None,
            root_response_id: None,
            metadata: serde_json::json!({}),
            created_at: now,
            updated_at: now,
        }
    }

    /// Conversation repository that owns exactly one conversation in one
    /// workspace. `get_by_id` behaves like the real repository: it only
    /// returns the conversation when both the ID and the workspace match.
    struct MockConversationRepo {
        conversation: models::Conversation,
    }

    #[async_trait]
    impl ports::ConversationRepository for MockConversationRepo {
        async fn create(
            &self,
            _workspace_id: WorkspaceId,
            _api_key_id: Uuid,
            _metadata: serde_json::Value,
        ) -> Result<models::Conversation> {
            panic!("create must not be called");
        }

        async fn get_by_id(
            &self,
            id: models::ConversationId,
            workspace_id: WorkspaceId,
        ) -> Result<Option<models::Conversation>> {
            if id.0 == self.conversation.id.0 && workspace_id == self.conversation.workspace_id {
                Ok(Some(self.conversation.clone()))
            } else {
                Ok(None)
            }
        }

        async fn update(
            &self,
            _id: models::ConversationId,
            _workspace_id: WorkspaceId,
            _metadata: serde_json::Value,
        ) -> Result<Option<models::Conversation>> {
            panic!("update must not be called");
        }

        async fn set_pinned(
            &self,
            _id: models::ConversationId,
            _workspace_id: WorkspaceId,
            _is_pinned: bool,
        ) -> Result<Option<models::Conversation>> {
            panic!("set_pinned must not be called");
        }

        async fn set_archived(
            &self,
            _id: models::ConversationId,
            _workspace_id: WorkspaceId,
            _is_archived: bool,
        ) -> Result<Option<models::Conversation>> {
            panic!("set_archived must not be called");
        }

        async fn clone_conversation(
            &self,
            _id: models::ConversationId,
            _workspace_id: WorkspaceId,
            _api_key_id: Uuid,
        ) -> Result<Option<models::Conversation>> {
            panic!("clone_conversation must not be called");
        }

        async fn delete(
            &self,
            _id: models::ConversationId,
            _workspace_id: WorkspaceId,
        ) -> Result<bool> {
            panic!("delete must not be called");
        }

        async fn batch_get_by_ids(
            &self,
            _ids: Vec<models::ConversationId>,
            _workspace_id: WorkspaceId,
        ) -> Result<Vec<models::Conversation>> {
            panic!("batch_get_by_ids must not be called");
        }
    }

    /// Response repository that panics on any write: the ownership check must
    /// reject foreign conversations before any response row is created.
    struct RejectingResponseRepo;

    #[async_trait]
    impl ResponseRepositoryTrait for RejectingResponseRepo {
        async fn create(
            &self,
            _workspace_id: WorkspaceId,
            _api_key_id: Uuid,
            _request: resp_models::CreateResponseRequest,
        ) -> anyhow::Result<resp_models::ResponseObject> {
            panic!("response create must not be called for a foreign conversation");
        }

        async fn get_by_id(
            &self,
            _id: resp_models::ResponseId,
            _workspace_id: WorkspaceId,
        ) -> anyhow::Result<Option<resp_models::ResponseObject>> {
            Ok(None)
        }

        async fn update(
            &self,
            _id: resp_models::ResponseId,
            _workspace_id: WorkspaceId,
            _output_message: Option<String>,
            _status: resp_models::ResponseStatus,
            _usage: Option<serde_json::Value>,
        ) -> anyhow::Result<Option<resp_models::ResponseObject>> {
            panic!("response update must not be called for a foreign conversation");
        }

        async fn delete(
            &self,
            _id: resp_models::ResponseId,
            _workspace_id: WorkspaceId,
        ) -> anyhow::Result<bool> {
            panic!("response delete must not be called");
        }

        async fn cancel(
            &self,
            _id: resp_models::ResponseId,
            _workspace_id: WorkspaceId,
        ) -> anyhow::Result<Option<resp_models::ResponseObject>> {
            panic!("response cancel must not be called");
        }

        async fn list_by_workspace(
            &self,
            _workspace_id: WorkspaceId,
            _limit: i64,
            _offset: i64,
        ) -> anyhow::Result<Vec<resp_models::ResponseObject>> {
            panic!("list_by_workspace must not be called");
        }

        async fn list_by_conversation(
            &self,
            _conversation_id: models::ConversationId,
            _workspace_id: WorkspaceId,
            _limit: i64,
        ) -> anyhow::Result<Vec<resp_models::ResponseObject>> {
            panic!("list_by_conversation must not be called");
        }

        async fn get_previous(
            &self,
            _response_id: resp_models::ResponseId,
            _workspace_id: WorkspaceId,
        ) -> anyhow::Result<Option<resp_models::ResponseObject>> {
            panic!("get_previous must not be called");
        }

        async fn get_latest_in_conversation(
            &self,
            _conversation_id: models::ConversationId,
            _workspace_id: WorkspaceId,
        ) -> anyhow::Result<Option<resp_models::ResponseObject>> {
            panic!("get_latest_in_conversation must not be called");
        }

        async fn get_or_create_root_response(
            &self,
            _conversation_id: models::ConversationId,
            _workspace_id: WorkspaceId,
            _api_key_id: Uuid,
        ) -> anyhow::Result<String> {
            panic!("get_or_create_root_response must not be called");
        }
    }

    /// Response-item repository that records list calls and panics on writes.
    #[derive(Default)]
    struct RecordingItemsRepo {
        list_calls: Mutex<Vec<(Uuid, Uuid, Option<String>)>>,
    }

    #[async_trait]
    impl ResponseItemRepositoryTrait for RecordingItemsRepo {
        async fn create(
            &self,
            _response_id: resp_models::ResponseId,
            _api_key_id: Uuid,
            _conversation_id: Option<models::ConversationId>,
            _item: resp_models::ResponseOutputItem,
        ) -> anyhow::Result<resp_models::ResponseOutputItem> {
            panic!("item create must not be called for a foreign conversation");
        }

        async fn get_by_id(
            &self,
            _id: resp_models::ResponseItemId,
            _workspace_id: WorkspaceId,
        ) -> anyhow::Result<Option<resp_models::ResponseOutputItem>> {
            Ok(None)
        }

        async fn update(
            &self,
            _id: resp_models::ResponseItemId,
            _item: resp_models::ResponseOutputItem,
        ) -> anyhow::Result<resp_models::ResponseOutputItem> {
            panic!("item update must not be called");
        }

        async fn delete(&self, _id: resp_models::ResponseItemId) -> anyhow::Result<bool> {
            panic!("item delete must not be called");
        }

        async fn list_by_response(
            &self,
            _response_id: resp_models::ResponseId,
        ) -> anyhow::Result<Vec<resp_models::ResponseOutputItem>> {
            panic!("list_by_response must not be called");
        }

        async fn list_by_api_key(
            &self,
            _api_key_id: Uuid,
        ) -> anyhow::Result<Vec<resp_models::ResponseOutputItem>> {
            panic!("list_by_api_key must not be called");
        }

        async fn list_by_conversation(
            &self,
            conversation_id: models::ConversationId,
            workspace_id: WorkspaceId,
            after: Option<String>,
            _limit: i64,
        ) -> anyhow::Result<Vec<resp_models::ResponseOutputItem>> {
            self.list_calls
                .lock()
                .unwrap()
                .push((conversation_id.0, workspace_id.0, after));
            Ok(vec![])
        }
    }

    fn service_with_owned_conversation(
        conversation_id: models::ConversationId,
        owner_workspace: WorkspaceId,
    ) -> (ConversationServiceImpl, Arc<RecordingItemsRepo>) {
        let items_repo = Arc::new(RecordingItemsRepo::default());
        let service = ConversationServiceImpl::new(
            Arc::new(MockConversationRepo {
                conversation: test_conversation(conversation_id, owner_workspace),
            }),
            Arc::new(RejectingResponseRepo),
            items_repo.clone(),
        );
        (service, items_repo)
    }

    #[tokio::test]
    async fn test_list_conversation_items_foreign_workspace_not_found() {
        let conversation_id = models::ConversationId(Uuid::new_v4());
        let owner_workspace = WorkspaceId(Uuid::new_v4());
        let foreign_workspace = WorkspaceId(Uuid::new_v4());

        let (service, items_repo) =
            service_with_owned_conversation(conversation_id, owner_workspace);

        let result = service
            .list_conversation_items(conversation_id, foreign_workspace, None, 10)
            .await;

        assert!(
            matches!(result, Err(errors::ConversationError::NotFound)),
            "foreign-workspace lookup must return NotFound, got: {result:?}"
        );
        assert!(
            items_repo.list_calls.lock().unwrap().is_empty(),
            "item repository must not be queried for a foreign conversation"
        );
    }

    #[tokio::test]
    async fn test_list_conversation_items_unknown_conversation_not_found() {
        let owner_workspace = WorkspaceId(Uuid::new_v4());
        let (service, items_repo) = service_with_owned_conversation(
            models::ConversationId(Uuid::new_v4()),
            owner_workspace.clone(),
        );

        // Same workspace, unknown conversation ID: identical NotFound outcome
        // as the foreign-workspace case (non-enumerating behavior).
        let result = service
            .list_conversation_items(
                models::ConversationId(Uuid::new_v4()),
                owner_workspace,
                None,
                10,
            )
            .await;

        assert!(matches!(result, Err(errors::ConversationError::NotFound)));
        assert!(items_repo.list_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_list_conversation_items_same_workspace_passes_workspace_to_repo() {
        let conversation_id = models::ConversationId(Uuid::new_v4());
        let owner_workspace = WorkspaceId(Uuid::new_v4());

        let (service, items_repo) =
            service_with_owned_conversation(conversation_id, owner_workspace.clone());

        let result = service
            .list_conversation_items(conversation_id, owner_workspace.clone(), None, 10)
            .await;

        assert!(result.is_ok(), "owner lookup must succeed: {result:?}");
        let calls = items_repo.list_calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "item repository queried exactly once");
        assert_eq!(calls[0].0, conversation_id.0);
        assert_eq!(
            calls[0].1, owner_workspace.0,
            "workspace must be threaded through to the repository query"
        );
    }

    #[tokio::test]
    async fn test_create_conversation_items_foreign_workspace_not_found() {
        let conversation_id = models::ConversationId(Uuid::new_v4());
        let owner_workspace = WorkspaceId(Uuid::new_v4());
        let foreign_workspace = WorkspaceId(Uuid::new_v4());

        let (service, _items_repo) =
            service_with_owned_conversation(conversation_id, owner_workspace);

        let item = resp_models::ResponseOutputItem::Message {
            id: format!("msg_{}", Uuid::new_v4().simple()),
            response_id: String::new(),
            previous_response_id: None,
            next_response_ids: vec![],
            created_at: 0,
            status: resp_models::ResponseItemStatus::Completed,
            role: "user".to_string(),
            content: vec![],
            model: String::new(),
            metadata: None,
        };

        // The mock response/item repositories panic on create, so this also
        // proves no response or response item is created on the foreign path.
        let result = service
            .create_conversation_items(
                conversation_id,
                foreign_workspace,
                Uuid::new_v4(),
                vec![item],
            )
            .await;

        assert!(
            matches!(result, Err(errors::ConversationError::NotFound)),
            "foreign-workspace item creation must return NotFound, got: {result:?}"
        );
    }
}
