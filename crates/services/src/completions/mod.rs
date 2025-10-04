pub mod ports;

use crate::attestation::ports::AttestationService;
use crate::inference_provider_pool::InferenceProviderPool;
use crate::usage::{RecordUsageServiceRequest, UsageService};
use inference_providers::{
    ChatMessage, InferenceProvider, MessageRole, StreamChunk, StreamingResult,
};
use std::sync::Arc;
use uuid::Uuid;

// Create a new stream that intercepts messages, but passes the original ones through
use futures_util::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};

struct InterceptStream<S>
where
    S: Stream<Item = Result<StreamChunk, inference_providers::CompletionError>> + Unpin,
{
    inner: S,
    attestation_service: Arc<dyn AttestationService>,
    usage_service: Arc<dyn UsageService + Send + Sync>,
    organization_id: Uuid,
    workspace_id: Uuid,
    api_key_id: Uuid,
    model_id: String,
    request_type: String,
}

impl<S> Stream for InterceptStream<S>
where
    S: Stream<Item = Result<StreamChunk, inference_providers::CompletionError>> + Unpin,
{
    type Item = Result<StreamChunk, inference_providers::CompletionError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(ref chunk))) => {
                if let StreamChunk::Chat(ref chat_chunk) = chunk {
                    if let Some(usage) = &chat_chunk.usage {
                        // Record attestation
                        let attestation_service = self.attestation_service.clone();
                        let chat_chunk_clone = chat_chunk.clone();
                        tokio::spawn(async move {
                            attestation_service
                                .get_chat_signature(chat_chunk_clone.id.as_str())
                                .await
                                .unwrap();
                        });

                        // Record usage
                        let usage_service = self.usage_service.clone();
                        let organization_id = self.organization_id;
                        let workspace_id = self.workspace_id;
                        let api_key_id = self.api_key_id;
                        let model_id = self.model_id.clone();
                        let request_type = self.request_type.clone();
                        let input_tokens = usage.prompt_tokens;
                        let output_tokens = usage.completion_tokens;

                        tokio::spawn(async move {
                            if let Err(e) = usage_service
                                .record_usage(RecordUsageServiceRequest {
                                    organization_id,
                                    workspace_id,
                                    api_key_id,
                                    response_id: None,
                                    model_id,
                                    input_tokens,
                                    output_tokens,
                                    request_type,
                                })
                                .await
                            {
                                tracing::error!(
                                    "Failed to record usage in completion service: {}",
                                    e
                                );
                            } else {
                                tracing::debug!(
                                    "Recorded usage for org {}: {} input, {} output tokens (api_key: {})",
                                    organization_id,
                                    input_tokens,
                                    output_tokens,
                                    api_key_id
                                );
                            }
                        });
                    }
                }
                Poll::Ready(Some(Ok(chunk.clone())))
            }
            other => other,
        }
    }
}

pub struct CompletionServiceImpl {
    pub inference_provider_pool: Arc<InferenceProviderPool>,
    pub attestation_service: Arc<dyn AttestationService>,
    pub usage_service: Arc<dyn UsageService + Send + Sync>,
}

impl CompletionServiceImpl {
    pub fn new(
        inference_provider_pool: Arc<InferenceProviderPool>,
        attestation_service: Arc<dyn AttestationService>,
        usage_service: Arc<dyn UsageService + Send + Sync>,
    ) -> Self {
        Self {
            inference_provider_pool,
            attestation_service,
            usage_service,
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

    async fn handle_stream_with_context(
        &self,
        llm_stream: StreamingResult,
        organization_id: Uuid,
        workspace_id: Uuid,
        api_key_id: Uuid,
        model_id: String,
        request_type: &str,
    ) -> StreamingResult {
        let intercepted_stream = InterceptStream {
            inner: llm_stream,
            attestation_service: self.attestation_service.clone(),
            usage_service: self.usage_service.clone(),
            organization_id,
            workspace_id,
            api_key_id,
            model_id,
            request_type: request_type.to_string(),
        };
        Box::pin(intercepted_stream)
    }
}

#[async_trait::async_trait]
impl ports::CompletionService for CompletionServiceImpl {
    async fn create_completion_stream(
        &self,
        request: ports::CompletionRequest,
    ) -> Result<StreamingResult, ports::CompletionError> {
        // Extract context for usage tracking
        let organization_id = request.organization_id;
        let workspace_id = request.workspace_id;
        let api_key_id = uuid::Uuid::parse_str(&request.api_key_id).map_err(|e| {
            ports::CompletionError::InvalidParams(format!("Invalid API key ID: {}", e))
        })?;
        let model_id = request.model.clone();
        let is_streaming = request.stream.unwrap_or(false);

        let chat_messages = Self::prepare_chat_messages(&request.messages);

        let chat_params = inference_providers::ChatCompletionParams {
            model: request.model,
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

        // Determine request type
        let request_type = if is_streaming {
            "chat_completion_stream"
        } else {
            "chat_completion"
        };

        // Create the completion event stream with usage tracking
        let event_stream = self
            .handle_stream_with_context(
                llm_stream,
                organization_id,
                workspace_id,
                api_key_id,
                model_id,
                request_type,
            )
            .await;

        Ok(event_stream)
    }
}

pub use ports::*;
