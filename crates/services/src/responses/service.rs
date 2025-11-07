use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;
use uuid::Uuid;

use crate::completions::ports::CompletionServiceTrait;
use crate::conversations::models::ConversationId;
use crate::conversations::ports::ConversationServiceTrait;
use crate::inference_provider_pool::InferenceProviderPool;
use crate::responses::tools;
use crate::responses::{errors, models, ports};

/// Context for processing a response stream
struct ProcessStreamContext {
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
}

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
            let context = ProcessStreamContext {
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
            };

            if let Err(e) = Self::process_response_stream(tx.clone(), context).await {
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
                    conversation_title: None,
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

            let conv_id = id.parse::<ConversationId>().map_err(|e| {
                errors::ResponseError::InvalidParams(format!("Invalid conversation ID: {e}"))
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
                    errors::ResponseError::InternalError(format!("Invalid response ID format: {e}"))
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

                    // Extract usage from the final chunk
                    Self::extract_and_accumulate_usage(&sse_event, ctx);

                    // Accumulate tool call fragments
                    Self::accumulate_tool_calls(&sse_event, &mut tool_call_accumulator);
                }
                Err(e) => {
                    tracing::error!("Error in completion stream: {}", e);
                    return Err(errors::ResponseError::InternalError(format!(
                        "Stream error: {e}"
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
        // Trim leading and trailing whitespace from the final text
        let trimmed_text = text.trim();

        // Event: response.output_text.done
        emitter
            .emit_text_done(ctx, message_item_id.to_string(), trimmed_text.to_string())
            .await?;

        // Event: response.content_part.done
        let part = models::ResponseOutputContent::OutputText {
            text: trimmed_text.to_string(),
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
                text: trimmed_text.to_string(),
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
                ctx.api_key_id,
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

        for (idx, (name_opt, args_str)) in tool_call_accumulator {
            // Check if name is None or empty string
            let name = match name_opt {
                Some(n) if !n.trim().is_empty() => n,
                _ => {
                    tracing::warn!(
                        "Tool call at index {} has no name or empty name. Args: {}",
                        idx,
                        args_str
                    );
                    // Create a special error tool call to inform the LLM
                    tool_calls_detected.push(ToolCallInfo {
                        tool_type: "__error__".to_string(),
                        query: format!(
                            "Tool call at index {idx} is missing a tool name. Please ensure all tool calls include a valid 'name' field. Arguments provided: {args_str}"
                        ),
                        params: Some(serde_json::json!({
                            "error_type": "missing_tool_name",
                            "index": idx,
                            "arguments": args_str
                        })),
                    });
                    continue;
                }
            };

            // Handle tools that don't require parameters (like current_date)
            if name == "current_date" {
                tracing::debug!("Tool call detected: {}", name);
                tool_calls_detected.push(ToolCallInfo {
                    tool_type: name,
                    query: String::new(),
                    params: None,
                });
            } else {
                // Try to parse the complete arguments for tools that need parameters
                if let Ok(args) = serde_json::from_str::<serde_json::Value>(&args_str) {
                    if let Some(query) = args.get("query").and_then(|v| v.as_str()) {
                        tracing::debug!(
                            "Tool call detected: {} with query: {} and params: {:?}",
                            name,
                            query,
                            args
                        );
                        tool_calls_detected.push(ToolCallInfo {
                            tool_type: name,
                            query: query.to_string(),
                            params: Some(args),
                        });
                    } else {
                        tracing::warn!(
                            "Tool call {} (index {}) has no 'query' field in arguments: {}",
                            name,
                            idx,
                            args_str
                        );
                        // Create an error tool call to inform the LLM about missing query
                        tool_calls_detected.push(ToolCallInfo {
                            tool_type: "__error__".to_string(),
                            query: format!(
                                "Tool call for '{name}' (index {idx}) is missing the required 'query' field in its arguments. Please include a 'query' parameter. Arguments provided: {args_str}"
                            ),
                            params: Some(serde_json::json!({
                                "error_type": "missing_query_field",
                                "tool_name": name,
                                "index": idx,
                                "arguments": args_str
                            })),
                        });
                    }
                } else {
                    tracing::warn!(
                        "Failed to parse tool call {} (index {}) arguments: {}",
                        name,
                        idx,
                        args_str
                    );
                    // Create an error tool call to inform the LLM about invalid JSON
                    tool_calls_detected.push(ToolCallInfo {
                        tool_type: "__error__".to_string(),
                        query: format!(
                            "Tool call for '{name}' (index {idx}) has invalid JSON arguments. Please ensure arguments are valid JSON. Arguments provided: {args_str}"
                        ),
                        params: Some(serde_json::json!({
                            "error_type": "invalid_json",
                            "tool_name": name,
                            "index": idx,
                            "arguments": args_str
                        })),
                    });
                }
            }
        }

        tool_calls_detected
    }

    /// Process the response stream - main logic
    async fn process_response_stream(
        tx: futures::channel::mpsc::UnboundedSender<models::ResponseStreamEvent>,
        context: ProcessStreamContext,
    ) -> Result<(), errors::ResponseError> {
        tracing::info!("Starting response stream processing");

        let conversation_id = Self::parse_conversation_id(&context.request)?;

        let workspace_id_domain = crate::workspace::WorkspaceId(context.workspace_id);
        let mut messages = Self::load_conversation_context(
            &context.request,
            &context.conversation_service,
            &context.response_items_repository,
            workspace_id_domain.clone(),
        )
        .await?;

        // Create the response in the database FIRST before creating any response items
        // This ensures the foreign key constraint is satisfied
        let api_key_uuid = Uuid::parse_str(&context.api_key_id).map_err(|e| {
            errors::ResponseError::InternalError(format!("Invalid API key ID: {e}"))
        })?;
        let initial_response = context
            .response_repository
            .create(
                workspace_id_domain.clone(),
                api_key_uuid,
                context.request.clone(),
            )
            .await
            .map_err(|e| {
                errors::ResponseError::InternalError(format!("Failed to create response: {e}"))
            })?;

        // Extract response_id from the created response
        let response_id = Self::extract_response_uuid(&initial_response)?;

        // Store user input messages as response_items
        if let Some(input) = &context.request.input {
            Self::store_input_as_response_items(
                &context.response_items_repository,
                response_id.clone(),
                api_key_uuid,
                conversation_id.clone(),
                input,
            )
            .await?;
        }

        // Initialize context and emitter
        let mut ctx = crate::responses::service_helpers::ResponseStreamContext::new(
            response_id.clone(),
            api_key_uuid,
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

        // Spawn background task to generate conversation title if needed
        let title_task_handle = Self::maybe_generate_conversation_title(
            conversation_id.clone(),
            &context.request,
            context.user_id.clone(),
            context.api_key_id.clone(),
            context.organization_id,
            context.workspace_id,
            context.conversation_service.clone(),
            context.completion_service.clone(),
            emitter.tx.clone(),
        );

        let tools = Self::prepare_tools(&context.request);
        let tool_choice = Self::prepare_tool_choice(&context.request);

        let max_iterations = 100; // Prevent infinite loops
        let mut iteration = 0;
        let mut final_response_text = String::new();

        // Run the agent loop to process completion and tool calls
        Self::run_agent_loop(
            &mut ctx,
            &mut emitter,
            &mut messages,
            &mut final_response_text,
            &context.request,
            context.user_id.clone(),
            &context.api_key_id,
            context.organization_id,
            context.workspace_id,
            &context.body_hash,
            &tools,
            &tool_choice,
            max_iterations,
            &mut iteration,
            &context.response_items_repository,
            &context.completion_service,
            &context.web_search_provider,
            &context.file_search_provider,
        )
        .await?;

        // Build final response
        let mut final_response = initial_response;
        final_response.status = models::ResponseStatus::Completed;

        // Load all response items from the database for this response
        let response_items = context
            .response_items_repository
            .list_by_response(ctx.response_id.clone())
            .await
            .map_err(|e| {
                errors::ResponseError::InternalError(format!("Failed to load response items: {e}"))
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

        // Set usage from accumulated token counts
        final_response.usage = models::Usage::new(ctx.total_input_tokens, ctx.total_output_tokens);
        tracing::debug!(
            "Final response usage: input={}, output={}, total={}",
            ctx.total_input_tokens,
            ctx.total_output_tokens,
            ctx.total_input_tokens + ctx.total_output_tokens
        );

        // Serialize usage to JSON for database storage
        let usage_json = serde_json::to_value(&final_response.usage).map_err(|e| {
            errors::ResponseError::InternalError(format!("Failed to serialize usage: {e}"))
        })?;

        // Update the response in the database with usage, status, and output message
        if let Err(e) = context
            .response_repository
            .update(
                ctx.response_id.clone(),
                workspace_id_domain.clone(),
                Some(final_response_text.clone()),
                final_response.status.clone(),
                Some(usage_json),
            )
            .await
        {
            tracing::warn!("Failed to update response with usage: {}", e);
            // Continue even if update fails - the response was already created
        }

        // Wait for title generation with a timeout (2 seconds)
        // This ensures the title event is sent before response.completed
        if let Some(handle) = title_task_handle {
            match tokio::time::timeout(std::time::Duration::from_secs(2), handle).await {
                Ok(Ok(Ok(()))) => {
                    tracing::debug!("Title generation completed before response");
                }
                Ok(Ok(Err(e))) => {
                    tracing::warn!("Title generation failed: {:?}", e);
                }
                Ok(Err(e)) => {
                    tracing::warn!("Title generation task panicked: {:?}", e);
                }
                Err(_) => {
                    tracing::debug!("Title generation timed out, continuing with response");
                }
            }
        }

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
                    errors::ResponseError::InternalError(format!("Completion error: {e}"))
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

        // Handle error tool calls (malformed tool calls detected during parsing)
        if tool_call.tool_type == "__error__" {
            // For error tool calls, just return the error message as the tool result
            // This allows the LLM to see what went wrong and retry
            messages.push(CompletionMessage {
                role: "tool".to_string(),
                content: format!(
                    "ERROR: {}\n\nPlease correct the tool call format and try again.",
                    tool_call.query
                ),
            });
            return Ok(());
        }

        // Emit tool-specific start events
        if tool_call.tool_type == "web_search" {
            Self::emit_web_search_start(ctx, emitter, &tool_call_id, tool_call).await?;
        }

        // Execute the tool and catch errors to provide feedback to the LLM
        let tool_result = match Self::execute_tool(
            tool_call,
            web_search_provider,
            file_search_provider,
            request,
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                // Convert tool execution errors into error messages for the LLM
                let error_message = match &e {
                    errors::ResponseError::UnknownTool(tool_name) => {
                        if tool_name.is_empty() {
                            "ERROR: Tool call is missing a tool name. Please ensure all tool calls include a valid 'name' field. Available tools: web_search, file_search, current_date".to_string()
                        } else {
                            format!(
                                "ERROR: Unknown tool '{tool_name}'. Available tools are: web_search, file_search, current_date. Please use one of these valid tool names."
                            )
                        }
                    }
                    errors::ResponseError::InvalidParams(msg) => {
                        format!(
                            "ERROR: Invalid parameters for tool '{}': {}. Please check the tool call arguments and try again.",
                            tool_call.tool_type, msg
                        )
                    }
                    errors::ResponseError::InternalError(msg) => {
                        format!(
                            "ERROR: Internal error while executing tool '{}': {}. Please try again or use a different approach.",
                            tool_call.tool_type, msg
                        )
                    }
                };
                tracing::warn!(
                    "Tool execution error for '{}': {}. Returning error message to LLM.",
                    tool_call.tool_type,
                    error_message
                );
                error_message
            }
        };

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
                ctx.api_key_id,
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
            conversation_title: None,
        };

        emitter.send_raw(event).await
    }

    /// Store user input messages as response_items
    async fn store_input_as_response_items(
        response_items_repository: &Arc<dyn ports::ResponseItemRepositoryTrait>,
        response_id: models::ResponseId,
        api_key_id: uuid::Uuid,
        conversation_id: Option<ConversationId>,
        input: &models::ResponseInput,
    ) -> Result<(), errors::ResponseError> {
        match input {
            models::ResponseInput::Text(text) => {
                // Create a message item for simple text input
                // Trim leading and trailing whitespace
                let trimmed_text = text.trim();
                let message_item = models::ResponseOutputItem::Message {
                    id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
                    status: models::ResponseItemStatus::Completed,
                    role: "user".to_string(),
                    content: vec![models::ResponseOutputContent::OutputText {
                        text: trimmed_text.to_string(),
                        annotations: vec![],
                        logprobs: vec![],
                    }],
                };

                response_items_repository
                    .create(
                        response_id.clone(),
                        api_key_id,
                        conversation_id.clone(),
                        message_item,
                    )
                    .await
                    .map_err(|e| {
                        errors::ResponseError::InternalError(format!(
                            "Failed to store user input: {e}"
                        ))
                    })?;
            }
            models::ResponseInput::Items(items) => {
                // Store each input item as a response_item
                for input_item in items {
                    let content = match &input_item.content {
                        models::ResponseContent::Text(text) => {
                            // Trim leading and trailing whitespace
                            vec![models::ResponseOutputContent::OutputText {
                                text: text.trim().to_string(),
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
                                        // Trim leading and trailing whitespace
                                        Some(models::ResponseOutputContent::OutputText {
                                            text: text.trim().to_string(),
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
                            api_key_id,
                            conversation_id.clone(),
                            message_item,
                        )
                        .await
                        .map_err(|e| {
                            errors::ResponseError::InternalError(format!(
                                "Failed to store user input item: {e}"
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
        workspace_id: crate::workspace::WorkspaceId,
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
                models::ConversationReference::Id(id) => id
                    .parse::<crate::conversations::models::ConversationId>()
                    .map_err(|e| {
                        errors::ResponseError::InvalidParams(format!(
                            "Invalid conversation ID: {e}"
                        ))
                    })?,
                models::ConversationReference::Object { id, metadata: _ } => id
                    .parse::<crate::conversations::models::ConversationId>()
                    .map_err(|e| {
                        errors::ResponseError::InvalidParams(format!(
                            "Invalid conversation ID: {e}"
                        ))
                    })?,
            };

            // Load conversation metadata to verify it exists
            let _conversation = conversation_service
                .get_conversation(conversation_id.clone(), workspace_id.clone())
                .await
                .map_err(|e| {
                    errors::ResponseError::InternalError(format!("Failed to get conversation: {e}"))
                })?;

            // Load all response items from the conversation
            // Use high limit (1000) and no 'after' cursor for context loading
            let conversation_items = response_items_repository
                .list_by_conversation(conversation_id.clone(), None, 1000)
                .await
                .map_err(|e| {
                    errors::ResponseError::InternalError(format!(
                        "Failed to load conversation items: {e}"
                    ))
                })?;

            // Convert response items to completion messages
            let messages_before = messages.len();
            for item in conversation_items {
                if let models::ResponseOutputItem::Message { role, content, .. } = item {
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
                                    "Search the web for current information. Use this when you need up-to-date information or facts that you don't have. \
                                    \n\nIMPORTANT PARAMETERS TO CONSIDER:\
                                    \n- Use 'freshness' for time-sensitive queries (news, recent events, current trends)\
                                    \n- Use 'country' for location-specific information\
                                    \n- Use 'result_filter' to focus on specific content (news, videos, discussions)\
                                    \n- Use 'count' to limit results when user asks for specific number\
                                    \n- Use 'safesearch' when dealing with sensitive topics".to_string()
                                ),
                                parameters: serde_json::json!({
                                    "type": "object",
                                    "properties": {
                                        "query": {
                                            "type": "string",
                                            "description": "The search query to look up (required, max 400 characters)"
                                        },
                                        "country": {
                                            "type": "string",
                                            "description": "2-character country code where results come from (e.g., 'US', 'GB', 'DE'). Use when user asks about location-specific information or mentions a country."
                                        },
                                        "search_lang": {
                                            "type": "string",
                                            "description": "2+ character language code for search results (e.g., 'en', 'es', 'de'). Use when user's query or language preference suggests non-English results."
                                        },
                                        "ui_lang": {
                                            "type": "string",
                                            "description": "User interface language (e.g., 'en-US', 'es-ES')"
                                        },
                                        "count": {
                                            "type": "integer",
                                            "description": "Number of search results to return (1-20, default: 20). Use lower values (5-10) for focused queries, higher values (15-20) for comprehensive research.",
                                            "minimum": 1,
                                            "maximum": 20
                                        },
                                        "offset": {
                                            "type": "integer",
                                            "description": "Zero-based offset for pagination (0-9)",
                                            "minimum": 0,
                                            "maximum": 9
                                        },
                                        "safesearch": {
                                            "type": "string",
                                            "description": "Safe search filter: 'strict' for educational/family content, 'moderate' (default) for general use, 'off' only when explicitly needed",
                                            "enum": ["off", "moderate", "strict"]
                                        },
                                        "freshness": {
                                            "type": "string",
                                            "description": "Filter by freshness: 'pd' (24h) for breaking news, 'pw' (7d) for recent events, 'pm' (31d) for current trends, 'py' (365d) for recent developments. Always use for: news, current events, latest updates, recent changes, today's info."
                                        },
                                        "text_decorations": {
                                            "type": "boolean",
                                            "description": "Include text highlighting markers (default: true)"
                                        },
                                        "spellcheck": {
                                            "type": "boolean",
                                            "description": "Enable spellcheck on query (default: true)"
                                        },
                                        "result_filter": {
                                            "type": "string",
                                            "description": "Comma-delimited result types: 'news' (for news/updates), 'videos' (for tutorials/demos), 'discussions' (for community opinions/Q&A), 'faq' (for how-to questions). Use to focus on most relevant content type."
                                        },
                                        "units": {
                                            "type": "string",
                                            "description": "Measurement units: 'metric' or 'imperial'",
                                            "enum": ["metric", "imperial"]
                                        },
                                        "extra_snippets": {
                                            "type": "boolean",
                                            "description": "Get up to 5 additional alternative excerpts (requires AI/Data plan)"
                                        },
                                        "summary": {
                                            "type": "boolean",
                                            "description": "Enable summary key generation (requires AI/Data plan)"
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

    /// Extract and accumulate usage information from SSE event
    /// Usage typically comes in the final chunk from the provider
    fn extract_and_accumulate_usage(
        event: &inference_providers::SSEEvent,
        ctx: &mut crate::responses::service_helpers::ResponseStreamContext,
    ) {
        use inference_providers::StreamChunk;

        if let StreamChunk::Chat(chat_chunk) = &event.chunk {
            if let Some(usage) = &chat_chunk.usage {
                tracing::debug!(
                    "Extracted usage from completion stream: input={}, output={}",
                    usage.prompt_tokens,
                    usage.completion_tokens
                );
                ctx.add_usage(usage.prompt_tokens, usage.completion_tokens);
            }
        }
    }

    /// Accumulate tool call fragments from streaming chunks
    fn accumulate_tool_calls(
        event: &inference_providers::SSEEvent,
        accumulator: &mut std::collections::HashMap<i64, (Option<String>, String)>,
    ) {
        use inference_providers::StreamChunk;

        if let StreamChunk::Chat(chat_chunk) = &event.chunk {
            for choice in &chat_chunk.choices {
                if let Some(delta) = &choice.delta {
                    if let Some(tool_calls) = &delta.tool_calls {
                        for tool_call in tool_calls {
                            // Get or default to index 0 if not present
                            let index = tool_call.index.unwrap_or(0);

                            // Get or create accumulator entry for this index
                            let entry = accumulator.entry(index).or_insert((None, String::new()));

                            // Handle function delta if present
                            if let Some(function) = &tool_call.function {
                                // Accumulate function name (only set once, typically in first chunk)
                                if let Some(name) = &function.name {
                                    tracing::debug!(
                                        "Accumulated tool call {} name: {}",
                                        index,
                                        name
                                    );
                                    entry.0 = Some(name.clone());
                                }

                                // Accumulate arguments (streamed across multiple chunks)
                                if let Some(args_fragment) = &function.arguments {
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
        }
    }

    /// Execute a tool call
    async fn execute_tool(
        tool_call: &crate::responses::service_helpers::ToolCallInfo,
        web_search_provider: &Option<Arc<dyn tools::WebSearchProviderTrait>>,
        file_search_provider: &Option<Arc<dyn tools::FileSearchProviderTrait>>,
        request: &models::CreateResponseRequest,
    ) -> Result<String, errors::ResponseError> {
        // Check for empty tool type
        if tool_call.tool_type.trim().is_empty() {
            return Err(errors::ResponseError::UnknownTool("".to_string()));
        }

        match tool_call.tool_type.as_str() {
            "web_search" => {
                if let Some(provider) = web_search_provider {
                    // Build WebSearchParams from tool call parameters
                    let mut search_params = tools::WebSearchParams::new(tool_call.query.clone());

                    // Parse additional parameters if present
                    if let Some(params) = &tool_call.params {
                        // Extract optional parameters from JSON
                        if let Some(country) = params.get("country").and_then(|v| v.as_str()) {
                            search_params.country = Some(country.to_string());
                        }
                        if let Some(lang) = params.get("search_lang").and_then(|v| v.as_str()) {
                            search_params.search_lang = Some(lang.to_string());
                        }
                        if let Some(ui_lang) = params.get("ui_lang").and_then(|v| v.as_str()) {
                            search_params.ui_lang = Some(ui_lang.to_string());
                        }
                        if let Some(count) = params.get("count").and_then(|v| v.as_u64()) {
                            search_params.count = Some(count as u32);
                        }
                        if let Some(offset) = params.get("offset").and_then(|v| v.as_u64()) {
                            search_params.offset = Some(offset as u32);
                        }
                        if let Some(safesearch) = params.get("safesearch").and_then(|v| v.as_str())
                        {
                            search_params.safesearch = Some(safesearch.to_string());
                        }
                        if let Some(freshness) = params.get("freshness").and_then(|v| v.as_str()) {
                            search_params.freshness = Some(freshness.to_string());
                        }
                        if let Some(text_decorations) =
                            params.get("text_decorations").and_then(|v| v.as_bool())
                        {
                            search_params.text_decorations = Some(text_decorations);
                        }
                        if let Some(spellcheck) = params.get("spellcheck").and_then(|v| v.as_bool())
                        {
                            search_params.spellcheck = Some(spellcheck);
                        }
                        if let Some(result_filter) =
                            params.get("result_filter").and_then(|v| v.as_str())
                        {
                            search_params.result_filter = Some(result_filter.to_string());
                        }
                        if let Some(units) = params.get("units").and_then(|v| v.as_str()) {
                            search_params.units = Some(units.to_string());
                        }
                        if let Some(extra_snippets) =
                            params.get("extra_snippets").and_then(|v| v.as_bool())
                        {
                            search_params.extra_snippets = Some(extra_snippets);
                        }
                        if let Some(summary) = params.get("summary").and_then(|v| v.as_bool()) {
                            search_params.summary = Some(summary);
                        }
                    }

                    let results = provider.search(search_params).await.map_err(|e| {
                        errors::ResponseError::InternalError(format!("Web search failed: {e}"))
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
                                    "Invalid conversation ID: {e}"
                                ))
                            })?
                        }
                        Some(models::ConversationReference::Object { id, .. }) => {
                            let uuid_str = id.strip_prefix("conv_").unwrap_or(id);
                            uuid::Uuid::parse_str(uuid_str).map_err(|e| {
                                errors::ResponseError::InvalidParams(format!(
                                    "Invalid conversation ID: {e}"
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
                            errors::ResponseError::InternalError(format!("File search failed: {e}"))
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

    /// Check if conversation needs title generation and spawn background task if needed
    /// Returns a JoinHandle that can be awaited to ensure title generation completes before response finishes
    #[allow(clippy::too_many_arguments)]
    fn maybe_generate_conversation_title(
        conversation_id: Option<ConversationId>,
        request: &models::CreateResponseRequest,
        user_id: crate::UserId,
        api_key_id: String,
        organization_id: uuid::Uuid,
        workspace_id: uuid::Uuid,
        conversation_service: Arc<dyn ConversationServiceTrait>,
        completion_service: Arc<dyn CompletionServiceTrait>,
        tx: futures::channel::mpsc::UnboundedSender<models::ResponseStreamEvent>,
    ) -> Option<tokio::task::JoinHandle<Result<(), errors::ResponseError>>> {
        let model = request.model.clone();
        // Only proceed if we have a conversation_id
        let conv_id = conversation_id?;

        // Extract first user message from request
        let user_message = match &request.input {
            Some(models::ResponseInput::Text(text)) => text.clone(),
            Some(models::ResponseInput::Items(items)) => {
                // Find first user message
                items
                    .iter()
                    .find(|item| item.role == "user")
                    .and_then(|item| match &item.content {
                        models::ResponseContent::Text(text) => Some(text.clone()),
                        models::ResponseContent::Parts(parts) => {
                            // Extract text from parts
                            let text = parts
                                .iter()
                                .filter_map(|part| match part {
                                    models::ResponseContentPart::InputText { text } => {
                                        Some(text.clone())
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            if text.is_empty() {
                                None
                            } else {
                                Some(text)
                            }
                        }
                    })
                    .unwrap_or_default()
            }
            None => return None,
        };

        if user_message.is_empty() {
            return None;
        }

        // Spawn background task to check and generate title
        let handle = tokio::spawn(async move {
            Self::generate_and_update_title(
                conv_id,
                user_id,
                user_message,
                model,
                api_key_id,
                organization_id,
                workspace_id,
                conversation_service,
                completion_service,
                tx,
            )
            .await
        });

        Some(handle)
    }

    /// Generate conversation title and update metadata (background task)
    #[allow(clippy::too_many_arguments)]
    async fn generate_and_update_title(
        conversation_id: ConversationId,
        user_id: crate::UserId,
        user_message: String,
        model: String,
        api_key_id: String,
        organization_id: uuid::Uuid,
        workspace_id: uuid::Uuid,
        conversation_service: Arc<dyn ConversationServiceTrait>,
        completion_service: Arc<dyn CompletionServiceTrait>,
        mut tx: futures::channel::mpsc::UnboundedSender<models::ResponseStreamEvent>,
    ) -> Result<(), errors::ResponseError> {
        // Get conversation to check if it already has a title
        let workspace_id_domain = crate::workspace::WorkspaceId(workspace_id);
        let conversation = conversation_service
            .get_conversation(conversation_id.clone(), workspace_id_domain.clone())
            .await
            .map_err(|e| {
                errors::ResponseError::InternalError(format!("Failed to get conversation: {e}"))
            })?;

        let conversation = match conversation {
            Some(c) => c,
            None => {
                tracing::debug!("Conversation not found, skipping title generation");
                return Ok(());
            }
        };

        // Check if conversation already has a title
        if let Some(title) = conversation.metadata.get("title") {
            if !title.is_null() && title.as_str().is_some() {
                tracing::debug!("Conversation already has a title, skipping generation");
                return Ok(());
            }
        }

        // Truncate user message for title generation (max 500 chars for context)
        let truncated_message = if user_message.len() > 500 {
            format!("{}...", &user_message[..500])
        } else {
            user_message.clone()
        };

        // Create prompt for title generation
        let title_prompt = format!(
            "Generate a short, descriptive title (maximum 60 characters) for a conversation that starts with this message. \
            Only respond with the title, nothing else.\n\nMessage: {truncated_message}"
        );

        // Generate title using completion service
        let completion_request = crate::completions::ports::CompletionRequest {
            model, // Use the same model as the user's request
            messages: vec![crate::completions::ports::CompletionMessage {
                role: "user".to_string(),
                content: title_prompt,
            }],
            max_tokens: Some(20), // Short response for title
            temperature: Some(0.7),
            top_p: None,
            stop: None,
            stream: Some(false),
            user_id: user_id.clone(),
            api_key_id, // Use the same API key as the user's request
            organization_id,
            workspace_id,
            metadata: None,
            body_hash: String::new(),
            n: None,
            extra: std::collections::HashMap::new(),
        };

        // Call completion service to generate title
        let completion_result = completion_service
            .create_chat_completion(completion_request)
            .await
            .map_err(|e| {
                errors::ResponseError::InternalError(format!("Failed to generate title: {e}"))
            })?;

        // Extract title from completion result
        let generated_title = completion_result
            .response
            .choices
            .first()
            .and_then(|choice| choice.message.content.as_ref())
            .map(|content| content.trim().to_string())
            .unwrap_or_else(|| "Conversation".to_string());

        // Truncate to max 60 characters
        let title = if generated_title.len() > 60 {
            format!("{}...", &generated_title[..57])
        } else {
            generated_title
        };

        // Update conversation metadata with title
        let mut updated_metadata = conversation.metadata.clone();
        updated_metadata["title"] = serde_json::Value::String(title.clone());

        let workspace_id_domain = crate::workspace::WorkspaceId(workspace_id);
        conversation_service
            .update_conversation(
                conversation_id.clone(),
                workspace_id_domain,
                updated_metadata,
            )
            .await
            .map_err(|e| {
                errors::ResponseError::InternalError(format!(
                    "Failed to update conversation metadata: {e}"
                ))
            })?;

        tracing::info!(
            "Generated title for conversation {}: {}",
            conversation_id,
            title
        );

        // Emit conversation.title.updated event
        use futures::SinkExt;
        let event = models::ResponseStreamEvent {
            event_type: "conversation.title.updated".to_string(),
            sequence_number: None, // No sequence number for background events
            response: None,
            output_index: None,
            content_index: None,
            item: None,
            item_id: None,
            part: None,
            delta: None,
            text: None,
            logprobs: None,
            obfuscation: None,
            annotation_index: None,
            annotation: None,
            conversation_title: Some(title),
        };

        let _ = tx.send(event).await;

        Ok(())
    }
}
