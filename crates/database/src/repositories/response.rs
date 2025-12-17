use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use services::common::RepositoryError;
use services::responses::models::*;
use services::responses::ports::*;
use services::workspace::WorkspaceId;
use uuid::Uuid;

pub struct PgResponseRepository {
    pool: DbPool,
}

impl PgResponseRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ResponseRepositoryTrait for PgResponseRepository {
    async fn create(
        &self,
        workspace_id: WorkspaceId,
        api_key_id: uuid::Uuid,
        request: CreateResponseRequest,
    ) -> Result<ResponseObject, anyhow::Error> {
        // Generate new response ID
        let response_uuid = Uuid::new_v4();
        let response_id = format!("resp_{}", response_uuid.simple());

        // Extract conversation_id if present in request
        let mut conversation_uuid = if let Some(conv_ref) = &request.conversation {
            match conv_ref {
                ConversationReference::Id(id) => {
                    let uuid_str = id
                        .strip_prefix(services::id_prefixes::PREFIX_CONV)
                        .unwrap_or(id);
                    Some(Uuid::parse_str(uuid_str).context("Invalid conversation ID")?)
                }
                ConversationReference::Object { id, .. } => {
                    let uuid_str = id
                        .strip_prefix(services::id_prefixes::PREFIX_CONV)
                        .unwrap_or(id);
                    Some(Uuid::parse_str(uuid_str).context("Invalid conversation ID")?)
                }
            }
        } else {
            None
        };

        // Extract previous_response_id if present (this is the parent response)
        // If not provided but conversation is provided, find the latest response in the conversation
        let previous_response_uuid = if let Some(prev_id) = &request.previous_response_id {
            let uuid_str = prev_id
                .strip_prefix(services::id_prefixes::PREFIX_RESP)
                .unwrap_or(prev_id);
            let prev_uuid = Uuid::parse_str(uuid_str).context("Invalid previous response ID")?;

            // If conversation_uuid is not explicitly set, inherit it from the previous response
            if conversation_uuid.is_none() {
                // Fetch the previous response to get its conversation_id
                let prev_response = retry_db!("fetch_previous_response_conversation", {
                    let client = self
                        .pool
                        .get()
                        .await
                        .context("Failed to get database connection")
                        .map_err(RepositoryError::PoolError)?;

                    client
                        .query_opt(
                            r#"
                            SELECT conversation_id
                            FROM responses
                            WHERE id = $1 AND workspace_id = $2
                            "#,
                            &[&prev_uuid, &workspace_id.0],
                        )
                        .await
                        .map_err(map_db_error)
                })?;

                if let Some(row) = prev_response {
                    let prev_conversation_id: Option<Uuid> = row.get("conversation_id");
                    conversation_uuid = prev_conversation_id;

                    if conversation_uuid.is_some() {
                        tracing::debug!(
                            "Inherited conversation_id from previous response {}",
                            prev_id
                        );
                    }
                }
            }

            Some(prev_uuid)
        } else if let Some(conv_uuid) = conversation_uuid {
            // No explicit previous_response_id, but conversation exists
            // Find the latest response in this conversation to link to it
            let latest_response = self
                .get_latest_in_conversation(
                    services::conversations::models::ConversationId(conv_uuid),
                    workspace_id.clone(),
                )
                .await?;

            if let Some(latest) = latest_response {
                // Extract UUID from the latest response ID (format: "resp_{uuid}")
                let latest_uuid_str = latest
                    .id
                    .strip_prefix(services::id_prefixes::PREFIX_RESP)
                    .unwrap_or(&latest.id);
                Some(Uuid::parse_str(latest_uuid_str).context("Invalid latest response ID")?)
            } else {
                None
            }
        } else {
            None
        };

        // Initial status is in_progress
        let status = "in_progress";

        // Prepare usage and metadata as JSONB
        let usage_json = serde_json::json!({
            "input_tokens": 0,
            "output_tokens": 0,
            "total_tokens": 0
        });
        let metadata_json = request.metadata.unwrap_or_else(|| serde_json::json!({}));
        let next_response_ids_json = serde_json::json!([]);

        // Insert response and update previous response in a single retry_db! block
        // This ensures all queries are retried together if needed
        retry_db!("insert_response_and_update_parent", {
            let now = Utc::now();
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            // Insert the new response
            client
                .execute(
                    r#"
                    INSERT INTO responses (
                        id, workspace_id, api_key_id, model, status, instructions, conversation_id,
                        previous_response_id, next_response_ids, usage, metadata,
                        created_at, updated_at
                    )
                    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                    "#,
                    &[
                        &response_uuid,
                        &workspace_id.0,
                        &api_key_id,
                        &request.model,
                        &status,
                        &request.instructions,
                        &conversation_uuid,
                        &previous_response_uuid,
                        &next_response_ids_json,
                        &usage_json,
                        &metadata_json,
                        &now,
                        &now,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            // If previous_response_id is present, update the previous response's next_response_ids array
            if let Some(parent_uuid) = previous_response_uuid {
                client
                    .execute(
                        r#"
                        UPDATE responses
                        SET next_response_ids = next_response_ids || $1::jsonb,
                            updated_at = $2
                        WHERE id = $3 AND workspace_id = $4
                        "#,
                        &[
                            &serde_json::json!([response_uuid.to_string()]),
                            &now,
                            &parent_uuid,
                            &workspace_id.0,
                        ],
                    )
                    .await
                    .map_err(map_db_error)?;
            }

            Ok(())
        })?;

        // Build conversation reference if conversation_id is present
        let conversation_ref = conversation_uuid.map(|uuid| ConversationResponseReference {
            id: format!("conv_{}", uuid.simple()),
        });

        let now = Utc::now();

        // Build ResponseObject from the database row
        let response_obj = ResponseObject {
            id: response_id,
            object: "response".to_string(),
            created_at: now.timestamp(),
            status: ResponseStatus::InProgress,
            background: request.background.unwrap_or(false),
            conversation: conversation_ref,
            error: None,
            incomplete_details: None,
            instructions: request.instructions,
            max_output_tokens: request.max_output_tokens,
            max_tool_calls: request.max_tool_calls,
            model: request.model,
            output: vec![],
            parallel_tool_calls: request.parallel_tool_calls.unwrap_or(false),
            previous_response_id: request.previous_response_id.clone(),
            next_response_ids: vec![],
            prompt_cache_key: request.prompt_cache_key,
            prompt_cache_retention: None,
            reasoning: None,
            safety_identifier: request.safety_identifier,
            service_tier: "default".to_string(),
            store: request.store.unwrap_or(false),
            temperature: request.temperature.unwrap_or(1.0),
            tool_choice: ResponseToolChoiceOutput::Auto("auto".to_string()),
            tools: request.tools.clone().unwrap_or_else(|| {
                vec![ResponseTool::WebSearch {
                    filters: None,
                    search_context_size: Some("medium".to_string()),
                    user_location: Some(UserLocation {
                        type_: "approximate".to_string(),
                        city: None,
                        country: Some("US".to_string()),
                        region: None,
                        timezone: None,
                    }),
                }]
            }),
            top_logprobs: 0,
            top_p: request.top_p.unwrap_or(1.0),
            truncation: "disabled".to_string(),
            usage: Usage::new(0, 0),
            user: None,
            metadata: Some(metadata_json),
        };

        tracing::info!(
            "Created response {} for workspace {} with api_key {}",
            response_obj.id,
            workspace_id.0,
            api_key_id
        );
        Ok(response_obj)
    }

    async fn get_by_id(
        &self,
        response_id: ResponseId,
        workspace_id: WorkspaceId,
    ) -> Result<Option<ResponseObject>, anyhow::Error> {
        let response_uuid = response_id.0;

        // Fetch the response
        let row_result = retry_db!("get_response_by_id", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
                    SELECT id, workspace_id, api_key_id, model, status, instructions,
                           conversation_id, previous_response_id, next_response_ids,
                           usage, metadata, created_at, updated_at
                    FROM responses
                    WHERE id = $1 AND workspace_id = $2
                    "#,
                    &[&response_uuid, &workspace_id.0],
                )
                .await
                .map_err(map_db_error)
        })?;

        let Some(row) = row_result else {
            return Ok(None);
        };

        let status = match row.get::<_, String>("status").as_str() {
            "in_progress" => ResponseStatus::InProgress,
            "completed" => ResponseStatus::Completed,
            "failed" => ResponseStatus::Failed,
            "cancelled" => ResponseStatus::Cancelled,
            "queued" => ResponseStatus::Queued,
            "incomplete" => ResponseStatus::Incomplete,
            _ => ResponseStatus::Failed,
        };

        let created_at: DateTime<Utc> = row.get("created_at");
        let conversation_uuid: Option<Uuid> = row.get("conversation_id");
        let usage_json: Option<serde_json::Value> = row.get("usage");
        let metadata_json: Option<serde_json::Value> = row.get("metadata");
        let model: String = row.get("model");
        let instructions: Option<String> = row.get("instructions");
        let previous_response_uuid: Option<Uuid> = row.get("previous_response_id");
        let next_response_ids_json: Option<serde_json::Value> = row.get("next_response_ids");

        // Parse next_response_ids from JSON array to Vec<String>
        let next_response_ids = if let Some(next_ids_val) = next_response_ids_json {
            serde_json::from_value::<Vec<String>>(next_ids_val)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|uuid_str| {
                    Uuid::parse_str(&uuid_str)
                        .ok()
                        .map(|uuid| format!("resp_{}", uuid.simple()))
                })
                .collect()
        } else {
            vec![]
        };

