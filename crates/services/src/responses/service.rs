use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;

use crate::completions::ports::CompletionServiceTrait;
use crate::conversations::ports::ConversationServiceTrait;
use crate::inference_provider_pool::InferenceProviderPool;
use crate::responses::tools;
use crate::responses::{errors, models, ports};

pub struct ResponseServiceImpl {
    pub response_repository: Arc<dyn ports::ResponseRepositoryTrait>,
    pub inference_provider_pool: Arc<InferenceProviderPool>,
    pub conversation_service: Arc<dyn ConversationServiceTrait>,
    pub completion_service: Arc<dyn CompletionServiceTrait>,
    pub web_search_provider: Option<Arc<dyn tools::WebSearchProviderTrait>>,
    pub file_search_provider: Option<Arc<dyn tools::FileSearchProviderTrait>>,
}

impl ResponseServiceImpl {
    pub fn new(
        response_repository: Arc<dyn ports::ResponseRepositoryTrait>,
        inference_provider_pool: Arc<InferenceProviderPool>,
        conversation_service: Arc<dyn ConversationServiceTrait>,
        completion_service: Arc<dyn CompletionServiceTrait>,
        web_search_provider: Option<Arc<dyn tools::WebSearchProviderTrait>>,
        file_search_provider: Option<Arc<dyn tools::FileSearchProviderTrait>>,
    ) -> Self {
        Self {
            response_repository,
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
    /// Process the response stream - main logic
    async fn process_response_stream(
        mut tx: futures::channel::mpsc::UnboundedSender<models::ResponseStreamEvent>,
        request: models::CreateResponseRequest,
        user_id: crate::UserId,
        api_key_id: String,
        organization_id: uuid::Uuid,
        workspace_id: uuid::Uuid,
        body_hash: String,
        _response_repository: Arc<dyn ports::ResponseRepositoryTrait>,
        completion_service: Arc<dyn CompletionServiceTrait>,
        _conversation_service: Arc<dyn ConversationServiceTrait>,
        web_search_provider: Option<Arc<dyn tools::WebSearchProviderTrait>>,
        file_search_provider: Option<Arc<dyn tools::FileSearchProviderTrait>>,
    ) -> Result<(), errors::ResponseError> {
        use crate::completions::ports::{CompletionMessage, CompletionRequest};
        use futures::SinkExt;
        use futures::StreamExt;

        tracing::info!("Starting response stream processing");

        let mut messages = Self::load_conversation_context(&request).await?;

        let response_id = uuid::Uuid::new_v4().simple();
        let response_id_str = format!("resp_{}", response_id);

        let initial_response = Self::create_initial_response_object(&request, &response_id_str);

        // Sequence number tracker
        let mut sequence_number: u64 = 0;

        // Event: response.created
        let created_event = models::ResponseStreamEvent {
            event_type: "response.created".to_string(),
            sequence_number: Some(sequence_number),
            response: Some(initial_response.clone()),
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
        };
        tx.send(created_event).await.map_err(|e| {
            errors::ResponseError::InternalError(format!("Failed to send event: {}", e))
        })?;
        sequence_number += 1;

        // Event: response.in_progress
        let in_progress_event = models::ResponseStreamEvent {
            event_type: "response.in_progress".to_string(),
            sequence_number: Some(sequence_number),
            response: Some(initial_response.clone()),
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
        };
        tx.send(in_progress_event).await.map_err(|e| {
            errors::ResponseError::InternalError(format!("Failed to send event: {}", e))
        })?;
        sequence_number += 1;

        let tools = Self::prepare_tools(&request);
        let tool_choice = Self::prepare_tool_choice(&request);

        let max_iterations = 10; // Prevent infinite loops
        let mut iteration = 0;
        let mut final_response_text = String::new();
        let mut output_item_index: usize = 0;

        loop {
            iteration += 1;
            if iteration > max_iterations {
                tracing::warn!("Max iterations reached in agent loop");
                break;
            }

            tracing::debug!("Agent loop iteration {}", iteration);

            // Call completion service
            // Note: tools and tool_choice will be added to the CompletionRequest
            // via the inference provider, which converts them properly
            let mut extra = std::collections::HashMap::new();

            // Pass tools as JSON in extra for now
            // TODO: Update CompletionRequest to have a tools field directly
            if !tools.is_empty() {
                extra.insert("tools".to_string(), serde_json::to_value(&tools).unwrap());
            }
            if let Some(tc) = &tool_choice {
                extra.insert("tool_choice".to_string(), serde_json::to_value(tc).unwrap());
            }

            let completion_request = CompletionRequest {
                model: request.model.clone(),
                messages: messages.clone(),
                max_tokens: request.max_output_tokens,
                temperature: request.temperature,
                top_p: request.top_p,
                stop: None,
                stream: Some(true),
                user_id: user_id.clone(),
                api_key_id: api_key_id.clone(),
                organization_id,
                workspace_id,
                metadata: request.metadata.clone(),
                body_hash: body_hash.clone(),
                n: None,
                extra,
            };

            let mut completion_stream = completion_service
                .create_chat_completion_stream(completion_request)
                .await
                .map_err(|e| {
                    errors::ResponseError::InternalError(format!("Completion error: {}", e))
                })?;

            // Process the stream
            let mut current_text = String::new();
            let mut tool_calls_detected = Vec::new();

            // Accumulate streaming tool calls by index
            let mut tool_call_accumulator: std::collections::HashMap<
                i64,
                (Option<String>, String),
            > = std::collections::HashMap::new();

            // Track whether we've emitted the message item and content part events
            let mut message_item_emitted = false;
            let message_item_id = format!("msg_{}", uuid::Uuid::new_v4().simple());

            while let Some(event) = completion_stream.next().await {
                match event {
                    Ok(sse_event) => {
                        // Parse the SSE event for content and tool calls
                        if let Some(delta_text) = Self::extract_text_delta(&sse_event) {
                            // First time we receive text, emit the item.added and content_part.added events
                            if !message_item_emitted {
                                // Event: response.output_item.added (for message)
                                let item_added_event = models::ResponseStreamEvent {
                                    event_type: "response.output_item.added".to_string(),
                                    sequence_number: Some(sequence_number),
                                    response: None,
                                    output_index: Some(output_item_index),
                                    content_index: None,
                                    item: Some(models::ResponseOutputItem::Message {
                                        id: message_item_id.clone(),
                                        status: models::ResponseItemStatus::InProgress,
                                        role: "assistant".to_string(),
                                        content: vec![],
                                    }),
                                    item_id: Some(message_item_id.clone()),
                                    part: None,
                                    delta: None,
                                    text: None,
                                    logprobs: None,
                                    obfuscation: None,
                                    annotation_index: None,
                                    annotation: None,
                                };
                                let _ = tx.send(item_added_event).await;
                                sequence_number += 1;

                                // Event: response.content_part.added
                                let content_part_added_event = models::ResponseStreamEvent {
                                    event_type: "response.content_part.added".to_string(),
                                    sequence_number: Some(sequence_number),
                                    response: None,
                                    output_index: Some(output_item_index),
                                    content_index: Some(0),
                                    item: None,
                                    item_id: Some(message_item_id.clone()),
                                    part: Some(models::ResponseOutputContent::OutputText {
                                        text: String::new(),
                                        annotations: vec![],
                                    }),
                                    delta: None,
                                    text: None,
                                    logprobs: None,
                                    obfuscation: None,
                                    annotation_index: None,
                                    annotation: None,
                                };
                                let _ = tx.send(content_part_added_event).await;
                                sequence_number += 1;

                                message_item_emitted = true;
                            }

                            current_text.push_str(&delta_text);

                            // Emit delta event
                            let delta_event = models::ResponseStreamEvent {
                                event_type: "response.output_text.delta".to_string(),
                                sequence_number: Some(sequence_number),
                                response: None,
                                output_index: Some(output_item_index),
                                content_index: Some(0),
                                item: None,
                                item_id: Some(message_item_id.clone()),
                                part: None,
                                delta: Some(delta_text.clone()),
                                text: None,
                                logprobs: Some(vec![]), // Empty array for now
                                obfuscation: None,
                                annotation_index: None,
                                annotation: None,
                            };
                            let _ = tx.send(delta_event).await;
                            sequence_number += 1;
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
                // Event: response.output_text.done
                let text_done_event = models::ResponseStreamEvent {
                    event_type: "response.output_text.done".to_string(),
                    sequence_number: Some(sequence_number),
                    response: None,
                    output_index: Some(output_item_index),
                    content_index: Some(0),
                    item: None,
                    item_id: Some(message_item_id.clone()),
                    part: None,
                    delta: None,
                    text: Some(current_text.clone()),
                    logprobs: Some(vec![]),
                    obfuscation: None,
                    annotation_index: None,
                    annotation: None,
                };
                let _ = tx.send(text_done_event).await;
                sequence_number += 1;

                // Event: response.content_part.done
                let content_part_done_event = models::ResponseStreamEvent {
                    event_type: "response.content_part.done".to_string(),
                    sequence_number: Some(sequence_number),
                    response: None,
                    output_index: Some(output_item_index),
                    content_index: Some(0),
                    item: None,
                    item_id: Some(message_item_id.clone()),
                    part: Some(models::ResponseOutputContent::OutputText {
                        text: current_text.clone(),
                        annotations: vec![],
                    }),
                    delta: None,
                    text: None,
                    logprobs: None,
                    obfuscation: None,
                    annotation_index: None,
                    annotation: None,
                };
                let _ = tx.send(content_part_done_event).await;
                sequence_number += 1;

                // Event: response.output_item.done
                let item_done_event = models::ResponseStreamEvent {
                    event_type: "response.output_item.done".to_string(),
                    sequence_number: Some(sequence_number),
                    response: None,
                    output_index: Some(output_item_index),
                    content_index: None,
                    item: Some(models::ResponseOutputItem::Message {
                        id: message_item_id.clone(),
                        status: models::ResponseItemStatus::Completed,
                        role: "assistant".to_string(),
                        content: vec![models::ResponseOutputContent::OutputText {
                            text: current_text.clone(),
                            annotations: vec![],
                        }],
                    }),
                    item_id: Some(message_item_id.clone()),
                    part: None,
                    delta: None,
                    text: None,
                    logprobs: None,
                    obfuscation: None,
                    annotation_index: None,
                    annotation: None,
                };
                let _ = tx.send(item_done_event).await;
                sequence_number += 1;

                output_item_index += 1;
            }

            final_response_text.push_str(&current_text);

            if !current_text.is_empty() {
                messages.push(CompletionMessage {
                    role: "assistant".to_string(),
                    content: current_text.clone(),
                });
            }

            // Convert accumulated tool calls to detected tool calls
            for (idx, (name_opt, args_str)) in tool_call_accumulator {
                if let Some(name) = name_opt {
                    // Handle tools that don't require parameters (like current_date)
                    if name == "current_date" {
                        tracing::debug!("Tool call {} complete: {}", idx, name);
                        tool_calls_detected.push(ToolCallInfo {
                            tool_type: name,
                            query: String::new(), // No query needed
                        });
                    } else {
                        // Try to parse the complete arguments for tools that need parameters
                        if let Ok(args) = serde_json::from_str::<serde_json::Value>(&args_str) {
                            if let Some(query) = args.get("query").and_then(|v| v.as_str()) {
                                tracing::debug!(
                                    "Tool call {} complete: {} with query: {}",
                                    idx,
                                    name,
                                    query
                                );
                                tool_calls_detected.push(ToolCallInfo {
                                    tool_type: name,
                                    query: query.to_string(),
                                });
                            }
                        } else {
                            tracing::warn!(
                                "Failed to parse tool call {} arguments: {}",
                                idx,
                                args_str
                            );
                        }
                    }
                }
            }

            if tool_calls_detected.is_empty() {
                // No more tool calls, we're done
                tracing::debug!("No tool calls detected, ending agent loop");
                break;
            }

            tracing::debug!("Executing {} tool calls", tool_calls_detected.len());

            for tool_call in tool_calls_detected {
                let tool_call_id =
                    format!("{}_{}", tool_call.tool_type, uuid::Uuid::new_v4().simple());

                // Emit tool call events
                if tool_call.tool_type == "web_search" {
                    // Event: response.output_item.added (for web search)
                    let item_added_event = models::ResponseStreamEvent {
                        event_type: "response.output_item.added".to_string(),
                        sequence_number: Some(sequence_number),
                        response: None,
                        output_index: Some(output_item_index),
                        content_index: None,
                        item: Some(models::ResponseOutputItem::WebSearchCall {
                            id: tool_call_id.clone(),
                            status: models::ResponseItemStatus::InProgress,
                            action: models::WebSearchAction::Search {
                                query: tool_call.query.clone(),
                            },
                        }),
                        item_id: Some(tool_call_id.clone()),
                        part: None,
                        delta: None,
                        text: None,
                        logprobs: None,
                        obfuscation: None,
                        annotation_index: None,
                        annotation: None,
                    };
                    let _ = tx.send(item_added_event).await;
                    sequence_number += 1;

                    // Event: response.web_search_call.in_progress
                    let web_search_in_progress_event = models::ResponseStreamEvent {
                        event_type: "response.web_search_call.in_progress".to_string(),
                        sequence_number: Some(sequence_number),
                        response: None,
                        output_index: Some(output_item_index),
                        content_index: None,
                        item: None,
                        item_id: Some(tool_call_id.clone()),
                        part: None,
                        delta: None,
                        text: None,
                        logprobs: None,
                        obfuscation: None,
                        annotation_index: None,
                        annotation: None,
                    };
                    let _ = tx.send(web_search_in_progress_event).await;
                    sequence_number += 1;

                    // Event: response.web_search_call.searching
                    let web_search_searching_event = models::ResponseStreamEvent {
                        event_type: "response.web_search_call.searching".to_string(),
                        sequence_number: Some(sequence_number),
                        response: None,
                        output_index: Some(output_item_index),
                        content_index: None,
                        item: None,
                        item_id: Some(tool_call_id.clone()),
                        part: None,
                        delta: None,
                        text: None,
                        logprobs: None,
                        obfuscation: None,
                        annotation_index: None,
                        annotation: None,
                    };
                    let _ = tx.send(web_search_searching_event).await;
                    sequence_number += 1;
                }

                let tool_result = Self::execute_tool(
                    &tool_call,
                    &web_search_provider,
                    &file_search_provider,
                    &request,
                )
                .await?;

                // Emit completion events for web search
                if tool_call.tool_type == "web_search" {
                    // Event: response.web_search_call.completed
                    let web_search_completed_event = models::ResponseStreamEvent {
                        event_type: "response.web_search_call.completed".to_string(),
                        sequence_number: Some(sequence_number),
                        response: None,
                        output_index: Some(output_item_index),
                        content_index: None,
                        item: None,
                        item_id: Some(tool_call_id.clone()),
                        part: None,
                        delta: None,
                        text: None,
                        logprobs: None,
                        obfuscation: None,
                        annotation_index: None,
                        annotation: None,
                    };
                    let _ = tx.send(web_search_completed_event).await;
                    sequence_number += 1;

                    // Event: response.output_item.done (for web search)
                    let item_done_event = models::ResponseStreamEvent {
                        event_type: "response.output_item.done".to_string(),
                        sequence_number: Some(sequence_number),
                        response: None,
                        output_index: Some(output_item_index),
                        content_index: None,
                        item: Some(models::ResponseOutputItem::WebSearchCall {
                            id: tool_call_id.clone(),
                            status: models::ResponseItemStatus::Completed,
                            action: models::WebSearchAction::Search {
                                query: tool_call.query.clone(),
                            },
                        }),
                        item_id: Some(tool_call_id.clone()),
                        part: None,
                        delta: None,
                        text: None,
                        logprobs: None,
                        obfuscation: None,
                        annotation_index: None,
                        annotation: None,
                    };
                    let _ = tx.send(item_done_event).await;
                    sequence_number += 1;

                    output_item_index += 1;
                }

                // Add tool result to message history
                messages.push(CompletionMessage {
                    role: "tool".to_string(),
                    content: tool_result,
                });
            }
        }

        let mut final_response = initial_response;
        final_response.status = models::ResponseStatus::Completed;
        final_response.output = vec![models::ResponseOutputItem::Message {
            id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
            status: models::ResponseItemStatus::Completed,
            role: "assistant".to_string(),
            content: vec![models::ResponseOutputContent::OutputText {
                text: final_response_text,
                annotations: vec![],
            }],
        }];

        let completed_event = models::ResponseStreamEvent {
            event_type: "response.completed".to_string(),
            sequence_number: Some(sequence_number),
            response: Some(final_response),
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
        };
        tx.send(completed_event).await.map_err(|e| {
            errors::ResponseError::InternalError(format!("Failed to send event: {}", e))
        })?;

        tracing::info!("Response stream completed successfully");
        Ok(())
    }

    /// Load conversation context based on conversation_id or previous_response_id
    async fn load_conversation_context(
        request: &models::CreateResponseRequest,
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

        // TODO: Load from conversation_id if present
        // if let Some(conversation_ref) = &request.conversation {
        //     // Load conversation history
        // }

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

    /// Create initial response object
    fn create_initial_response_object(
        request: &models::CreateResponseRequest,
        response_id: &str,
    ) -> models::ResponseObject {
        models::ResponseObject {
            id: response_id.to_string(),
            object: "response".to_string(),
            created_at: chrono::Utc::now().timestamp(),
            status: models::ResponseStatus::InProgress,
            error: None,
            incomplete_details: None,
            instructions: request.instructions.clone(),
            max_output_tokens: request.max_output_tokens,
            max_tool_calls: request.max_tool_calls,
            model: request.model.clone(),
            output: vec![],
            parallel_tool_calls: request.parallel_tool_calls.unwrap_or(false),
            previous_response_id: request.previous_response_id.clone(),
            reasoning: None,
            store: request.store.unwrap_or(false),
            temperature: request.temperature.unwrap_or(0.7),
            text: request.text.clone(),
            tool_choice: models::ResponseToolChoiceOutput::Auto("auto".to_string()),
            tools: request.tools.clone().unwrap_or_default(),
            top_p: request.top_p.unwrap_or(1.0),
            truncation: "stop".to_string(),
            usage: models::Usage::new(0, 0),
            user: None,
            metadata: request.metadata.clone(),
        }
    }

    /// Prepare tools configuration for LLM in OpenAI function calling format
    fn prepare_tools(
        request: &models::CreateResponseRequest,
    ) -> Vec<inference_providers::ToolDefinition> {
        let mut tool_definitions = Vec::new();

        if let Some(tools) = &request.tools {
            for tool in tools {
                match tool {
                    models::ResponseTool::WebSearch {} => {
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
        tool_call: &ToolCallInfo,
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

/// Tool call information extracted from LLM response
#[derive(Debug, Clone)]
struct ToolCallInfo {
    tool_type: String,
    query: String,
}
