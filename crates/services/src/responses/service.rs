use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;

use crate::completions::ports::CompletionServiceTrait;
use crate::conversations::models::ConversationId;
use crate::conversations::ports::ConversationServiceTrait;
use crate::inference_provider_pool::InferenceProviderPool;
use crate::responses::tools;
use crate::responses::{errors, models, ports};

pub struct ResponseServiceImpl {
    pub response_repository: Arc<dyn ports::ResponseRepositoryTrait>,
    pub response_items_repository: Arc<dyn ports::ResponseItemRepositoryTrait>,
    pub inference_provider_pool: Arc<InferenceProviderPool>,
    pub conversation_service: Arc<dyn ConversationServiceTrait>,
    pub completion_service: Arc<dyn CompletionServiceTrait>,
    pub web_search_provider: Option<Arc<dyn tools::WebSearchProviderTrait>>,
    pub file_search_provider: Option<Arc<dyn tools::FileSearchProviderTrait>>,
}

impl ResponseServiceImpl {
    pub fn new(
        response_repository: Arc<dyn ports::ResponseRepositoryTrait>,
        response_items_repository: Arc<dyn ports::ResponseItemRepositoryTrait>,
        inference_provider_pool: Arc<InferenceProviderPool>,
        conversation_service: Arc<dyn ConversationServiceTrait>,
        completion_service: Arc<dyn CompletionServiceTrait>,
        web_search_provider: Option<Arc<dyn tools::WebSearchProviderTrait>>,
        file_search_provider: Option<Arc<dyn tools::FileSearchProviderTrait>>,
    ) -> Self {
        Self {
            response_repository,
            response_items_repository,
            inference_provider_pool,
            conversation_service,
            completion_service,
            web_search_provider,
            file_search_provider,
        }
    }
}

#[async_trait]
impl ports::ResponseServiceTrait for ResponseServiceImpl {
    async fn create_response_stream(
        &self,
        request: models::CreateResponseRequest,
        user_id: crate::UserId,
        api_key_id: String,
        organization_id: uuid::Uuid,
        workspace_id: uuid::Uuid,
        body_hash: String,
    ) -> Result<
        Pin<Box<dyn Stream<Item = models::ResponseStreamEvent> + Send>>,
        errors::ResponseError,
    > {
        use futures::channel::mpsc;
        use futures::SinkExt;

        // Create a channel for streaming events
        let (mut tx, rx) = mpsc::unbounded::<models::ResponseStreamEvent>();

        // Clone necessary references for the async task
        let response_repository = self.response_repository.clone();
        let response_items_repository = self.response_items_repository.clone();
        let completion_service = self.completion_service.clone();
        let conversation_service = self.conversation_service.clone();
        let web_search_provider = self.web_search_provider.clone();
        let file_search_provider = self.file_search_provider.clone();

        tokio::spawn(async move {
            if let Err(e) = Self::process_response_stream(
                tx.clone(),
                request,
                user_id,
                api_key_id,
                organization_id,
                workspace_id,
                body_hash,
                response_repository,
                response_items_repository,
                completion_service,
                conversation_service,
                web_search_provider,
                file_search_provider,
            )
            .await
            {
                tracing::error!("Error processing response stream: {:?}", e);

                // Send error event
                let error_event = models::ResponseStreamEvent {
                    event_type: "response.failed".to_string(),
                    sequence_number: None,
                    response: None,
                    output_index: None,
                    content_index: None,
                    item: None,
                    item_id: None,
                    part: None,
                    delta: None,
                    text: Some(e.to_string()),
                    logprobs: None,
                    obfuscation: None,
                    annotation_index: None,
                    annotation: None,
                };
                let _ = tx.send(error_event).await;
            }
        });

        Ok(Box::pin(rx))
    }
}

impl ResponseServiceImpl {
    /// Parse conversation ID from request
    fn parse_conversation_id(
        request: &models::CreateResponseRequest,
    ) -> Result<Option<ConversationId>, errors::ResponseError> {
        if let Some(conversation_ref) = &request.conversation {
            let id = match conversation_ref {
                models::ConversationReference::Id(id) => id,
                models::ConversationReference::Object { id, .. } => id,
            };

            let conv_id = ConversationId::from_str(id).map_err(|e| {
                errors::ResponseError::InvalidParams(format!("Invalid conversation ID: {}", e))
            })?;

            Ok(Some(conv_id))
        } else {
            Ok(None)
        }
    }

    /// Extract response ID UUID from response object
    fn extract_response_uuid(
        response: &models::ResponseObject,
    ) -> Result<models::ResponseId, errors::ResponseError> {
        let response_uuid =
            uuid::Uuid::parse_str(response.id.strip_prefix("resp_").unwrap_or(&response.id))
                .map_err(|e| {
                    errors::ResponseError::InternalError(format!(
                        "Invalid response ID format: {}",
                        e
                    ))
                })?;

        Ok(models::ResponseId(response_uuid))
    }

