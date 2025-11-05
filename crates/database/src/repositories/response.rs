use crate::pool::DbPool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use services::responses::models::*;
use services::{responses::ports::*, UserId};
use uuid::Uuid;

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
        user_id: UserId,
        request: CreateResponseRequest,
    ) -> Result<ResponseObject, anyhow::Error> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        // Generate new response ID
        let response_uuid = Uuid::new_v4();
        let response_id = format!("resp_{}", response_uuid.simple());
        let now = Utc::now();

        // Extract conversation_id if present
        let conversation_uuid = if let Some(conv_ref) = &request.conversation {
            match conv_ref {
                ConversationReference::Id(id) => {
                    let uuid_str = id.strip_prefix("conv_").unwrap_or(id);
                    Some(Uuid::parse_str(uuid_str).context("Invalid conversation ID")?)
                }
                ConversationReference::Object { id, .. } => {
                    let uuid_str = id.strip_prefix("conv_").unwrap_or(id);
                    Some(Uuid::parse_str(uuid_str).context("Invalid conversation ID")?)
                }
            }
        } else {
            None
        };

        // Extract previous_response_id if present
        let previous_response_uuid = if let Some(prev_id) = &request.previous_response_id {
            let uuid_str = prev_id.strip_prefix("resp_").unwrap_or(prev_id);
            Some(Uuid::parse_str(uuid_str).context("Invalid previous response ID")?)
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

        // Insert response into database (without input_messages or output_message)
        // Messages are stored separately as response_items
        client
            .execute(
                r#"
                INSERT INTO responses (
                    id, user_id, model, status, instructions, conversation_id, 
                    previous_response_id, usage, metadata, created_at, updated_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                "#,
                &[
                    &response_uuid,
                    &user_id.0,
                    &request.model,
                    &status,
                    &request.instructions,
                    &conversation_uuid,
                    &previous_response_uuid,
                    &usage_json,
                    &metadata_json,
                    &now,
                    &now,
                ],
            )
            .await
            .context("Failed to insert response")?;

        // Build conversation reference if conversation_id is present
        let conversation_ref = conversation_uuid.map(|uuid| ConversationResponseReference {
            id: format!("conv_{}", uuid.simple()),
        });

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
            previous_response_id: request.previous_response_id,
            prompt_cache_key: request.prompt_cache_key,
            prompt_cache_retention: None,
            reasoning: None,
            safety_identifier: request.safety_identifier,
            service_tier: "default".to_string(),
            store: request.store.unwrap_or(false),
            temperature: request.temperature.unwrap_or(1.0),
            text: request.text.or(Some(ResponseTextConfig {
                format: ResponseTextFormat::Text,
                verbosity: Some("medium".to_string()),
            })),
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
            "Created response {} for user {}",
            response_obj.id,
            user_id.0
        );
        Ok(response_obj)
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
