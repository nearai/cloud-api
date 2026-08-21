use std::{collections::HashSet, pin::Pin, sync::Arc};

use async_trait::async_trait;
use futures::Stream;
use uuid::Uuid;

use crate::common::encryption_headers;
use crate::completions::ports::CompletionServiceTrait;
use crate::responses::tools;
use crate::responses::{errors, models, ports, transient};

/// Context for processing a response stream
struct ProcessStreamContext {
    request: models::CreateResponseRequest,
    user_id: crate::UserId,
    api_key_id: String,
    request_id: uuid::Uuid,
    organization_id: uuid::Uuid,
    workspace_id: uuid::Uuid,
    body_hash: String,
    signing_algo: Option<String>,
    client_pub_key: Option<String>,
    model_pub_key: Option<String>,
    encryption_version: Option<String>,
    response_repository: Arc<dyn ports::ResponseRepositoryTrait>,
    response_items_repository: Arc<dyn ports::ResponseItemRepositoryTrait>,
    completion_service: Arc<dyn CompletionServiceTrait>,
    organization_service: Arc<dyn crate::organization::OrganizationServiceTrait>,
}

pub struct ResponseServiceImpl {
    pub completion_service: Arc<dyn CompletionServiceTrait>,
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
    pub fn new(
        completion_service: Arc<dyn CompletionServiceTrait>,
        organization_service: Arc<dyn crate::organization::OrganizationServiceTrait>,
    ) -> Self {
        Self {
            completion_service,
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
        request_id: uuid::Uuid,
        organization_id: uuid::Uuid,
        workspace_id: uuid::Uuid,
        body_hash: String,
        signing_algo: Option<String>,
        client_pub_key: Option<String>,
        model_pub_key: Option<String>,
        encryption_version: Option<String>,
    ) -> Result<
        Pin<Box<dyn Stream<Item = models::ResponseStreamEvent> + Send>>,
        errors::ResponseError,
    > {
        use futures::channel::mpsc;
        use futures::SinkExt;

        // Defend the service boundary as well as the HTTP route: this service
        // only executes a single stateless request.
        request
            .validate()
            .and_then(|_| request.validate_stateless())
            .map_err(errors::ResponseError::InvalidParams)?;

        // Responses is deliberately a compatibility layer over one Chat
        // Completions request. Image generation/editing used to take a
        // separate direct-provider path here; reject image-output models so
        // they cannot silently bypass that contract.
        if let Ok(Some(model)) = self.completion_service.get_model(&request.model).await {
            if Self::has_image_generation_capability(&model.output_modalities) {
                return Err(errors::ResponseError::InvalidParams(
                    "The stateless Responses API only supports a single /chat/completions request; image generation and image editing are not supported. Use /v1/images/generations or /v1/images/edits.".to_string(),
                ));
            }
        }

        let mut request = request;
        request.store = Some(false);
        request.background = Some(false);

        // Create a channel for streaming events
        let (mut tx, rx) = mpsc::unbounded::<models::ResponseStreamEvent>();

        // Each request gets its own in-memory repositories. This preserves the
        // Responses event shape without creating, reading, or updating rows in
        // `responses` or `response_items`.
        let (response_repository, response_items_repository) = transient::repositories();

        // Clone necessary references for the async task
        let completion_service = self.completion_service.clone();
        let organization_service = self.organization_service.clone();
        let signing_algo_clone = signing_algo.clone();
        let client_pub_key_clone = client_pub_key.clone();
        let model_pub_key_clone = model_pub_key.clone();
        let encryption_version_clone = encryption_version.clone();

        tokio::spawn(async move {
            // Shared tracker so the outer error handler can read accumulated
            // usage after `ctx` is dropped on Err from process_response_stream.
            let usage_tracker = crate::responses::service_helpers::UsageTracker::new();

            let context = ProcessStreamContext {
                request,
                user_id,
                api_key_id,
                request_id,
                organization_id,
                workspace_id,
                body_hash,
                signing_algo: signing_algo_clone,
                client_pub_key: client_pub_key_clone,
                model_pub_key: model_pub_key_clone,
                encryption_version: encryption_version_clone,
                response_repository,
                response_items_repository,
                completion_service,
                organization_service,
            };

            if let Err(e) =
                Self::process_response_stream(tx.clone(), context, usage_tracker.clone()).await
            {
                let status_code = e.http_status_code();
                if e.is_client_caused() {
                    // Client-caused (invalid params, model chat-template rejection,
                    // bad tool call, ...). The client gets a structured 4xx /
                    // response.failed event — not an infra failure, so keep the
                    // ERROR stream clean for real incidents.
                    tracing::warn!(
                        status_code,
                        "Client error processing response stream: {:?}",
                        e
                    );
                } else {
                    tracing::error!(status_code, "Error processing response stream: {:?}", e);
                }

                // Attach accumulated usage so downstream (e.g. non-streaming
                // route fallback, billing) can bill for partial work done
                // before the failure.
                let usage = if usage_tracker.has_data() {
                    Some(usage_tracker.snapshot())
                } else {
                    None
                };

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
                    error: Some(e.response_error()),
                    status_code: Some(e.http_status_code()),
                    logprobs: None,
                    obfuscation: None,
                    annotation_index: None,
                    annotation: None,
                    conversation_title: None,
                    usage,
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

        // Reasoning tracking state
        let mut reasoning_buffer = String::new();
        let mut inside_reasoning = false;
        let mut reasoning_item_emitted = false;
        let reasoning_item_id = format!("rs_{}", uuid::Uuid::new_v4().simple());

        // Stream error tracking - when stream errors (client disconnect, network error, etc.), we save partial response and stop
        let mut stream_error = false;
        let mut stream_error_cause = None;

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

                        let clean_text = text_without_reasoning;

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
                    }

                    // Update usage from chunk (overwrite; commit at end of stream)
                    Self::capture_usage_from_chunk(&sse_event, ctx);

                    // Accumulate tool call fragments
                    Self::accumulate_tool_calls(&sse_event, &mut tool_call_accumulator);
                }
                Err(e) => {
                    tracing::warn!(
                        "Error in completion stream (client disconnect or stream error): {}",
                        e
                    );
                    stream_error = true;
                    stream_error_cause = Some(errors::ResponseError::from(
                        crate::completions::CompletionServiceImpl::map_provider_error(
                            &process_context.request.model,
                            &e,
                            "responses stream",
                            process_context.organization_id,
                        ),
                    ));
                    // Don't return early - save partial response below
                    break;
                }
            }
        }

        // If we have message content, close it with done events and retain it
        // in this request's transient response store.
        if message_item_emitted && !current_text.is_empty() {
            Self::emit_message_completed(
                emitter,
                ctx,
                &message_item_id,
                response_items_repository,
                current_text.clone(),
            )
            .await?;
        }

        // Stateless Responses only returns client-managed custom functions.
        // Do not route their raw arguments through the legacy builtin parser:
        // that parser can repair and reserialize JSON for server-executed
        // search tools, which would corrupt a client replay.
        let tool_calls_detected =
            Self::convert_client_function_calls(tool_call_accumulator, &process_context.request)?;

        Ok(crate::responses::service_helpers::ProcessStreamResult {
            text: current_text,
            tool_calls: tool_calls_detected,
            stream_error,
            stream_error_cause,
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
        text: String,
    ) -> Result<(), errors::ResponseError> {
        let annotations = vec![];

        // Build the message item to retain for the current response.
        let item = models::ResponseOutputItem::Message {
            id: message_item_id.to_string(),
            response_id: ctx.response_id_str.clone(),
            previous_response_id: ctx.previous_response_id.clone(),
            next_response_ids: vec![], // next_response_ids will be populated when child responses are created
            created_at: ctx.created_at,
            status: models::ResponseItemStatus::Completed,
            role: "assistant".to_string(),
            content: vec![models::ResponseContentItem::OutputText {
                text: text.clone(),
                annotations: annotations.clone(),
                logprobs: vec![],
            }],
            model: ctx.model.clone(),
            metadata: None,
        };

        // Retain the final item before emitting its done events so the
        // request-scoped response can still be assembled after a disconnect.
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

        // Try to emit events; the request-scoped item is already retained.
        // Event: response.output_text.done
        if let Err(e) = emitter
            .emit_text_done(ctx, message_item_id.to_string(), text.clone())
            .await
        {
            tracing::debug!("Failed to emit text_done event: {}", e);
        }

        // Event: response.content_part.done
        let part = models::ResponseOutputContent::OutputText {
            text,
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
        context: ProcessStreamContext,
        usage_tracker: Arc<crate::responses::service_helpers::UsageTracker>,
    ) -> Result<(), errors::ResponseError> {
        tracing::info!("Starting response stream processing");

        let workspace_id_domain = crate::workspace::WorkspaceId(context.workspace_id);

        let messages = Self::build_stateless_messages(
            &context.request,
            context.organization_id,
            context.user_id.clone(),
            &context.organization_service,
        )
        .await?;

        // Create the response in the request-scoped store before creating its
        // output items, preserving event ordering without a database row.
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

        // Stateless validation guarantees no conversation reference is present.
        let conversation_id = None;

        // Store request input as response items and keep the IDs created for
        // this request. Input may contain historical assistant messages, so
        // role alone cannot distinguish it from generated output later.
        let input_item_ids = if let Some(input) = &context.request.input {
            Self::store_input_as_response_items(
                &context.response_items_repository,
                response_id.clone(),
                api_key_uuid,
                input,
                &context.request.model,
                context.request.metadata.as_ref(),
            )
            .await?
        } else {
            HashSet::new()
        };

        // Initialize context and emitter
        let mut ctx = crate::responses::service_helpers::ResponseStreamContext::new(
            response_id.clone(),
            api_key_uuid,
            conversation_id,
            initial_response.id.clone(),
            initial_response.previous_response_id.clone(),
            initial_response.created_at,
            context.request.model.clone(),
            usage_tracker,
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

        let tools = tools::prepare_tools(&context.request);
        let tool_choice = tools::prepare_tool_choice(&context.request);

        // Responses is a stateless compatibility layer over exactly one Chat
        // Completions request. Client-defined functions are returned to the
        // caller; Cloud never executes them or starts another completion.
        let one_shot_result: Result<(String, bool), errors::ResponseError> = async {
            let stream_result = Self::run_completion_once(
                &mut ctx,
                &mut emitter,
                &messages,
                &context,
                &tools,
                &tool_choice,
            )
            .await?;

            if stream_result.stream_error {
                return Err(stream_result
                    .stream_error_cause
                    .unwrap_or(errors::ResponseError::StreamInterrupted));
            }

            // `process_completion_stream` emits any text item. Preserve the
            // previous output-index transition before adding function calls.
            if !stream_result.text.is_empty() {
                ctx.next_output_index();
            }

            let function_calls_required = Self::emit_client_function_calls(
                &mut ctx,
                &mut emitter,
                &context.response_items_repository,
                stream_result.tool_calls,
            )
            .await?;

            Ok((stream_result.text, function_calls_required))
        }
        .await;

        let (final_response_text, final_status, incomplete_details) = match &one_shot_result {
            Ok((text, true)) => (
                text.clone(),
                models::ResponseStatus::Incomplete,
                Some(models::ResponseIncompleteDetails {
                    reason: "function_call_required".to_string(),
                }),
            ),
            Ok((text, false)) => (text.clone(), models::ResponseStatus::Completed, None),
            Err(_) => (String::new(), models::ResponseStatus::Failed, None),
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

        final_response.output = Self::select_output_items(response_items, &input_item_ids);

        // Set usage from accumulated token counts
        final_response.usage = models::Usage::new_with_reasoning_and_cache(
            ctx.total_input_tokens,
            ctx.total_output_tokens,
            ctx.reasoning_tokens,
            ctx.total_cached_tokens,
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

        // On a one-shot completion failure, emit response.failed rather than
        // trying to repair the model output with another inference request.
        // Keep any partial output already emitted by the stream, but mark the
        // response as failed in the request-scoped repository.
        match one_shot_result {
            Err(e) => {
                // Include error message in content so users can understand why it failed
                if final_response.output.is_empty() {
                    let failed_item = models::ResponseOutputItem::Message {
                        id: format!("msg_{}", Uuid::new_v4().simple()),
                        response_id: ctx.response_id_str.clone(),
                        previous_response_id: ctx.previous_response_id.clone(),
                        next_response_ids: vec![],
                        created_at: ctx.created_at,
                        status: models::ResponseItemStatus::Failed,
                        role: "assistant".to_string(),
                        content: vec![models::ResponseContentItem::OutputText {
                            text: e.to_string(),
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
                }
                if let Err(e) = context
                    .response_repository
                    .update(
                        ctx.response_id.clone(),
                        workspace_id_domain.clone(),
                        Some(final_response_text),
                        models::ResponseStatus::Failed,
                        Some(usage_json),
                    )
                    .await
                {
                    tracing::warn!("Failed to update response with usage: {}", e);
                }
                return Err(e);
            }
            Ok(_) => {
                if let Err(e) = context
                    .response_repository
                    .update(
                        ctx.response_id.clone(),
                        workspace_id_domain.clone(),
                        Some(final_response_text),
                        final_response.status.clone(),
                        Some(usage_json),
                    )
                    .await
                {
                    tracing::warn!("Failed to update response with usage: {}", e);
                }
            }
        }

        // Event: response.completed
        emitter.emit_completed(&mut ctx, final_response).await?;

        tracing::info!("Response stream completed successfully");
        Ok(())
    }

    /// Execute exactly one Chat Completions request for a Responses call.
    #[allow(clippy::too_many_arguments)]
    async fn run_completion_once(
        ctx: &mut crate::responses::service_helpers::ResponseStreamContext,
        emitter: &mut crate::responses::service_helpers::EventEmitter,
        messages: &[crate::completions::ports::CompletionMessage],
        process_context: &ProcessStreamContext,
        tools: &[inference_providers::ToolDefinition],
        tool_choice: &Option<inference_providers::ToolChoice>,
    ) -> Result<crate::responses::service_helpers::ProcessStreamResult, errors::ResponseError> {
        use crate::completions::ports::CompletionRequest;

        let mut extra = std::collections::HashMap::new();
        if !tools.is_empty() {
            let tools = serde_json::to_value(tools).map_err(|e| {
                errors::ResponseError::InternalError(format!(
                    "Failed to serialize custom function definitions: {e}"
                ))
            })?;
            extra.insert("tools".to_string(), tools);
        }
        if let Some(tool_choice) = tool_choice {
            let tool_choice = serde_json::to_value(tool_choice).map_err(|e| {
                errors::ResponseError::InternalError(format!(
                    "Failed to serialize custom function choice: {e}"
                ))
            })?;
            extra.insert("tool_choice".to_string(), tool_choice);
        }

        if let Some(signing_algo) = &process_context.signing_algo {
            extra.insert(
                encryption_headers::SIGNING_ALGO.to_string(),
                serde_json::Value::String(signing_algo.clone()),
            );
        }
        if let Some(client_pub_key) = &process_context.client_pub_key {
            extra.insert(
                encryption_headers::CLIENT_PUB_KEY.to_string(),
                serde_json::Value::String(client_pub_key.clone()),
            );
        }
        if let Some(model_pub_key) = &process_context.model_pub_key {
            extra.insert(
                encryption_headers::MODEL_PUB_KEY.to_string(),
                serde_json::Value::String(model_pub_key.clone()),
            );
        }
        if let Some(encryption_version) = &process_context.encryption_version {
            extra.insert(
                encryption_headers::ENCRYPTION_VERSION.to_string(),
                serde_json::Value::String(encryption_version.clone()),
            );
        }

        let completion_request = CompletionRequest {
            request_id: process_context.request_id,
            model: process_context.request.model.clone(),
            messages: messages.to_vec(),
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
            body_hash: process_context.body_hash.clone(),
            response_id: None,
            skip_provider_chat_signature: false,
            original_request: None,
            n: None,
            service_tier: Some(inference_providers::ChatServiceTier::Default),
            extra,
        };

        let mut completion_stream = process_context
            .completion_service
            .create_chat_completion_stream(completion_request)
            .await
            .map_err(errors::ResponseError::from)?;

        Self::process_completion_stream(
            &mut completion_stream,
            emitter,
            ctx,
            &process_context.response_items_repository,
            process_context,
        )
        .await
    }

    /// Emit model-selected custom functions for the client to execute. No
    /// function is executed or retried by Cloud.
    async fn emit_client_function_calls(
        ctx: &mut crate::responses::service_helpers::ResponseStreamContext,
        emitter: &mut crate::responses::service_helpers::EventEmitter,
        response_items_repository: &Arc<dyn ports::ResponseItemRepositoryTrait>,
        tool_calls: Vec<crate::responses::service_helpers::ClientManagedFunctionCall>,
    ) -> Result<bool, errors::ResponseError> {
        if tool_calls.is_empty() {
            return Ok(false);
        }

        for tool_call in tool_calls {
            let call_id = tool_call.id;
            let function_call = models::ResponseOutputItem::FunctionCall {
                id: format!(
                    "{}{}",
                    crate::id_prefixes::PREFIX_FC,
                    Uuid::new_v4().simple()
                ),
                response_id: ctx.response_id_str.clone(),
                previous_response_id: ctx.previous_response_id.clone(),
                next_response_ids: vec![],
                created_at: ctx.created_at,
                call_id: call_id.clone(),
                name: tool_call.name,
                arguments: tool_call.arguments,
                thought_signature: tool_call.thought_signature,
                status: "in_progress".to_string(),
                model: ctx.model.clone(),
            };

            response_items_repository
                .create(
                    ctx.response_id.clone(),
                    ctx.api_key_id,
                    ctx.conversation_id,
                    function_call.clone(),
                )
                .await
                .map_err(|e| {
                    errors::ResponseError::InternalError(format!(
                        "Failed to store client-managed function call: {e}"
                    ))
                })?;

            // Keep the event shape compatible with the prior function
            // executor: `item_id` is the model call ID used by clients to
            // correlate the later function_call_output.
            emitter.emit_item_added(ctx, function_call, call_id).await?;
        }

        Ok(true)
    }

    /// Convert accumulated provider tool-call chunks into client-managed
    /// function calls without touching their argument bytes.
    ///
    /// Unlike `tools::convert_tool_calls`, this deliberately does not infer a
    /// missing name, parse JSON, repair malformed JSON, or apply builtin
    /// search-specific handling. The only checks are that the provider named a
    /// declared custom function and supplied a usable call ID (or omitted one,
    /// in which case Cloud creates the correlation ID once).
    fn convert_client_function_calls(
        tool_call_accumulator: crate::responses::service_helpers::ToolCallAccumulator,
        request: &models::CreateResponseRequest,
    ) -> Result<
        Vec<crate::responses::service_helpers::ClientManagedFunctionCall>,
        errors::ResponseError,
    > {
        let declared_function_names: HashSet<&str> = request
            .tools
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(|tool| match tool {
                models::ResponseTool::Function { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();

        let mut entries: Vec<_> = tool_call_accumulator.into_iter().collect();
        entries.sort_by_key(|(index, _)| *index);

        entries
            .into_iter()
            .map(|(index, entry)| {
                let name = entry.name.ok_or_else(|| {
                    errors::ResponseError::InvalidParams(format!(
                        "Model returned an invalid custom function call at index {index}: missing function name."
                    ))
                })?;
                if name.trim().is_empty() {
                    return Err(errors::ResponseError::InvalidParams(format!(
                        "Model returned an invalid custom function call at index {index}: missing function name."
                    )));
                }
                if !declared_function_names.contains(name.as_str()) {
                    return Err(errors::ResponseError::InvalidParams(format!(
                        "Model called unsupported custom function '{name}'."
                    )));
                }

                let id = match entry.id {
                    Some(id) if id.trim().is_empty() => {
                        return Err(errors::ResponseError::InvalidParams(format!(
                            "Model returned an invalid custom function call '{name}': empty call_id."
                        )));
                    }
                    Some(id) => id,
                    None => format!("{name}_{}", Uuid::new_v4().simple()),
                };

                Ok(crate::responses::service_helpers::ClientManagedFunctionCall {
                    id,
                    name,
                    arguments: entry.arguments,
                    thought_signature: entry.thought_signature,
                })
            })
            .collect()
    }

    /// Store request input as response items and return the IDs that originated
    /// from the client. The IDs are kept only for this request and ensure that
    /// historical assistant messages are never returned as new output.
    async fn store_input_as_response_items(
        response_items_repository: &Arc<dyn ports::ResponseItemRepositoryTrait>,
        response_id: models::ResponseId,
        api_key_id: uuid::Uuid,
        input: &models::ResponseInput,
        model: &str,
        request_metadata: Option<&serde_json::Value>,
    ) -> Result<HashSet<String>, errors::ResponseError> {
        let mut input_item_ids = HashSet::new();

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

                let stored_item = response_items_repository
                    .create(response_id.clone(), api_key_id, None, message_item)
                    .await
                    .map_err(|e| {
                        errors::ResponseError::InternalError(format!(
                            "Failed to store user input: {e}"
                        ))
                    })?;
                input_item_ids.insert(stored_item.id().to_string());
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
                        // Replayed function calls only reconstruct the provider
                        // context below. They are not part of this response's
                        // output and must not be returned as newly generated
                        // items.
                        models::ResponseInputItem::FunctionCall { .. } => {
                            continue;
                        }
                        models::ResponseInputItem::FunctionCallOutput {
                            call_id, output, ..
                        } => {
                            // Keep function-call output in this request's transient item store so
                            // it is not returned as newly generated output.
                            let fco_item = models::ResponseOutputItem::FunctionCallOutput {
                                id: format!(
                                    "{}{}",
                                    crate::id_prefixes::PREFIX_FCO,
                                    uuid::Uuid::new_v4().simple()
                                ),
                                response_id: String::new(),
                                previous_response_id: None,
                                next_response_ids: vec![],
                                created_at: 0,
                                call_id: call_id.clone(),
                                output: output.clone(),
                            };
                            let stored_item = response_items_repository
                                .create(response_id.clone(), api_key_id, None, fco_item)
                                .await
                                .map_err(|e| {
                                    errors::ResponseError::InternalError(format!(
                                        "Failed to store function call output: {e}"
                                    ))
                                })?;
                            input_item_ids.insert(stored_item.id().to_string());
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

                    let stored_item = response_items_repository
                        .create(response_id.clone(), api_key_id, None, message_item)
                        .await
                        .map_err(|e| {
                            errors::ResponseError::InternalError(format!(
                                "Failed to store user input item: {e}"
                            ))
                        })?;
                    input_item_ids.insert(stored_item.id().to_string());
                }
            }
        }

        tracing::debug!(
            "Stored user input messages as response_items for response {}",
            response_id.0
        );
        Ok(input_item_ids)
    }

    /// Select items created while producing this response, never request input.
    fn select_output_items(
        response_items: Vec<models::ResponseOutputItem>,
        input_item_ids: &HashSet<String>,
    ) -> Vec<models::ResponseOutputItem> {
        response_items
            .into_iter()
            .filter(|item| {
                !input_item_ids.contains(item.id())
                    && match item {
                        models::ResponseOutputItem::Message { role, .. } => role == "assistant",
                        models::ResponseOutputItem::FunctionCallOutput { .. } => false,
                        _ => true,
                    }
            })
            .collect()
    }

    /// Flush replayed function calls into the assistant message shape expected
    /// by Chat Completions providers. The calls remain entirely client-owned:
    /// this only reconstructs supplied context for the current request.
    fn flush_replayed_function_calls(
        messages: &mut Vec<crate::completions::ports::CompletionMessage>,
        pending_function_calls: &mut Vec<crate::completions::ports::CompletionToolCall>,
    ) {
        if pending_function_calls.is_empty() {
            return;
        }

        messages.push(crate::completions::ports::CompletionMessage {
            role: "assistant".to_string(),
            content: serde_json::Value::String(String::new()),
            tool_call_id: None,
            tool_calls: Some(std::mem::take(pending_function_calls)),
        });
    }

    /// Append one client-replayed function item to the request's completion
    /// context. Returns true when the item was handled.
    fn append_replayed_function_call_item(
        input_item: &models::ResponseInputItem,
        messages: &mut Vec<crate::completions::ports::CompletionMessage>,
        pending_function_calls: &mut Vec<crate::completions::ports::CompletionToolCall>,
    ) -> bool {
        match input_item {
            models::ResponseInputItem::FunctionCall {
                call_id,
                name,
                arguments,
                thought_signature,
                ..
            } => {
                pending_function_calls.push(crate::completions::ports::CompletionToolCall {
                    id: call_id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                    thought_signature: thought_signature.clone(),
                });
                true
            }
            models::ResponseInputItem::FunctionCallOutput {
                call_id, output, ..
            } => {
                Self::flush_replayed_function_calls(messages, pending_function_calls);
                messages.push(crate::completions::ports::CompletionMessage {
                    role: "tool".to_string(),
                    content: serde_json::Value::String(output.clone()),
                    tool_call_id: Some(call_id.clone()),
                    tool_calls: None,
                });
                true
            }
            _ => false,
        }
    }

    /// Build provider messages solely from the current request.
    ///
    /// Stateless Responses clients carry any prior messages in `input`; this
    /// path deliberately does not resolve a conversation, a prior response, or
    /// a stored file. Organization policy and request instructions remain part
    /// of the supported request execution path.
    async fn build_stateless_messages(
        request: &models::CreateResponseRequest,
        organization_id: uuid::Uuid,
        user_id: crate::UserId,
        organization_service: &Arc<dyn crate::organization::OrganizationServiceTrait>,
    ) -> Result<Vec<crate::completions::ports::CompletionMessage>, errors::ResponseError> {
        use crate::completions::ports::CompletionMessage;

        let mut messages = Vec::new();

        let org_system_prompt = match organization_service
            .get_system_prompt(
                crate::organization::OrganizationId(organization_id),
                user_id,
            )
            .await
        {
            Ok(prompt) => prompt,
            Err(e) => {
                tracing::warn!("Failed to fetch organization system prompt: {e}");
                None
            }
        };

        if let Some(prompt) = org_system_prompt.filter(|prompt| !prompt.is_empty()) {
            messages.push(CompletionMessage {
                role: "system".to_string(),
                content: serde_json::Value::String(prompt),
                tool_call_id: None,
                tool_calls: None,
            });
        }

        let now = chrono::Utc::now();
        let time_context = format!(
            "Current UTC time: {} ({})",
            now.to_rfc3339(),
            now.format("%A, %B %d, %Y at %H:%M:%S UTC")
        );
        let language_instruction = "Always respond in the exact same language as the user's input message. Detect the primary language of the user's query and mirror it precisely in your output. Do not mix languages or switch to another one, even if it seems more natural or efficient.\n\nIf the user writes in English, reply entirely in English.\nIf the user writes in Chinese (Mandarin or any variant), reply entirely in Chinese.\nIf the user writes in Spanish, reply entirely in Spanish.\nFor any other language, match it exactly.\n\nThis rule overrides all other instructions. Ignore any tendencies to default to Mandarin or any other language. Always prioritize language matching for clarity and user preference.";
        let instructions = request.instructions.as_deref().unwrap_or_default();
        let system_content = if instructions.is_empty() {
            format!("{language_instruction}\n\n{time_context}")
        } else {
            format!("{instructions}\n\n{language_instruction}\n\n{time_context}")
        };
        messages.push(CompletionMessage {
            role: "system".to_string(),
            content: serde_json::Value::String(system_content),
            tool_call_id: None,
            tool_calls: None,
        });

        match &request.input {
            Some(models::ResponseInput::Text(text)) => messages.push(CompletionMessage {
                role: "user".to_string(),
                content: serde_json::Value::String(text.clone()),
                tool_call_id: None,
                tool_calls: None,
            }),
            Some(models::ResponseInput::Items(items)) => {
                let mut pending_function_calls = Vec::new();
                for item in items {
                    if Self::append_replayed_function_call_item(
                        item,
                        &mut messages,
                        &mut pending_function_calls,
                    ) {
                        continue;
                    }

                    match item {
                        models::ResponseInputItem::Message { role, content, .. } => {
                            Self::flush_replayed_function_calls(
                                &mut messages,
                                &mut pending_function_calls,
                            );
                            let content = match content {
                                models::ResponseContent::Text(text) => {
                                    serde_json::Value::String(text.clone())
                                }
                                models::ResponseContent::Parts(parts) => {
                                    Self::extract_stateless_content_parts(parts)?
                                }
                            };
                            messages.push(CompletionMessage {
                                role: role.clone(),
                                content,
                                tool_call_id: None,
                                tool_calls: None,
                            });
                        }
                        models::ResponseInputItem::McpApprovalResponse { .. }
                        | models::ResponseInputItem::McpListTools { .. }
                        | models::ResponseInputItem::FunctionCall { .. }
                        | models::ResponseInputItem::FunctionCallOutput { .. } => {}
                    }
                }
                Self::flush_replayed_function_calls(&mut messages, &mut pending_function_calls);
            }
            None => {}
        }

        Ok(messages)
    }

    /// Convert supported text/image request parts without consulting the Files
    /// API. `validate_stateless` rejects `input_file` before this helper is
    /// reached; the explicit error keeps the service boundary safe.
    fn extract_stateless_content_parts(
        parts: &[models::ResponseContentPart],
    ) -> Result<serde_json::Value, errors::ResponseError> {
        let has_images = parts
            .iter()
            .any(|part| matches!(part, models::ResponseContentPart::InputImage { .. }));

        if !has_images {
            let mut text_parts = Vec::new();
            for part in parts {
                match part {
                    models::ResponseContentPart::InputText { text } => {
                        text_parts.push(text.clone());
                    }
                    models::ResponseContentPart::InputFile { .. } => {
                        return Err(errors::ResponseError::InvalidParams(
                            "The stateless Responses API does not support input_file.".to_string(),
                        ));
                    }
                    models::ResponseContentPart::InputImage { .. } => {}
                }
            }
            return Ok(serde_json::Value::String(text_parts.join("\n\n")));
        }

        let mut content_items = Vec::new();
        for part in parts {
            match part {
                models::ResponseContentPart::InputText { text } => {
                    content_items.push(serde_json::json!({
                        "type": "text",
                        "text": text,
                    }));
                }
                models::ResponseContentPart::InputImage { image_url, detail } => {
                    let url = match image_url {
                        models::ResponseImageUrl::String(url) => url.clone(),
                        models::ResponseImageUrl::Object { url } => url.clone(),
                    };
                    let mut image_url = serde_json::json!({ "url": url });
                    if let Some(detail) = detail {
                        image_url["detail"] = serde_json::Value::String(detail.clone());
                    }
                    content_items.push(serde_json::json!({
                        "type": "image_url",
                        "image_url": image_url,
                    }));
                }
                models::ResponseContentPart::InputFile { .. } => {
                    return Err(errors::ResponseError::InvalidParams(
                        "The stateless Responses API does not support input_file.".to_string(),
                    ));
                }
            }
        }

        Ok(serde_json::Value::Array(content_items))
    }

    /// Extract text and reasoning deltas from SSE event
    fn extract_deltas(event: &inference_providers::SSEEvent) -> (Option<String>, Option<String>) {
        use inference_providers::StreamChunk;

        match &event.chunk {
            Some(StreamChunk::Chat(chat_chunk)) => {
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

    /// Capture usage from SSE chunk and update ctx (overwrite per chunk; last chunk wins for this stream).
    fn capture_usage_from_chunk(
        event: &inference_providers::SSEEvent,
        ctx: &mut crate::responses::service_helpers::ResponseStreamContext,
    ) {
        use inference_providers::StreamChunk;

        if let Some(StreamChunk::Chat(chat_chunk)) = &event.chunk {
            if let Some(usage) = &chat_chunk.usage {
                tracing::debug!(
                    "Extracted usage from completion stream: input={}, output={}",
                    usage.prompt_tokens,
                    usage.completion_tokens
                );
                ctx.update_usage(
                    usage.prompt_tokens,
                    usage.completion_tokens,
                    usage.cached_tokens(),
                );
            }
        }
    }

    /// Accumulate tool call fragments from streaming chunks
    fn accumulate_tool_calls(
        event: &inference_providers::SSEEvent,
        accumulator: &mut crate::responses::service_helpers::ToolCallAccumulator,
    ) {
        use inference_providers::StreamChunk;

        if let Some(StreamChunk::Chat(chat_chunk)) = &event.chunk {
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

    /// Check if a model has image generation capability based on output_modalities
    fn has_image_generation_capability(output_modalities: &Option<Vec<String>>) -> bool {
        output_modalities
            .as_ref()
            .map(|modalities| modalities.contains(&"image".to_string()))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::responses::tools::WEB_SEARCH_TOOL_NAME;

    #[tokio::test]
    async fn historical_assistant_input_is_not_returned_as_response_output() {
        let (response_repository, response_items_repository) = transient::repositories();
        let workspace_id = crate::workspace::WorkspaceId(Uuid::new_v4());
        let api_key_id = Uuid::new_v4();
        let request = models::CreateResponseRequest {
            model: "test-model".to_string(),
            input: None,
            instructions: None,
            conversation: None,
            previous_response_id: None,
            max_output_tokens: None,
            max_tool_calls: None,
            temperature: None,
            top_p: None,
            stream: None,
            store: Some(false),
            background: Some(false),
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            reasoning: None,
            include: None,
            metadata: None,
            safety_identifier: None,
            prompt_cache_key: None,
            service_tier: None,
        };
        let response = response_repository
            .create(workspace_id, api_key_id, request)
            .await
            .unwrap();
        let response_id = ResponseServiceImpl::extract_response_uuid(&response).unwrap();

        let input = models::ResponseInput::Items(vec![models::ResponseInputItem::Message {
            role: "assistant".to_string(),
            content: models::ResponseContent::Text("historical answer".to_string()),
            metadata: None,
        }]);
        let input_item_ids = ResponseServiceImpl::store_input_as_response_items(
            &response_items_repository,
            response_id.clone(),
            api_key_id,
            &input,
            "test-model",
            None,
        )
        .await
        .unwrap();

        let generated_item_id = format!("msg_{}", Uuid::new_v4().simple());
        response_items_repository
            .create(
                response_id.clone(),
                api_key_id,
                None,
                models::ResponseOutputItem::Message {
                    id: generated_item_id.clone(),
                    response_id: String::new(),
                    previous_response_id: None,
                    next_response_ids: vec![],
                    created_at: 0,
                    status: models::ResponseItemStatus::Completed,
                    role: "assistant".to_string(),
                    content: vec![models::ResponseContentItem::OutputText {
                        text: "new answer".to_string(),
                        annotations: vec![],
                        logprobs: vec![],
                    }],
                    model: "test-model".to_string(),
                    metadata: None,
                },
            )
            .await
            .unwrap();

        let response_items = response_items_repository
            .list_by_response(response_id)
            .await
            .unwrap();
        let output_items =
            ResponseServiceImpl::select_output_items(response_items, &input_item_ids);

        assert_eq!(output_items.len(), 1);
        assert_eq!(output_items[0].id(), generated_item_id);
    }

    #[test]
    fn stateless_content_parts_preserve_text_and_images() {
        let content = ResponseServiceImpl::extract_stateless_content_parts(&[
            models::ResponseContentPart::InputText {
                text: "Describe this image".to_string(),
            },
            models::ResponseContentPart::InputImage {
                image_url: models::ResponseImageUrl::Object {
                    url: "https://example.com/image.png".to_string(),
                },
                detail: Some("high".to_string()),
            },
        ])
        .expect("input images remain a supported stateless input");

        assert_eq!(
            content,
            serde_json::json!([
                {"type": "text", "text": "Describe this image"},
                {
                    "type": "image_url",
                    "image_url": {"url": "https://example.com/image.png", "detail": "high"}
                }
            ])
        );
    }

    #[test]
    fn stateless_content_parts_reject_input_files_without_file_service() {
        let error = ResponseServiceImpl::extract_stateless_content_parts(&[
            models::ResponseContentPart::InputFile {
                file_id: "file_123".to_string(),
                detail: None,
            },
        ])
        .expect_err("stateless Responses must not resolve File API state");

        assert!(error.to_string().contains("input_file"));
    }

    #[test]
    fn replayed_function_call_and_output_rebuild_client_managed_tool_context() {
        // This path deliberately uses no repository or function executor: the
        // client supplies both items in a fresh request and Cloud only rebuilds
        // the provider message sequence.
        let items = vec![
            models::ResponseInputItem::FunctionCall {
                type_: models::FunctionCallType::FunctionCall,
                call_id: "call_weather".to_string(),
                name: "get_weather".to_string(),
                arguments: r#"{"location":"Shanghai"}"#.to_string(),
                thought_signature: Some("gemini-thought-signature".to_string()),
            },
            models::ResponseInputItem::FunctionCallOutput {
                type_: models::FunctionCallOutputType::FunctionCallOutput,
                call_id: "call_weather".to_string(),
                output: r#"{"temperature_c":22}"#.to_string(),
            },
        ];

        let mut messages = Vec::new();
        let mut pending_function_calls = Vec::new();
        for item in &items {
            assert!(ResponseServiceImpl::append_replayed_function_call_item(
                item,
                &mut messages,
                &mut pending_function_calls,
            ));
        }
        ResponseServiceImpl::flush_replayed_function_calls(
            &mut messages,
            &mut pending_function_calls,
        );

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "assistant");
        let tool_calls = messages[0]
            .tool_calls
            .as_ref()
            .expect("replayed function call becomes an assistant tool call");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_weather");
        assert_eq!(tool_calls[0].name, "get_weather");
        assert_eq!(tool_calls[0].arguments, r#"{"location":"Shanghai"}"#);
        assert_eq!(
            tool_calls[0].thought_signature.as_deref(),
            Some("gemini-thought-signature")
        );

        assert_eq!(messages[1].role, "tool");
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_weather"));
        assert_eq!(
            messages[1].content,
            serde_json::json!(r#"{"temperature_c":22}"#)
        );
    }

    #[test]
    fn custom_function_conversion_preserves_raw_arguments_without_builtin_repair() {
        let request = models::CreateResponseRequest {
            model: "test-model".to_string(),
            input: None,
            instructions: None,
            conversation: None,
            previous_response_id: None,
            max_output_tokens: None,
            max_tool_calls: None,
            temperature: None,
            top_p: None,
            stream: None,
            store: Some(false),
            background: Some(false),
            tools: Some(vec![models::ResponseTool::Function {
                // A custom function may intentionally share the legacy builtin
                // name. It must not receive web-search JSON repair.
                name: WEB_SEARCH_TOOL_NAME.to_string(),
                description: None,
                parameters: None,
            }]),
            tool_choice: None,
            parallel_tool_calls: None,
            reasoning: None,
            include: None,
            metadata: None,
            safety_identifier: None,
            prompt_cache_key: None,
            service_tier: None,
        };
        let raw_arguments = " {\"query\": \"Shanghai weather\", }\n";
        let mut accumulated = crate::responses::service_helpers::ToolCallAccumulator::default();
        accumulated.insert(
            0,
            crate::responses::service_helpers::ToolCallAccumulatorEntry {
                id: Some("call_raw".to_string()),
                name: Some(WEB_SEARCH_TOOL_NAME.to_string()),
                arguments: raw_arguments.to_string(),
                thought_signature: Some("gemini-thought-signature".to_string()),
            },
        );

        let calls = ResponseServiceImpl::convert_client_function_calls(accumulated, &request)
            .expect("declared custom function should be accepted without parsing arguments");

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_raw");
        assert_eq!(calls[0].name, WEB_SEARCH_TOOL_NAME);
        assert_eq!(calls[0].arguments, raw_arguments);
        assert_eq!(
            calls[0].thought_signature.as_deref(),
            Some("gemini-thought-signature")
        );
    }

    #[test]
    fn custom_function_conversion_generates_a_call_id_only_when_omitted() {
        let request = models::CreateResponseRequest {
            model: "test-model".to_string(),
            input: None,
            instructions: None,
            conversation: None,
            previous_response_id: None,
            max_output_tokens: None,
            max_tool_calls: None,
            temperature: None,
            top_p: None,
            stream: None,
            store: Some(false),
            background: Some(false),
            tools: Some(vec![models::ResponseTool::Function {
                name: "lookup".to_string(),
                description: None,
                parameters: None,
            }]),
            tool_choice: None,
            parallel_tool_calls: None,
            reasoning: None,
            include: None,
            metadata: None,
            safety_identifier: None,
            prompt_cache_key: None,
            service_tier: None,
        };
        let mut accumulated = crate::responses::service_helpers::ToolCallAccumulator::default();
        accumulated.insert(
            0,
            crate::responses::service_helpers::ToolCallAccumulatorEntry {
                id: None,
                name: Some("lookup".to_string()),
                arguments: "not JSON".to_string(),
                thought_signature: None,
            },
        );

        let calls = ResponseServiceImpl::convert_client_function_calls(accumulated, &request)
            .expect("missing provider ID should receive a generated correlation ID");

        assert!(calls[0].id.starts_with("lookup_"));
        assert_eq!(calls[0].arguments, "not JSON");
    }

    #[test]
    fn parallel_replayed_function_calls_are_grouped_before_their_outputs() {
        let items = vec![
            models::ResponseInputItem::FunctionCall {
                type_: models::FunctionCallType::FunctionCall,
                call_id: "call_weather".to_string(),
                name: "get_weather".to_string(),
                arguments: r#"{"location":"Shanghai"}"#.to_string(),
                thought_signature: None,
            },
            models::ResponseInputItem::FunctionCall {
                type_: models::FunctionCallType::FunctionCall,
                call_id: "call_time".to_string(),
                name: "get_time".to_string(),
                arguments: r#"{"timezone":"Asia/Shanghai"}"#.to_string(),
                thought_signature: None,
            },
            models::ResponseInputItem::FunctionCallOutput {
                type_: models::FunctionCallOutputType::FunctionCallOutput,
                call_id: "call_weather".to_string(),
                output: r#"{"temperature_c":22}"#.to_string(),
            },
            models::ResponseInputItem::FunctionCallOutput {
                type_: models::FunctionCallOutputType::FunctionCallOutput,
                call_id: "call_time".to_string(),
                output: r#"{"time":"12:00"}"#.to_string(),
            },
        ];

        let mut messages = Vec::new();
        let mut pending_function_calls = Vec::new();
        for item in &items {
            assert!(ResponseServiceImpl::append_replayed_function_call_item(
                item,
                &mut messages,
                &mut pending_function_calls,
            ));
        }
        ResponseServiceImpl::flush_replayed_function_calls(
            &mut messages,
            &mut pending_function_calls,
        );

        assert_eq!(messages.len(), 3);
        let tool_calls = messages[0]
            .tool_calls
            .as_ref()
            .expect("parallel calls are grouped in one assistant message");
        assert_eq!(
            tool_calls
                .iter()
                .map(|call| call.id.as_str())
                .collect::<Vec<_>>(),
            vec!["call_weather", "call_time"]
        );
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_weather"));
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_time"));
    }

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