    /// Process a completion stream and emit events for text deltas
    /// Returns the accumulated text and detected tool calls
    async fn process_completion_stream(
        completion_stream: &mut Pin<
            Box<
                dyn Stream<
                        Item = Result<
                            inference_providers::SSEEvent,
                            inference_providers::CompletionError,
                        >,
                    > + Send,
            >,
        >,
        emitter: &mut crate::responses::service_helpers::EventEmitter,
        ctx: &mut crate::responses::service_helpers::ResponseStreamContext,
        response_items_repository: &Arc<dyn ports::ResponseItemRepositoryTrait>,
    ) -> Result<(String, Vec<crate::responses::service_helpers::ToolCallInfo>), errors::ResponseError>
    {
        use crate::responses::service_helpers::ToolCallAccumulator;
        use futures::StreamExt;

        let mut current_text = String::new();
        let mut tool_call_accumulator: ToolCallAccumulator = std::collections::HashMap::new();
        let mut message_item_emitted = false;
        let message_item_id = format!("msg_{}", uuid::Uuid::new_v4().simple());

        while let Some(event) = completion_stream.next().await {
            match event {
                Ok(sse_event) => {
                    // Parse the SSE event for content and tool calls
                    if let Some(delta_text) = Self::extract_text_delta(&sse_event) {
                        // First time we receive text, emit the item.added and content_part.added events
                        if !message_item_emitted {
                            Self::emit_message_started(emitter, ctx, &message_item_id).await?;
                            message_item_emitted = true;
                        }

                        current_text.push_str(&delta_text);

                        // Emit delta event
                        emitter
                            .emit_text_delta(ctx, message_item_id.clone(), delta_text)
                            .await?;
                    }

                    // Accumulate tool call fragments
                    Self::accumulate_tool_calls(&sse_event, &mut tool_call_accumulator);
                }
                Err(e) => {
                    tracing::error!("Error in completion stream: {}", e);
                    return Err(errors::ResponseError::InternalError(format!(
                        "Stream error: {}",
                        e
                    )));
                }
            }
        }

        // If we emitted a message, close it with done events
        if message_item_emitted {
            Self::emit_message_completed(
                emitter,
                ctx,
                &message_item_id,
                &current_text,
                response_items_repository,
            )
            .await?;
        }

        // Convert accumulated tool calls to detected tool calls
        let tool_calls_detected = Self::convert_tool_calls(tool_call_accumulator);

        Ok((current_text, tool_calls_detected))
    }

    /// Emit events when a message starts streaming
    async fn emit_message_started(
        emitter: &mut crate::responses::service_helpers::EventEmitter,
        ctx: &mut crate::responses::service_helpers::ResponseStreamContext,
        message_item_id: &str,
    ) -> Result<(), errors::ResponseError> {
        // Event: response.output_item.added (for message)
        let item = models::ResponseOutputItem::Message {
            id: message_item_id.to_string(),
            status: models::ResponseItemStatus::InProgress,
            role: "assistant".to_string(),
            content: vec![],
        };
        emitter
            .emit_item_added(ctx, item, message_item_id.to_string())
            .await?;

        // Event: response.content_part.added
        let part = models::ResponseOutputContent::OutputText {
            text: String::new(),
            annotations: vec![],
            logprobs: vec![],
        };
        emitter
            .emit_content_part_added(ctx, message_item_id.to_string(), part)
            .await?;

        Ok(())
    }

    /// Emit events when a message completes
    async fn emit_message_completed(
        emitter: &mut crate::responses::service_helpers::EventEmitter,
        ctx: &mut crate::responses::service_helpers::ResponseStreamContext,
        message_item_id: &str,
        text: &str,
        response_items_repository: &Arc<dyn ports::ResponseItemRepositoryTrait>,
    ) -> Result<(), errors::ResponseError> {
        // Event: response.output_text.done
        emitter
            .emit_text_done(ctx, message_item_id.to_string(), text.to_string())
            .await?;

        // Event: response.content_part.done
        let part = models::ResponseOutputContent::OutputText {
            text: text.to_string(),
            annotations: vec![],
            logprobs: vec![],
        };
        emitter
            .emit_content_part_done(ctx, message_item_id.to_string(), part)
            .await?;

        // Event: response.output_item.done
        let item = models::ResponseOutputItem::Message {
            id: message_item_id.to_string(),
            status: models::ResponseItemStatus::Completed,
            role: "assistant".to_string(),
            content: vec![models::ResponseOutputContent::OutputText {
                text: text.to_string(),
                annotations: vec![],
                logprobs: vec![],
            }],
        };
        emitter
            .emit_item_done(ctx, item.clone(), message_item_id.to_string())
            .await?;

        // Store the message item in the database
        if let Err(e) = response_items_repository
            .create(
                ctx.response_id.clone(),
                ctx.user_id.clone(),
                ctx.conversation_id.clone(),
                item,
            )
            .await
        {
            tracing::warn!("Failed to store message item: {}", e);
        }

        Ok(())
    }

