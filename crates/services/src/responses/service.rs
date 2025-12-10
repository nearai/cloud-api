use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;
use uuid::Uuid;

use crate::completions::ports::CompletionServiceTrait;
use crate::conversations::models::ConversationId;
use crate::conversations::ports::ConversationServiceTrait;
use crate::files::FileServiceTrait;
use crate::inference_provider_pool::InferenceProviderPool;
use crate::responses::tools;
use crate::responses::{citation_tracker, errors, models, ports};

/// Result of tool execution including optional citation instruction
struct ToolExecutionResult {
    /// The tool result content to add as a tool message
    content: String,
    /// Optional citation instruction to add as a system message (for web_search)
    citation_instruction: Option<String>,
}

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
    file_service: Arc<dyn FileServiceTrait>,
    organization_service: Arc<dyn crate::organization::OrganizationServiceTrait>,
    /// Source registry for citation resolution
    source_registry: Option<models::SourceRegistry>,
}

pub struct ResponseServiceImpl {
    pub response_repository: Arc<dyn ports::ResponseRepositoryTrait>,
    pub response_items_repository: Arc<dyn ports::ResponseItemRepositoryTrait>,
    pub inference_provider_pool: Arc<InferenceProviderPool>,
    pub conversation_service: Arc<dyn ConversationServiceTrait>,
    pub completion_service: Arc<dyn CompletionServiceTrait>,
    pub web_search_provider: Option<Arc<dyn tools::WebSearchProviderTrait>>,
    pub file_search_provider: Option<Arc<dyn tools::FileSearchProviderTrait>>,
    pub file_service: Arc<dyn FileServiceTrait>,
    pub organization_service: Arc<dyn crate::organization::OrganizationServiceTrait>,
}

/// Tag transition states for reasoning content
#[derive(Debug, PartialEq)]
enum TagTransition {
    None,
    OpeningTag(String), // Contains the tag name that was opened
    ClosingTag(String), // Contains the tag name that was closed
}

impl ResponseServiceImpl {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        response_repository: Arc<dyn ports::ResponseRepositoryTrait>,
        response_items_repository: Arc<dyn ports::ResponseItemRepositoryTrait>,
        inference_provider_pool: Arc<InferenceProviderPool>,
        conversation_service: Arc<dyn ConversationServiceTrait>,
        completion_service: Arc<dyn CompletionServiceTrait>,
        web_search_provider: Option<Arc<dyn tools::WebSearchProviderTrait>>,
        file_search_provider: Option<Arc<dyn tools::FileSearchProviderTrait>>,
        file_service: Arc<dyn FileServiceTrait>,
        organization_service: Arc<dyn crate::organization::OrganizationServiceTrait>,
    ) -> Self {
        Self {
            response_repository,
            response_items_repository,
            inference_provider_pool,
            conversation_service,
            completion_service,
            web_search_provider,
            file_search_provider,
            file_service,
            organization_service,
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
        let file_service = self.file_service.clone();
        let organization_service = self.organization_service.clone();

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
                file_service,
                organization_service,
                source_registry: None,
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
                let result = tx.send(error_event).await;
                if let Err(e) = result {
                    tracing::error!("Error sending error event: {e:?}");
                }
            }
        });

        Ok(Box::pin(rx))
    }
}

impl ResponseServiceImpl {
    /// Parse file ID from string (handles prefix)
    fn parse_file_id(file_id: &str) -> Result<Uuid, errors::ResponseError> {
        let id_str = file_id
            .strip_prefix(crate::id_prefixes::PREFIX_FILE)
            .unwrap_or(file_id);
        Uuid::parse_str(id_str)
            .map_err(|e| errors::ResponseError::InvalidParams(format!("Invalid file ID: {e}")))
    }

