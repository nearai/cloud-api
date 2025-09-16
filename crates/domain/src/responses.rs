use crate::{
    models::{ChatCompletionParams, ChatMessage, MessageRole, TokenUsage},
    services::CompletionHandler,
    errors::CompletionError,
    conversations::ConversationService,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::{DateTime, Utc};
use std::sync::Arc;
use futures::stream::{Stream, StreamExt};
use std::pin::Pin;

/// Domain model for a response request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseRequest {
    pub model: String,
    pub input: Option<ResponseInput>,
    pub instructions: Option<String>,
    pub conversation_id: Option<String>,
    pub previous_response_id: Option<String>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub user_id: String,
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

/// Domain model for a stored response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub user_id: String,
    pub model: String,
    pub input_messages: Vec<ResponseMessage>,
    pub output_message: Option<String>,
    pub status: ResponseStatus,
    pub instructions: Option<String>,
    pub conversation_id: Option<String>,
    pub previous_response_id: Option<String>,
    pub usage: Option<TokenUsage>,
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseStatus {
    InProgress,
    Completed,
    Failed,
    Cancelled,
}

/// Streaming event for response API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseStreamEvent {
    pub event_name: String,
    pub data: serde_json::Value,
}

/// Response service for managing responses
pub struct ResponseService {
    completion_handler: Arc<dyn CompletionHandler>,
    database: Option<Arc<database::Database>>,
    conversation_service: Arc<ConversationService>,
}

impl ResponseService {
    pub fn new(completion_handler: Arc<dyn CompletionHandler>, database: Option<Arc<database::Database>>, conversation_service: Arc<ConversationService>) -> Self {
        Self {
            completion_handler,
            database,
            conversation_service,
        }
    }

    /// Helper: Prepare input and LLM context messages
    async fn prepare_messages(&self, request: &ResponseRequest) -> Result<(Vec<ResponseMessage>, Vec<ResponseMessage>), CompletionError> {
        // Convert input to messages (these are the NEW messages for this response only)
        let input_messages = match &request.input {
            Some(ResponseInput::Text(text)) => {
                vec![ResponseMessage {
                    role: "user".to_string(),
                    content: text.clone(),
                }]
            }
            Some(ResponseInput::Messages(messages)) => messages.clone(),
            None => vec![],
        };

        // For LLM context, build full conversation history if conversation_id is provided
        let llm_context_messages = if let Some(conversation_id) = &request.conversation_id {
            // Fetch existing conversation messages for LLM context
            match self.conversation_service.get_conversation_messages(conversation_id, &request.user_id, None).await {
                Ok(conversation_messages) => {
                    // Convert conversation messages to response messages
                    let mut all_messages: Vec<ResponseMessage> = conversation_messages
                        .into_iter()
                        .map(|msg| ResponseMessage {
                            role: msg.role,
                            content: msg.content,
                        })
                        .collect();
                    
                    // Add the new input messages to the end for LLM context
                    all_messages.extend(input_messages.clone());
                    all_messages
                }
                Err(e) => {
                    // If we can't fetch conversation history, log the error and use just the current input
                    tracing::warn!("Failed to fetch conversation history for {}: {}", conversation_id, e);
                    input_messages.clone()
                }
            }
        } else {
            // No conversation context, just use the current input
            input_messages.clone()
        };

        Ok((input_messages, llm_context_messages))
    }