    /// Convert accumulated tool calls to ToolCallInfo
    fn convert_tool_calls(
        tool_call_accumulator: std::collections::HashMap<i64, (Option<String>, String)>,
    ) -> Vec<crate::responses::service_helpers::ToolCallInfo> {
        use crate::responses::service_helpers::ToolCallInfo;

        let mut tool_calls_detected = Vec::new();

        for (_idx, (name_opt, args_str)) in tool_call_accumulator {
            if let Some(name) = name_opt {
                // Handle tools that don't require parameters (like current_date)
                if name == "current_date" {
                    tracing::debug!("Tool call detected: {}", name);
                    tool_calls_detected.push(ToolCallInfo {
                        tool_type: name,
                        query: String::new(),
                    });
                } else {
                    // Try to parse the complete arguments for tools that need parameters
                    if let Ok(args) = serde_json::from_str::<serde_json::Value>(&args_str) {
                        if let Some(query) = args.get("query").and_then(|v| v.as_str()) {
                            tracing::debug!("Tool call detected: {} with query: {}", name, query);
                            tool_calls_detected.push(ToolCallInfo {
                                tool_type: name,
                                query: query.to_string(),
                            });
                        }
                    } else {
                        tracing::warn!("Failed to parse tool call arguments: {}", args_str);
                    }
                }
            }
        }

        tool_calls_detected
    }

    /// Process the response stream - main logic
    async fn process_response_stream(
        tx: futures::channel::mpsc::UnboundedSender<models::ResponseStreamEvent>,
        request: models::CreateResponseRequest,
        user_id: crate::UserId,
        api_key_id: String,
        organization_id: uuid::Uuid,
        workspace_id: uuid::Uuid,
        body_hash: String,
        response_repository: Arc<dyn ports::ResponseRepositoryTrait>,
        response_items_repository: Arc<dyn ports::ResponseItemRepositoryTrait>,
        completion_service: Arc<dyn CompletionServiceTrait>,
        conversation_service: Arc<dyn ConversationServiceTrait>,
        web_search_provider: Option<Arc<dyn tools::WebSearchProviderTrait>>,
        file_search_provider: Option<Arc<dyn tools::FileSearchProviderTrait>>,
    ) -> Result<(), errors::ResponseError> {
        tracing::info!("Starting response stream processing");

        let conversation_id = Self::parse_conversation_id(&request)?;

        let mut messages = Self::load_conversation_context(
            &request,
            &conversation_service,
            &response_items_repository,
            user_id.clone(),
        )
        .await?;

        // Create the response in the database FIRST before creating any response items
        // This ensures the foreign key constraint is satisfied
        let initial_response = response_repository
            .create(user_id.clone(), request.clone())
            .await
            .map_err(|e| {
                errors::ResponseError::InternalError(format!("Failed to create response: {}", e))
            })?;

        // Extract response_id from the created response
        let response_id = Self::extract_response_uuid(&initial_response)?;

        // Store user input messages as response_items
        if let Some(input) = &request.input {
            Self::store_input_as_response_items(
                &response_items_repository,
                response_id.clone(),
                user_id.clone(),
                conversation_id.clone(),
                input,
            )
            .await?;
        }

        // Initialize context and emitter
        let mut ctx = crate::responses::service_helpers::ResponseStreamContext::new(
            response_id.clone(),
            user_id.clone(),
            conversation_id.clone(),
        );
        let mut emitter = crate::responses::service_helpers::EventEmitter::new(tx);

        // Event: response.created
        emitter
            .emit_created(&mut ctx, initial_response.clone())
            .await?;

        // Event: response.in_progress
        emitter
            .emit_in_progress(&mut ctx, initial_response.clone())
            .await?;

        let tools = Self::prepare_tools(&request);
        let tool_choice = Self::prepare_tool_choice(&request);

        let max_iterations = 10; // Prevent infinite loops
        let mut iteration = 0;
        let mut final_response_text = String::new();

        // Run the agent loop to process completion and tool calls
        Self::run_agent_loop(
            &mut ctx,
            &mut emitter,
            &mut messages,
            &mut final_response_text,
            &request,
            user_id.clone(),
            &api_key_id,
            organization_id,
            workspace_id,
            &body_hash,
            &tools,
            &tool_choice,
            max_iterations,
            &mut iteration,
            &response_items_repository,
            &completion_service,
            &web_search_provider,
            &file_search_provider,
        )
        .await?;

        // Build final response
        let mut final_response = initial_response;
        final_response.status = models::ResponseStatus::Completed;

        // Load all response items from the database for this response
        let response_items = response_items_repository
            .list_by_response(ctx.response_id.clone())
            .await
            .map_err(|e| {
                errors::ResponseError::InternalError(format!(
                    "Failed to load response items: {}",
                    e
                ))
            })?;

        // Filter to get only assistant output items (excluding user input items)
        let output_items: Vec<_> = response_items
            .into_iter()
            .filter(|item| match item {
                models::ResponseOutputItem::Message { role, .. } => role == "assistant",
                _ => true, // Include all non-message items (tool calls, web searches, etc.)
            })
            .collect();

        final_response.output = output_items;

        // Event: response.completed
        emitter.emit_completed(&mut ctx, final_response).await?;

        tracing::info!("Response stream completed successfully");
        Ok(())
    }