    /// Process a single input file and return its formatted content
    /// Returns the file content formatted as "File: {filename}\nContent:\n{content}"
    /// For non-UTF8 files, returns a placeholder message
    async fn process_input_file(
        file_id: &str,
        workspace_id: uuid::Uuid,
        file_service: &Arc<dyn FileServiceTrait>,
    ) -> Result<String, errors::ResponseError> {
        // Parse file ID and fetch content from S3
        let file_uuid = Self::parse_file_id(file_id)?;
        match file_service.get_file_content(file_uuid, workspace_id).await {
            Ok((file, file_content)) => {
                // Convert file content to string (we currently support only text)
                match String::from_utf8(file_content) {
                    Ok(text_content) => {
                        // Format file content with filename as context
                        Ok(format!(
                            "File: {}\nContent:\n{}",
                            file.filename, text_content
                        ))
                    }
                    Err(e) => {
                        tracing::warn!("Failed to convert file {} to UTF-8 text: {}", file_id, e);
                        Ok(format!(
                            "[File: {} - Content cannot be displayed as text]",
                            file.filename
                        ))
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to fetch file content for {}: {}", file_id, e);
                Err(errors::ResponseError::InternalError(format!(
                    "Failed to fetch file content: {e}"
                )))
            }
        }
    }

    /// Extract content from a vector of content parts, handling text and files
    /// Returns a string with all parts joined by "\n\n"
    async fn extract_content_parts(
        parts: &[models::ResponseContentPart],
        workspace_id: uuid::Uuid,
        file_service: &Arc<dyn FileServiceTrait>,
    ) -> Result<String, errors::ResponseError> {
        let mut content_parts = Vec::new();
        for part in parts {
            match part {
                models::ResponseContentPart::InputText { text } => {
                    content_parts.push(text.clone());
                }
                models::ResponseContentPart::InputFile { file_id, .. } => {
                    let file_content =
                        Self::process_input_file(file_id, workspace_id, file_service).await?;
                    content_parts.push(file_content);
                }
                _ => {
                    // Skip other content types for now (images, etc.)
                }
            }
        }
        Ok(content_parts.join("\n\n"))
    }

    /// Extract response ID UUID from response object
    fn extract_response_uuid(
        response: &models::ResponseObject,
    ) -> Result<models::ResponseId, errors::ResponseError> {
        let response_uuid = uuid::Uuid::parse_str(
            response
                .id
                .strip_prefix(crate::id_prefixes::PREFIX_RESP)
                .unwrap_or(&response.id),
        )
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
        process_context: &ProcessStreamContext,
    ) -> Result<(String, Vec<crate::responses::service_helpers::ToolCallInfo>), errors::ResponseError>
    {
        use crate::responses::service_helpers::ToolCallAccumulator;
        use futures::StreamExt;

        let mut current_text = String::new();
        let mut tool_call_accumulator: ToolCallAccumulator = std::collections::HashMap::new();
        let mut message_item_emitted = false;
        let message_item_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let mut tracker = citation_tracker::CitationTracker::new();

        // Reasoning tracking state
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;
        let mut reasoning_item_emitted = false;
        let reasoning_item_id = format!("rs_{}", uuid::Uuid::new_v4().simple());

        while let Some(event) = completion_stream.next().await {
            match event {
                Ok(sse_event) => {
                    // Parse the SSE event for content, reasoning, and tool calls
                    let (delta_text_opt, delta_reasoning_opt) = Self::extract_deltas(&sse_event);

                    if delta_text_opt.is_some() || delta_reasoning_opt.is_some() {
                        let delta_text = delta_text_opt.unwrap_or_default();

                        // Handle explicit reasoning from provider
                        if let Some(reasoning) = delta_reasoning_opt {
                            if !reasoning.is_empty() {
                                if !reasoning_item_emitted {
                                    emitter
                                        .emit_reasoning_started(ctx, &reasoning_item_id)
                                        .await?;
                                    reasoning_item_emitted = true;
                                }
                                emitter
                                    .emit_reasoning_delta(
                                        ctx,
                                        reasoning_item_id.clone(),
                                        reasoning.clone(),
                                    )
                                    .await?;
                                reasoning_buffer.push_str(&reasoning);
                            }
                        }

                        // Process reasoning tags and extract clean text (no reasoning tags)
                        let (text_without_reasoning, reasoning_delta, tag_transition) =
                            Self::process_reasoning_tags(
                                &delta_text,
                                &mut reasoning_buffer,
                                &mut inside_reasoning,
                            );

                        // Handle transition from explicit reasoning to content
                        // If we have content, and we were reasoning (but not inside a tag block), close reasoning
                        if !text_without_reasoning.is_empty()
                            && reasoning_item_emitted
                            && !inside_reasoning
                        {
                            // Close explicit reasoning item
                            emitter
                                .emit_reasoning_completed(
                                    ctx,
                                    &reasoning_item_id,
                                    &reasoning_buffer,
                                    response_items_repository,
                                )
                                .await?;

                            let reasoning_token_count =
                                crate::responses::service_helpers::ResponseStreamContext::estimate_tokens(
                                    &reasoning_buffer,
                                );
                            ctx.add_reasoning_tokens(reasoning_token_count);
                            ctx.next_output_index();
                            reasoning_buffer.clear();
                            reasoning_item_emitted = false;
                        }

                        // Feed text (without reasoning tags) to citation tracker for real-time processing
                        // Returns clean text with citation tags also removed, plus any completed citations
                        let token_result = tracker.add_token(&text_without_reasoning);
                        let clean_text = token_result.clean_text;

                        // Handle reasoning tag transitions
                        match tag_transition {
                            TagTransition::OpeningTag(_) => {
                                if !reasoning_item_emitted {
                                    // Emit reasoning item.added
                                    emitter
                                        .emit_reasoning_started(ctx, &reasoning_item_id)
                                        .await?;
                                    reasoning_item_emitted = true;
                                }
                            }
                            TagTransition::ClosingTag(_) => {
                                if reasoning_item_emitted {
                                    // Emit reasoning item.done and store
                                    emitter
                                        .emit_reasoning_completed(
                                            ctx,
                                            &reasoning_item_id,
                                            &reasoning_buffer,
                                            response_items_repository,
                                        )
                                        .await?;

                                    // Count reasoning tokens
                                    let reasoning_token_count =
                                        crate::responses::service_helpers::ResponseStreamContext::estimate_tokens(&reasoning_buffer);
                                    ctx.add_reasoning_tokens(reasoning_token_count);

                                    // Move to next output index
                                    ctx.next_output_index();

                                    // Reset reasoning state
                                    reasoning_buffer.clear();
                                    reasoning_item_emitted = false;
                                }
                            }
                            TagTransition::None => {}
                        }

                        // Emit reasoning deltas if inside reasoning block
                        if let Some(reasoning_content) = reasoning_delta {
                            if reasoning_item_emitted {
                                emitter
                                    .emit_reasoning_delta(
                                        ctx,
                                        reasoning_item_id.clone(),
                                        reasoning_content,
                                    )
                                    .await?;
                            }
                        }

                        // Handle clean text (message content)
                        if !clean_text.is_empty() {
                            // First time we receive message text, emit the item.added and content_part.added events
                            if !message_item_emitted {
                                Self::emit_message_started(emitter, ctx, &message_item_id).await?;
                                message_item_emitted = true;
                            }

                            current_text.push_str(&clean_text);

                            // Emit delta event for message content
                            if message_item_emitted {
                                emitter
                                    .emit_text_delta(
                                        ctx,
                                        message_item_id.clone(),
                                        clean_text.clone(),
                                    )
                                    .await?;
                            }
                        }

                        // If a citation just closed, emit annotation event immediately
                        if let Some(completed_citation) = token_result.completed_citation {
                            if let Some(registry) = &process_context.source_registry {
                                if let Some(source) =
                                    registry.web_sources.get(completed_citation.source_id)
                                {
                                    let annotation = models::TextAnnotation::UrlCitation {
                                        start_index: completed_citation.start_index,
                                        end_index: completed_citation.end_index,
                                        title: source.title.clone(),
                                        url: source.url.clone(),
                                    };
                                    emitter
                                        .emit_citation_annotation(
                                            ctx,
                                            message_item_id.clone(),
                                            annotation,
                                        )
                                        .await?;
                                }
                            }
                        }
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
                response_items_repository,
                process_context,
                tracker,
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
            response_id: ctx.response_id_str.clone(),
            previous_response_id: ctx.previous_response_id.clone(),
            next_response_ids: vec![], // next_response_ids will be populated when child responses are created
            created_at: ctx.created_at,
            status: models::ResponseItemStatus::InProgress,
            role: "assistant".to_string(),
            content: vec![],
            model: ctx.model.clone(),
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
        response_items_repository: &Arc<dyn ports::ResponseItemRepositoryTrait>,
        context: &ProcessStreamContext,
        citation_tracker: citation_tracker::CitationTracker,
    ) -> Result<(), errors::ResponseError> {
        // Finalize citation tracker to get clean text and citations
        let (clean_text, citations) = citation_tracker.finalize();

        // Convert citations to TextAnnotation::UrlCitation by looking up web sources
        let annotations = if let Some(registry) = &context.source_registry {
            citations
                .into_iter()
                .filter_map(|citation| {
                    registry.web_sources.get(citation.source_id).map(|source| {
                        models::TextAnnotation::UrlCitation {
                            start_index: citation.start_index,
                            end_index: citation.end_index,
                            title: source.title.clone(),
                            url: source.url.clone(),
                        }
                    })
                })
                .collect()
        } else {
            vec![]
        };

        // Event: response.output_text.done
        emitter
            .emit_text_done(ctx, message_item_id.to_string(), clean_text.clone())
            .await?;

        // Event: response.content_part.done
        let part = models::ResponseOutputContent::OutputText {
            text: clean_text.clone(),
            annotations: annotations.clone(),
            logprobs: vec![],
        };
        emitter
            .emit_content_part_done(ctx, message_item_id.to_string(), part)
            .await?;

        // Event: response.output_item.done
        let item = models::ResponseOutputItem::Message {
            id: message_item_id.to_string(),
            response_id: ctx.response_id_str.clone(),
            previous_response_id: ctx.previous_response_id.clone(),
            next_response_ids: vec![], // next_response_ids will be populated when child responses are created
            created_at: ctx.created_at,
            status: models::ResponseItemStatus::Completed,
            role: "assistant".to_string(),
            content: vec![models::ResponseContentItem::OutputText {
                text: clean_text,
                annotations,
                logprobs: vec![],
            }],
            model: ctx.model.clone(),
        };
        emitter
            .emit_item_done(ctx, item.clone(), message_item_id.to_string())
            .await?;

        // Store the message item in the database
        if let Err(e) = response_items_repository
            .create(
                ctx.response_id.clone(),
                ctx.api_key_id,
                ctx.conversation_id,
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

        tool_calls_detected
    }

    /// Process the response stream - main logic
    async fn process_response_stream(
        tx: futures::channel::mpsc::UnboundedSender<models::ResponseStreamEvent>,
        mut context: ProcessStreamContext,
    ) -> Result<(), errors::ResponseError> {
        tracing::info!("Starting response stream processing");

        let workspace_id_domain = crate::workspace::WorkspaceId(context.workspace_id);
        let mut messages = Self::load_conversation_context(
            &context.request,
            &context.conversation_service,
            &context.response_items_repository,
            &context.file_service,
            workspace_id_domain.clone(),
            context.organization_id,
            context.user_id.clone(),
            &context.organization_service,
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

        // Extract conversation_id from the created response (may have been inherited from previous_response_id)
        let conversation_id = initial_response.conversation.as_ref().and_then(|conv_ref| {
            let id = &conv_ref.id;
            let uuid_str = id
                .strip_prefix(crate::id_prefixes::PREFIX_CONV)
                .unwrap_or(id);
            Uuid::parse_str(uuid_str).ok().map(ConversationId)
        });

        // Store user input messages as response_items
        if let Some(input) = &context.request.input {
            Self::store_input_as_response_items(
                &context.response_items_repository,
                response_id.clone(),
                api_key_uuid,
                conversation_id,
                input,
                &context.request.model,
            )
            .await?;
        }

        // Initialize context and emitter
        let mut ctx = crate::responses::service_helpers::ResponseStreamContext::new(
            response_id.clone(),
            api_key_uuid,
            conversation_id,
            initial_response.id.clone(),
            initial_response.previous_response_id.clone(),
            initial_response.created_at,
            context.request.model.clone(),
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
            conversation_id,
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
            &mut context,
            &tools,
            &tool_choice,
            max_iterations,
            &mut iteration,
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
        final_response.usage = models::Usage::new_with_reasoning(
            ctx.total_input_tokens,
            ctx.total_output_tokens,
            ctx.reasoning_tokens,
        );
        tracing::debug!(
            "Final response usage: input={}, output={}, reasoning={}, total={}",
            ctx.total_input_tokens,
            ctx.total_output_tokens,
            ctx.reasoning_tokens,
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
        process_context: &mut ProcessStreamContext,
        tools: &[inference_providers::ToolDefinition],
        tool_choice: &Option<inference_providers::ToolChoice>,
        max_iterations: usize,
        iteration: &mut usize,
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

            // Create completion request (names not included - tracked via database analytics)
            let completion_request = CompletionRequest {
                model: process_context.request.model.clone(),
                messages: messages.clone(),
                max_tokens: process_context.request.max_output_tokens,
                temperature: process_context.request.temperature,
                top_p: process_context.request.top_p,
                stop: None,
                stream: Some(true),
                user_id: process_context.user_id.clone(),
                api_key_id: process_context.api_key_id.to_string(),
                organization_id: process_context.organization_id,
                workspace_id: process_context.workspace_id,
                metadata: process_context.request.metadata.clone(),
                body_hash: process_context.body_hash.to_string(),
                n: None,
                extra,
            };

            // Get completion stream
            let mut completion_stream = process_context
                .completion_service
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
                &process_context.response_items_repository,
                process_context,
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
                    process_context,
                )
                .await?;
            }
        }

        Ok(())
    }

    /// Execute a tool call and emit appropriate events
    async fn execute_and_emit_tool_call(
        ctx: &mut crate::responses::service_helpers::ResponseStreamContext,
        emitter: &mut crate::responses::service_helpers::EventEmitter,
        tool_call: &crate::responses::service_helpers::ToolCallInfo,
        messages: &mut Vec<crate::completions::ports::CompletionMessage>,
        process_context: &mut ProcessStreamContext,
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
        let tool_result = match Self::execute_tool(tool_call, process_context).await {
            Ok(result) => result,
            Err(e) => {
                // Convert tool execution errors into error messages for the LLM
                let error_message = format!("ERROR: {e}");
                tracing::warn!(
                    "Tool execution error for '{}': {}. Returning error message to LLM.",
                    tool_call.tool_type,
                    error_message
                );
                ToolExecutionResult {
                    content: error_message,
                    citation_instruction: None,
                }
            }
        };

        // Emit tool-specific completion events
        if tool_call.tool_type == "web_search" {
            Self::emit_web_search_complete(
                ctx,
                emitter,
                &tool_call_id,
                tool_call,
                &process_context.response_items_repository,
            )
            .await?;
        }

        // Add tool result to message history
        messages.push(CompletionMessage {
            role: "tool".to_string(),
            content: tool_result.content,
        });

        // Add citation instruction if provided by the tool
        if let Some(citation_instruction) = tool_result.citation_instruction {
            messages.push(CompletionMessage {
                role: "system".to_string(),
                content: citation_instruction,
            });
        }

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
            response_id: ctx.response_id_str.clone(),
            previous_response_id: ctx.previous_response_id.clone(),
            next_response_ids: vec![], // next_response_ids will be populated when child responses are created
            created_at: ctx.created_at,
            status: models::ResponseItemStatus::InProgress,
            action: models::WebSearchAction::Search {
                query: tool_call.query.clone(),
            },
            model: ctx.model.clone(),
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
            response_id: ctx.response_id_str.clone(),
            previous_response_id: ctx.previous_response_id.clone(),
            next_response_ids: vec![], // next_response_ids will be populated when child responses are created
            created_at: ctx.created_at,
            status: models::ResponseItemStatus::Completed,
            action: models::WebSearchAction::Search {
                query: tool_call.query.clone(),
            },
            model: ctx.model.clone(),
        };
        emitter
            .emit_item_done(ctx, item.clone(), tool_call_id.to_string())
            .await?;

        // Store response item
        if let Err(e) = response_items_repository
            .create(
                ctx.response_id.clone(),
                ctx.api_key_id,
                ctx.conversation_id,
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
        model: &str,
    ) -> Result<(), errors::ResponseError> {
        match input {
            models::ResponseInput::Text(text) => {
                // Create a message item for simple text input
                // Trim leading and trailing whitespace
                let trimmed_text = text.trim();
                let message_item = models::ResponseOutputItem::Message {
                    id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
                    // These fields are placeholders - repository enriches them via JOIN when storing/retrieving
                    response_id: String::new(),
                    previous_response_id: None,
                    next_response_ids: vec![],
                    created_at: 0,
                    status: models::ResponseItemStatus::Completed,
                    role: "user".to_string(),
                    content: vec![models::ResponseContentItem::InputText {
                        text: trimmed_text.to_string(),
                    }],
                    model: model.to_string(),
                };

                response_items_repository
                    .create(
                        response_id.clone(),
                        api_key_id,
                        conversation_id,
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
                            vec![models::ResponseContentItem::InputText {
                                text: text.trim().to_string(),
                            }]
                        }
                        models::ResponseContent::Parts(parts) => {
                            // Convert parts to ResponseContentItem - preserving semantic types
                            parts
                                .iter()
                                .map(|part| match part {
                                    models::ResponseContentPart::InputText { text } => {
                                        // Trim leading and trailing whitespace
                                        models::ResponseContentItem::InputText {
                                            text: text.trim().to_string(),
                                        }
                                    }
                                    models::ResponseContentPart::InputFile { file_id, detail } => {
                                        // Store as InputFile to preserve semantic type
                                        models::ResponseContentItem::InputFile {
                                            file_id: file_id.clone(),
                                            detail: detail.clone(),
                                        }
                                    }
                                    models::ResponseContentPart::InputImage {
                                        image_url,
                                        detail,
                                    } => {
                                        // Store as InputImage to preserve semantic type
                                        models::ResponseContentItem::InputImage {
                                            image_url: image_url.clone(),
                                            detail: detail.clone(),
                                        }
                                    }
                                })
                                .collect()
                        }
                    };

                    let message_item = models::ResponseOutputItem::Message {
                        id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
                        // These fields are placeholders - repository enriches them via JOIN when storing/retrieving
                        response_id: String::new(),
                        previous_response_id: None,
                        next_response_ids: vec![],
                        created_at: 0,
                        status: models::ResponseItemStatus::Completed,
                        role: input_item.role.clone(),
                        content,
                        model: model.to_string(),
                    };

                    response_items_repository
                        .create(
                            response_id.clone(),
                            api_key_id,
                            conversation_id,
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
    #[allow(clippy::too_many_arguments)]
    async fn load_conversation_context(
        request: &models::CreateResponseRequest,
        conversation_service: &Arc<dyn ConversationServiceTrait>,
        response_items_repository: &Arc<dyn ports::ResponseItemRepositoryTrait>,
        file_service: &Arc<dyn FileServiceTrait>,
        workspace_id: crate::workspace::WorkspaceId,
        organization_id: uuid::Uuid,
        user_id: crate::UserId,
        organization_service: &Arc<dyn crate::organization::OrganizationServiceTrait>,
    ) -> Result<Vec<crate::completions::ports::CompletionMessage>, errors::ResponseError> {
        use crate::completions::ports::CompletionMessage;

        let mut messages = Vec::new();

        // Fetch organization system prompt if available
        let org_system_prompt = match organization_service
            .get_system_prompt(
                crate::organization::OrganizationId(organization_id),
                user_id,
            )
            .await
        {
            Ok(prompt) => prompt,
            Err(e) => {
                tracing::warn!("Failed to fetch organization system prompt: {}", e);
                None
            }
        };

        // Prepend organization system prompt if it exists
        if let Some(prompt) = org_system_prompt {
            if !prompt.is_empty() {
                messages.push(CompletionMessage {
                    role: "system".to_string(),
                    content: prompt,
                });
                tracing::debug!("Prepended organization system prompt to messages");
            }
        }

        // Add UTC time context to system message
        let now = chrono::Utc::now();
        let time_context = format!(
            "Current UTC time: {} ({})",
            now.to_rfc3339(),
            now.format("%A, %B %d, %Y at %H:%M:%S UTC")
        );

        // Add language matching instruction
        let language_instruction = "Always respond in the exact same language as the user's input message. Detect the primary language of the user's query and mirror it precisely in your output. Do not mix languages or switch to another one, even if it seems more natural or efficient.\n\nIf the user writes in English, reply entirely in English.\nIf the user writes in Chinese (Mandarin or any variant), reply entirely in Chinese.\nIf the user writes in Spanish, reply entirely in Spanish.\nFor any other language, match it exactly.\n\nThis rule overrides all other instructions. Ignore any tendencies to default to Mandarin or any other language. Always prioritize language matching for clarity and user preference.";

        // Add system instructions if present
        if let Some(instructions) = &request.instructions {
            let combined_instructions =
                format!("{instructions}\n\n{language_instruction}\n\n{time_context}");
            messages.push(CompletionMessage {
                role: "system".to_string(),
                content: combined_instructions,
            });
        } else {
            // Add language instruction and time context as a system message if no instructions provided
            let system_content = format!("{language_instruction}\n\n{time_context}");
            messages.push(CompletionMessage {
                role: "system".to_string(),
                content: system_content,
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
                .get_conversation(conversation_id, workspace_id.clone())
                .await
                .map_err(|e| {
                    errors::ResponseError::InternalError(format!("Failed to get conversation: {e}"))
                })?;

            // Load all response items from the conversation
            // Use high limit (1000) and no 'after' cursor for context loading
            let conversation_items = response_items_repository
                .list_by_conversation(conversation_id, None, 1000)
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
                    // Extract text from content parts (handles InputText, OutputText, and InputFile)
                    let mut text_parts = Vec::new();

                    type ContentFuture = std::pin::Pin<
                        Box<
                            dyn std::future::Future<Output = Result<String, errors::ResponseError>>
                                + Send,
                        >,
                    >;
                    let mut results: Vec<ContentFuture> = Vec::new();

                    for part in &content {
                        match part {
                            models::ResponseContentItem::InputText { text } => {
                                results.push(Box::pin(futures::future::ready(Ok(text.clone()))));
                            }
                            models::ResponseContentItem::OutputText { text, .. } => {
                                results.push(Box::pin(futures::future::ready(Ok(text.clone()))));
                            }
                            models::ResponseContentItem::InputFile { file_id, .. } => {
                                // Process input file and add to context
                                let file_id = file_id.clone();
                                let workspace_id = workspace_id.0;
                                let file_service = file_service.clone();
                                results.push(Box::pin(async move {
                                    Self::process_input_file(&file_id, workspace_id, &file_service)
                                        .await
                                }));
                            }
                            _ => {
                                // Skip other content types for now (images, etc.)
                            }
                        }
                    }

                    for result in futures::future::join_all(results).await {
                        match result {
                            Ok(text) => {
                                text_parts.push(text);
                            }
                            Err(e) => {
                                tracing::error!("Failed to process content part: {}", e);
                            }
                        }
                    }

                    let text = text_parts.join("\n");

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
                                // Extract text from parts and fetch file content if needed
                                Self::extract_content_parts(parts, workspace_id.0, file_service)
                                    .await?
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
                }
            }
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

    /// Extract text and reasoning deltas from SSE event
    fn extract_deltas(event: &inference_providers::SSEEvent) -> (Option<String>, Option<String>) {
        use inference_providers::StreamChunk;

        match &event.chunk {
            StreamChunk::Chat(chat_chunk) => {
                // Extract delta content from choices
                for choice in &chat_chunk.choices {
                    if let Some(delta) = &choice.delta {
                        let content = delta.content.clone();
                        // Check for reasoning_content or reasoning (some providers use one or the other)
                        let reasoning = delta
                            .reasoning_content
                            .clone()
                            .or_else(|| delta.reasoning.clone());

                        if content.is_some() || reasoning.is_some() {
                            return (content, reasoning);
                        }
                    }
                }
                (None, None)
            }
            _ => (None, None),
        }
    }

    /// Process reasoning tags in text delta
    /// Returns (clean_text, reasoning_delta, tag_transition)
    ///
    /// Handles common reasoning tags: <think>, <reasoning>, <thought>, <reflect>, <analysis>
    fn process_reasoning_tags(
        delta_text: &str,
        reasoning_buffer: &mut String,
        inside_reasoning: &mut bool,
    ) -> (String, Option<String>, TagTransition) {
        const REASONING_TAGS: &[&str] = &["think", "reasoning", "thought", "reflect", "analysis"];

        // Fast path: if we're not currently inside a reasoning block and this delta
        // contains no potential tags at all, we can safely treat the entire chunk
        // as clean text without walking it character-by-character.
        //
        // We MUST still run the full logic when:
        // - inside_reasoning == true (the text should be routed to reasoning_buffer)
        // - the chunk contains '<' (may start or close reasoning tags, or HTML tags we
        //   want to preserve exactly, like <!DOCTYPE> or <br/>)
        if !*inside_reasoning && !delta_text.contains('<') {
            return (delta_text.to_string(), None, TagTransition::None);
        }

        let mut clean_text = String::new();
        let mut reasoning_delta = String::new();
        let mut tag_transition = TagTransition::None;
        let mut chars = delta_text.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '<' {
                // Start collecting the entire tag to handle complex tags like <!DOCTYPE>
                let mut full_tag = String::from("<");
                let mut tag_candidate = String::new();
                let mut is_closing = false;
                let mut found_non_tag_char = false;
                let mut is_self_closing = false;

                // Check if this is a closing tag
                if chars.peek() == Some(&'/') {
                    is_closing = true;
                    full_tag.push('/');
                    chars.next(); // consume '/'
                }

                // Collect tag content until '>'
                while let Some(&next_ch) = chars.peek() {
                    if next_ch == '>' {
                        full_tag.push('>');
                        chars.next(); // consume '>'
                        break;
                    } else if !found_non_tag_char
                        && (next_ch.is_alphanumeric() || next_ch == '_' || next_ch == '-')
                    {
                        // Still collecting tag name for reasoning tag detection
                        tag_candidate.push(next_ch);
                        full_tag.push(next_ch);
                        chars.next();
                    } else if next_ch == '/' {
                        // Check if this is a self-closing tag (like <br/> or <think/>)
                        // Look ahead to see if '/' is followed by '>' or space+'>'
                        let mut peek_iter = chars.clone();
                        peek_iter.next(); // skip '/'
                        let mut found_gt = false;
                        // Skip whitespace after '/'
                        while let Some(&peek_ch) = peek_iter.peek() {
                            if peek_ch == '>' {
                                // This is a self-closing tag
                                found_gt = true;
                                is_self_closing = true;
                                full_tag.push('/');
                                chars.next(); // consume '/'
                                              // Don't set found_non_tag_char yet - we want to check if it's a reasoning tag
                                break;
                            } else if peek_ch.is_whitespace() {
                                peek_iter.next();
                            } else {
                                // Not a self-closing tag, just a regular non-tag-char
                                break;
                            }
                        }
                        if !found_gt {
                            // No '>' found after '/' (incomplete tag in streaming input)
                            // Treat '/' as a regular non-tag-char to avoid infinite loop
                            found_non_tag_char = true;
                            full_tag.push('/');
                            chars.next(); // consume '/' to prevent infinite loop
                        } else if is_self_closing {
                            // Continue to collect '>' in the next iteration
                            continue;
                        }
                    } else {
                        // Hit a non-tag-name character (like '!' in <!DOCTYPE, space, etc.)
                        // This is not a simple reasoning tag, collect the entire tag content
                        found_non_tag_char = true;
                        full_tag.push(next_ch);
                        chars.next();
                    }
                }

                // Check for reasoning tags: check tag name even if it has attributes
                // This ensures symmetric handling of opening and closing tags
                // Only check if tag is complete (ended with '>') or is self-closing
                let tag_name = tag_candidate.to_lowercase();
                if !tag_name.is_empty()
                    && REASONING_TAGS.contains(&tag_name.as_str())
                    && (full_tag.ends_with('>') || is_self_closing)
                {
                    if is_self_closing {
                        // Self-closing reasoning tag: treat as no-op (empty reasoning block)
                        // Don't change inside_reasoning state, just ignore the tag
                        tag_transition = TagTransition::None;
                        tracing::debug!("Detected self-closing reasoning tag: <{}/>", tag_name);
                        // Don't include the tag itself in any output
                        continue;
                    } else if is_closing && *inside_reasoning {
                        // Closing reasoning tag
                        *inside_reasoning = false;
                        tag_transition = TagTransition::ClosingTag(tag_name.clone());
                        tracing::debug!("Detected closing reasoning tag: </{}>", tag_name);
                    } else if !is_closing && !*inside_reasoning {
                        // Opening reasoning tag (even with attributes)
                        *inside_reasoning = true;
                        tag_transition = TagTransition::OpeningTag(tag_name.clone());
                        tracing::debug!("Detected opening reasoning tag: <{}>", tag_name);
                    } else if is_closing && !*inside_reasoning {
                        // Closing tag encountered but not inside reasoning (malformed or extra closing tag)
                        tracing::debug!(
                            "Ignoring closing reasoning tag </{}> - not currently inside reasoning block",
                            tag_name
                        );
                    } else if !is_closing && *inside_reasoning {
                        // Opening tag encountered while already inside reasoning (nested or malformed)
                        tracing::debug!(
                            "Ignoring opening reasoning tag <{}> - already inside reasoning block",
                            tag_name
                        );
                    }
                    // Don't include the tag itself in any output
                    continue;
                }

                // Not a reasoning tag, output the full tag as-is
                if *inside_reasoning {
                    reasoning_delta.push_str(&full_tag);
                    reasoning_buffer.push_str(&full_tag);
                } else {
                    clean_text.push_str(&full_tag);
                }
            } else {
                // Regular character
                if *inside_reasoning {
                    reasoning_delta.push(ch);
                    reasoning_buffer.push(ch);
                } else {
                    clean_text.push(ch);
                }
            }
        }

        let reasoning_result = if !reasoning_delta.is_empty() {
            Some(reasoning_delta)
        } else {
            None
        };

        (clean_text, reasoning_result, tag_transition)
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
        context: &mut ProcessStreamContext,
    ) -> Result<ToolExecutionResult, errors::ResponseError> {
        // Check for empty tool type
        if tool_call.tool_type.trim().is_empty() {
            return Err(errors::ResponseError::EmptyToolName);
        }

        match tool_call.tool_type.as_str() {
            "web_search" => {
                if let Some(provider) = &context.web_search_provider {
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

                    // Calculate cumulative offset from current registry size
                    let search_start_index = context
                        .source_registry
                        .as_ref()
                        .map(|r| r.web_sources.len())
                        .unwrap_or(0);

                    // Accumulate results into registry and generate citation instruction if first search
                    let citation_instruction = if let Some(ref mut registry) =
                        context.source_registry
                    {
                        registry.web_sources.extend(results.clone());
                        None
                    } else {
                        context.source_registry =
                            Some(models::SourceRegistry::with_results(results.clone()));
                        Some(
                            r#"CITATION REQUIREMENT: Use [s:N]text[/s:N] for EVERY fact from web search results.

FORMAT: [s:N]fact from source N[/s:N]
- N = source number (0, 1, 2, 3, etc. - cumulative across all searches)
- ALWAYS use BOTH opening [s:N] and closing [/s:N] tags together
- The number N MUST match in opening and closing tags
- Cite specific facts, names, numbers, and statements from sources
- Every factual claim must be wrapped

CORRECT EXAMPLES:
[s:0]San Francisco's top restaurant is The French Laundry[/s:0]
[s:1]The app TikTok has over 2 billion downloads[/s:1]
[s:2]Instagram was founded in 2010[/s:2]

DO NOT USE THESE FORMATS:
✗ [s:0]Missing closing tag
✗ [s:0]Mismatched[/s:1] numbers
✗ Statements without any citation tags"#
                                .to_string(),
                        )
                    };

                    // Format results with cumulative indices
                    let formatted = results
                        .iter()
                        .enumerate()
                        .map(|(idx, r)| {
                            format!(
                                "Source: {}\nTitle: {}\nURL: {}\nSnippet: {}\n",
                                search_start_index + idx,
                                r.title,
                                r.url,
                                r.snippet
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");

                    Ok(ToolExecutionResult {
                        content: formatted,
                        citation_instruction,
                    })
                } else {
                    Err(errors::ResponseError::UnknownTool("web_search".to_string()))
                }
            }
            "file_search" => {
                if let Some(provider) = &context.file_search_provider {
                    // Get conversation ID from request
                    let conversation_id = match &context.request.conversation {
                        Some(models::ConversationReference::Id(id)) => {
                            // Parse conversation ID
                            let uuid_str = id
                                .strip_prefix(crate::id_prefixes::PREFIX_CONV)
                                .unwrap_or(id);
                            uuid::Uuid::parse_str(uuid_str).map_err(|e| {
                                errors::ResponseError::InvalidParams(format!(
                                    "Invalid conversation ID: {e}"
                                ))
                            })?
                        }
                        Some(models::ConversationReference::Object { id, .. }) => {
                            let uuid_str = id
                                .strip_prefix(crate::id_prefixes::PREFIX_CONV)
                                .unwrap_or(id);
                            uuid::Uuid::parse_str(uuid_str).map_err(|e| {
                                errors::ResponseError::InvalidParams(format!(
                                    "Invalid conversation ID: {e}"
                                ))
                            })?
                        }
                        None => {
                            return Ok(ToolExecutionResult {
                                content: "File search requires a conversation context".to_string(),
                                citation_instruction: None,
                            });
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

                    Ok(ToolExecutionResult {
                        content: formatted,
                        citation_instruction: None,
                    })
                } else {
                    Ok(ToolExecutionResult {
                        content: "File search not available (no provider configured)".to_string(),
                        citation_instruction: None,
                    })
                }
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
            .get_conversation(conversation_id, workspace_id_domain.clone())
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

        // Generate title using completion service (names not included - tracked via database)
        let title_model = std::env::var("TITLE_GENERATION_MODEL")
            .unwrap_or_else(|_| "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string());
        let completion_request = crate::completions::ports::CompletionRequest {
            model: title_model,
            messages: vec![crate::completions::ports::CompletionMessage {
                role: "user".to_string(),
                content: title_prompt,
            }],
            max_tokens: Some(150),
            temperature: Some(1.0),
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
            extra: std::collections::HashMap::from([(
                "chat_template_kwargs".to_string(),
                serde_json::json!({ "enable_thinking": false }),
            )]),
        };

        // Call completion service to generate title
        let completion_result = completion_service
            .create_chat_completion(completion_request)
            .await
            .map_err(|e| {
                errors::ResponseError::InternalError(format!("Failed to generate title: {e}"))
            })?;

        // Extract title from completion result
        let raw_title = completion_result
            .response
            .choices
            .first()
            .and_then(|choice| choice.message.content.as_ref())
            .map(|content| content.trim().to_string());
        let raw_title = if let Some(title) = raw_title {
            title
        } else {
            tracing::warn!(
                conversation_id = %conversation_id,
                "LLM response doesn't contain title for conversation, using default"
            );
            "Conversation".to_string()
        };

        // Strip reasoning tags from title
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;
        let (generated_title, _, _) =
            Self::process_reasoning_tags(&raw_title, &mut reasoning_buffer, &mut inside_reasoning);
        let generated_title = generated_title.trim();
        let generated_title = if generated_title.is_empty() {
            "Conversation".to_string()
        } else {
            generated_title.to_string()
        };

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
            .update_conversation(conversation_id, workspace_id_domain, updated_metadata)
            .await
            .map_err(|e| {
                errors::ResponseError::InternalError(format!(
                    "Failed to update conversation metadata: {e}"
                ))
            })?;

        tracing::info!(
            conversation_id = %conversation_id,
            title_length = title.len(),
            truncated = title.len() > 60,
            "Generated conversation title"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_reasoning_tags_simple_think() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test opening tag
        let (clean, reasoning, transition) = ResponseServiceImpl::process_reasoning_tags(
            "<think>",
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );
        assert_eq!(clean, "");
        assert_eq!(reasoning, None);
        assert_eq!(transition, TagTransition::OpeningTag("think".to_string()));
        assert!(inside_reasoning);

        // Test content inside reasoning
        let (clean, reasoning, transition) = ResponseServiceImpl::process_reasoning_tags(
            "This is reasoning",
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );
        assert_eq!(clean, "");
        assert_eq!(reasoning, Some("This is reasoning".to_string()));
        assert_eq!(transition, TagTransition::None);
        assert!(inside_reasoning);
        assert_eq!(reasoning_buffer, "This is reasoning");

        // Test closing tag
        let (clean, reasoning, transition) = ResponseServiceImpl::process_reasoning_tags(
            "</think>",
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );
        assert_eq!(clean, "");
        assert_eq!(reasoning, None);
        assert_eq!(transition, TagTransition::ClosingTag("think".to_string()));
        assert!(!inside_reasoning);
    }

    #[test]
    fn test_process_reasoning_tags_mixed_content() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test text before reasoning tag
        let (clean, reasoning, _transition) = ResponseServiceImpl::process_reasoning_tags(
            "Hello <think>reasoning content</think> world",
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );
        assert_eq!(clean, "Hello  world");
        assert!(reasoning.is_some());
        assert_eq!(reasoning_buffer, "reasoning content");
        assert!(!inside_reasoning); // Should end outside reasoning
    }

    #[test]
    fn test_process_reasoning_tags_multiple_tags() {
        let test_tags = vec!["think", "reasoning", "thought", "reflect", "analysis"];

        for tag in test_tags {
            let mut reasoning_buffer = String::new();
            let mut inside_reasoning = false;

            let input = format!("<{tag}>test content</{tag}>");
            let (clean, reasoning, _) = ResponseServiceImpl::process_reasoning_tags(
                &input,
                &mut reasoning_buffer,
                &mut inside_reasoning,
            );

            assert_eq!(clean, "");
            assert!(reasoning.is_some() || reasoning_buffer.contains("test content"));
            assert!(!inside_reasoning);
        }
    }

    #[test]
    fn test_process_reasoning_tags_strips_from_message() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        let input = "The answer is <think>Let me think about this carefully</think> 42";
        let (clean, _, _) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        assert_eq!(clean, "The answer is  42");
        assert_eq!(reasoning_buffer, "Let me think about this carefully");
    }

    #[test]
    fn test_process_reasoning_tags_partial_chunks() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Note: The current implementation handles full tags but not tags split mid-name.
        // This is acceptable for real-world streaming where complete tokens are usually sent together.
        // Testing with complete tag boundaries that come in separate chunks:
        let chunks = vec!["<think>", "reasoning", " content", "</think>"];
        let mut all_clean = String::new();

        for chunk in chunks {
            let (clean, reasoning, _) = ResponseServiceImpl::process_reasoning_tags(
                chunk,
                &mut reasoning_buffer,
                &mut inside_reasoning,
            );
            all_clean.push_str(&clean);
            if let Some(r) = reasoning {
                // Just accumulating
                let _ = r;
            }
        }

        assert_eq!(all_clean, "");
        assert_eq!(reasoning_buffer, "reasoning content");
    }

    #[test]
    fn test_process_reasoning_tags_nested_html() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        let input = "<think>Consider <b>this</b> carefully</think>";
        let (clean, _, _) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        assert_eq!(clean, "");
        assert_eq!(reasoning_buffer, "Consider <b>this</b> carefully");
    }

    #[test]
    fn test_process_reasoning_tags_no_closing() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        let (clean, reasoning, _) = ResponseServiceImpl::process_reasoning_tags(
            "<think>Never closed",
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        assert_eq!(clean, "");
        assert_eq!(reasoning, Some("Never closed".to_string()));
        assert!(inside_reasoning);
    }

    #[test]
    fn test_estimate_tokens() {
        use crate::responses::service_helpers::ResponseStreamContext;

        assert_eq!(ResponseStreamContext::estimate_tokens("test"), 1);
        assert_eq!(ResponseStreamContext::estimate_tokens("Hello world"), 2);
        assert_eq!(
            ResponseStreamContext::estimate_tokens("This is a longer text"),
            5
        );
    }

    #[test]
    fn test_process_reasoning_tags_clean_text_before_reasoning() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test that clean text before reasoning tag is correctly extracted
        let input = "Hello <think>";
        let (clean, reasoning, transition) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        assert_eq!(clean, "Hello ");
        assert_eq!(reasoning, None);
        assert_eq!(transition, TagTransition::OpeningTag("think".to_string()));
        assert!(inside_reasoning);
    }

    #[test]
    fn test_process_reasoning_tags_clean_text_after_reasoning() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // First open the reasoning tag
        let (clean, _, _) = ResponseServiceImpl::process_reasoning_tags(
            "<think>reasoning",
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );
        assert_eq!(clean, "");
        assert!(inside_reasoning);

        // Then close it and add clean text after
        let (clean, _, transition) = ResponseServiceImpl::process_reasoning_tags(
            "</think> world",
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        assert_eq!(clean, " world");
        assert_eq!(transition, TagTransition::ClosingTag("think".to_string()));
        assert!(!inside_reasoning);
    }

    #[test]
    fn test_process_reasoning_tags_html_doctype() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test that HTML DOCTYPE and other HTML tags are preserved correctly
        let input = "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n    <meta charset=\"UTF-8\">\n    <title>Test</title>\n</head>\n<body>\n    <h1>Hello</h1>\n</body>\n</html>";
        let (clean, reasoning, _) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        // All HTML tags should be preserved in clean text
        assert_eq!(clean, input);
        assert_eq!(reasoning, None);
        assert!(!inside_reasoning);
        assert!(reasoning_buffer.is_empty());
    }

    #[test]
    fn test_process_reasoning_tags_html_with_attributes() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test HTML tags with attributes
        let input = "<html lang=\"en\">\n<head>\n    <meta charset=\"UTF-8\">\n    <title>SVG Drawing Example</title>\n</head>";
        let (clean, reasoning, _) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        // All HTML tags should be preserved
        assert_eq!(clean, input);
        assert_eq!(reasoning, None);
        assert!(!inside_reasoning);
        assert!(reasoning_buffer.is_empty());
    }

    #[test]
    fn test_process_reasoning_tags_self_closing_tags() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test self-closing tags without space: <br/>, <hr/>, <img/>
        let input = "Line 1<br/>Line 2<hr/>Line 3<img/>";
        let (clean, reasoning, _) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        assert_eq!(clean, input);
        assert_eq!(reasoning, None);
        assert!(!inside_reasoning);
        assert!(reasoning_buffer.is_empty());
    }

    #[test]
    fn test_process_reasoning_tags_self_closing_tags_with_space() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test self-closing tags with space: <br />, <hr />, <img />
        let input = "Line 1<br />Line 2<hr />Line 3<img />";
        let (clean, reasoning, _) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        assert_eq!(clean, input);
        assert_eq!(reasoning, None);
        assert!(!inside_reasoning);
        assert!(reasoning_buffer.is_empty());
    }

    #[test]
    fn test_process_reasoning_tags_self_closing_tags_with_attributes() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test self-closing tags with attributes
        let input =
            r#"<img src="image.jpg" alt="Test" /><br class="clear" /><meta charset="UTF-8" />"#;
        let (clean, reasoning, _) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        assert_eq!(clean, input);
        assert_eq!(reasoning, None);
        assert!(!inside_reasoning);
        assert!(reasoning_buffer.is_empty());
    }

    #[test]
    fn test_process_reasoning_tags_self_closing_in_reasoning_block() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test self-closing tags inside reasoning block should be preserved in reasoning
        let input = "<think>Think about <br/> this</think>";
        let (clean, reasoning, _) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        // Self-closing tag should be in reasoning buffer, not in clean text
        assert_eq!(clean, "");
        assert!(reasoning.is_some());
        assert_eq!(reasoning_buffer, "Think about <br/> this");
        assert!(!inside_reasoning);
    }

    #[test]
    fn test_process_reasoning_tags_mixed_self_closing_and_normal_tags() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test mix of self-closing and normal HTML tags
        let input = r#"<div><p>Paragraph 1</p><br/><p>Paragraph 2</p><hr/></div>"#;
        let (clean, reasoning, _) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        assert_eq!(clean, input);
        assert_eq!(reasoning, None);
        assert!(!inside_reasoning);
        assert!(reasoning_buffer.is_empty());
    }

    #[test]
    fn test_process_reasoning_tags_self_closing_xml_tags() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test XML-style self-closing tags
        let input = "<root><child attr=\"value\"/><another/></root>";
        let (clean, reasoning, _) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        assert_eq!(clean, input);
        assert_eq!(reasoning, None);
        assert!(!inside_reasoning);
        assert!(reasoning_buffer.is_empty());
    }

    #[test]
    fn test_process_reasoning_tags_self_closing_reasoning_tag() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test self-closing reasoning tag <think/> - should be treated as reasoning tag, not output
        let input = "<think/>";
        let (clean, reasoning, transition) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        // Self-closing reasoning tag should be ignored (not in output)
        assert_eq!(clean, "");
        assert_eq!(reasoning, None);
        // Self-closing tag should not change reasoning state (no-op)
        assert!(!inside_reasoning);
        assert!(reasoning_buffer.is_empty());
        assert_eq!(transition, TagTransition::None);
    }

    #[test]
    fn test_process_reasoning_tags_self_closing_reasoning_tag_with_space() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test self-closing reasoning tag with space <think /> - should be treated as reasoning tag
        let input = "<think />";
        let (clean, reasoning, transition) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        // Self-closing reasoning tag should be ignored (not in output)
        assert_eq!(clean, "");
        assert_eq!(reasoning, None);
        assert!(!inside_reasoning);
        assert!(reasoning_buffer.is_empty());
        assert_eq!(transition, TagTransition::None);
    }

    #[test]
    fn test_process_reasoning_tags_self_closing_reasoning_tag_mixed() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test mix of self-closing reasoning tag and regular HTML
        let input = "Text <think/> more text <br/>";
        let (clean, reasoning, _) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        // Self-closing reasoning tag should be removed, but HTML tags should remain
        assert_eq!(clean, "Text  more text <br/>");
        assert_eq!(reasoning, None);
        assert!(!inside_reasoning);
        assert!(reasoning_buffer.is_empty());
    }

    #[test]
    fn test_process_reasoning_tags_malformed_extra_closing() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test extra closing tag when not inside reasoning (malformed)
        let input = "</think>Text";
        let (clean, reasoning, transition) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        // Extra closing tag should be ignored, text should remain
        assert_eq!(clean, "Text");
        assert_eq!(reasoning, None);
        assert!(!inside_reasoning);
        assert_eq!(transition, TagTransition::None);
        assert!(reasoning_buffer.is_empty());
    }

    #[test]
    fn test_process_reasoning_tags_malformed_nested_opening() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test nested opening tag (malformed - opening while already inside reasoning)
        let input = "<think>First<think>Second</think></think>";
        let (clean, reasoning, _) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        // Nested opening tag should be ignored, but content should be in reasoning
        assert_eq!(clean, "");
        assert!(reasoning.is_some());
        assert_eq!(reasoning_buffer, "FirstSecond");
        assert!(!inside_reasoning);
    }

    #[test]
    fn test_process_reasoning_tags_malformed_double_closing() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test double closing tag
        let input = "<think>Content</think></think>";
        let (clean, reasoning, _) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        // First closing tag should work, second should be ignored
        assert_eq!(clean, "");
        assert!(reasoning.is_some());
        assert_eq!(reasoning_buffer, "Content");
        assert!(!inside_reasoning);
    }

    #[test]
    fn test_process_reasoning_tags_incomplete_self_closing() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test incomplete self-closing tag (like <br/ in streaming input)
        // This should not cause an infinite loop
        let input = "<br/";
        let (clean, reasoning, _) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        // Incomplete tag should be treated as regular text to avoid infinite loop
        assert_eq!(clean, "<br/");
        assert_eq!(reasoning, None);
        assert!(!inside_reasoning);
        assert!(reasoning_buffer.is_empty());
    }

