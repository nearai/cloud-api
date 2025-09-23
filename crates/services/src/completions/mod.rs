pub mod ports;

use crate::inference_provider_pool::InferenceProviderPool;
use futures::Stream;
use inference_providers::{ChatMessage, InferenceProvider, MessageRole, StreamChunk};
use std::{pin::Pin, sync::Arc};
use uuid::Uuid;

pub struct CompletionServiceImpl {
    pub inference_provider_pool: Arc<InferenceProviderPool>,
}

impl CompletionServiceImpl {
    pub fn new(inference_provider_pool: Arc<InferenceProviderPool>) -> Self {
        Self {
            inference_provider_pool,
        }
    }

    /// Convert completion messages to chat messages for inference providers
    fn prepare_chat_messages(messages: &[ports::CompletionMessage]) -> Vec<ChatMessage> {
        messages
            .iter()
            .map(|msg| ChatMessage {
                role: match msg.role.as_str() {
                    "system" => MessageRole::System,
                    "assistant" => MessageRole::Assistant,
                    "tool" => MessageRole::Tool,
                    _ => MessageRole::User,
                },
                content: Some(msg.content.clone()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            })
            .collect()
    }

    /// Create the completion stream events from LLM stream
    fn create_event_stream(
        llm_stream: Pin<
            Box<
                dyn Stream<Item = Result<StreamChunk, inference_providers::models::CompletionError>>
                    + Send,
            >,
        >,
        completion_id: CompletionId,
    ) -> impl Stream<Item = ports::CompletionStreamEvent> + Send {
        use futures::stream::{self, StreamExt};

        // State to track accumulated content
        let accumulated_content = Arc::new(std::sync::Mutex::new(String::new()));

        // Initial events
        let initial_events = stream::iter(vec![
            Self::create_start_event(&completion_id),
            Self::create_progress_event(&completion_id),
        ]);

        // Transform LLM chunks to completion events
        let content_stream = llm_stream.filter_map(move |chunk_result| {
            let completion_id = completion_id.clone();
            let accumulated_content = accumulated_content.clone();

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
                                                return Some(Self::create_delta_event(
                                                    &completion_id,
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

                                    return Some(Self::create_completion_event(
                                        &completion_id,
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
                        Some(Self::create_error_event(&completion_id, &error_msg))
                    }
                }
            }
        });

        // Chain initial events with content stream
        initial_events.chain(content_stream)
    }

    fn create_start_event(completion_id: &CompletionId) -> ports::CompletionStreamEvent {
        ports::CompletionStreamEvent {
            event_name: "completion.started".to_string(),
            data: serde_json::json!({
                "completion_id": completion_id,
                "status": "started"
            }),
        }
    }

    fn create_progress_event(completion_id: &CompletionId) -> ports::CompletionStreamEvent {
        ports::CompletionStreamEvent {
            event_name: "completion.progress".to_string(),
            data: serde_json::json!({
                "completion_id": completion_id,
                "status": "in_progress"
            }),
        }
    }

    fn create_delta_event(
        completion_id: &CompletionId,
        delta: &str,
    ) -> ports::CompletionStreamEvent {
        ports::CompletionStreamEvent {
            event_name: "completion.delta".to_string(),
            data: serde_json::json!({
                "completion_id": completion_id,
                "delta": delta
            }),
        }
    }

    fn create_completion_event(
        completion_id: &CompletionId,
        content: &str,
        usage: &inference_providers::TokenUsage,
    ) -> ports::CompletionStreamEvent {
        ports::CompletionStreamEvent {
            event_name: "completion.completed".to_string(),
            data: serde_json::json!({
                "completion_id": completion_id,
                "status": "completed",
                "content": content,
                "usage": {
                    "prompt_tokens": usage.prompt_tokens,
                    "completion_tokens": usage.completion_tokens,
                    "total_tokens": usage.total_tokens
                }
            }),
        }
    }

    fn create_error_event(
        completion_id: &CompletionId,
        error: &str,
    ) -> ports::CompletionStreamEvent {
        ports::CompletionStreamEvent {
            event_name: "completion.error".to_string(),
            data: serde_json::json!({
                "completion_id": completion_id,
                "status": "failed",
                "error": error
            }),
        }
    }
}

#[async_trait::async_trait]
impl ports::CompletionService for CompletionServiceImpl {
    async fn create_completion_stream(
        &self,
        request: ports::CompletionRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = ports::CompletionStreamEvent> + Send>>,
        ports::CompletionError,
    > {
        // Generate a completion ID
        let completion_id = CompletionId::from(Uuid::new_v4());

        // Convert messages to chat format for LLM
        let chat_messages = Self::prepare_chat_messages(&request.messages);

        tracing::info!("Starting streaming completion for {}", completion_id);

        let chat_params = inference_providers::ChatCompletionParams {
            model: request.model.clone(),
            messages: chat_messages,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            top_p: request.top_p,
            stop: request.stop,
            stream: Some(true),
            tools: None,
            max_completion_tokens: None,
            n: Some(1),
            frequency_penalty: None,
            presence_penalty: None,
            logit_bias: None,
            logprobs: None,
            top_logprobs: None,
            user: Some(request.user_id.to_string()),
            response_format: None,
            seed: None,
            tool_choice: None,
            parallel_tool_calls: None,
            metadata: request.metadata,
            store: None,
            stream_options: None,
        };

        // Get the LLM stream
        let llm_stream = self
            .inference_provider_pool
            .chat_completion_stream(chat_params)
            .await
            .map_err(|e| {
                ports::CompletionError::ProviderError(format!("Failed to create LLM stream: {}", e))
            })?;

        // Create the completion event stream
        let event_stream = Self::create_event_stream(llm_stream, completion_id.into());

        Ok(Box::pin(event_stream))
    }
}

pub use ports::*;