        // Build conversation reference if conversation_id is present
        let conversation_ref = conversation_uuid.map(|uuid| ConversationResponseReference {
            id: format!("conv_{}", uuid.simple()),
        });

        // Parse usage from JSON
        let usage = if let Some(usage_val) = usage_json {
            serde_json::from_value(usage_val).unwrap_or_else(|_| Usage::new(0, 0))
        } else {
            Usage::new(0, 0)
        };

        // Build ResponseObject from the database row
        let response_obj = ResponseObject {
            id: format!("resp_{}", response_uuid.simple()),
            object: "response".to_string(),
            created_at: created_at.timestamp(),
            status,
            background: false, // Default, not stored in DB
            conversation: conversation_ref,
            error: None,
            incomplete_details: None,
            instructions,
            max_output_tokens: None,
            max_tool_calls: None,
            model,
            output: vec![], // Would need to fetch from response_items if needed
            parallel_tool_calls: false,
            previous_response_id: previous_response_uuid
                .map(|uuid| format!("resp_{}", uuid.simple())),
            next_response_ids,
            prompt_cache_key: None,
            prompt_cache_retention: None,
            reasoning: None,
            safety_identifier: None,
            service_tier: "default".to_string(),
            store: false,
            temperature: 1.0,
            tool_choice: ResponseToolChoiceOutput::Auto("auto".to_string()),
            tools: vec![],
            top_logprobs: 0,
            top_p: 1.0,
            truncation: "disabled".to_string(),
            usage,
            user: None,
            metadata: metadata_json,
        };

