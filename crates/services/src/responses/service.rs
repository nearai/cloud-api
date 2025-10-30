use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;

use crate::completions::ports::CompletionServiceTrait;
use crate::conversations::ports::ConversationServiceTrait;
use crate::inference_provider_pool::InferenceProviderPool;
use crate::responses::{errors, models, ports, tool_providers};

pub struct ResponseServiceImpl {
    pub response_repository: Arc<dyn ports::ResponseRepositoryTrait>,
    pub inference_provider_pool: Arc<InferenceProviderPool>,
    pub conversation_service: Arc<dyn ConversationServiceTrait>,
    pub completion_service: Arc<dyn CompletionServiceTrait>,
    pub web_search_provider: Option<Arc<dyn tool_providers::WebSearchProviderTrait>>,
    pub file_search_provider: Option<Arc<dyn tool_providers::FileSearchProviderTrait>>,
}

impl ResponseServiceImpl {
    pub fn new(
        response_repository: Arc<dyn ports::ResponseRepositoryTrait>,
        inference_provider_pool: Arc<InferenceProviderPool>,
        conversation_service: Arc<dyn ConversationServiceTrait>,
        completion_service: Arc<dyn CompletionServiceTrait>,
        web_search_provider: Option<Arc<dyn tool_providers::WebSearchProviderTrait>>,
        file_search_provider: Option<Arc<dyn tool_providers::FileSearchProviderTrait>>,
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

        // Spawn the actual work in a background task
        tokio::spawn(async move {
            if let Err(e) = Self::process_response_stream(
                tx.clone(),
                request,
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
                    response: None,
                    output_index: None,
                    content_index: None,
                    item: None,
                    item_id: None,
                    part: None,
                    delta: None,
                    text: Some(e.to_string()),
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
        _response_repository: Arc<dyn ports::ResponseRepositoryTrait>,
        completion_service: Arc<dyn CompletionServiceTrait>,
        _conversation_service: Arc<dyn ConversationServiceTrait>,
        web_search_provider: Option<Arc<dyn tool_providers::WebSearchProviderTrait>>,
        file_search_provider: Option<Arc<dyn tool_providers::FileSearchProviderTrait>>,
    ) -> Result<(), errors::ResponseError> {
        use crate::completions::ports::{CompletionMessage, CompletionRequest};
        use futures::SinkExt;
        use futures::StreamExt;

        tracing::info!("Starting response stream processing");

        // Step 1: Load conversation context
        let mut messages = Self::load_conversation_context(&request).await?;

        // Step 2: Create initial response object (skeleton - not saving to DB yet)
        let response_id = uuid::Uuid::new_v4();
        let response_id_str = format!("resp_{}", response_id);

        let initial_response = Self::create_initial_response_object(&request, &response_id_str);

        // Emit response.created event
        let created_event = models::ResponseStreamEvent {
            event_type: "response.created".to_string(),
            response: Some(initial_response.clone()),
            output_index: None,
            content_index: None,
            item: None,
            item_id: None,
            part: None,
            delta: None,
            text: None,
        };
        tx.send(created_event).await.map_err(|e| {
            errors::ResponseError::InternalError(format!("Failed to send event: {}", e))
        })?;

        // Step 3: Prepare tools for LLM
        let tools = Self::prepare_tools(&request);
        let tool_choice = Self::prepare_tool_choice(&request);

        // Step 4: Agent loop - iteratively call LLM and execute tools
        let max_iterations = 10; // Prevent infinite loops
        let mut iteration = 0;
        let mut final_response_text = String::new();

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
                user_id: crate::UserId::from(uuid::Uuid::new_v4()), // TODO: Get from request
                api_key_id: "placeholder".to_string(),              // TODO: Get from request
                organization_id: uuid::Uuid::new_v4(),              // TODO: Get from request
                workspace_id: uuid::Uuid::new_v4(),                 // TODO: Get from request
                metadata: request.metadata.clone(),
                body_hash: "placeholder".to_string(), // TODO: Get from request
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

            while let Some(event) = completion_stream.next().await {
                match event {
                    Ok(sse_event) => {
                        // Parse the SSE event for content and tool calls
                        if let Some(delta_text) = Self::extract_text_delta(&sse_event) {
                            current_text.push_str(&delta_text);

                            // Emit delta event
                            let delta_event = models::ResponseStreamEvent {
                                event_type: "response.output_text.delta".to_string(),
                                response: None,
                                output_index: Some(0),
                                content_index: Some(0),
                                item: None,
                                item_id: None,
                                part: None,
                                delta: Some(delta_text.clone()),
                                text: None,
                            };
                            let _ = tx.send(delta_event).await;
                        }

                        // Check for tool calls in the event
                        if let Some(tool_call) = Self::extract_tool_call(&sse_event) {
                            tool_calls_detected.push(tool_call);
                        }
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

            final_response_text.push_str(&current_text);

            // Add assistant response to message history
            if !current_text.is_empty() {
                messages.push(CompletionMessage {
                    role: "assistant".to_string(),
                    content: current_text.clone(),
                });
            }

            // Step 5: Execute tools if any were called
            if tool_calls_detected.is_empty() {
                // No more tool calls, we're done
                tracing::debug!("No tool calls detected, ending agent loop");
                break;
            }

            tracing::debug!("Executing {} tool calls", tool_calls_detected.len());

            // Execute each tool and add results to messages
            for tool_call in tool_calls_detected {
                let tool_result = Self::execute_tool(
                    &tool_call,
                    &web_search_provider,
                    &file_search_provider,
                    &request,
                )
                .await?;

                // Add tool result to message history
                messages.push(CompletionMessage {
                    role: "tool".to_string(),
                    content: tool_result,
                });
            }
        }

        // Step 6: Finalize and emit completion event
        let mut final_response = initial_response;
        final_response.status = models::ResponseStatus::Completed;
        final_response.output = vec![models::ResponseOutputItem::Message {
            id: format!("msg_{}", uuid::Uuid::new_v4()),
            status: models::ResponseItemStatus::Completed,
            role: "assistant".to_string(),
            content: vec![models::ResponseOutputContent::OutputText {
                text: final_response_text,
                annotations: vec![],
            }],
        }];

        let completed_event = models::ResponseStreamEvent {
            event_type: "response.completed".to_string(),
            response: Some(final_response),
            output_index: None,
            content_index: None,
            item: None,
            item_id: None,
            part: None,
            delta: None,
            text: None,
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

    /// Extract tool call from SSE event
    fn extract_tool_call(event: &inference_providers::SSEEvent) -> Option<ToolCallInfo> {
        use inference_providers::StreamChunk;

        match &event.chunk {
            StreamChunk::Chat(chat_chunk) => {
                // Extract tool calls from choices
                for choice in &chat_chunk.choices {
                    if let Some(delta) = &choice.delta {
                        if let Some(tool_calls) = &delta.tool_calls {
                            for tool_call in tool_calls {
                                // Parse the tool call
                                let function_name = tool_call.function.name.clone();

                                // Parse arguments to extract the query parameter
                                if let Some(args_str) = &tool_call.function.arguments {
                                    if let Ok(args) =
                                        serde_json::from_str::<serde_json::Value>(args_str)
                                    {
                                        if let Some(query) =
                                            args.get("query").and_then(|v| v.as_str())
                                        {
                                            return Some(ToolCallInfo {
                                                tool_type: function_name,
                                                query: query.to_string(),
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// Execute a tool call
    async fn execute_tool(
        tool_call: &ToolCallInfo,
        web_search_provider: &Option<Arc<dyn tool_providers::WebSearchProviderTrait>>,
        file_search_provider: &Option<Arc<dyn tool_providers::FileSearchProviderTrait>>,
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

                    // Format results as text
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
                    Ok("Web search not available (no provider configured)".to_string())
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
            _ => Ok(format!("Unknown tool type: {}", tool_call.tool_type)),
        }
    }
}

/// Tool call information extracted from LLM response
#[derive(Debug, Clone)]
struct ToolCallInfo {
    tool_type: String,
    query: String,
}
