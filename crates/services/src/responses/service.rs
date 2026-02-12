use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;
use uuid::Uuid;

use crate::common::encryption_headers;
use crate::completions::ports::CompletionServiceTrait;
use crate::conversations::models::ConversationId;
use crate::conversations::ports::ConversationServiceTrait;
use crate::files::FileServiceTrait;
use crate::inference_provider_pool::InferenceProviderPool;
use crate::responses::tools;
use crate::responses::{citation_tracker, errors, models, ports};

use tools::{ERROR_TOOL_TYPE, MAX_CONSECUTIVE_TOOL_FAILURES};

/// Result of the agent loop execution
enum AgentLoopResult {
    /// Agent loop completed normally
    Completed,
    /// Agent loop paused due to MCP approval required
    ApprovalRequired,
    /// Agent loop paused due to external function calls requiring client execution
    FunctionCallsRequired,
}

/// Context for processing a response stream
struct ProcessStreamContext {
    request: models::CreateResponseRequest,
    user_id: crate::UserId,
    api_key_id: String,
    organization_id: uuid::Uuid,
    workspace_id: uuid::Uuid,
    body_hash: String,
    signing_algo: Option<String>,
    client_pub_key: Option<String>,
    model_pub_key: Option<String>,
    response_repository: Arc<dyn ports::ResponseRepositoryTrait>,
    response_items_repository: Arc<dyn ports::ResponseItemRepositoryTrait>,
    completion_service: Arc<dyn CompletionServiceTrait>,
    conversation_service: Arc<dyn ConversationServiceTrait>,
    file_service: Arc<dyn FileServiceTrait>,
    organization_service: Arc<dyn crate::organization::OrganizationServiceTrait>,
    source_registry: Option<models::SourceRegistry>,
    web_search_failure_count: u32,
    mcp_executor: Option<Arc<tools::McpToolExecutor>>,
    mcp_client_factory: Option<Arc<dyn tools::McpClientFactory>>,
    tool_registry: tools::ToolRegistry,
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
    /// Optional MCP client factory for testing (if None, uses RealMcpClientFactory)
    pub mcp_client_factory: Option<Arc<dyn tools::McpClientFactory>>,
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
            mcp_client_factory: None,
        }
    }

    /// Create a new ResponseServiceImpl with a custom MCP client factory (for testing)
    #[allow(clippy::too_many_arguments)]
    pub fn with_mcp_client_factory(
        response_repository: Arc<dyn ports::ResponseRepositoryTrait>,
        response_items_repository: Arc<dyn ports::ResponseItemRepositoryTrait>,
        inference_provider_pool: Arc<InferenceProviderPool>,
        conversation_service: Arc<dyn ConversationServiceTrait>,
        completion_service: Arc<dyn CompletionServiceTrait>,
        web_search_provider: Option<Arc<dyn tools::WebSearchProviderTrait>>,
        file_search_provider: Option<Arc<dyn tools::FileSearchProviderTrait>>,
        file_service: Arc<dyn FileServiceTrait>,
        organization_service: Arc<dyn crate::organization::OrganizationServiceTrait>,
        mcp_client_factory: Arc<dyn tools::McpClientFactory>,
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
            mcp_client_factory: Some(mcp_client_factory),
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
        signing_algo: Option<String>,
        client_pub_key: Option<String>,
        model_pub_key: Option<String>,
    ) -> Result<
        Pin<Box<dyn Stream<Item = models::ResponseStreamEvent> + Send>>,
        errors::ResponseError,
    > {
        use futures::channel::mpsc;
        use futures::SinkExt;

        // Validate: function_call_output items require a previous_response_id
        let has_function_outputs = match &request.input {
            Some(models::ResponseInput::Items(items)) => {
                items.iter().any(|item| item.is_function_call_output())
            }
            _ => false,
        };
        if has_function_outputs && request.previous_response_id.is_none() {
            return Err(errors::ResponseError::FunctionCallNotFound(
                "function_call_output requires previous_response_id".to_string(),
            ));
        }

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
        let mcp_client_factory = self.mcp_client_factory.clone();
        let signing_algo_clone = signing_algo.clone();
        let client_pub_key_clone = client_pub_key.clone();
        let model_pub_key_clone = model_pub_key.clone();

        tokio::spawn(async move {
            let mut tool_registry = tools::ToolRegistry::new();
            if let Some(provider) = web_search_provider {
                tool_registry.register(Arc::new(tools::WebSearchToolExecutor::new(provider)));
            }
            if let Some(provider) = file_search_provider {
                tool_registry.register(Arc::new(tools::FileSearchToolExecutor::new(provider)));
            }
            // Note: MCP executor is added later after connecting to servers

            let context = ProcessStreamContext {
                request,
                user_id,
                api_key_id,
                organization_id,
                workspace_id,
                body_hash,
                signing_algo: signing_algo_clone,
                client_pub_key: client_pub_key_clone,
                model_pub_key: model_pub_key_clone,
                response_repository,
                response_items_repository,
                completion_service,
                conversation_service,
                file_service,
                organization_service,
                source_registry: None,
                web_search_failure_count: 0,
                mcp_executor: None,
                mcp_client_factory,
                tool_registry,
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

    /// Filter conversation items to only include those in the ancestor chain
    /// If target_response_id is None, returns all items unchanged
    fn filter_to_ancestor_branch(
        items: Vec<models::ResponseOutputItem>,
        target_response_id: &Option<String>,
    ) -> Vec<models::ResponseOutputItem> {
        let Some(target_id) = target_response_id else {
            return items;
        };

        // Build map of response_id -> previous_response_id from items
        // Multiple items can share the same response_id (tool calls, messages, web searches
        // from the same agent loop), so we use entry() to only insert once per response_id
        let mut response_parent_map: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        for item in &items {
            if let Some(response_id) = item.response_id() {
                response_parent_map
                    .entry(response_id.to_string())
                    .or_insert_with(|| item.previous_response_id().map(|s| s.to_string()));
            }
        }

        // Walk up from target to collect ancestor response IDs
        let mut ancestors: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut current = Some(target_id.clone());
        while let Some(resp_id) = current {
            ancestors.insert(resp_id.clone());
            current = response_parent_map.get(&resp_id).cloned().flatten();
        }

        // Filter items to only those in ancestor chain
        items
            .into_iter()
            .filter(|item| {
                item.response_id()
                    .map(|r| ancestors.contains(r))
                    .unwrap_or(false)
            })
            .collect()
    }

    /// Extract content from a vector of content parts, handling text and files
    ///
    /// Returns different formats depending on content:
    /// - Plain text: Text parts joined by "\n\n" (for text-only requests)
    /// - JSON array string: OpenAI-compatible multimodal format `[{"type":"text",...}, {"type":"image_url",...}]` (for requests with images)
    async fn extract_content_parts(
        parts: &[models::ResponseContentPart],
        workspace_id: uuid::Uuid,
        file_service: &Arc<dyn FileServiceTrait>,
    ) -> Result<String, errors::ResponseError> {
        // Check if there are any images in the parts
        let has_images = parts
            .iter()
            .any(|part| matches!(part, models::ResponseContentPart::InputImage { .. }));

        if has_images {
            // Build multimodal content array for vision models
            Self::extract_multimodal_content(parts, workspace_id, file_service).await
        } else {
            // Build simple text content
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
                        // Skip other content types
                    }
                }
            }
            Ok(content_parts.join("\n\n"))
        }
    }

    /// Build multimodal content array with text and images in OpenAI format
    async fn extract_multimodal_content(
        parts: &[models::ResponseContentPart],
        workspace_id: uuid::Uuid,
        file_service: &Arc<dyn FileServiceTrait>,
    ) -> Result<String, errors::ResponseError> {
        let mut content_items = Vec::new();

        for part in parts {
            match part {
                models::ResponseContentPart::InputText { text } => {
                    // Add text as OpenAI-compatible content object
                    content_items.push(serde_json::json!({
                        "type": "text",
                        "text": text
                    }));
                }
                models::ResponseContentPart::InputImage { image_url, detail } => {
                    // Extract the URL string (handle both String and Object variants)
                    let url = match image_url {
                        models::ResponseImageUrl::String(s) => s.clone(),
                        models::ResponseImageUrl::Object { url } => url.clone(),
                    };

                    // Build image_url object with optional detail parameter
                    let mut image_url_obj = serde_json::json!({
                        "url": url
                    });
                    if let Some(detail_level) = detail {
                        image_url_obj["detail"] = serde_json::Value::String(detail_level.clone());
                    }

                    content_items.push(serde_json::json!({
                        "type": "image_url",
                        "image_url": image_url_obj
                    }));
                }
                models::ResponseContentPart::InputFile { file_id, detail } => {
                    // Process file to get its content
                    let file_content =
                        Self::process_input_file(file_id, workspace_id, file_service).await?;
                    // For files, treat as text content
                    content_items.push(serde_json::json!({
                        "type": "text",
                        "text": file_content
                    }));
                    if let Some(detail_level) = detail {
                        // If detail was specified, add it to the previous item
                        if let Some(last) = content_items.last_mut() {
                            last["detail"] = serde_json::Value::String(detail_level.clone());
                        }
                    }
                }
            }
        }

        // Serialize the content array as JSON string
        serde_json::to_string(&content_items).map_err(|e| {
            errors::ResponseError::InternalError(format!(
                "Failed to serialize multimodal content: {}",
                e
            ))
        })
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

    /// Process a completion stream and emit events for text deltas.
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
    ) -> Result<crate::responses::service_helpers::ProcessStreamResult, errors::ResponseError> {
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

        // Stream error tracking - when stream errors (client disconnect, network error, etc.), we save partial response and stop
        let mut stream_error = false;

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
                                    if let Err(e) = emitter
                                        .emit_reasoning_started(ctx, &reasoning_item_id)
                                        .await
                                    {
                                        tracing::debug!("emit_reasoning_started failed: {}", e);
                                    }
                                    reasoning_item_emitted = true;
                                }
                                if let Err(e) = emitter
                                    .emit_reasoning_delta(
                                        ctx,
                                        reasoning_item_id.clone(),
                                        reasoning.clone(),
                                    )
                                    .await
                                {
                                    tracing::debug!("emit_reasoning_delta failed: {}", e);
                                }
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
                            if let Err(e) = emitter
                                .emit_reasoning_completed(
                                    ctx,
                                    &reasoning_item_id,
                                    &reasoning_buffer,
                                    response_items_repository,
                                )
                                .await
                            {
                                tracing::debug!("emit_reasoning_completed failed: {}", e);
                            }

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
                                    if let Err(e) = emitter
                                        .emit_reasoning_started(ctx, &reasoning_item_id)
                                        .await
                                    {
                                        tracing::debug!("emit_reasoning_started failed: {}", e);
                                    }
                                    reasoning_item_emitted = true;
                                }
                            }
                            TagTransition::ClosingTag(_) => {
                                if reasoning_item_emitted {
                                    // Emit reasoning item.done and store
                                    if let Err(e) = emitter
                                        .emit_reasoning_completed(
                                            ctx,
                                            &reasoning_item_id,
                                            &reasoning_buffer,
                                            response_items_repository,
                                        )
                                        .await
                                    {
                                        tracing::debug!("emit_reasoning_completed failed: {}", e);
                                    }

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
                                if let Err(e) = emitter
                                    .emit_reasoning_delta(
                                        ctx,
                                        reasoning_item_id.clone(),
                                        reasoning_content,
                                    )
                                    .await
                                {
                                    tracing::debug!("emit_reasoning_delta failed: {}", e);
                                }
                            }
                        }

                        // Handle clean text (message content)
                        if !clean_text.is_empty() {
                            // First time we receive message text, emit the item.added and content_part.added events
                            if !message_item_emitted && !stream_error {
                                if let Err(e) =
                                    Self::emit_message_started(emitter, ctx, &message_item_id).await
                                {
                                    tracing::debug!("emit_message_started failed: {}", e);
                                    stream_error = true;
                                } else {
                                    message_item_emitted = true;
                                }
                            }

                            current_text.push_str(&clean_text);

                            // Emit delta event for message content
                            if !stream_error {
                                if let Err(e) = emitter
                                    .emit_text_delta(
                                        ctx,
                                        message_item_id.clone(),
                                        clean_text.clone(),
                                    )
                                    .await
                                {
                                    tracing::debug!("emit_text_delta failed: {}", e);
                                    // Client disconnected - save partial response and stop consuming stream
                                    stream_error = true;
                                }
                            }
                        }

                        // If client disconnected, break out of loop to save partial response
                        if stream_error {
                            break;
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
                                    if let Err(e) = emitter
                                        .emit_citation_annotation(
                                            ctx,
                                            message_item_id.clone(),
                                            annotation,
                                        )
                                        .await
                                    {
                                        tracing::debug!("emit_citation_annotation failed: {}", e);
                                    }
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
                    tracing::warn!(
                        "Error in completion stream (client disconnect or stream error): {}",
                        e
                    );
                    stream_error = true;
                    // Don't return early - save partial response below
                    break;
                }
            }
        }

        // If we have message content, close it with done events and save to DB
        // Only save if we successfully emitted the message start AND have content
        if message_item_emitted && !current_text.is_empty() {
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
        let available_tool_names = tools::get_tool_names(&process_context.request);
        let function_tool_names = tools::get_function_tool_names(&process_context.request);
        let tool_calls_detected = tools::convert_tool_calls(
            tool_call_accumulator,
            &process_context.request.model,
            &available_tool_names,
            &function_tool_names,
        );

        Ok(crate::responses::service_helpers::ProcessStreamResult {
            text: current_text,
            tool_calls: tool_calls_detected,
            stream_error,
        })
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
            metadata: None,
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

        // Build the message item to save
        let item = models::ResponseOutputItem::Message {
            id: message_item_id.to_string(),
            response_id: ctx.response_id_str.clone(),
            previous_response_id: ctx.previous_response_id.clone(),
            next_response_ids: vec![], // next_response_ids will be populated when child responses are created
            created_at: ctx.created_at,
            status: models::ResponseItemStatus::Completed,
            role: "assistant".to_string(),
            content: vec![models::ResponseContentItem::OutputText {
                text: clean_text.clone(),
                annotations: annotations.clone(),
                logprobs: vec![],
            }],
            model: ctx.model.clone(),
            metadata: None,
        };

        // CRITICAL: Store to database FIRST before emitting events
        // This ensures the message is persisted even if client disconnected and emit calls fail
        if let Err(e) = response_items_repository
            .create(
                ctx.response_id.clone(),
                ctx.api_key_id,
                ctx.conversation_id,
                item.clone(),
            )
            .await
        {
            tracing::warn!("Failed to store message item: {}", e);
        }

        // Try to emit events (may fail if client disconnected, but data is already saved)
        // Event: response.output_text.done
        if let Err(e) = emitter
            .emit_text_done(ctx, message_item_id.to_string(), clean_text.clone())
            .await
        {
            tracing::debug!("Failed to emit text_done event: {}", e);
        }

        // Event: response.content_part.done
        let part = models::ResponseOutputContent::OutputText {
            text: clean_text,
            annotations: annotations.clone(),
            logprobs: vec![],
        };
        if let Err(e) = emitter
            .emit_content_part_done(ctx, message_item_id.to_string(), part)
            .await
        {
            tracing::debug!("Failed to emit content_part_done event: {}", e);
        }

        // Event: response.output_item.done
        if let Err(e) = emitter
            .emit_item_done(ctx, item, message_item_id.to_string())
            .await
        {
            tracing::debug!("Failed to emit item_done event: {}", e);
        }

        Ok(())
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
                context.request.metadata.as_ref(),
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
            context.signing_algo.clone(),
            context.client_pub_key.clone(),
        );

        let mut tools = tools::prepare_tools(&context.request);
        let tool_choice = tools::prepare_tool_choice(&context.request);

        // Set up MCP: connect to servers, discover tools, and process approvals
        if let Some(mcp_setup) = tools::setup_mcp(
            &context.request,
            context.mcp_client_factory.as_ref(),
            &context.response_items_repository,
            &mut ctx,
            &mut emitter,
        )
        .await?
        {
            tools.extend(mcp_setup.tool_definitions);
            context.tool_registry.register(mcp_setup.executor.clone());
            context.mcp_executor = Some(mcp_setup.executor);
            messages.extend(mcp_setup.approval_messages);
        }

        // Set up Function tools: register executor and process any function call outputs
        let function_executor = tools::FunctionToolExecutor::new(&context.request);
        if !function_executor.is_empty() {
            context.tool_registry.register(Arc::new(function_executor));
        }

        // Process function call outputs from input (client-provided results)
        let function_output_messages = Self::process_function_call_outputs(
            &context.request,
            &context.response_items_repository,
        )
        .await?;
        messages.extend(function_output_messages);

        // Check if this is an image model and handle it specially
        if let Ok(Some(model)) = context
            .completion_service
            .get_model(&context.request.model)
            .await
        {
            if Self::has_image_generation_capability(&model.output_modalities) {
                tracing::info!(
                    "Image generation model detected, handling image operation: {}",
                    model.model_name
                );

                // Handle image generation/editing and return early
                let image_result = Self::process_image_operation(
                    &mut ctx,
                    &mut emitter,
                    &mut context,
                    &initial_response,
                    workspace_id_domain.clone(),
                )
                .await;

                // Handle errors by updating response status to Failed
                if let Err(e) = image_result {
                    let error_message = e.to_string();
                    let failed_item = models::ResponseOutputItem::Message {
                        id: format!("msg_{}", Uuid::new_v4().simple()),
                        response_id: ctx.response_id_str.clone(),
                        previous_response_id: ctx.previous_response_id.clone(),
                        next_response_ids: vec![],
                        created_at: ctx.created_at,
                        status: models::ResponseItemStatus::Failed,
                        role: "assistant".to_string(),
                        content: vec![models::ResponseContentItem::OutputText {
                            text: error_message.clone(),
                            annotations: vec![],
                            logprobs: vec![],
                        }],
                        model: ctx.model.clone(),
                        metadata: None,
                    };
                    if let Err(create_err) = context
                        .response_items_repository
                        .create(
                            ctx.response_id.clone(),
                            ctx.api_key_id,
                            ctx.conversation_id,
                            failed_item,
                        )
                        .await
                    {
                        tracing::warn!(
                            "Failed to store failed image response item: {}",
                            create_err
                        );
                    }
                    if let Err(update_err) = context
                        .response_repository
                        .update(
                            ctx.response_id.clone(),
                            workspace_id_domain.clone(),
                            None,
                            models::ResponseStatus::Failed,
                            None,
                        )
                        .await
                    {
                        tracing::warn!(
                            "Failed to update response status to failed: {}",
                            update_err
                        );
                    }
                    return Err(e);
                }
                return Ok(());
            }
        }

        let max_iterations = 10; // Prevent infinite loops
        let mut iteration = 0;
        let mut final_response_text = String::new();

        // Run the agent loop to process completion and tool calls
        // Capture errors but continue to save partial data if client disconnected
        let agent_loop_result = Self::run_agent_loop(
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
        .await;

        // Determine final response status based on agent loop result
        let (final_status, incomplete_details) = match &agent_loop_result {
            Ok(AgentLoopResult::Completed) => (models::ResponseStatus::Completed, None),
            Ok(AgentLoopResult::ApprovalRequired) => (
                models::ResponseStatus::Incomplete,
                Some(models::ResponseIncompleteDetails {
                    reason: "mcp_approval_required".to_string(),
                }),
            ),
            Ok(AgentLoopResult::FunctionCallsRequired) => (
                models::ResponseStatus::Incomplete,
                Some(models::ResponseIncompleteDetails {
                    reason: "function_call_required".to_string(),
                }),
            ),
            Err(ref e) => {
                // Log error but continue - we want to save partial response even on disconnect
                tracing::warn!("Agent loop error (may be client disconnect): {:?}", e);
                (models::ResponseStatus::Completed, None)
            }
        };

        // Build final response
        let mut final_response = initial_response;
        final_response.status = final_status;
        final_response.incomplete_details = incomplete_details;

        // Load all response items from the database for this response
        let response_items = context
            .response_items_repository
            .list_by_response(ctx.response_id.clone())
            .await
            .map_err(|e| {
                errors::ResponseError::InternalError(format!("Failed to load response items: {e}"))
            })?;

        // Filter to get only assistant output items (excluding user input items)
        let mut output_items: Vec<_> = response_items
            .into_iter()
            .filter(|item| match item {
                models::ResponseOutputItem::Message { role, .. } => role == "assistant",
                _ => true, // Include all non-message items (tool calls, web searches, etc.)
            })
            .collect();

        // Prepend MCP list tools items (emitted but not stored in DB)
        if let Some(ref mcp_executor) = context.mcp_executor {
            let mcp_items = mcp_executor.get_mcp_list_tools_items().to_vec();
            output_items.splice(0..0, mcp_items);
        }

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

        // Initial failure: agent loop failed with no output/usage → create failed item, update status, return Err (client gets response.failed).
        // Otherwise (Ok or Err with partial output): update DB with final response, then emit response.completed.
        match agent_loop_result {
            Err(e)
                if final_response.output.is_empty() && final_response.usage.total_tokens == 0 =>
            {
                // Include error message in content so users can understand why it failed
                let error_message = e.to_string();
                let failed_item = models::ResponseOutputItem::Message {
                    id: format!("msg_{}", Uuid::new_v4().simple()),
                    response_id: ctx.response_id_str.clone(),
                    previous_response_id: ctx.previous_response_id.clone(),
                    next_response_ids: vec![],
                    created_at: ctx.created_at,
                    status: models::ResponseItemStatus::Failed,
                    role: "assistant".to_string(),
                    content: vec![models::ResponseContentItem::OutputText {
                        text: error_message,
                        annotations: vec![],
                        logprobs: vec![],
                    }],
                    model: ctx.model.clone(),
                    metadata: None,
                };
                if let Err(create_err) = context
                    .response_items_repository
                    .create(
                        ctx.response_id.clone(),
                        ctx.api_key_id,
                        ctx.conversation_id,
                        failed_item,
                    )
                    .await
                {
                    tracing::warn!("Failed to store failed response item: {}", create_err);
                }
                if let Err(update_err) = context
                    .response_repository
                    .update(
                        ctx.response_id.clone(),
                        workspace_id_domain.clone(),
                        None,
                        models::ResponseStatus::Failed,
                        None,
                    )
                    .await
                {
                    tracing::warn!("Failed to update response status to failed: {}", update_err);
                }
                return Err(e);
            }
            _ => {
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
                }
            }
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
    ) -> Result<AgentLoopResult, errors::ResponseError> {
        use crate::completions::ports::{CompletionMessage, CompletionRequest};

        // Track consecutive error tool calls to detect excessive retry loops
        let mut consecutive_error_count = 0;

        loop {
            *iteration += 1;
            if *iteration > max_iterations {
                tracing::warn!("Max iterations reached in agent loop");
                break;
            }

            tracing::debug!("Agent loop iteration {}", iteration);

            // Prepare extra params with tools and encryption headers
            let mut extra = std::collections::HashMap::new();
            if !tools.is_empty() {
                extra.insert("tools".to_string(), serde_json::to_value(tools).unwrap());
            }
            if let Some(tc) = tool_choice {
                extra.insert("tool_choice".to_string(), serde_json::to_value(tc).unwrap());
            }

            // Add encryption headers to extra for passing to completion service
            if let Some(ref signing_algo) = process_context.signing_algo {
                extra.insert(
                    encryption_headers::SIGNING_ALGO.to_string(),
                    serde_json::Value::String(signing_algo.clone()),
                );
            }
            if let Some(ref client_pub_key) = process_context.client_pub_key {
                extra.insert(
                    encryption_headers::CLIENT_PUB_KEY.to_string(),
                    serde_json::Value::String(client_pub_key.clone()),
                );
            }
            if let Some(ref model_pub_key) = process_context.model_pub_key {
                extra.insert(
                    encryption_headers::MODEL_PUB_KEY.to_string(),
                    serde_json::Value::String(model_pub_key.clone()),
                );
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
                store: process_context.request.store,
                body_hash: process_context.body_hash.to_string(),
                response_id: Some(ctx.response_id.clone()),
                n: None,
                extra,
            };

            // Get completion stream
            let completion_result = process_context
                .completion_service
                .create_chat_completion_stream(completion_request)
                .await
                .map_err(|e| {
                    errors::ResponseError::InternalError(format!("Completion error: {e}"))
                })?;

            let mut completion_stream = completion_result;

            // Process the completion stream and extract text + tool calls
            let stream_result = Self::process_completion_stream(
                &mut completion_stream,
                emitter,
                ctx,
                &process_context.response_items_repository,
                process_context,
            )
            .await?;

            // If stream errored (client disconnect, network error, etc.), stop the agent loop
            if stream_result.stream_error {
                tracing::info!("Stream error detected, stopping agent loop");
                return Err(errors::ResponseError::StreamInterrupted);
            }

            // Update response state
            if !stream_result.text.is_empty() {
                final_response_text.push_str(&stream_result.text);
            }

            // Check if we're done (no tool calls)
            if stream_result.tool_calls.is_empty() {
                // No tool calls - add assistant message with just text (if any)
                if !stream_result.text.is_empty() {
                    messages.push(CompletionMessage {
                        role: "assistant".to_string(),
                        content: stream_result.text.clone(),
                        tool_call_id: None,
                        tool_calls: None,
                    });
                    ctx.next_output_index();
                }
                tracing::debug!("No tool calls detected, ending agent loop");
                break;
            }

            // Tool calls present - add assistant message with tool_calls
            // This is REQUIRED by all providers (OpenAI, Anthropic, Gemini):
            // "messages with role 'tool' must be a response to a preceding message with 'tool_calls'"
            let completion_tool_calls: Vec<crate::completions::ports::CompletionToolCall> =
                stream_result
                    .tool_calls
                    .iter()
                    .map(|tc| {
                        let id = tc.id.clone().unwrap_or_else(|| {
                            // Fallback: generate ID if LLM didn't provide one
                            format!("{}_{}", tc.tool_type, uuid::Uuid::new_v4().simple())
                        });
                        crate::completions::ports::CompletionToolCall {
                            id,
                            name: tc.tool_type.clone(),
                            arguments: tc
                                .params
                                .as_ref()
                                .map(|p| p.to_string())
                                .unwrap_or_else(|| "{}".to_string()),
                            thought_signature: tc.thought_signature.clone(),
                        }
                    })
                    .collect();

            // Defensive: only set tool_calls if non-empty (some providers reject empty arrays)
            let tool_calls = if completion_tool_calls.is_empty() {
                None
            } else {
                Some(completion_tool_calls.clone())
            };

            messages.push(CompletionMessage {
                role: "assistant".to_string(),
                content: stream_result.text.clone(),
                tool_call_id: None,
                tool_calls,
            });
            if !stream_result.text.is_empty() {
                ctx.next_output_index();
            }

            let has_errors = stream_result
                .tool_calls
                .iter()
                .any(|tc| tc.tool_type == ERROR_TOOL_TYPE);
            if has_errors {
                consecutive_error_count += 1;
                if consecutive_error_count >= MAX_CONSECUTIVE_TOOL_FAILURES {
                    tracing::error!(
                        "Agent loop: {} consecutive iterations with tool call errors, stopping to prevent infinite retry",
                        consecutive_error_count
                    );
                    return Err(errors::ResponseError::InternalError(
                        format!("Tool calls failed {} consecutive iterations due to malformed arguments from model", MAX_CONSECUTIVE_TOOL_FAILURES),
                    ));
                }
            } else {
                consecutive_error_count = 0;
            }

            tracing::debug!("Executing {} tool calls", stream_result.tool_calls.len());

            // Execute each tool call, collecting any deferred instructions
            // Also track pending function calls for batching
            let mut deferred_instructions: Vec<String> = Vec::new();
            let mut pending_function_calls: Vec<tools::FunctionCallInfo> = Vec::new();

            for tool_call in stream_result.tool_calls {
                match Self::execute_and_emit_tool_call(
                    ctx,
                    emitter,
                    &tool_call,
                    messages,
                    process_context,
                    &mut deferred_instructions,
                )
                .await?
                {
                    tools::ToolExecutionResult::Success => {
                        // Continue processing tool calls
                    }
                    tools::ToolExecutionResult::ApprovalRequired => {
                        // MCP tool requires approval - flush any deferred instructions before pausing
                        if !deferred_instructions.is_empty() {
                            messages.push(CompletionMessage {
                                role: "system".to_string(),
                                content: std::mem::take(&mut deferred_instructions).join("\n\n"),
                                tool_call_id: None,
                                tool_calls: None,
                            });
                        }
                        return Ok(AgentLoopResult::ApprovalRequired);
                    }
                    tools::ToolExecutionResult::FunctionCallPending(info) => {
                        // Collect pending function calls for batching
                        pending_function_calls.push(info);
                    }
                }
            }

            // If there are pending function calls, return them all at once
            // This supports parallel tool calls - we batch all function calls before pausing
            if !pending_function_calls.is_empty() {
                if !deferred_instructions.is_empty() {
                    messages.push(CompletionMessage {
                        role: "system".to_string(),
                        content: std::mem::take(&mut deferred_instructions).join("\n\n"),
                        tool_call_id: None,
                        tool_calls: None,
                    });
                }
                return Ok(AgentLoopResult::FunctionCallsRequired);
            }

            // Add deferred instructions AFTER all tool results (combined into one system message)
            // This ensures tool results are consecutive (required by OpenAI/Anthropic/Gemini)
            if !deferred_instructions.is_empty() {
                messages.push(CompletionMessage {
                    role: "system".to_string(),
                    content: deferred_instructions.join("\n\n"),
                    tool_call_id: None,
                    tool_calls: None,
                });
            }
        }

        Ok(AgentLoopResult::Completed)
    }

    /// Execute a tool call and emit appropriate events.
    ///
    /// Returns `Ok(ToolExecutionResult::Success)` if the tool executed normally,
    /// or `Ok(ToolExecutionResult::ApprovalRequired)` if the tool requires user approval.
    ///
    /// Any instructions (e.g., citation instructions from web search) are collected into
    /// `deferred_instructions` to be added after all tool results. This ensures tool results
    /// are consecutive (required by OpenAI/Anthropic/Gemini for parallel tool calls).
    async fn execute_and_emit_tool_call(
        ctx: &mut crate::responses::service_helpers::ResponseStreamContext,
        emitter: &mut crate::responses::service_helpers::EventEmitter,
        tool_call: &crate::responses::service_helpers::ToolCallInfo,
        messages: &mut Vec<crate::completions::ports::CompletionMessage>,
        process_context: &mut ProcessStreamContext,
        deferred_instructions: &mut Vec<String>,
    ) -> Result<tools::ToolExecutionResult, errors::ResponseError> {
        use crate::completions::ports::CompletionMessage;

        // Use the LLM's tool call ID (required for matching tool results to tool calls)
        // Fallback to generated ID if the LLM didn't provide one
        let tool_call_id = tool_call.id.clone().unwrap_or_else(|| {
            format!("{}_{}", tool_call.tool_type, uuid::Uuid::new_v4().simple())
        });

        // Handle error tool calls (malformed tool calls detected during parsing)
        if tool_call.tool_type == ERROR_TOOL_TYPE {
            // For error tool calls, return the error message as the tool result
            // This allows the LLM to see what went wrong and retry
            // Note: tool_call_id is required for the API to match results to calls
            messages.push(CompletionMessage {
                role: "tool".to_string(),
                content: format!(
                    "ERROR: {}\n\nPlease correct the tool call format and try again.",
                    tool_call.query
                ),
                tool_call_id: Some(tool_call_id),
                tool_calls: None,
            });
            return Ok(tools::ToolExecutionResult::Success);
        }

        {
            let mut event_ctx = tools::ToolEventContext {
                stream_ctx: ctx,
                emitter,
                tool_call_id: &tool_call_id,
                response_items_repository: Some(&process_context.response_items_repository),
            };
            process_context
                .tool_registry
                .emit_start(tool_call, &mut event_ctx)
                .await?;
        }

        // Execute the tool using the registry
        let exec_context = tools::ToolExecutionContext {
            request: &process_context.request,
        };

        let tool_output = match process_context
            .tool_registry
            .execute(tool_call, &exec_context)
            .await
        {
            Ok(output) => Some(output),

            Err(e) => {
                // Check if this is a function call that needs client execution
                let is_function_call =
                    matches!(&e, errors::ResponseError::FunctionCallRequired { .. });
                let function_call_info =
                    if let errors::ResponseError::FunctionCallRequired { name, call_id } = &e {
                        Some(tools::FunctionCallInfo {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            arguments: tool_call
                                .params
                                .as_ref()
                                .map(|p| serde_json::to_string(p).unwrap_or_default())
                                .unwrap_or_default(),
                        })
                    } else {
                        None
                    };

                // Delegate error handling to the executor (e.g., MCP approval flow)
                let mut event_ctx = tools::ToolEventContext {
                    stream_ctx: ctx,
                    emitter,
                    tool_call_id: &tool_call_id,
                    response_items_repository: Some(&process_context.response_items_repository),
                };

                match process_context
                    .tool_registry
                    .handle_error(e, tool_call, &mut event_ctx)
                    .await?
                {
                    Some(output) => Some(output),
                    None => {
                        // None means special control flow
                        // For function calls, return FunctionCallPending so we can batch them
                        // For MCP approval, return ApprovalRequired
                        if is_function_call {
                            return Ok(tools::ToolExecutionResult::FunctionCallPending(
                                function_call_info.expect("function_call_info should be set"),
                            ));
                        }
                        return Ok(tools::ToolExecutionResult::ApprovalRequired);
                    }
                }
            }
        };

        // If we got here with None, something went wrong
        let tool_output = match tool_output {
            Some(output) => output,
            None => return Ok(tools::ToolExecutionResult::ApprovalRequired),
        };

        // Handle tool-specific side effects via pattern matching
        let (tool_content, instruction) = match tool_output {
            tools::ToolOutput::WebSearch { sources, .. } => {
                // Calculate start index based on current registry size
                let start_index = process_context
                    .source_registry
                    .as_ref()
                    .map(|r| r.web_sources.len())
                    .unwrap_or(0);

                // Format content with correct cumulative indices
                // format_results returns FormattedWebSearchResult with formatted text and optional instruction
                let result = tools::web_search::format_results(&sources, start_index);

                // Accumulate sources into registry
                if let Some(ref mut registry) = process_context.source_registry {
                    registry.web_sources.extend(sources);
                } else {
                    process_context.source_registry =
                        Some(models::SourceRegistry::with_results(sources));
                }

                // Reset failure counter on successful web search
                process_context.web_search_failure_count = 0;

                (result.formatted, result.instruction)
            }
            tools::ToolOutput::FileSearch { results } => {
                // Format file search results
                let formatted =
                    tools::file_search::FileSearchToolExecutor::format_results(&results);
                (formatted, None)
            }
            tools::ToolOutput::Text(content) => {
                // Plain text has no side effects
                (content, None)
            }
        };

        // Emit tool-specific completion events via registry
        {
            let mut event_ctx = tools::ToolEventContext {
                stream_ctx: ctx,
                emitter,
                tool_call_id: &tool_call_id,
                response_items_repository: Some(&process_context.response_items_repository),
            };
            process_context
                .tool_registry
                .emit_complete(tool_call, &mut event_ctx)
                .await?;
        }

        // Add tool result to message history with matching tool_call_id
        // This is REQUIRED by all providers for the agent loop to work correctly
        messages.push(CompletionMessage {
            role: "tool".to_string(),
            content: tool_content,
            tool_call_id: Some(tool_call_id),
            tool_calls: None,
        });

        // Defer citation instruction to be added after all tool results
        // This ensures tool results are consecutive (required by OpenAI/Anthropic/Gemini)
        if let Some(instruction) = instruction {
            deferred_instructions.push(instruction);
        }

        Ok(tools::ToolExecutionResult::Success)
    }

    /// Process function call outputs from the request input.
    ///
    /// When resuming a response after function calls, the client provides
    /// FunctionCallOutput items with the results. This function:
    /// 1. Extracts FunctionCallOutput items from the request input
    /// 2. Fetches the stored FunctionCall items by call_id
    /// 3. Creates tool result messages for the conversation context
    ///
    /// Returns messages to add to the conversation context.
    async fn process_function_call_outputs(
        request: &models::CreateResponseRequest,
        response_items_repository: &Arc<dyn ports::ResponseItemRepositoryTrait>,
    ) -> Result<Vec<crate::completions::ports::CompletionMessage>, errors::ResponseError> {
        use crate::completions::ports::CompletionMessage;

        let mut messages = Vec::new();

        // Extract function call outputs from input
        let function_outputs: Vec<_> = match &request.input {
            Some(models::ResponseInput::Items(items)) => items
                .iter()
                .filter_map(|item| item.as_function_call_output())
                .collect(),
            _ => return Ok(messages),
        };

        if function_outputs.is_empty() {
            return Ok(messages);
        }

        // Function call outputs require a previous_response_id so we can verify
        // the call_id matches a real FunctionCall from that response.
        let prev_response_id = request.previous_response_id.as_ref().ok_or_else(|| {
            errors::ResponseError::FunctionCallNotFound(
                "function_call_output requires previous_response_id".to_string(),
            )
        })?;

        let uuid_str = prev_response_id
            .strip_prefix(crate::id_prefixes::PREFIX_RESP)
            .unwrap_or(prev_response_id);

        let response_uuid = uuid::Uuid::parse_str(uuid_str).map_err(|e| {
            errors::ResponseError::FunctionCallNotFound(format!(
                "invalid previous_response_id: {e}"
            ))
        })?;

        let prev_items = response_items_repository
            .list_by_response(models::ResponseId(response_uuid))
            .await
            .map_err(|e| {
                errors::ResponseError::InternalError(format!("Failed to fetch response items: {e}"))
            })?;

        for (call_id, output) in function_outputs {
            let found = prev_items.iter().any(|item| {
                matches!(item, models::ResponseOutputItem::FunctionCall {
                    call_id: item_call_id,
                    ..
                } if item_call_id == call_id)
            });

            if !found {
                return Err(errors::ResponseError::FunctionCallNotFound(
                    call_id.to_string(),
                ));
            }

            // Create tool result message with the function output
            messages.push(CompletionMessage {
                role: "tool".to_string(),
                content: output.to_string(),
                tool_call_id: Some(call_id.to_string()),
                tool_calls: None,
            });
        }

        tracing::debug!(
            "Processed {} function call outputs into tool result messages",
            messages.len()
        );

        Ok(messages)
    }

    /// Store user input messages as response_items
    async fn store_input_as_response_items(
        response_items_repository: &Arc<dyn ports::ResponseItemRepositoryTrait>,
        response_id: models::ResponseId,
        api_key_id: uuid::Uuid,
        conversation_id: Option<ConversationId>,
        input: &models::ResponseInput,
        model: &str,
        request_metadata: Option<&serde_json::Value>,
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
                    metadata: request_metadata.cloned(),
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
                    let (role, input_content, metadata) = match &input_item {
                        models::ResponseInputItem::Message {
                            role,
                            content,
                            metadata,
                        } => (role.clone(), content, metadata.clone()),
                        models::ResponseInputItem::McpApprovalResponse { .. } => {
                            continue;
                        }
                        models::ResponseInputItem::McpListTools { .. } => {
                            continue;
                        }
                        models::ResponseInputItem::FunctionCallOutput {
                            call_id, output, ..
                        } => {
                            // Store function call output as a response item for conversation history
                            let fco_item = models::ResponseOutputItem::FunctionCallOutput {
                                id: format!("fco_{}", uuid::Uuid::new_v4().simple()),
                                response_id: String::new(),
                                previous_response_id: None,
                                next_response_ids: vec![],
                                created_at: 0,
                                call_id: call_id.clone(),
                                output: output.clone(),
                            };
                            response_items_repository
                                .create(response_id.clone(), api_key_id, conversation_id, fco_item)
                                .await
                                .map_err(|e| {
                                    errors::ResponseError::InternalError(format!(
                                        "Failed to store function call output: {e}"
                                    ))
                                })?;
                            continue;
                        }
                    };

                    let content = match input_content {
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

                    // Use item-level metadata if present, otherwise fall back to request metadata
                    let metadata = metadata.or_else(|| request_metadata.cloned());

                    let message_item = models::ResponseOutputItem::Message {
                        id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
                        // These fields are placeholders - repository enriches them via JOIN when storing/retrieving
                        response_id: String::new(),
                        previous_response_id: None,
                        next_response_ids: vec![],
                        created_at: 0,
                        status: models::ResponseItemStatus::Completed,
                        role,
                        content,
                        model: model.to_string(),
                        metadata,
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
                    tool_call_id: None,
                    tool_calls: None,
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
                tool_call_id: None,
                tool_calls: None,
            });
        } else {
            // Add language instruction and time context as a system message if no instructions provided
            let system_content = format!("{language_instruction}\n\n{time_context}");
            messages.push(CompletionMessage {
                role: "system".to_string(),
                content: system_content,
                tool_call_id: None,
                tool_calls: None,
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

            // Filter to ancestor branch if previous_response_id is specified
            let conversation_items =
                Self::filter_to_ancestor_branch(conversation_items, &request.previous_response_id);

            // Convert response items to completion messages.
            //
            // Items come in chronological order. We need to reconstruct the full
            // message history including assistant messages with tool_calls and
            // tool result messages. The structure LLM providers expect is:
            //
            //   [user] -> [assistant with tool_calls] -> [tool results...] -> [assistant]
            //
            // FunctionCall/McpCall/ToolCall items from the same response represent
            // tool_calls on a single assistant message. We collect them and emit the
            // assistant message when we encounter the next non-tool-call item (or
            // reach the end of items).
            let messages_before = messages.len();

            // Accumulator for consecutive tool call items from the same response
            let mut pending_tool_calls: Vec<crate::completions::ports::CompletionToolCall> =
                Vec::new();
            let mut pending_tool_calls_response_id: Option<String> = None;

            // Helper closure: flush pending tool calls as an assistant message
            let flush_tool_calls =
                |pending: &mut Vec<crate::completions::ports::CompletionToolCall>,
                 pending_resp_id: &mut Option<String>,
                 msgs: &mut Vec<CompletionMessage>| {
                    if !pending.is_empty() {
                        msgs.push(CompletionMessage {
                            role: "assistant".to_string(),
                            content: String::new(),
                            tool_call_id: None,
                            tool_calls: Some(std::mem::take(pending)),
                        });
                        *pending_resp_id = None;
                    }
                };

            for item in conversation_items {
                match item {
                    models::ResponseOutputItem::Message { role, content, .. } => {
                        // Flush any pending tool calls before processing this message
                        flush_tool_calls(
                            &mut pending_tool_calls,
                            &mut pending_tool_calls_response_id,
                            &mut messages,
                        );

                        // Extract text from content parts
                        let mut text_parts = Vec::new();

                        type ContentFuture = std::pin::Pin<
                            Box<
                                dyn std::future::Future<
                                        Output = Result<String, errors::ResponseError>,
                                    > + Send,
                            >,
                        >;
                        let mut results: Vec<ContentFuture> = Vec::new();

                        for part in &content {
                            match part {
                                models::ResponseContentItem::InputText { text } => {
                                    results
                                        .push(Box::pin(futures::future::ready(Ok(text.clone()))));
                                }
                                models::ResponseContentItem::OutputText { text, .. } => {
                                    results
                                        .push(Box::pin(futures::future::ready(Ok(text.clone()))));
                                }
                                models::ResponseContentItem::InputFile { file_id, .. } => {
                                    let file_id = file_id.clone();
                                    let workspace_id = workspace_id.0;
                                    let file_service = file_service.clone();
                                    results.push(Box::pin(async move {
                                        Self::process_input_file(
                                            &file_id,
                                            workspace_id,
                                            &file_service,
                                        )
                                        .await
                                    }));
                                }
                                _ => {}
                            }
                        }

                        for result in futures::future::join_all(results).await {
                            match result {
                                Ok(text) => text_parts.push(text),
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
                                tool_call_id: None,
                                tool_calls: None,
                            });
                        }
                    }
                    models::ResponseOutputItem::FunctionCall {
                        response_id,
                        call_id,
                        name,
                        arguments,
                        ..
                    } => {
                        // If this is from a different response than pending, flush first
                        if pending_tool_calls_response_id.as_ref() != Some(&response_id)
                            && !pending_tool_calls.is_empty()
                        {
                            flush_tool_calls(
                                &mut pending_tool_calls,
                                &mut pending_tool_calls_response_id,
                                &mut messages,
                            );
                        }
                        pending_tool_calls_response_id = Some(response_id);
                        pending_tool_calls.push(crate::completions::ports::CompletionToolCall {
                            id: call_id,
                            name,
                            arguments,
                            thought_signature: None,
                        });
                    }
                    models::ResponseOutputItem::FunctionCallOutput {
                        call_id, output, ..
                    } => {
                        // Flush pending tool calls first (the assistant message must precede tool results)
                        flush_tool_calls(
                            &mut pending_tool_calls,
                            &mut pending_tool_calls_response_id,
                            &mut messages,
                        );
                        messages.push(CompletionMessage {
                            role: "tool".to_string(),
                            content: output,
                            tool_call_id: Some(call_id),
                            tool_calls: None,
                        });
                    }
                    models::ResponseOutputItem::McpCall {
                        response_id,
                        name,
                        arguments,
                        output,
                        id,
                        ..
                    } => {
                        // MCP calls are server-executed: both the tool_call and result are stored.
                        // If this has output, we emit an assistant message with a tool_call and
                        // a tool result message. If no output (pending approval), skip.
                        if let Some(tool_output) = output {
                            // If from a different response, flush first
                            if pending_tool_calls_response_id.as_ref() != Some(&response_id)
                                && !pending_tool_calls.is_empty()
                            {
                                flush_tool_calls(
                                    &mut pending_tool_calls,
                                    &mut pending_tool_calls_response_id,
                                    &mut messages,
                                );
                            }

                            // Use the item id as the tool_call_id for correlation
                            let tool_call_id = id;
                            pending_tool_calls_response_id = Some(response_id);
                            pending_tool_calls.push(
                                crate::completions::ports::CompletionToolCall {
                                    id: tool_call_id.clone(),
                                    name,
                                    arguments,
                                    thought_signature: None,
                                },
                            );

                            // Immediately flush and add the tool result since we have it
                            flush_tool_calls(
                                &mut pending_tool_calls,
                                &mut pending_tool_calls_response_id,
                                &mut messages,
                            );
                            messages.push(CompletionMessage {
                                role: "tool".to_string(),
                                content: tool_output,
                                tool_call_id: Some(tool_call_id),
                                tool_calls: None,
                            });
                        }
                    }
                    models::ResponseOutputItem::ToolCall {
                        response_id,
                        id: tool_call_id,
                        function,
                        ..
                    } => {
                        // Reconstruct from the legacy ToolCall variant
                        if pending_tool_calls_response_id.as_ref() != Some(&response_id)
                            && !pending_tool_calls.is_empty()
                        {
                            flush_tool_calls(
                                &mut pending_tool_calls,
                                &mut pending_tool_calls_response_id,
                                &mut messages,
                            );
                        }
                        pending_tool_calls_response_id = Some(response_id);
                        pending_tool_calls.push(crate::completions::ports::CompletionToolCall {
                            id: tool_call_id,
                            name: function.name,
                            arguments: function.arguments,
                            thought_signature: None,
                        });
                    }
                    // Skip items that don't contribute to conversation context
                    models::ResponseOutputItem::WebSearchCall { .. }
                    | models::ResponseOutputItem::Reasoning { .. }
                    | models::ResponseOutputItem::McpListTools { .. }
                    | models::ResponseOutputItem::McpApprovalRequest { .. } => {}
                }
            }

            // Flush any remaining pending tool calls at end of items
            flush_tool_calls(
                &mut pending_tool_calls,
                &mut pending_tool_calls_response_id,
                &mut messages,
            );

            let loaded_count = messages.len() - messages_before;
            tracing::info!(
                "Loaded {} messages from conversation {}",
                loaded_count,
                conversation_id
            );
        }

        // Add input messages
        if let Some(input) = &request.input {
            match input {
                models::ResponseInput::Text(text) => {
                    messages.push(CompletionMessage {
                        role: "user".to_string(),
                        content: text.clone(),
                        tool_call_id: None,
                        tool_calls: None,
                    });
                }
                models::ResponseInput::Items(items) => {
                    for item in items {
                        // Only process message input items
                        let (role, item_content) = match item {
                            models::ResponseInputItem::Message { role, content, .. } => {
                                (role.clone(), content)
                            }
                            models::ResponseInputItem::McpApprovalResponse { .. } => {
                                continue;
                            }
                            models::ResponseInputItem::McpListTools { .. } => {
                                continue;
                            }
                            models::ResponseInputItem::FunctionCallOutput { .. } => {
                                // Function call outputs are processed separately
                                continue;
                            }
                        };

                        let content = match item_content {
                            models::ResponseContent::Text(text) => text.clone(),
                            models::ResponseContent::Parts(parts) => {
                                // Extract text from parts and fetch file content if needed
                                Self::extract_content_parts(parts, workspace_id.0, file_service)
                                    .await?
                            }
                        };

                        messages.push(CompletionMessage {
                            role,
                            content,
                            tool_call_id: None,
                            tool_calls: None,
                        });
                    }
                }
            }
        }

        Ok(messages)
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
        // Base reasoning tag names (used when matching parsed tag names)
        const REASONING_TAGS: &[&str] = &["think", "reasoning", "thought", "reflect", "analysis"];

        // String prefixes (with '<' and '</') used for cheap substring checks in the fast path.
        // This avoids allocating strings with `format!` on every call and keeps the list
        // in sync with REASONING_TAGS.
        const REASONING_TAG_PREFIXES: &[&str] =
            &["<think", "<reasoning", "<thought", "<reflect", "<analysis"];

        // Fast paths for common cases when we're not currently inside a reasoning block.
        //
        // We MUST still run the full logic when:
        // - inside_reasoning == true (the text should be routed to reasoning_buffer)
        // - the chunk contains '<' that might start or close reasoning tags, or HTML
        //   tags we want to preserve exactly, like <!DOCTYPE> or <br/>.
        if !*inside_reasoning {
            // 1) No '<' at all: impossible to contain reasoning tags or HTML markup
            // we need to specially handle. Treat entire chunk as clean text.
            if !delta_text.contains('<') {
                return (delta_text.to_string(), None, TagTransition::None);
            }

            // 2) Contains '<' but clearly no reasoning tag prefixes. We still want to
            // preserve HTML tags exactly, but we don't need to walk character-by-character
            // to strip reasoning, because there is none.
            //
            // This is a conservative check: we only skip detailed parsing if we see
            // no known reasoning tag prefixes at all (case-insensitive). This does not
            // try to handle cross-chunk partial tags – those are already treated as
            // literal text by design.
            let lower = delta_text.to_ascii_lowercase();
            let has_reasoning_prefix = REASONING_TAG_PREFIXES
                .iter()
                .any(|prefix| lower.contains(prefix));
            if !has_reasoning_prefix {
                return (delta_text.to_string(), None, TagTransition::None);
            }
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
                        // This is not a simple reasoning tag, collect the entire tag content.
                        //
                        // However, if we've already seen non-tag-name characters for this tag
                        // and now see another '<', this likely indicates the start of a new tag
                        // (e.g. in sequences like "<think>1 < 2</think>"). In that case we
                        // should stop parsing the current tag and let the outer loop handle
                        // the next '<' as a new tag, instead of greedily consuming
                        // "</think>" into this tag.
                        if found_non_tag_char && next_ch == '<' {
                            break;
                        }

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
        accumulator: &mut crate::responses::service_helpers::ToolCallAccumulator,
    ) {
        use inference_providers::StreamChunk;

        if let StreamChunk::Chat(chat_chunk) = &event.chunk {
            for choice in &chat_chunk.choices {
                if let Some(delta) = &choice.delta {
                    if let Some(tool_calls) = &delta.tool_calls {
                        for tool_call in tool_calls {
                            let index = tool_call.index.unwrap_or(0);
                            let entry = accumulator.entry(index).or_default();

                            if let Some(id) = &tool_call.id {
                                entry.id = Some(id.clone());
                            }

                            if let Some(function) = &tool_call.function {
                                if let Some(name) = &function.name {
                                    entry.name = Some(name.clone());
                                }
                                if let Some(args_fragment) = &function.arguments {
                                    entry.arguments.push_str(args_fragment);
                                }
                            }

                            if let Some(thought_sig) = &tool_call.thought_signature {
                                entry.thought_signature = Some(thought_sig.clone());
                            }
                        }
                    }
                }
            }
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
        signing_algo: Option<String>,
        client_pub_key: Option<String>,
    ) -> Option<tokio::task::JoinHandle<Result<(), errors::ResponseError>>> {
        // Skip title generation if request is encrypted
        // (both headers X-Signing-Algo and X-Client-Pub-Key are set)
        if signing_algo.is_some() && client_pub_key.is_some() {
            return None;
        }

        // Only proceed if we have a conversation_id
        let conv_id = conversation_id?;

        // Extract first user message from request
        let user_message = match &request.input {
            Some(models::ResponseInput::Text(text)) => text.clone(),
            Some(models::ResponseInput::Items(items)) => {
                // Find first user message
                items
                    .iter()
                    .filter_map(|item| match item {
                        models::ResponseInputItem::Message { role, content, .. }
                            if role == "user" =>
                        {
                            Some(content)
                        }
                        _ => None,
                    })
                    .next()
                    .and_then(|content| match content {
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
        // Use iterator to safely handle UTF-8 (cannot panic)
        let mut chars = user_message.chars();
        let truncated_message: String = chars.by_ref().take(500).collect();
        let truncated_message = if chars.next().is_some() {
            format!("{truncated_message}...")
        } else {
            truncated_message
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
                tool_call_id: None,
                tool_calls: None,
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
            store: None,
            body_hash: String::new(),
            response_id: None, // Title generation is not tied to a specific response
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

        // Truncate to max 60 characters (iterator approach cannot panic)
        let mut chars = generated_title.chars();
        let title: String = chars.by_ref().take(57).collect();
        let title = if chars.next().is_some() {
            format!("{title}...")
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

    /// Check if a model has image generation capability based on output_modalities
    fn has_image_generation_capability(output_modalities: &Option<Vec<String>>) -> bool {
        output_modalities
            .as_ref()
            .map(|modalities| modalities.contains(&"image".to_string()))
            .unwrap_or(false)
    }

    /// Process image generation or editing operations
    async fn process_image_operation(
        ctx: &mut crate::responses::service_helpers::ResponseStreamContext,
        emitter: &mut crate::responses::service_helpers::EventEmitter,
        process_context: &mut ProcessStreamContext,
        initial_response: &models::ResponseObject,
        workspace_id_domain: crate::workspace::WorkspaceId,
    ) -> Result<(), errors::ResponseError> {
        // Extract text prompt from request
        let prompt = Self::extract_prompt_from_request(&process_context.request)?;

        // Determine if this is image editing or generation
        let (has_input_image, _has_input_text) =
            Self::analyze_input_content(&process_context.request);

        // Build encryption headers
        let mut extra_params = std::collections::HashMap::new();
        if let Some(model_pub_key) = &process_context.model_pub_key {
            extra_params.insert(
                crate::common::encryption_headers::MODEL_PUB_KEY.to_string(),
                serde_json::json!(model_pub_key),
            );
        }

        // Call the appropriate image API
        let response = if has_input_image {
            let image_bytes =
                Self::extract_input_image_from_request(&process_context.request).await?;
            let params = inference_providers::ImageEditParams {
                model: process_context.request.model.clone(),
                image: std::sync::Arc::new(image_bytes),
                prompt,
                size: None,
                response_format: Some("b64_json".to_string()),
            };

            process_context
                .completion_service
                .get_inference_provider_pool()
                .image_edit(params, process_context.body_hash.clone())
                .await
                .map_err(|e| {
                    tracing::error!(error = %e, "Image edit request failed");
                    errors::ResponseError::InternalError(
                        "Image edit processing failed. Please try again later.".to_string(),
                    )
                })?
        } else {
            let params = inference_providers::ImageGenerationParams {
                model: process_context.request.model.clone(),
                prompt,
                size: None,
                quality: None,
                style: None,
                n: Some(1),
                response_format: Some("b64_json".to_string()),
                extra: extra_params,
            };

            process_context
                .completion_service
                .get_inference_provider_pool()
                .image_generation(params, process_context.body_hash.clone())
                .await
                .map_err(|e| {
                    tracing::error!(error = %e, "Image generation request failed");
                    errors::ResponseError::InternalError(
                        "Image generation processing failed. Please try again later.".to_string(),
                    )
                })?
        };

        // Extract base64 images from response
        let image_data: Vec<models::ImageOutputData> = response
            .response
            .data
            .iter()
            .filter_map(|img| {
                img.b64_json.as_ref().map(|b64| models::ImageOutputData {
                    b64_json: Some(b64.clone()),
                    url: None,
                    revised_prompt: None,
                })
            })
            .collect();

        if image_data.is_empty() {
            return Err(errors::ResponseError::InternalError(
                "Image generation returned no results".to_string(),
            ));
        }

        // Create message item with image output
        let message_item = models::ResponseOutputItem::Message {
            id: format!("msg_{}", Uuid::new_v4().simple()),
            response_id: ctx.response_id_str.clone(),
            previous_response_id: ctx.previous_response_id.clone(),
            next_response_ids: vec![],
            created_at: ctx.created_at,
            status: models::ResponseItemStatus::Completed,
            role: "assistant".to_string(),
            content: vec![models::ResponseContentItem::OutputImage {
                data: image_data.clone(),
                url: None,
            }],
            model: process_context.request.model.clone(),
            metadata: None,
        };

        // Store the image message item in database
        process_context
            .response_items_repository
            .create(
                ctx.response_id.clone(),
                ctx.api_key_id,
                ctx.conversation_id,
                message_item.clone(),
            )
            .await
            .map_err(|e| {
                errors::ResponseError::InternalError(format!(
                    "Failed to store image response item: {e}"
                ))
            })?;

        // Emit image output item event
        let event = models::ResponseStreamEvent {
            event_type: "response.output_item.added".to_string(),
            sequence_number: None,
            response: None,
            output_index: Some(0),
            content_index: None,
            item: Some(message_item.clone()),
            item_id: None,
            part: None,
            delta: None,
            text: None,
            logprobs: None,
            obfuscation: None,
            annotation_index: None,
            annotation: None,
            conversation_title: None,
        };
        use futures_util::SinkExt;
        let _ = emitter.tx.clone().send(event).await;

        // Update response status and usage
        let mut final_response = initial_response.clone();
        final_response.status = models::ResponseStatus::Completed;
        // For image operations, report image count as output tokens
        final_response.usage = models::Usage::new(0, image_data.len() as i32);
        // Include the message item with image in the output
        final_response.output = vec![message_item.clone()];

        // Emit completion event
        let completion_event = models::ResponseStreamEvent {
            event_type: "response.completed".to_string(),
            sequence_number: None,
            response: Some(final_response.clone()),
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
            conversation_title: None,
        };
        let _ = emitter.tx.clone().send(completion_event).await;

        // Update response in database
        let usage_json = serde_json::to_value(&final_response.usage).map_err(|e| {
            errors::ResponseError::InternalError(format!("Failed to serialize usage: {e}"))
        })?;

        process_context
            .response_repository
            .update(
                ctx.response_id.clone(),
                workspace_id_domain,
                None,
                final_response.status.clone(),
                Some(usage_json),
            )
            .await
            .map_err(|e| {
                errors::ResponseError::InternalError(format!(
                    "Failed to update response with image usage: {e}"
                ))
            })?;

        ctx.total_output_tokens += image_data.len() as i32;

        Ok(())
    }

    /// Analyze input content to determine if it contains images or text
    fn analyze_input_content(request: &models::CreateResponseRequest) -> (bool, bool) {
        let mut has_image = false;
        let mut has_text = false;

        if let Some(models::ResponseInput::Text(text)) = &request.input {
            has_text = !text.trim().is_empty();
        } else if let Some(models::ResponseInput::Items(items)) = &request.input {
            for item in items {
                match item.content() {
                    Some(models::ResponseContent::Text(text)) => {
                        if !text.trim().is_empty() {
                            has_text = true;
                        }
                    }
                    Some(models::ResponseContent::Parts(parts)) => {
                        for part in parts {
                            match part {
                                models::ResponseContentPart::InputImage { .. } => {
                                    has_image = true;
                                }
                                models::ResponseContentPart::InputText { text } => {
                                    if !text.trim().is_empty() {
                                        has_text = true;
                                    }
                                }
                                models::ResponseContentPart::InputFile { .. } => {
                                    has_text = true;
                                }
                            }
                        }
                    }
                    None => {}
                }
            }
        }

        (has_image, has_text)
    }

    /// Validate image format by checking magic bytes
    fn validate_image_format(data: &[u8]) -> Result<(), errors::ResponseError> {
        if data.len() < 3 {
            return Err(errors::ResponseError::InvalidParams(
                "Image data too small".to_string(),
            ));
        }

        // Check for JPEG (FF D8 FF) or PNG (89 50 4E 47)
        if (data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF)
            || (data.len() >= 4
                && data[0] == 0x89
                && data[1] == 0x50
                && data[2] == 0x4E
                && data[3] == 0x47)
        {
            Ok(())
        } else {
            Err(errors::ResponseError::InvalidParams(
                "Invalid image format (must be PNG or JPEG)".to_string(),
            ))
        }
    }

    /// Extract text prompt from request
    fn extract_prompt_from_request(
        request: &models::CreateResponseRequest,
    ) -> Result<String, errors::ResponseError> {
        match &request.input {
            Some(models::ResponseInput::Text(text)) => Ok(text.clone()),
            Some(models::ResponseInput::Items(items)) => {
                let mut text_parts = Vec::new();
                for item in items {
                    match item.content() {
                        Some(models::ResponseContent::Text(text)) => {
                            text_parts.push(text.clone());
                        }
                        Some(models::ResponseContent::Parts(parts)) => {
                            for part in parts {
                                if let models::ResponseContentPart::InputText { text } = part {
                                    text_parts.push(text.clone());
                                }
                            }
                        }
                        None => {}
                    }
                }
                if text_parts.is_empty() {
                    return Err(errors::ResponseError::InvalidParams(
                        "No text prompt found".to_string(),
                    ));
                }
                Ok(text_parts.join(" "))
            }
            None => Err(errors::ResponseError::InvalidParams(
                "No input provided".to_string(),
            )),
        }
    }

    /// Extract image from request (for edit operations)
    async fn extract_input_image_from_request(
        request: &models::CreateResponseRequest,
    ) -> Result<Vec<u8>, errors::ResponseError> {
        use base64::Engine;

        if let Some(models::ResponseInput::Items(items)) = &request.input {
            for item in items {
                if let Some(models::ResponseContent::Parts(parts)) = item.content() {
                    for part in parts {
                        if let models::ResponseContentPart::InputImage {
                            image_url,
                            detail: _,
                        } = part
                        {
                            let url_str = match image_url {
                                models::ResponseImageUrl::String(s) => s.clone(),
                                models::ResponseImageUrl::Object { url } => url.clone(),
                            };

                            if let Some(comma_pos) = url_str.find(',') {
                                let base64_str = &url_str[comma_pos + 1..];
                                let base64_str_owned = base64_str.to_string();

                                let decoded = tokio::task::spawn_blocking(move || {
                                    base64::engine::general_purpose::STANDARD
                                        .decode(&base64_str_owned)
                                })
                                .await
                                .map_err(|e| {
                                    errors::ResponseError::InternalError(format!(
                                        "Base64 decode failed: {e}"
                                    ))
                                })?
                                .map_err(|e| {
                                    errors::ResponseError::InvalidParams(format!(
                                        "Failed to decode base64: {e}"
                                    ))
                                })?;

                                Self::validate_image_format(&decoded)?;
                                return Ok(decoded);
                            } else {
                                // Found an image but it's not in data URL format
                                return Err(errors::ResponseError::InvalidParams(
                                    "Unsupported image URL format: expected data URL with base64 encoding (e.g., 'data:image/png;base64,...')".to_string(),
                                ));
                            }
                        }
                    }
                }
            }
        }

        Err(errors::ResponseError::InvalidParams(
            "No input image found".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::responses::tools::WEB_SEARCH_TOOL_NAME;

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

        // The whole text should remain
        assert_eq!(clean, input);
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

    #[test]
    fn test_filter_to_ancestor_branch_no_filter() {
        // When target_response_id is None, all items should be returned unchanged
        let items = vec![
            models::ResponseOutputItem::Message {
                id: "msg_1".to_string(),
                response_id: "resp_a".to_string(),
                previous_response_id: None,
                next_response_ids: vec!["resp_b".to_string()],
                created_at: 1000,
                role: "user".to_string(),
                content: vec![],
                status: models::ResponseItemStatus::Completed,
                model: "test-model".to_string(),
                metadata: None,
            },
            models::ResponseOutputItem::Message {
                id: "msg_2".to_string(),
                response_id: "resp_b".to_string(),
                previous_response_id: Some("resp_a".to_string()),
                next_response_ids: vec![],
                created_at: 2000,
                role: "assistant".to_string(),
                content: vec![],
                status: models::ResponseItemStatus::Completed,
                model: "test-model".to_string(),
                metadata: None,
            },
        ];

        let result = ResponseServiceImpl::filter_to_ancestor_branch(items.clone(), &None);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_filter_to_ancestor_branch_filters_to_chain() {
        // Create a tree structure:
        //     resp_a (root)
        //     /    \
        //  resp_b  resp_c
        //    |
        //  resp_d
        //
        // resp_b has multiple items (message + tool call) to verify all items
        // from the same response are included.
        // Filtering to resp_d should only include resp_a, resp_b (all items), resp_d
        let items = vec![
            models::ResponseOutputItem::Message {
                id: "msg_a".to_string(),
                response_id: "resp_a".to_string(),
                previous_response_id: None,
                next_response_ids: vec!["resp_b".to_string(), "resp_c".to_string()],
                created_at: 1000,
                role: "user".to_string(),
                content: vec![],
                status: models::ResponseItemStatus::Completed,
                model: "test-model".to_string(),
                metadata: None,
            },
            // resp_b has a message
            models::ResponseOutputItem::Message {
                id: "msg_b".to_string(),
                response_id: "resp_b".to_string(),
                previous_response_id: Some("resp_a".to_string()),
                next_response_ids: vec!["resp_d".to_string()],
                created_at: 2000,
                role: "assistant".to_string(),
                content: vec![],
                status: models::ResponseItemStatus::Completed,
                model: "test-model".to_string(),
                metadata: None,
            },
            // resp_b also has a tool call (same response_id, multiple items)
            models::ResponseOutputItem::ToolCall {
                id: "tool_b".to_string(),
                response_id: "resp_b".to_string(),
                previous_response_id: Some("resp_a".to_string()),
                next_response_ids: vec!["resp_d".to_string()],
                created_at: 2001,
                status: models::ResponseItemStatus::Completed,
                tool_type: "function".to_string(),
                function: models::ResponseOutputFunction {
                    name: WEB_SEARCH_TOOL_NAME.to_string(),
                    arguments: "{}".to_string(),
                },
                model: "test-model".to_string(),
            },
            models::ResponseOutputItem::Message {
                id: "msg_c".to_string(),
                response_id: "resp_c".to_string(),
                previous_response_id: Some("resp_a".to_string()),
                next_response_ids: vec![],
                created_at: 2000,
                role: "assistant".to_string(),
                content: vec![],
                status: models::ResponseItemStatus::Completed,
                model: "test-model".to_string(),
                metadata: None,
            },
            models::ResponseOutputItem::Message {
                id: "msg_d".to_string(),
                response_id: "resp_d".to_string(),
                previous_response_id: Some("resp_b".to_string()),
                next_response_ids: vec![],
                created_at: 3000,
                role: "user".to_string(),
                content: vec![],
                status: models::ResponseItemStatus::Completed,
                model: "test-model".to_string(),
                metadata: None,
            },
        ];

        // Filter to resp_d - should include:
        // - resp_a (1 item)
        // - resp_b (2 items: message + tool call)
        // - resp_d (1 item)
        // - NOT resp_c
        let result = ResponseServiceImpl::filter_to_ancestor_branch(
            items.clone(),
            &Some("resp_d".to_string()),
        );

        // Total: 4 items (1 from resp_a, 2 from resp_b, 1 from resp_d)
        assert_eq!(result.len(), 4);

        let item_ids: Vec<&str> = result
            .iter()
            .map(|item| match item {
                models::ResponseOutputItem::Message { id, .. } => id.as_str(),
                models::ResponseOutputItem::ToolCall { id, .. } => id.as_str(),
                _ => "",
            })
            .collect();

        // Verify all items from ancestor responses are included
        assert!(item_ids.contains(&"msg_a"));
        assert!(item_ids.contains(&"msg_b"));
        assert!(item_ids.contains(&"tool_b")); // Both items from resp_b
        assert!(item_ids.contains(&"msg_d"));
        // resp_c should be excluded
        assert!(!item_ids.contains(&"msg_c"));
    }

    #[test]
    fn test_filter_to_ancestor_branch_single_item() {
        // Test with a single root item
        let items = vec![models::ResponseOutputItem::Message {
            id: "msg_1".to_string(),
            response_id: "resp_a".to_string(),
            previous_response_id: None,
            next_response_ids: vec![],
            created_at: 1000,
            role: "user".to_string(),
            content: vec![],
            status: models::ResponseItemStatus::Completed,
            model: "test-model".to_string(),
            metadata: None,
        }];

        let result = ResponseServiceImpl::filter_to_ancestor_branch(
            items.clone(),
            &Some("resp_a".to_string()),
        );
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_filter_to_ancestor_branch_nonexistent_target() {
        // Test with a target that doesn't exist in the items
        let items = vec![models::ResponseOutputItem::Message {
            id: "msg_1".to_string(),
            response_id: "resp_a".to_string(),
            previous_response_id: None,
            next_response_ids: vec![],
            created_at: 1000,
            role: "user".to_string(),
            content: vec![],
            status: models::ResponseItemStatus::Completed,
            model: "test-model".to_string(),
            metadata: None,
        }];

        // Target "resp_z" doesn't exist - should return only items for "resp_z" (none)
        let result = ResponseServiceImpl::filter_to_ancestor_branch(
            items.clone(),
            &Some("resp_z".to_string()),
        );
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_utf8_truncation_does_not_panic() {
        // Regression test: truncation must handle multi-byte UTF-8 characters
        // Bug: byte index 500 falling inside Chinese character '文' (bytes 498..501)

        // Helper mimicking the fixed truncation logic
        fn truncate_safe(s: &str, max_chars: usize) -> String {
            let mut chars = s.chars();
            let truncated: String = chars.by_ref().take(max_chars).collect();
            if chars.next().is_some() {
                format!("{truncated}...")
            } else {
                truncated
            }
        }

        // Case 1: Exact reproduction of the bug - 498 ASCII + Chinese chars
        // '文' is UTF-8 bytes E6 96 87, so byte 500 = 0x87 (mid-character)
        let input = format!("{}文件内容", "a".repeat(498));
        assert_eq!(input.as_bytes()[500], 0x87); // Verify byte 500 is mid-char

        // Old code: &input[..500] would panic here
        let result = truncate_safe(&input, 500);
        assert!(result.ends_with("..."));
        assert_eq!(result.chars().count(), 503); // 500 chars + "..."

        // Case 2: All multi-byte characters
        let chinese = "中".repeat(200); // 600 bytes, 200 chars
        let result = truncate_safe(&chinese, 100);
        assert_eq!(result.chars().count(), 103);

        // Case 3: 4-byte emoji at boundary
        let emoji_input = format!("{}🎉", "x".repeat(499));
        let result = truncate_safe(&emoji_input, 500);
        assert_eq!(result, emoji_input); // Exactly 500 chars, no truncation

        // Case 4: Title truncation (57 chars) with Chinese (needs 60+ chars)
        let title = "中".repeat(70); // 70 Chinese chars, 210 bytes
        let result = truncate_safe(&title, 57);
        assert!(result.ends_with("..."));
        assert_eq!(result.chars().count(), 60); // 57 + "..."
    }
}