    /// Run the agent loop - repeatedly call completion API and execute tool calls
    #[allow(clippy::too_many_arguments)]
    async fn run_agent_loop(
        ctx: &mut crate::responses::service_helpers::ResponseStreamContext,
        emitter: &mut crate::responses::service_helpers::EventEmitter,
        messages: &mut Vec<crate::completions::ports::CompletionMessage>,
        final_response_text: &mut String,
        request: &models::CreateResponseRequest,
        user_id: crate::UserId,
        api_key_id: &str,
        organization_id: uuid::Uuid,
        workspace_id: uuid::Uuid,
        body_hash: &str,
        tools: &[inference_providers::ToolDefinition],
        tool_choice: &Option<inference_providers::ToolChoice>,
        max_iterations: usize,
        iteration: &mut usize,
        response_items_repository: &Arc<dyn ports::ResponseItemRepositoryTrait>,
        completion_service: &Arc<dyn CompletionServiceTrait>,
        web_search_provider: &Option<Arc<dyn tools::WebSearchProviderTrait>>,
        file_search_provider: &Option<Arc<dyn tools::FileSearchProviderTrait>>,
    ) -> Result<(), errors::ResponseError> {
        use crate::completions::ports::{CompletionMessage, CompletionRequest};

        loop {
            *iteration += 1;
            if *iteration > max_iterations {
                tracing::warn!("Max iterations reached in agent loop");
                break;
            }

            tracing::debug!("Agent loop iteration {}", iteration);

            // Prepare extra params with tools
            let mut extra = std::collections::HashMap::new();
            if !tools.is_empty() {
                extra.insert("tools".to_string(), serde_json::to_value(tools).unwrap());
            }
            if let Some(tc) = tool_choice {
                extra.insert("tool_choice".to_string(), serde_json::to_value(tc).unwrap());
            }

            // Create completion request
            let completion_request = CompletionRequest {
                model: request.model.clone(),
                messages: messages.clone(),
                max_tokens: request.max_output_tokens,
                temperature: request.temperature,
                top_p: request.top_p,
                stop: None,
                stream: Some(true),
                user_id: user_id.clone(),
                api_key_id: api_key_id.to_string(),
                organization_id,
                workspace_id,
                metadata: request.metadata.clone(),
                body_hash: body_hash.to_string(),
                n: None,
                extra,
            };

            // Get completion stream
            let mut completion_stream = completion_service
                .create_chat_completion_stream(completion_request)
                .await
                .map_err(|e| {
                    errors::ResponseError::InternalError(format!("Completion error: {}", e))
                })?;

            // Process the completion stream and extract text + tool calls
            let (current_text, tool_calls_detected) = Self::process_completion_stream(
                &mut completion_stream,
                emitter,
                ctx,
                response_items_repository,
            )
            .await?;

            // Update response state
            if !current_text.is_empty() {
                final_response_text.push_str(&current_text);
                messages.push(CompletionMessage {
                    role: "assistant".to_string(),
                    content: current_text.clone(),
                });
                ctx.next_output_index();
            }

            // Check if we're done
            if tool_calls_detected.is_empty() {
                tracing::debug!("No tool calls detected, ending agent loop");
                break;
            }

            tracing::debug!("Executing {} tool calls", tool_calls_detected.len());

            // Execute each tool call
            for tool_call in tool_calls_detected {
                Self::execute_and_emit_tool_call(
                    ctx,
                    emitter,
                    &tool_call,
                    messages,
                    request,
                    response_items_repository,
                    web_search_provider,
                    file_search_provider,
                )
                .await?;
            }
        }

        Ok(())
    }

    /// Execute a tool call and emit appropriate events
    #[allow(clippy::too_many_arguments)]
    async fn execute_and_emit_tool_call(
        ctx: &mut crate::responses::service_helpers::ResponseStreamContext,
        emitter: &mut crate::responses::service_helpers::EventEmitter,
        tool_call: &crate::responses::service_helpers::ToolCallInfo,
        messages: &mut Vec<crate::completions::ports::CompletionMessage>,
        request: &models::CreateResponseRequest,
        response_items_repository: &Arc<dyn ports::ResponseItemRepositoryTrait>,
        web_search_provider: &Option<Arc<dyn tools::WebSearchProviderTrait>>,
        file_search_provider: &Option<Arc<dyn tools::FileSearchProviderTrait>>,
    ) -> Result<(), errors::ResponseError> {
        use crate::completions::ports::CompletionMessage;

        let tool_call_id = format!("{}_{}", tool_call.tool_type, uuid::Uuid::new_v4().simple());

        // Emit tool-specific start events
        if tool_call.tool_type == "web_search" {
            Self::emit_web_search_start(ctx, emitter, &tool_call_id, tool_call).await?;
        }

        // Execute the tool
        let tool_result = Self::execute_tool(
            tool_call,
            web_search_provider,
            file_search_provider,
            request,
        )
        .await?;

        // Emit tool-specific completion events
        if tool_call.tool_type == "web_search" {
            Self::emit_web_search_complete(
                ctx,
                emitter,
                &tool_call_id,
                tool_call,
                response_items_repository,
            )
            .await?;
        }

        // Add tool result to message history
        messages.push(CompletionMessage {
            role: "tool".to_string(),
            content: tool_result,
        });

        Ok(())
    }