    #[test]
    fn test_process_reasoning_tags_incomplete_self_closing_with_text() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test incomplete self-closing tag followed by text
        let input = "<br/Text";
        let (clean, reasoning, _) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        // Incomplete tag should be treated as regular text
        assert_eq!(clean, "<br/Text");
        assert_eq!(reasoning, None);
        assert!(!inside_reasoning);
        assert!(reasoning_buffer.is_empty());
    }

    #[test]
    fn test_process_reasoning_tags_incomplete_self_closing_reasoning() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test incomplete self-closing reasoning tag (like <think/ in streaming input)
        let input = "<think/";
        let (clean, reasoning, _) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        // Incomplete tag should be treated as regular text to avoid infinite loop
        assert_eq!(clean, "<think/");
        assert_eq!(reasoning, None);
        assert!(!inside_reasoning);
        assert!(reasoning_buffer.is_empty());
    }

    #[test]
    fn test_process_reasoning_tags_with_attributes() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test reasoning tag with attributes - should be recognized and stripped
        let input = r#"<think attr="val">content</think>"#;
        let (clean, reasoning, _) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        // Both opening and closing tags should be stripped, content should be in reasoning
        assert_eq!(clean, "");
        assert!(reasoning.is_some());
        assert_eq!(reasoning_buffer, "content");
        assert!(!inside_reasoning);
    }

    #[test]
    fn test_process_reasoning_tags_with_attributes_symmetric() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test that opening tag with attributes and closing tag are both handled
        let input = r#"<think id="1" class="test">reasoning content</think> normal text"#;
        let (clean, reasoning, _) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        // Tags should be stripped, content should be in reasoning, normal text should remain
        assert_eq!(clean, " normal text");
        assert!(reasoning.is_some());
        assert_eq!(reasoning_buffer, "reasoning content");
        assert!(!inside_reasoning);
    }

    #[test]
    fn test_process_reasoning_tags_with_attributes_no_unclosed_tag() {
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;

        // Test that we don't leave unclosed tags in output
        let input = r#"<think attr="val">content</think>"#;
        let (clean, reasoning, _) = ResponseServiceImpl::process_reasoning_tags(
            input,
            &mut reasoning_buffer,
            &mut inside_reasoning,
        );

        // Should not contain any unclosed tags
        assert!(!clean.contains("<think"));
        assert!(!clean.contains("attr"));
        assert_eq!(clean, "");
        assert!(reasoning.is_some());
        assert_eq!(reasoning_buffer, "content");
        assert!(!inside_reasoning);
    }

    #[test]
    fn test_multiple_web_search_registry_accumulation() {
        use crate::responses::models::SourceRegistry;
        use crate::responses::tools::WebSearchResult;

        // Simulate first web search with 3 results
        let first_search_results = vec![
            WebSearchResult {
                title: "First Result".to_string(),
                url: "https://example.com/1".to_string(),
                snippet: "First snippet".to_string(),
            },
            WebSearchResult {
                title: "Second Result".to_string(),
                url: "https://example.com/2".to_string(),
                snippet: "Second snippet".to_string(),
            },
            WebSearchResult {
                title: "Third Result".to_string(),
                url: "https://example.com/3".to_string(),
                snippet: "Third snippet".to_string(),
            },
        ];

        // Simulate second web search with 2 results
        let second_search_results = vec![
            WebSearchResult {
                title: "Fourth Result".to_string(),
                url: "https://example.com/4".to_string(),
                snippet: "Fourth snippet".to_string(),
            },
            WebSearchResult {
                title: "Fifth Result".to_string(),
                url: "https://example.com/5".to_string(),
                snippet: "Fifth snippet".to_string(),
            },
        ];

        // First search: registry starts None, should create new registry
        let mut registry: Option<SourceRegistry> = None;
        let first_offset = registry.as_ref().map(|r| r.web_sources.len()).unwrap_or(0);
        assert_eq!(first_offset, 0, "First search should have offset 0");

        // Create registry with first search results
        if let Some(ref mut reg) = registry {
            reg.web_sources.extend(first_search_results.clone());
        } else {
            registry = Some(SourceRegistry::with_results(first_search_results.clone()));
        }
        assert_eq!(
            registry.as_ref().unwrap().web_sources.len(),
            3,
            "Registry should have 3 results after first search"
        );

        // Second search: registry exists, should accumulate
        let second_offset = registry.as_ref().map(|r| r.web_sources.len()).unwrap_or(0);
        assert_eq!(second_offset, 3, "Second search should have offset 3");

        // Accumulate second search results
        if let Some(ref mut reg) = registry {
            reg.web_sources.extend(second_search_results.clone());
        }
        assert_eq!(
            registry.as_ref().unwrap().web_sources.len(),
            5,
            "Registry should have 5 results after second search"
        );

        // Verify correct indices
        let final_registry = registry.unwrap();
        assert_eq!(
            final_registry.web_sources[0].title, "First Result",
            "Index 0 should be first result"
        );
        assert_eq!(
            final_registry.web_sources[1].title, "Second Result",
            "Index 1 should be second result"
        );
        assert_eq!(
            final_registry.web_sources[2].title, "Third Result",
            "Index 2 should be third result"
        );
        assert_eq!(
            final_registry.web_sources[3].title, "Fourth Result",
            "Index 3 should be fourth result"
        );
        assert_eq!(
            final_registry.web_sources[4].title, "Fifth Result",
            "Index 4 should be fifth result"
        );

        // Verify that searching for index 0 gets first result
        assert_eq!(
            final_registry.web_sources.first().unwrap().url,
            "https://example.com/1"
        );
        // Verify that searching for index 3 gets fourth result
        assert_eq!(final_registry.web_sources[3].url, "https://example.com/4");
    }
}
