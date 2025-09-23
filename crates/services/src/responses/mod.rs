pub mod ports;

use crate::conversations::ports::ConversationService;
use crate::responses::ports::{
    ResponseId, ResponseInput, ResponseMessage, ResponseRequest, ResponseStreamEvent,
};
use crate::{
    inference_provider_pool::InferenceProviderPool, responses::ports::ResponseError, UserId,
};
use futures::Stream;
use inference_providers::{
    ChatCompletionParams, ChatMessage, InferenceProvider, MessageRole, StreamChunk,
};
use std::{pin::Pin, sync::Arc};
use uuid::Uuid;

pub struct ResponseService {
    pub response_repository: Arc<dyn ports::ResponseRepository>,
    pub inference_provider_pool: Arc<InferenceProviderPool>,
    pub conversation_service: Arc<ConversationService>,
}

impl ResponseService {
    pub fn new(
        response_repository: Arc<dyn ports::ResponseRepository>,
        inference_provider_pool: Arc<InferenceProviderPool>,
        conversation_service: Arc<ConversationService>,
    ) -> Self {
        Self {
            response_repository,
            inference_provider_pool,
            conversation_service,
        }
    }

    /// Helper: Prepare input and LLM context messages
    async fn prepare_messages(
        &self,
        request: &ResponseRequest,
    ) -> Result<(Vec<ResponseMessage>, Vec<ResponseMessage>), ResponseError> {
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
            match self
                .conversation_service
                .get_conversation_messages(conversation_id, &request.user_id, None)
                .await
            {
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
                    tracing::warn!(
                        "Failed to fetch conversation history for {}: {}",
                        conversation_id,
                        e
                    );
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
    fn prepare_chat_messages(
        &self,
        request: &ResponseRequest,
        llm_context_messages: &[ResponseMessage],
    ) -> Vec<ChatMessage> {
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
    async fn create_database_response(
        &self,
        request: &ResponseRequest,
        input_messages: &[ResponseMessage],
    ) -> Result<ResponseId, ResponseError> {
        // Prepare input messages as JSON for database
        let input_messages_json = serde_json::to_value(input_messages).map_err(|e| {
            ResponseError::InternalError(format!("Failed to serialize messages: {}", e))
        })?;

        let db_response = self
            .response_repository
            .create(
                request.user_id.clone(),
                request.model.clone(),
                input_messages_json,
                request.instructions.clone(),
                request.conversation_id.clone(),
                request.previous_response_id.clone(),
                request.metadata.clone(),
            )
            .await
            .map_err(|e| {
                ResponseError::InternalError(format!("Failed to create response: {}", e))
            })?;

        Ok(db_response.id)
    }

    pub async fn create_response_stream(
        &self,
        request: ResponseRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = ResponseStreamEvent> + Send>>, ResponseError> {
        tracing::info!(
            user_id = %request.user_id,
            model = %request.model,
            "Starting response stream creation"
        );

        // Prepare messages
        tracing::debug!("Preparing messages for response");
        let (input_messages, llm_context_messages) =
            self.prepare_messages(&request).await.map_err(|e| {
                tracing::error!("Failed to prepare messages: {}", e);
                e
            })?;
        tracing::debug!(
            "Successfully prepared {} input messages",
            input_messages.len()
        );

        // Create response in database
        tracing::debug!("Creating response record in database");
        let response_id = self
            .create_database_response(&request, &input_messages)
            .await
            .map_err(|e| {
                tracing::error!("Failed to create database response: {}", e);
                e
            })?;
        tracing::info!(
            response_id = %response_id,
            "Successfully created response record in database"
        );

        // Prepare chat messages for LLM
        tracing::debug!("Preparing chat messages for LLM");
        let chat_messages = self.prepare_chat_messages(&request, &llm_context_messages);
        tracing::debug!("Prepared {} chat messages for LLM", chat_messages.len());

        tracing::info!("Starting streaming response for {}", response_id);

        let chat_params = ChatCompletionParams {
            model: request.model.clone(),
            messages: chat_messages.clone(),
            max_tokens: request.max_output_tokens,
            temperature: request.temperature,
            top_p: request.top_p,
            stop: None,
            stream: Some(true),
            tools: None,
            max_completion_tokens: None,
            n: Some(1),
            frequency_penalty: None,
            presence_penalty: None,
            logit_bias: None,
            logprobs: None,
            top_logprobs: None,
            user: Some(request.user_id.clone().to_string()),
            response_format: None,
            seed: None,
            tool_choice: None,
            parallel_tool_calls: None,
            metadata: request.metadata.clone(),
            store: None,
            stream_options: None,
        };

        tracing::debug!(
            model = %chat_params.model,
            message_count = chat_messages.len(),
            max_tokens = ?chat_params.max_tokens,
            temperature = ?chat_params.temperature,
            "Calling inference provider with chat completion params"
        );

        // Get the LLM stream
        let llm_stream = self
            .inference_provider_pool
            .chat_completion_stream(chat_params)
            .await
            .map_err(|e| {
                tracing::error!(
                    model = %request.model,
                    error = %e,
                    "Failed to create LLM stream from inference provider"
                );
                ResponseError::InternalError(format!("Failed to create LLM stream: {}", e))
            })?;

        tracing::info!(
            response_id = %response_id,
            "Successfully created LLM stream, creating event stream"
        );

        // Create the response event stream
        let event_stream = Self::create_event_stream(
            llm_stream,
            response_id.clone(),
            self.response_repository.clone(),
            request.user_id.clone(),
        );

        tracing::info!(
            response_id = %response_id,
            "Successfully created response event stream"
        );

        Ok(Box::pin(event_stream))
    }

    /// Helper method to create the streaming events from LLM stream
    fn create_event_stream(
        llm_stream: Pin<
            Box<
                dyn Stream<Item = Result<StreamChunk, inference_providers::models::CompletionError>>
                    + Send,
            >,
        >,
        response_id: ResponseId,
        response_repository: Arc<dyn ports::ResponseRepository>,
        user_id: UserId,
    ) -> impl Stream<Item = ResponseStreamEvent> + Send {
        use futures::stream::{self, StreamExt};

        // State to track accumulated content and message IDs
        let accumulated_content = Arc::new(std::sync::Mutex::new(String::new()));
        let message_id = format!("msg_{}", uuid::Uuid::new_v4());
        let message_id_clone = message_id.clone();

        // Initial events with full response objects
        let initial_events = stream::iter(vec![
            Self::create_response_created_event(&response_id),
            Self::create_response_in_progress_event(&response_id),
            Self::create_output_item_added_event(&response_id, &message_id, 0),
            Self::create_content_part_added_event(&response_id, &message_id, 0, 0),
        ]);

        // Transform LLM chunks to response events
        let content_stream = llm_stream.filter_map(move |chunk_result| {
            let response_id = response_id.clone();
            let response_repository = response_repository.clone();
            let user_id = user_id.clone();
            let accumulated_content = accumulated_content.clone();
            let message_id = message_id_clone.clone();

            async move {
                match chunk_result {
                    Ok(stream_chunk) => {
                        match stream_chunk {
                            StreamChunk::Chat(chunk) => {
                                // Extract delta content
                                if let Some(choice) = chunk.choices.first() {
                                    if let Some(delta) = &choice.delta {
                                        if let Some(delta_content) = &delta.content {
                                            if !delta_content.is_empty() {
                                                // Accumulate content
                                                if let Ok(mut acc) = accumulated_content.lock() {
                                                    acc.push_str(delta_content);
                                                }
                                                return Some(Self::create_output_text_delta_event(
                                                    &response_id,
                                                    &message_id,
                                                    0,
                                                    0,
                                                    delta_content,
                                                ));
                                            }
                                        }
                                    }
                                }

                                // Check for usage (final chunk)
                                if let Some(usage) = chunk.usage {
                                    let final_content = accumulated_content
                                        .lock()
                                        .map(|acc| acc.clone())
                                        .unwrap_or_default();

                                    // Update database asynchronously

                                    let db_id = response_id.clone();
                                    let response_repository = response_repository.clone();
                                    let final_content_clone = final_content.clone();
                                    let usage_clone = usage.clone();
                                    tokio::spawn(async move {
                                        let usage_json = serde_json::to_value(&usage_clone).ok();
                                        if let Ok(user_uuid) = Uuid::parse_str(&user_id.to_string())
                                        {
                                            if let Err(e) = response_repository
                                                .update(
                                                    db_id,
                                                    user_uuid.into(),
                                                    Some(final_content_clone),
                                                    ports::ResponseStatus::Completed,
                                                    usage_json,
                                                )
                                                .await
                                            {
                                                tracing::error!(
                                                    "Failed to update response in database: {}",
                                                    e
                                                );
                                            }
                                        }
                                    });

                                    // Create completion events sequence - for now just return the response.completed event
                                    // TODO: We should emit the full sequence (text.done, content_part.done, output_item.done, response.completed)
                                    return Some(Self::create_response_completed_event(
                                        &response_id,
                                        &final_content,
                                        &usage,
                                    ));
                                }
                            }
                            StreamChunk::Text(_chunk) => {
                                // Handle text completion if needed
                                tracing::debug!("Received text chunk in chat completion stream");
                            }
                        }
                        None
                    }
                    Err(e) => {
                        let error_msg = format!("LLM stream error: {}", e);

                        // Update database with error asynchronously
                        let db_id = response_id.clone();
                        let response_repository = response_repository.clone();
                        let error_msg_clone = error_msg.clone();
                        tokio::spawn(async move {
                            if let Err(e) = response_repository
                                .update(
                                    db_id,
                                    user_id.clone(),
                                    Some(error_msg_clone),
                                    ports::ResponseStatus::Failed,
                                    None,
                                )
                                .await
                            {
                                tracing::error!(
                                    "Failed to update failed response in database: {}",
                                    e
                                );
                            }
                        });

                        Some(Self::create_error_event(&response_id, &error_msg))
                    }
                }
            }
        });

        // Chain initial events with content stream
        initial_events.chain(content_stream)
    }

    // API Spec-compliant event creation methods

    fn create_response_created_event(response_id: &ResponseId) -> ResponseStreamEvent {
        ResponseStreamEvent {
            event_type: "response.created".to_string(),
            response: Some(Self::create_base_response_json(response_id, "in_progress")),
            output_index: None,
            content_index: None,
            item: None,
            item_id: None,
            part: None,
            delta: None,
            text: None,
        }
    }

    fn create_response_in_progress_event(response_id: &ResponseId) -> ResponseStreamEvent {
        ResponseStreamEvent {
            event_type: "response.in_progress".to_string(),
            response: Some(Self::create_base_response_json(response_id, "in_progress")),
            output_index: None,
            content_index: None,
            item: None,
            item_id: None,
            part: None,
            delta: None,
            text: None,
        }
    }

    fn create_output_item_added_event(
        _response_id: &ResponseId,
        message_id: &str,
        output_index: usize,
    ) -> ResponseStreamEvent {
        let message_item = serde_json::json!({
            "id": message_id,
            "type": "message",
            "status": "in_progress",
            "role": "assistant",
            "content": []
        });

        ResponseStreamEvent {
            event_type: "response.output_item.added".to_string(),
            response: None,
            output_index: Some(output_index),
            content_index: None,
            item: Some(message_item),
            item_id: None,
            part: None,
            delta: None,
            text: None,
        }
    }

    fn create_content_part_added_event(
        _response_id: &ResponseId,
        item_id: &str,
        output_index: usize,
        content_index: usize,
    ) -> ResponseStreamEvent {
        let text_part = serde_json::json!({
            "type": "output_text",
            "text": "",
            "annotations": []
        });

        ResponseStreamEvent {
            event_type: "response.content_part.added".to_string(),
            response: None,
            output_index: Some(output_index),
            content_index: Some(content_index),
            item: None,
            item_id: Some(item_id.to_string()),
            part: Some(text_part),
            delta: None,
            text: None,
        }
    }

    fn create_output_text_delta_event(
        _response_id: &ResponseId,
        item_id: &str,
        output_index: usize,
        content_index: usize,
        delta: &str,
    ) -> ResponseStreamEvent {
        ResponseStreamEvent {
            event_type: "response.output_text.delta".to_string(),
            response: None,
            output_index: Some(output_index),
            content_index: Some(content_index),
            item: None,
            item_id: Some(item_id.to_string()),
            part: None,
            delta: Some(delta.to_string()),
            text: None,
        }
    }

    fn create_response_completed_event(
        response_id: &ResponseId,
        final_content: &str,
        usage: &inference_providers::TokenUsage,
    ) -> ResponseStreamEvent {
        let message_item = serde_json::json!({
            "id": format!("msg_{}", uuid::Uuid::new_v4()),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": final_content,
                "annotations": []
            }]
        });

        let mut response_obj = Self::create_base_response_json(response_id, "completed");
        if let Some(response_map) = response_obj.as_object_mut() {
            response_map.insert("output".to_string(), serde_json::json!([message_item]));
            response_map.insert(
                "usage".to_string(),
                serde_json::json!({
                    "input_tokens": usage.prompt_tokens,
                    "input_tokens_details": {
                        "cached_tokens": 0
                    },
                    "output_tokens": usage.completion_tokens,
                    "output_tokens_details": {
                        "reasoning_tokens": 0
                    },
                    "total_tokens": usage.total_tokens
                }),
            );
        }

        ResponseStreamEvent {
            event_type: "response.completed".to_string(),
            response: Some(response_obj),
            output_index: None,
            content_index: None,
            item: None,
            item_id: None,
            part: None,
            delta: None,
            text: None,
        }
    }

    fn create_error_event(response_id: &ResponseId, error: &str) -> ResponseStreamEvent {
        let mut response_obj = Self::create_base_response_json(response_id, "failed");
        if let Some(response_map) = response_obj.as_object_mut() {
            response_map.insert(
                "error".to_string(),
                serde_json::json!({
                    "message": error,
                    "type": "internal_error"
                }),
            );
        }

        ResponseStreamEvent {
            event_type: "response.failed".to_string(),
            response: Some(response_obj),
            output_index: None,
            content_index: None,
            item: None,
            item_id: None,
            part: None,
            delta: None,
            text: None,
        }
    }

    // Helper to create base response JSON object
    fn create_base_response_json(response_id: &ResponseId, status: &str) -> serde_json::Value {
        serde_json::json!({
            "id": response_id.to_string(),
            "object": "response",
            "created_at": chrono::Utc::now().timestamp(),
            "status": status,
            "error": null,
            "incomplete_details": null,
            "instructions": "You are a helpful assistant.",
            "max_output_tokens": null,
            "max_tool_calls": null,
            "model": "gpt-4", // TODO: Get from request context
            "output": [],
            "parallel_tool_calls": true,
            "previous_response_id": null,
            "reasoning": {
                "effort": null,
                "summary": null
            },
            "store": true,
            "temperature": 1.0,
            "text": {
                "format": {
                    "type": "text"
                }
            },
            "tool_choice": "auto",
            "tools": [],
            "top_p": 1.0,
            "truncation": "disabled",
            "usage": {
                "input_tokens": 0,
                "input_tokens_details": {
                    "cached_tokens": 0
                },
                "output_tokens": 0,
                "output_tokens_details": {
                    "reasoning_tokens": 0
                },
                "total_tokens": 0
            },
            "user": null,
            "metadata": {}
        })
    }
}