    /// Emit web search start events
    async fn emit_web_search_start(
        ctx: &mut crate::responses::service_helpers::ResponseStreamContext,
        emitter: &mut crate::responses::service_helpers::EventEmitter,
        tool_call_id: &str,
        tool_call: &crate::responses::service_helpers::ToolCallInfo,
    ) -> Result<(), errors::ResponseError> {
        // Event: response.output_item.added
        let item = models::ResponseOutputItem::WebSearchCall {
            id: tool_call_id.to_string(),
            status: models::ResponseItemStatus::InProgress,
            action: models::WebSearchAction::Search {
                query: tool_call.query.clone(),
            },
        };
        emitter
            .emit_item_added(ctx, item, tool_call_id.to_string())
            .await?;

        // Emit web-search-specific progress events
        Self::emit_simple_event(
            emitter,
            ctx,
            "response.web_search_call.in_progress",
            Some(tool_call_id.to_string()),
        )
        .await?;

        Self::emit_simple_event(
            emitter,
            ctx,
            "response.web_search_call.searching",
            Some(tool_call_id.to_string()),
        )
        .await?;

        Ok(())
    }

    /// Emit web search completion events
    async fn emit_web_search_complete(
        ctx: &mut crate::responses::service_helpers::ResponseStreamContext,
        emitter: &mut crate::responses::service_helpers::EventEmitter,
        tool_call_id: &str,
        tool_call: &crate::responses::service_helpers::ToolCallInfo,
        response_items_repository: &Arc<dyn ports::ResponseItemRepositoryTrait>,
    ) -> Result<(), errors::ResponseError> {
        // Event: response.web_search_call.completed
        Self::emit_simple_event(
            emitter,
            ctx,
            "response.web_search_call.completed",
            Some(tool_call_id.to_string()),
        )
        .await?;

        // Event: response.output_item.done
        let item = models::ResponseOutputItem::WebSearchCall {
            id: tool_call_id.to_string(),
            status: models::ResponseItemStatus::Completed,
            action: models::WebSearchAction::Search {
                query: tool_call.query.clone(),
            },
        };
        emitter
            .emit_item_done(ctx, item.clone(), tool_call_id.to_string())
            .await?;

        // Store response item
        if let Err(e) = response_items_repository
            .create(
                ctx.response_id.clone(),
                ctx.user_id.clone(),
                ctx.conversation_id.clone(),
                item,
            )
            .await
        {
            tracing::warn!("Failed to store response item: {:?}", e);
        }

        ctx.next_output_index();

        Ok(())
    }

    /// Emit a simple event with just event_type and optional item_id
    async fn emit_simple_event(
        emitter: &mut crate::responses::service_helpers::EventEmitter,
        ctx: &mut crate::responses::service_helpers::ResponseStreamContext,
        event_type: &str,
        item_id: Option<String>,
    ) -> Result<(), errors::ResponseError> {
        let event = models::ResponseStreamEvent {
            event_type: event_type.to_string(),
            sequence_number: Some(ctx.next_sequence()),
            response: None,
            output_index: Some(ctx.output_item_index),
            content_index: None,
            item: None,
            item_id,
            part: None,
            delta: None,
            text: None,
            logprobs: None,
            obfuscation: None,
            annotation_index: None,
            annotation: None,
        };

        emitter.send_raw(event).await
    }

    /// Store user input messages as response_items
    async fn store_input_as_response_items(
        response_items_repository: &Arc<dyn ports::ResponseItemRepositoryTrait>,
        response_id: models::ResponseId,
        user_id: crate::UserId,
        conversation_id: Option<ConversationId>,
        input: &models::ResponseInput,
    ) -> Result<(), errors::ResponseError> {
        match input {
            models::ResponseInput::Text(text) => {
                // Create a message item for simple text input
                let message_item = models::ResponseOutputItem::Message {
                    id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
                    status: models::ResponseItemStatus::Completed,
                    role: "user".to_string(),
                    content: vec![models::ResponseOutputContent::OutputText {
                        text: text.clone(),
                        annotations: vec![],
                        logprobs: vec![],
                    }],
                };

                response_items_repository
                    .create(
                        response_id.clone(),
                        user_id.clone(),
                        conversation_id.clone(),
                        message_item,
                    )
                    .await
                    .map_err(|e| {
                        errors::ResponseError::InternalError(format!(
                            "Failed to store user input: {}",
                            e
                        ))
                    })?;
            }
            models::ResponseInput::Items(items) => {
                // Store each input item as a response_item
                for input_item in items {
                    let content = match &input_item.content {
                        models::ResponseContent::Text(text) => {
                            vec![models::ResponseOutputContent::OutputText {
                                text: text.clone(),
                                annotations: vec![],
                                logprobs: vec![],
                            }]
                        }
                        models::ResponseContent::Parts(parts) => {
                            // Convert parts to output content
                            parts
                                .iter()
                                .filter_map(|part| match part {
                                    models::ResponseContentPart::InputText { text } => {
                                        Some(models::ResponseOutputContent::OutputText {
                                            text: text.clone(),
                                            annotations: vec![],
                                            logprobs: vec![],
                                        })
                                    }
                                    // TODO: Handle other content types (images, files, etc.)
                                    _ => None,
                                })
                                .collect()
                        }
                    };

                    let message_item = models::ResponseOutputItem::Message {
                        id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
                        status: models::ResponseItemStatus::Completed,
                        role: input_item.role.clone(),
                        content,
                    };

                    response_items_repository
                        .create(
                            response_id.clone(),
                            user_id.clone(),
                            conversation_id.clone(),
                            message_item,
                        )
                        .await
                        .map_err(|e| {
                            errors::ResponseError::InternalError(format!(
                                "Failed to store user input item: {}",
                                e
                            ))
                        })?;
                }
            }
        }

        tracing::debug!(
            "Stored user input messages as response_items for response {}",
            response_id.0
        );
        Ok(())
    }