        Ok(Some(response_obj))
    }

    async fn update(
        &self,
        response_id: ResponseId,
        workspace_id: WorkspaceId,
        _output_message: Option<String>, // Not used - messages stored as response_items
        status: ResponseStatus,
        usage: Option<serde_json::Value>,
    ) -> Result<Option<ResponseObject>, anyhow::Error> {
        let response_uuid = response_id.0;
        let status_str = match status {
            ResponseStatus::InProgress => "in_progress",
            ResponseStatus::Completed => "completed",
            ResponseStatus::Failed => "failed",
            ResponseStatus::Cancelled => "cancelled",
            ResponseStatus::Queued => "queued",
            ResponseStatus::Incomplete => "incomplete",
        };

        // Update the response in the database
        // Note: output_message column was removed in migration V0021
        // Messages are now stored as response_items instead
        let rows_affected = retry_db!("update_response", {
            let now = Utc::now();
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute(
                    r#"
                    UPDATE responses
                    SET status = $1,
                        usage = COALESCE($2, usage),
                        updated_at = $3
                    WHERE id = $4 AND workspace_id = $5
                    "#,
                    &[&status_str, &usage, &now, &response_uuid, &workspace_id.0],
                )
                .await
                .map_err(map_db_error)
        })?;

        if rows_affected == 0 {
            // Response not found or doesn't belong to this workspace
            return Ok(None);
        }

        // Fetch the updated response
        let row = retry_db!("fetch_updated_response", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    r#"
                    SELECT id, workspace_id, api_key_id, model, status, instructions,
                           conversation_id, previous_response_id, next_response_ids,
                           usage, metadata, created_at, updated_at
                    FROM responses
                    WHERE id = $1 AND workspace_id = $2
                    "#,
                    &[&response_uuid, &workspace_id.0],
                )
                .await
                .map_err(map_db_error)
        })?;

        // Parse usage from JSONB
        let usage_value: Option<serde_json::Value> = row.get(9);
        let usage_obj = if let Some(usage_json) = usage_value {
            serde_json::from_value(usage_json.clone()).unwrap_or_else(|_| {
                // Fallback to default if deserialization fails
                Usage::new(0, 0)
            })
        } else {
            Usage::new(0, 0)
        };

        // Parse conversation_id
        let conversation_uuid: Option<Uuid> = row.get(6);
        let conversation_ref = conversation_uuid.map(|uuid| ConversationResponseReference {
            id: format!("conv_{}", uuid.simple()),
        });

        // Parse next_response_ids
        let next_response_ids_json: Option<serde_json::Value> = row.get(8);
        let next_response_ids = if let Some(next_ids_val) = next_response_ids_json {
            serde_json::from_value::<Vec<String>>(next_ids_val)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|uuid_str| {
                    Uuid::parse_str(&uuid_str)
                        .ok()
                        .map(|uuid| format!("resp_{}", uuid.simple()))
                })
                .collect()
        } else {
            vec![]
        };

        // Parse metadata
        let metadata_value: Option<serde_json::Value> = row.get(10);
        let metadata = metadata_value;

        // Parse status
        let status_str: String = row.get(4);
        let response_status = match status_str.as_str() {
            "in_progress" => ResponseStatus::InProgress,
            "completed" => ResponseStatus::Completed,
            "failed" => ResponseStatus::Failed,
            "cancelled" => ResponseStatus::Cancelled,
            _ => ResponseStatus::InProgress,
        };

        let response_obj = ResponseObject {
            id: format!("resp_{}", response_uuid.simple()),
            object: "response".to_string(),
            created_at: row.get::<_, chrono::DateTime<Utc>>(11).timestamp(),
            status: response_status,
            background: false, // Not stored in DB, default value
            conversation: conversation_ref,
            error: None,
            incomplete_details: None,
            instructions: row.get(5),
            max_output_tokens: None, // Not stored in DB
            max_tool_calls: None,    // Not stored in DB
            model: row.get(3),
            output: vec![],             // Output items are stored separately
            parallel_tool_calls: false, // Not stored in DB
            previous_response_id: row
                .get::<_, Option<Uuid>>(7)
                .map(|uuid| format!("resp_{}", uuid.simple())),
            next_response_ids,
            prompt_cache_key: None, // Not stored in DB
            prompt_cache_retention: None,
            reasoning: None,
            safety_identifier: None, // Not stored in DB
            service_tier: "default".to_string(),
            store: false,     // Not stored in DB
            temperature: 1.0, // Not stored in DB
            tool_choice: ResponseToolChoiceOutput::Auto("auto".to_string()),
            tools: vec![], // Not stored in DB
            top_logprobs: 0,
            top_p: 1.0, // Not stored in DB
            truncation: "disabled".to_string(),
            usage: usage_obj,
            user: None,
            metadata,
        };

        tracing::debug!(
            "Updated response {} with status={}, usage={:?}",
            response_obj.id,
            status_str,
            usage
        );

        Ok(Some(response_obj))
    }

    async fn delete(
        &self,
        _response_id: ResponseId,
        _workspace_id: WorkspaceId,
    ) -> Result<bool, anyhow::Error> {
        unimplemented!("delete not yet implemented")
    }

    async fn cancel(
        &self,
        _response_id: ResponseId,
        _workspace_id: WorkspaceId,
    ) -> Result<Option<ResponseObject>, anyhow::Error> {
        unimplemented!("cancel not yet implemented")
    }

    async fn list_by_workspace(
        &self,
        _workspace_id: WorkspaceId,
        _limit: i64,
        _offset: i64,
    ) -> Result<Vec<ResponseObject>, anyhow::Error> {
        unimplemented!("list_by_workspace not yet implemented")
    }

    async fn list_by_conversation(
        &self,
        _conversation_id: services::conversations::models::ConversationId,
        _workspace_id: WorkspaceId,
        _limit: i64,
    ) -> Result<Vec<ResponseObject>, anyhow::Error> {
        unimplemented!("list_by_conversation not yet implemented")
    }

    async fn get_previous(
        &self,
        _response_id: ResponseId,
        _workspace_id: WorkspaceId,
    ) -> Result<Option<ResponseObject>, anyhow::Error> {
        unimplemented!("get_previous not yet implemented")
    }

    async fn get_latest_in_conversation(
        &self,
        conversation_id: services::conversations::models::ConversationId,
        workspace_id: WorkspaceId,
    ) -> Result<Option<ResponseObject>, anyhow::Error> {
        // Fetch the most recent response in this conversation
        let row_result = retry_db!("get_latest_response_in_conversation", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
                    SELECT id, workspace_id, api_key_id, model, status, instructions,
                           conversation_id, previous_response_id, next_response_ids,
                           usage, metadata, created_at, updated_at
                    FROM responses
                    WHERE conversation_id = $1 AND workspace_id = $2
                    ORDER BY created_at DESC
                    LIMIT 1
                    "#,
                    &[&conversation_id.0, &workspace_id.0],
                )
                .await
                .map_err(map_db_error)
        })?;

        let Some(row) = row_result else {
            return Ok(None);
        };

        let response_uuid: Uuid = row.get("id");
        let status = match row.get::<_, String>("status").as_str() {
            "in_progress" => ResponseStatus::InProgress,
            "completed" => ResponseStatus::Completed,
            "failed" => ResponseStatus::Failed,
            "cancelled" => ResponseStatus::Cancelled,
            "queued" => ResponseStatus::Queued,
            "incomplete" => ResponseStatus::Incomplete,
            _ => ResponseStatus::Failed,
        };

        let created_at: DateTime<Utc> = row.get("created_at");
        let conversation_uuid: Option<Uuid> = row.get("conversation_id");
        let usage_json: Option<serde_json::Value> = row.get("usage");
        let metadata_json: Option<serde_json::Value> = row.get("metadata");
        let model: String = row.get("model");
        let instructions: Option<String> = row.get("instructions");
        let previous_response_uuid: Option<Uuid> = row.get("previous_response_id");
        let next_response_ids_json: Option<serde_json::Value> = row.get("next_response_ids");

        // Parse next_response_ids from JSON array to Vec<String>
        let next_response_ids = if let Some(next_ids_val) = next_response_ids_json {
            serde_json::from_value::<Vec<String>>(next_ids_val)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|uuid_str| {
                    Uuid::parse_str(&uuid_str)
                        .ok()
                        .map(|uuid| format!("resp_{}", uuid.simple()))
                })
                .collect()
        } else {
            vec![]
        };

        // Build conversation reference if conversation_id is present
        let conversation_ref = conversation_uuid.map(|uuid| ConversationResponseReference {
            id: format!("conv_{}", uuid.simple()),
        });

        // Parse usage from JSON
        let usage = if let Some(usage_val) = usage_json {
            serde_json::from_value(usage_val).unwrap_or_else(|_| Usage::new(0, 0))
        } else {
            Usage::new(0, 0)
        };

        // Build ResponseObject from the database row
        let response_obj = ResponseObject {
            id: format!("resp_{}", response_uuid.simple()),
            object: "response".to_string(),
            created_at: created_at.timestamp(),
            status,
            background: false,
            conversation: conversation_ref,
            error: None,
            incomplete_details: None,
            instructions,
            max_output_tokens: None,
            max_tool_calls: None,
            model,
            output: vec![],
            parallel_tool_calls: false,
            previous_response_id: previous_response_uuid
                .map(|uuid| format!("resp_{}", uuid.simple())),
            next_response_ids,
            prompt_cache_key: None,
            prompt_cache_retention: None,
            reasoning: None,
            safety_identifier: None,
            service_tier: "default".to_string(),
            store: false,
            temperature: 1.0,
            tool_choice: ResponseToolChoiceOutput::Auto("auto".to_string()),
            tools: vec![],
            top_logprobs: 0,
            top_p: 1.0,
            truncation: "disabled".to_string(),
            usage,
            user: None,
            metadata: metadata_json,
        };

        Ok(Some(response_obj))
    }
}