    /// Helper: Convert messages to chat format for LLM
    fn prepare_chat_messages(&self, request: &ResponseRequest, llm_context_messages: &[ResponseMessage]) -> Vec<ChatMessage> {
        let mut chat_messages = vec![];
        
        // Add system message if instructions provided
        if let Some(instructions) = &request.instructions {
            chat_messages.push(ChatMessage {
                role: MessageRole::System,
                content: Some(instructions.clone()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            });
        }

        // Add LLM context messages (full conversation history + new input)
        for msg in llm_context_messages {
            let role = match msg.role.as_str() {
                "system" => MessageRole::System,
                "assistant" => MessageRole::Assistant,
                "tool" => MessageRole::Tool,
                _ => MessageRole::User,
            };
            chat_messages.push(ChatMessage {
                role,
                content: Some(msg.content.clone()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            });
        }

        chat_messages
    }

    /// Helper: Create database response entry
    async fn create_database_response(&self, request: &ResponseRequest, input_messages: &[ResponseMessage]) -> Result<(String, Option<Uuid>), CompletionError> {
        // Prepare input messages as JSON for database
        let input_messages_json = serde_json::to_value(input_messages)
            .map_err(|e| CompletionError::InternalError(format!("Failed to serialize messages: {}", e)))?;

        if let Some(ref db) = self.database {
            let user_uuid = parse_uuid(&request.user_id)?;
            let conversation_uuid = request.conversation_id.as_ref()
                .map(|id| parse_uuid_from_prefixed(id, "conv_"))
                .transpose()?;
            let previous_response_uuid = request.previous_response_id.as_ref()
                .map(|id| parse_uuid_from_prefixed(id, "resp_"))
                .transpose()?;
            
            let db_response = db.responses.create(
                user_uuid,
                request.model.clone(),
                input_messages_json,
                request.instructions.clone(),
                conversation_uuid,
                previous_response_uuid,
                request.metadata.clone(),
            ).await
            .map_err(|e| CompletionError::InternalError(format!("Failed to create response: {}", e)))?;
            
            Ok((format!("resp_{}", db_response.id), Some(db_response.id)))
        } else {
            Ok((format!("resp_{}", Uuid::new_v4()), None))
        }
    }

    /// Create a new response
    pub async fn create_response(&self, request: ResponseRequest) -> Result<Response, CompletionError> {
        // Prepare messages
        let (input_messages, llm_context_messages) = self.prepare_messages(&request).await?;
        
        // Create response in database
        let (response_id, db_response_id) = self.create_database_response(&request, &input_messages).await?;

        let now = Utc::now();

        let mut response = Response {
            id: response_id.clone(),
            user_id: request.user_id.clone(),
            model: request.model.clone(),
            input_messages: input_messages.clone(),
            output_message: None,
            status: ResponseStatus::InProgress,
            instructions: request.instructions.clone(),
            conversation_id: request.conversation_id.clone(),
            previous_response_id: request.previous_response_id.clone(),
            usage: None,
            metadata: request.metadata.clone(),
            created_at: now,
            updated_at: now,
        };

        // Prepare chat messages for LLM
        let chat_messages = self.prepare_chat_messages(&request, &llm_context_messages);

        let chat_params = ChatCompletionParams {
            model_id: request.model,
            messages: chat_messages,
            max_tokens: request.max_output_tokens,
            temperature: request.temperature,
            top_p: request.top_p,
            stop_sequences: None,
            stream: None,
            tools: None,
        };

        // Generate completion using AI
        let completion_result = self.completion_handler.chat_completion(chat_params).await?;

        // Update response with AI output
        response.output_message = completion_result.message.content.clone();
        response.status = ResponseStatus::Completed;
        response.usage = Some(completion_result.usage.clone());
        response.updated_at = Utc::now();

        // Update database if available
        if let Some(ref db) = self.database {
            if let Some(db_id) = db_response_id {
                let user_uuid = Uuid::parse_str(&request.user_id)
                    .map_err(|_| CompletionError::InvalidParams("Invalid user ID".to_string()))?;
                
                let usage_json = serde_json::to_value(&completion_result.usage)
                    .map_err(|e| CompletionError::InternalError(format!("Failed to serialize usage: {}", e)))?;
                
                let db_status = database::ResponseStatus::Completed;
                
                db.responses.update(
                    db_id,
                    user_uuid,
                    completion_result.message.content,
                    db_status,
                    Some(usage_json),
                ).await
                .map_err(|e| CompletionError::InternalError(format!("Failed to update response: {}", e)))?;
            }
        }

        Ok(response)
    }

    /// Create a new response with streaming
    pub async fn create_response_stream(
        &self, 
        request: ResponseRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = ResponseStreamEvent> + Send>>, CompletionError> {
        // Prepare messages
        let (input_messages, llm_context_messages) = self.prepare_messages(&request).await?;
        
        // Create response in database BEFORE streaming starts
        let (response_id, db_response_id) = self.create_database_response(&request, &input_messages).await?;

        // Prepare chat messages for LLM
        let chat_messages = self.prepare_chat_messages(&request, &llm_context_messages);

        let chat_params = ChatCompletionParams {
            model_id: request.model.clone(),
            messages: chat_messages,
            max_tokens: request.max_output_tokens,
            temperature: request.temperature,
            top_p: request.top_p,
            stop_sequences: None,
            stream: Some(true),
            tools: None,
        };

        // Get timestamp for response
        let created_at = Utc::now().timestamp() as u64;
        
        // Clone database reference and user_id for the stream closure
        let database = self.database.clone();
        let user_id = request.user_id.clone();
        
        // Get the LLM stream
        let llm_stream = self.completion_handler.chat_completion_stream(chat_params).await?;
        
        // Convert LLM stream to response stream events
        let event_stream = futures::stream::unfold(
            (llm_stream, response_id, created_at, String::new(), false, false, false, false, None::<TokenUsage>, database, db_response_id, user_id),
            move |(mut stream, resp_id, created, mut accumulated_text, mut sent_created, mut sent_in_progress, mut sent_item_added, mut sent_content_added, mut final_usage, db, db_id, user_id_str)| {
                let req = request.clone();
                async move {
                    // Helper to create response object
                    let create_response_obj = |status: &str, text: Option<&str>, usage: Option<&TokenUsage>| {
                        serde_json::json!({
                            "id": resp_id,
                            "object": "response",
                            "created_at": created,
                            "status": status,
                            "error": null,
                            "incomplete_details": null,
                            "instructions": req.instructions.as_ref(),
                            "max_output_tokens": req.max_output_tokens,
                            "model": req.model.as_str(),
                            "output": if text.is_some() && !text.unwrap().is_empty() {
                                vec![serde_json::json!({
                                    "id": format!("msg_{}", Uuid::new_v4()),
                                    "type": "message",
                                    "status": if status == "completed" { "completed" } else { "in_progress" },
                                    "role": "assistant",
                                    "content": vec![serde_json::json!({
                                        "type": "output_text",
                                        "text": text.unwrap_or(""),
                                        "annotations": []
                                    })]
                                })]
                            } else {
                                vec![]
                            },
                            "parallel_tool_calls": true,
                            "previous_response_id": req.previous_response_id.as_ref(),
                            "reasoning": {
                                "effort": null,
                                "summary": null
                            },
                            "store": true,
                            "temperature": req.temperature.unwrap_or(1.0),
                            "text": {
                                "format": {
                                    "type": "text"
                                }
                            },
                            "tool_choice": "auto",
                            "tools": [],
                            "top_p": req.top_p.unwrap_or(1.0),
                            "truncation": "disabled",
                            "usage": usage.map(|u| serde_json::json!({
                                "input_tokens": u.prompt_tokens,
                                "output_tokens": u.completion_tokens,
                                "output_tokens_details": {
                                    "reasoning_tokens": 0
                                },
                                "total_tokens": u.total_tokens
                            })),
                            "user": null,
                            "metadata": {}
                        })
                    };

                    // Send initial events if not sent yet
                    if !sent_created {
                        sent_created = true;
                        return Some((
                            ResponseStreamEvent {
                                event_name: "response.created".to_string(),
                                data: serde_json::json!({
                                    "type": "response.created",
                                    "response": create_response_obj("in_progress", None, None)
                                })
                            },
                            (stream, resp_id, created, accumulated_text, sent_created, sent_in_progress, sent_item_added, sent_content_added, final_usage, db, db_id, user_id_str)
                        ));
                    }

                    if !sent_in_progress {
                        sent_in_progress = true;
                        return Some((
                            ResponseStreamEvent {
                                event_name: "response.in_progress".to_string(),
                                data: serde_json::json!({
                                    "type": "response.in_progress",
                                    "response": create_response_obj("in_progress", None, None)
                                })
                            },
                            (stream, resp_id, created, accumulated_text, sent_created, sent_in_progress, sent_item_added, sent_content_added, final_usage, db, db_id, user_id_str)
                        ));
                    }

                    if !sent_item_added {
                        sent_item_added = true;
                        let msg_id = format!("msg_{}", Uuid::new_v4());
                        return Some((
                            ResponseStreamEvent {
                                event_name: "response.output_item.added".to_string(),
                                data: serde_json::json!({
                                    "type": "response.output_item.added",
                                    "output_index": 0,
                                    "item": {
                                        "id": msg_id,
                                        "type": "message",
                                        "status": "in_progress",
                                        "role": "assistant",
                                        "content": []
                                    }
                                })
                            },
                            (stream, resp_id, created, accumulated_text, sent_created, sent_in_progress, sent_item_added, sent_content_added, final_usage, db, db_id, user_id_str)
                        ));
                    }

                    if !sent_content_added {
                        sent_content_added = true;
                        let msg_id = format!("msg_{}", Uuid::new_v4());
                        return Some((
                            ResponseStreamEvent {
                                event_name: "response.content_part.added".to_string(),
                                data: serde_json::json!({
                                    "type": "response.content_part.added",
                                    "item_id": msg_id,
                                    "output_index": 0,
                                    "content_index": 0,
                                    "part": {
                                        "type": "output_text",
                                        "text": "",
                                        "annotations": []
                                    }
                                })
                            },
                            (stream, resp_id, created, accumulated_text, sent_created, sent_in_progress, sent_item_added, sent_content_added, final_usage, db, db_id, user_id_str)
                        ));
                    }

                    // Process chunks from LLM stream
                    match stream.next().await {
                        Some(Ok(chunk)) => {
                            // Extract delta content from the chunk
                            let delta_content = chunk.choices.first()
                                .and_then(|choice| choice.delta.content.as_ref())
                                .map(|s| s.as_str())
                                .unwrap_or("");
                            
                            if !delta_content.is_empty() {
                                accumulated_text.push_str(delta_content);
                                let msg_id = format!("msg_{}", Uuid::new_v4());
                                Some((
                                    ResponseStreamEvent {
                                        event_name: "response.output_text.delta".to_string(),
                                        data: serde_json::json!({
                                            "type": "response.output_text.delta",
                                            "item_id": msg_id,
                                            "output_index": 0,
                                            "content_index": 0,
                                            "delta": delta_content
                                        })
                                    },
                                    (stream, resp_id, created, accumulated_text, sent_created, sent_in_progress, sent_item_added, sent_content_added, final_usage, db, db_id, user_id_str)
                                ))
                            } else if chunk.usage.is_some() {
                                // Final chunk with usage - update database and send completion event
                                final_usage = chunk.usage.clone();
                                
                                // Update database with completion
                                if let (Some(ref database), Some(id)) = (&db, db_id) {
                                    if let Ok(user_uuid) = Uuid::parse_str(&user_id_str) {
                                        if let Some(ref usage) = final_usage {
                                            if let Ok(usage_json) = serde_json::to_value(usage) {
                                                let _ = database.responses.update(
                                                    id,
                                                    user_uuid,
                                                    Some(accumulated_text.clone()),
                                                    database::ResponseStatus::Completed,
                                                    Some(usage_json),
                                                ).await;
                                            }
                                        }
                                    }
                                }
                                
                                Some((
                                    ResponseStreamEvent {
                                        event_name: "response.completed".to_string(),
                                        data: serde_json::json!({
                                            "type": "response.completed",
                                            "response": create_response_obj("completed", Some(&accumulated_text), final_usage.as_ref())
                                        })
                                    },
                                    (stream, resp_id, created, accumulated_text, sent_created, sent_in_progress, sent_item_added, sent_content_added, final_usage, db, db_id, user_id_str)
                                ))
                            } else {
                                // Continue to next chunk
                                Some((
                                    ResponseStreamEvent {
                                        event_name: "response.output_text.delta".to_string(),
                                        data: serde_json::json!({
                                            "type": "response.output_text.delta",
                                            "item_id": format!("msg_{}", Uuid::new_v4()),
                                            "output_index": 0,
                                            "content_index": 0,
                                            "delta": ""
                                        })
                                    },
                                    (stream, resp_id, created, accumulated_text, sent_created, sent_in_progress, sent_item_added, sent_content_added, final_usage, db, db_id, user_id_str)
                                ))
                            }
                        }
                        Some(Err(e)) => {
                            // Error occurred - update database with failed status
                            if let (Some(ref database), Some(id)) = (&db, db_id) {
                                if let Ok(user_uuid) = Uuid::parse_str(&user_id_str) {
                                    let _ = database.responses.update(
                                        id,
                                        user_uuid,
                                        Some(format!("Error: {}", e)),
                                        database::ResponseStatus::Failed,
                                        None,
                                    ).await;
                                }
                            }
                            None
                        }
                        None => {
                            // Stream ended normally without a usage chunk (shouldn't happen but handle gracefully)
                            // Update database if we haven't already
                            if let (Some(ref database), Some(id)) = (&db, db_id) {
                                if let Ok(user_uuid) = Uuid::parse_str(&user_id_str) {
                                    if final_usage.is_none() && !accumulated_text.is_empty() {
                                        // Mark as completed even without usage data
                                        let _ = database.responses.update(
                                            id,
                                            user_uuid,
                                            Some(accumulated_text.clone()),
                                            database::ResponseStatus::Completed,
                                            None,
                                        ).await;
                                    }
                                }
                            }
                            None
                        }
                    }
                }
            }
        );

        Ok(Box::pin(event_stream))
    }

    /// Get a response by ID
    pub async fn get_response(&self, response_id: &str, user_id: &str) -> Result<Option<Response>, CompletionError> {
        if let Some(ref db) = self.database {
            let resp_uuid = parse_uuid_from_prefixed(response_id, "resp_")?;
            let user_uuid = parse_uuid(user_id)?;
            
            let db_response = db.responses.get_by_id(resp_uuid, user_uuid).await
                .map_err(|e| CompletionError::InternalError(format!("Failed to get response: {}", e)))?;
            
            Ok(db_response.map(|r| {
                // Deserialize input messages
                let input_messages: Vec<ResponseMessage> = serde_json::from_value(r.input_messages)
                    .unwrap_or_else(|_| vec![]);
                
                // Deserialize usage
                let usage: Option<TokenUsage> = r.usage.and_then(|u| serde_json::from_value(u).ok());
                
                // Map database status to domain status
                let status = match r.status {
                    database::ResponseStatus::InProgress => ResponseStatus::InProgress,
                    database::ResponseStatus::Completed => ResponseStatus::Completed,
                    database::ResponseStatus::Failed => ResponseStatus::Failed,
                    database::ResponseStatus::Cancelled => ResponseStatus::Cancelled,
                };
                
                Response {
                    id: format!("resp_{}", r.id),
                    user_id: r.user_id.to_string(),
                    model: r.model,
                    input_messages,
                    output_message: r.output_message,
                    status,
                    instructions: r.instructions,
                    conversation_id: r.conversation_id.map(|id| format!("conv_{}", id)),
                    previous_response_id: r.previous_response_id.map(|id| format!("resp_{}", id)),
                    usage,
                    metadata: r.metadata,
                    created_at: r.created_at,
                    updated_at: r.updated_at,
                }
            }))
        } else {
            // Mock implementation without database
            Ok(None)
        }
    }

    /// Delete a response
    pub async fn delete_response(&self, response_id: &str, user_id: &str) -> Result<bool, CompletionError> {
        if let Some(ref db) = self.database {
            let resp_uuid = parse_uuid_from_prefixed(response_id, "resp_")?;
            let user_uuid = parse_uuid(user_id)?;
            
            db.responses.delete(resp_uuid, user_uuid).await
                .map_err(|e| CompletionError::InternalError(format!("Failed to delete response: {}", e)))
        } else {
            // Mock implementation
            Ok(true)
        }
    }

    /// Cancel a response
    pub async fn cancel_response(&self, response_id: &str, user_id: &str) -> Result<Option<Response>, CompletionError> {
        if let Some(ref db) = self.database {
            let resp_uuid = parse_uuid_from_prefixed(response_id, "resp_")?;
            let user_uuid = parse_uuid(user_id)?;
            
            let db_response = db.responses.cancel(resp_uuid, user_uuid).await
                .map_err(|e| CompletionError::InternalError(format!("Failed to cancel response: {}", e)))?;
            
            // Return the cancelled response in domain format
            Ok(db_response.map(|r| {
                // Deserialize input messages
                let input_messages: Vec<ResponseMessage> = serde_json::from_value(r.input_messages)
                    .unwrap_or_else(|_| vec![]);
                
                // Deserialize usage
                let usage: Option<TokenUsage> = r.usage.and_then(|u| serde_json::from_value(u).ok());
                
                Response {
                    id: format!("resp_{}", r.id),
                    user_id: r.user_id.to_string(),
                    model: r.model,
                    input_messages,
                    output_message: r.output_message,
                    status: ResponseStatus::Cancelled,
                    instructions: r.instructions,
                    conversation_id: r.conversation_id.map(|id| format!("conv_{}", id)),
                    previous_response_id: r.previous_response_id.map(|id| format!("resp_{}", id)),
                    usage,
                    metadata: r.metadata,
                    created_at: r.created_at,
                    updated_at: r.updated_at,
                }
            }))
        } else {
            // Mock implementation
            Ok(None)
        }
    }
}

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