    /// Load conversation context based on conversation_id or previous_response_id
    async fn load_conversation_context(
        request: &models::CreateResponseRequest,
        conversation_service: &Arc<dyn ConversationServiceTrait>,
        response_items_repository: &Arc<dyn ports::ResponseItemRepositoryTrait>,
        user_id: crate::UserId,
    ) -> Result<Vec<crate::completions::ports::CompletionMessage>, errors::ResponseError> {
        use crate::completions::ports::CompletionMessage;

        let mut messages = Vec::new();

        // Add system instructions if present
        if let Some(instructions) = &request.instructions {
            messages.push(CompletionMessage {
                role: "system".to_string(),
                content: instructions.clone(),
            });
        }

        // Load from conversation_id if present
        if let Some(conversation_ref) = &request.conversation {
            let conversation_id = match conversation_ref {
                models::ConversationReference::Id(id) => {
                    crate::conversations::models::ConversationId::from_str(id).map_err(|e| {
                        errors::ResponseError::InvalidParams(format!(
                            "Invalid conversation ID: {}",
                            e
                        ))
                    })?
                }
                models::ConversationReference::Object { id, metadata: _ } => {
                    crate::conversations::models::ConversationId::from_str(id).map_err(|e| {
                        errors::ResponseError::InvalidParams(format!(
                            "Invalid conversation ID: {}",
                            e
                        ))
                    })?
                }
            };

            // Load conversation metadata to verify it exists
            let _conversation = conversation_service
                .get_conversation(conversation_id.clone(), user_id.clone())
                .await
                .map_err(|e| {
                    errors::ResponseError::InternalError(format!(
                        "Failed to get conversation: {}",
                        e
                    ))
                })?;

            // Load all response items from the conversation
            let conversation_items = response_items_repository
                .list_by_conversation(conversation_id.clone())
                .await
                .map_err(|e| {
                    errors::ResponseError::InternalError(format!(
                        "Failed to load conversation items: {}",
                        e
                    ))
                })?;

            // Convert response items to completion messages
            let messages_before = messages.len();
            for item in conversation_items {
                match item {
                    models::ResponseOutputItem::Message { role, content, .. } => {
                        // Extract text from content parts
                        let text = content
                            .iter()
                            .filter_map(|part| match part {
                                models::ResponseOutputContent::OutputText { text, .. } => {
                                    Some(text.clone())
                                }
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");

                        if !text.is_empty() {
                            messages.push(CompletionMessage {
                                role: role.clone(),
                                content: text,
                            });
                        }
                    }
                    // For now, we only process message items
                    // TODO: Handle other item types (tool calls, web searches, etc.) if needed
                    _ => {}
                }
            }

            let loaded_count = messages.len() - messages_before;
            tracing::info!(
                "Loaded {} messages from conversation {}",
                loaded_count,
                conversation_id
            );
        }

        // TODO: Load from previous_response_id if present
        // if let Some(prev_response_id) = &request.previous_response_id {
        //     // Load previous response
        // }

        // Add input messages
        if let Some(input) = &request.input {
            match input {
                models::ResponseInput::Text(text) => {
                    messages.push(CompletionMessage {
                        role: "user".to_string(),
                        content: text.clone(),
                    });
                }
                models::ResponseInput::Items(items) => {
                    for item in items {
                        let content = match &item.content {
                            models::ResponseContent::Text(text) => text.clone(),
                            models::ResponseContent::Parts(parts) => {
                                // Extract text from parts
                                parts
                                    .iter()
                                    .filter_map(|part| match part {
                                        models::ResponseContentPart::InputText { text } => {
                                            Some(text.clone())
                                        }
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            }
                        };

                        messages.push(CompletionMessage {
                            role: item.role.clone(),
                            content,
                        });
                    }
                }
            }
        }

        Ok(messages)
    }

    /// Prepare tools configuration for LLM in OpenAI function calling format
    fn prepare_tools(
        request: &models::CreateResponseRequest,
    ) -> Vec<inference_providers::ToolDefinition> {
        let mut tool_definitions = Vec::new();

        if let Some(tools) = &request.tools {
            for tool in tools {
                match tool {
                    models::ResponseTool::WebSearch { .. } => {
                        tool_definitions.push(inference_providers::ToolDefinition {
                            type_: "function".to_string(),
                            function: inference_providers::FunctionDefinition {
                                name: "web_search".to_string(),
                                description: Some(
                                    "Search the web for current information. Use this when you need up-to-date information or facts that you don't have.".to_string()
                                ),
                                parameters: serde_json::json!({
                                    "type": "object",
                                    "properties": {
                                        "query": {
                                            "type": "string",
                                            "description": "The search query to look up"
                                        }
                                    },
                                    "required": ["query"]
                                }),
                            },
                        });
                    }
                    models::ResponseTool::FileSearch {} => {
                        tool_definitions.push(inference_providers::ToolDefinition {
                            type_: "function".to_string(),
                            function: inference_providers::FunctionDefinition {
                                name: "file_search".to_string(),
                                description: Some(
                                    "Search through files in the current conversation. Use this to find information from uploaded documents or previous file content.".to_string()
                                ),
                                parameters: serde_json::json!({
                                    "type": "object",
                                    "properties": {
                                        "query": {
                                            "type": "string",
                                            "description": "The search query to look up in files"
                                        }
                                    },
                                    "required": ["query"]
                                }),
                            },
                        });
                    }
                    models::ResponseTool::Function {
                        name,
                        description,
                        parameters,
                    } => {
                        tool_definitions.push(inference_providers::ToolDefinition {
                            type_: "function".to_string(),
                            function: inference_providers::FunctionDefinition {
                                name: name.clone(),
                                description: description.clone(),
                                parameters: parameters.clone().unwrap_or_else(|| {
                                    serde_json::json!({
                                        "type": "object",
                                        "properties": {}
                                    })
                                }),
                            },
                        });
                    }
                    models::ResponseTool::CodeInterpreter {} => {
                        tool_definitions.push(inference_providers::ToolDefinition {
                            type_: "function".to_string(),
                            function: inference_providers::FunctionDefinition {
                                name: "code_interpreter".to_string(),
                                description: Some(
                                    "Execute Python code in a sandboxed environment.".to_string(),
                                ),
                                parameters: serde_json::json!({
                                    "type": "object",
                                    "properties": {
                                        "code": {
                                            "type": "string",
                                            "description": "Python code to execute"
                                        }
                                    },
                                    "required": ["code"]
                                }),
                            },
                        });
                    }
                    models::ResponseTool::Computer {} => {
                        tool_definitions.push(inference_providers::ToolDefinition {
                            type_: "function".to_string(),
                            function: inference_providers::FunctionDefinition {
                                name: "computer".to_string(),
                                description: Some(
                                    "Control computer actions like mouse clicks and keyboard input.".to_string()
                                ),
                                parameters: serde_json::json!({
                                    "type": "object",
                                    "properties": {
                                        "action": {
                                            "type": "string",
                                            "description": "The action to perform"
                                        }
                                    },
                                    "required": ["action"]
                                }),
                            },
                        });
                    }
                    models::ResponseTool::CurrentDate {} => {
                        // Note: current_date is added by default below, so this case
                        // should not typically be hit unless explicitly requested
                        tool_definitions.push(inference_providers::ToolDefinition {
                            type_: "function".to_string(),
                            function: inference_providers::FunctionDefinition {
                                name: "current_date".to_string(),
                                description: Some(
                                    "Get the current date and time. Use this when you need to know what day it is, the current time, or to answer questions about temporal information.".to_string()
                                ),
                                parameters: serde_json::json!({
                                    "type": "object",
                                    "properties": {},
                                    "required": []
                                }),
                            },
                        });
                    }
                }
            }
        }

        // Always add current_date tool by default (not visible at API level)
        // Check if it's not already added to avoid duplicates
        if !tool_definitions
            .iter()
            .any(|t| t.function.name == "current_date")
        {
            tool_definitions.push(inference_providers::ToolDefinition {
                type_: "function".to_string(),
                function: inference_providers::FunctionDefinition {
                    name: "current_date".to_string(),
                    description: Some(
                        "Get the current date and time. Use this when you need to know what day it is, the current time, or to answer questions about temporal information.".to_string()
                    ),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {},
                        "required": []
                    }),
                },
            });
        }

        tool_definitions
    }

    /// Prepare tool choice configuration
    fn prepare_tool_choice(
        request: &models::CreateResponseRequest,
    ) -> Option<inference_providers::ToolChoice> {
        request.tool_choice.as_ref().map(|choice| match choice {
            models::ResponseToolChoice::Auto(s) => {
                inference_providers::ToolChoice::String(s.clone())
            }
            models::ResponseToolChoice::Specific { type_, function } => {
                inference_providers::ToolChoice::Function {
                    type_: type_.clone(),
                    function: inference_providers::FunctionChoice {
                        name: function.name.clone(),
                    },
                }
            }
        })
    }

    /// Extract text delta from SSE event (placeholder)
    fn extract_text_delta(event: &inference_providers::SSEEvent) -> Option<String> {
        use inference_providers::StreamChunk;

        match &event.chunk {
            StreamChunk::Chat(chat_chunk) => {
                // Extract delta content from choices
                for choice in &chat_chunk.choices {
                    if let Some(delta) = &choice.delta {
                        if let Some(content) = &delta.content {
                            return Some(content.clone());
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// Accumulate tool call fragments from streaming chunks
    fn accumulate_tool_calls(
        event: &inference_providers::SSEEvent,
        accumulator: &mut std::collections::HashMap<i64, (Option<String>, String)>,
    ) {
        use inference_providers::StreamChunk;

        match &event.chunk {
            StreamChunk::Chat(chat_chunk) => {
                for choice in &chat_chunk.choices {
                    if let Some(delta) = &choice.delta {
                        if let Some(tool_calls) = &delta.tool_calls {
                            for tool_call in tool_calls {
                                // Get or default to index 0 if not present
                                let index = tool_call.index.unwrap_or(0);

                                // Get or create accumulator entry for this index
                                let entry =
                                    accumulator.entry(index).or_insert((None, String::new()));

                                // Accumulate function name (only set once, typically in first chunk)
                                if let Some(name) = &tool_call.function.name {
                                    tracing::debug!(
                                        "Accumulated tool call {} name: {}",
                                        index,
                                        name
                                    );
                                    entry.0 = Some(name.clone());
                                }

                                // Accumulate arguments (streamed across multiple chunks)
                                if let Some(args_fragment) = &tool_call.function.arguments {
                                    tracing::debug!(
                                        "Accumulated tool call {} args fragment: {}",
                                        index,
                                        args_fragment
                                    );
                                    entry.1.push_str(args_fragment);
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Execute a tool call
    async fn execute_tool(
        tool_call: &crate::responses::service_helpers::ToolCallInfo,
        web_search_provider: &Option<Arc<dyn tools::WebSearchProviderTrait>>,
        file_search_provider: &Option<Arc<dyn tools::FileSearchProviderTrait>>,
        request: &models::CreateResponseRequest,
    ) -> Result<String, errors::ResponseError> {
        match tool_call.tool_type.as_str() {
            "web_search" => {
                if let Some(provider) = web_search_provider {
                    let results = provider
                        .search(tool_call.query.clone())
                        .await
                        .map_err(|e| {
                            errors::ResponseError::InternalError(format!(
                                "Web search failed: {}",
                                e
                            ))
                        })?;
                    let formatted = results
                        .iter()
                        .map(|r| {
                            format!(
                                "Title: {}\nURL: {}\nSnippet: {}\n",
                                r.title, r.url, r.snippet
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    Ok(formatted)
                } else {
                    Err(errors::ResponseError::UnknownTool("web_search".to_string()))
                }
            }
            "file_search" => {
                if let Some(provider) = file_search_provider {
                    // Get conversation ID from request
                    let conversation_id = match &request.conversation {
                        Some(models::ConversationReference::Id(id)) => {
                            // Parse conversation ID
                            let uuid_str = id.strip_prefix("conv_").unwrap_or(id);
                            uuid::Uuid::parse_str(uuid_str).map_err(|e| {
                                errors::ResponseError::InvalidParams(format!(
                                    "Invalid conversation ID: {}",
                                    e
                                ))
                            })?
                        }
                        Some(models::ConversationReference::Object { id, .. }) => {
                            let uuid_str = id.strip_prefix("conv_").unwrap_or(id);
                            uuid::Uuid::parse_str(uuid_str).map_err(|e| {
                                errors::ResponseError::InvalidParams(format!(
                                    "Invalid conversation ID: {}",
                                    e
                                ))
                            })?
                        }
                        None => {
                            return Ok("File search requires a conversation context".to_string());
                        }
                    };

                    let results = provider
                        .search_conversation_files(
                            crate::conversations::models::ConversationId::from(conversation_id),
                            tool_call.query.clone(),
                        )
                        .await
                        .map_err(|e| {
                            errors::ResponseError::InternalError(format!(
                                "File search failed: {}",
                                e
                            ))
                        })?;

                    // Format results as text
                    let formatted = results
                        .iter()
                        .map(|r| {
                            format!(
                                "File: {}\nContent: {}\nRelevance: {}\n",
                                r.file_name, r.content, r.relevance_score
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");

                    Ok(formatted)
                } else {
                    Ok("File search not available (no provider configured)".to_string())
                }
            }
            "current_date" => {
                // Get current date and time
                let now = chrono::Utc::now();
                let formatted = format!(
                    "Current Date and Time:\n\
                    Date: {}\n\
                    Time: {} UTC\n\
                    ISO 8601: {}\n\
                    Unix timestamp: {}",
                    now.format("%A, %B %d, %Y"),
                    now.format("%H:%M:%S"),
                    now.to_rfc3339(),
                    now.timestamp()
                );
                Ok(formatted)
            }
            _ => Err(errors::ResponseError::UnknownTool(
                tool_call.tool_type.clone(),
            )),
        }
    }
}
