pub mod ports;

use crate::attestation::ports::AttestationServiceTrait;
use crate::inference_provider_pool::InferenceProviderPool;
use crate::models::ModelsRepository;
use crate::usage::{RecordUsageServiceRequest, UsageServiceTrait};
use inference_providers::{ChatMessage, MessageRole, SSEEvent, StreamChunk, StreamingResult};
use std::sync::Arc;
use uuid::Uuid;

// Create a new stream that intercepts messages, but passes the original ones through
use futures_util::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};

struct InterceptStream<S>
where
    S: Stream<Item = Result<SSEEvent, inference_providers::CompletionError>> + Unpin,
{
    inner: S,
    attestation_service: Arc<dyn AttestationServiceTrait>,
    usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
    organization_id: Uuid,
    workspace_id: Uuid,
    api_key_id: Uuid,
    model_id: Uuid,
    request_type: String,
}

impl<S> Stream for InterceptStream<S>
where
    S: Stream<Item = Result<SSEEvent, inference_providers::CompletionError>> + Unpin,
{
    type Item = Result<SSEEvent, inference_providers::CompletionError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(ref event))) => {
                if let StreamChunk::Chat(ref chat_chunk) = event.chunk {
                    if let Some(usage) = &chat_chunk.usage {
                        // Store attestation signature when completion finishes
                        let attestation_service = self.attestation_service.clone();
                        let chat_id = chat_chunk.id.clone();
                        tokio::spawn(async move {
                            if let Err(e) = attestation_service
                                .store_chat_signature_from_provider(chat_id.as_str())
                                .await
                            {
                                tracing::error!("Failed to store chat signature: {:?}", e);
                            } else {
                                tracing::debug!("Stored signature for chat_id: {}", chat_id);
                            }
                        });

                        // Record usage
                        let usage_service = self.usage_service.clone();
                        let organization_id = self.organization_id;
                        let workspace_id = self.workspace_id;
                        let api_key_id = self.api_key_id;
                        let model_id = self.model_id;
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
                Poll::Ready(Some(Ok(event.clone())))
            }
            other => other,
        }
    }
}

pub struct CompletionServiceImpl {
    pub inference_provider_pool: Arc<InferenceProviderPool>,
    pub attestation_service: Arc<dyn AttestationServiceTrait>,
    pub usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
    pub models_repository: Arc<dyn ModelsRepository>,
}

impl CompletionServiceImpl {
    pub fn new(
        inference_provider_pool: Arc<InferenceProviderPool>,
        attestation_service: Arc<dyn AttestationServiceTrait>,
        usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
        models_repository: Arc<dyn ModelsRepository>,
    ) -> Self {
        Self {
            inference_provider_pool,
            attestation_service,
            usage_service,
            models_repository,
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
        model_id: Uuid,
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
impl ports::CompletionServiceTrait for CompletionServiceImpl {
    async fn create_chat_completion_stream(
        &self,
        request: ports::CompletionRequest,
    ) -> Result<StreamingResult, ports::CompletionError> {
        // Extract context for usage tracking
        let organization_id = request.organization_id;
        let workspace_id = request.workspace_id;
        let api_key_id = uuid::Uuid::parse_str(&request.api_key_id).map_err(|e| {
            ports::CompletionError::InvalidParams(format!("Invalid API key ID: {e}"))
        })?;
        let is_streaming = request.stream.unwrap_or(false);

        let chat_messages = Self::prepare_chat_messages(&request.messages);

        let mut chat_params = inference_providers::ChatCompletionParams {
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
            extra: request.extra.clone(),
        };

        // Resolve model name (could be an alias) and get model details in a single DB call
        // This also validates that the model exists and is active
        let model = self
            .models_repository
            .resolve_and_get_model(&request.model)
            .await
            .map_err(|e| {
                ports::CompletionError::InternalError(format!("Failed to resolve model: {e}"))
            })?
            .ok_or_else(|| {
                ports::CompletionError::InvalidModel(format!(
                    "Model '{}' not found. It's not a valid model name or alias.",
                    request.model
                ))
            })?;

        let canonical_name = &model.model_name;

        // Update params with canonical name if it's different
        if canonical_name != &request.model {
            tracing::debug!(
                requested_model = %request.model,
                canonical_model = %canonical_name,
                "Resolved alias to canonical model name"
            );
            chat_params.model = canonical_name.clone();
        }

        // Get the LLM stream
        let llm_stream = self
            .inference_provider_pool
            .chat_completion_stream(chat_params, request.body_hash.clone())
            .await
            .map_err(|e| {
                // Check if this is a client error (HTTP 4xx) from the provider
                let error_str = e.to_string();
                if error_str.contains("HTTP 4") || error_str.contains("Bad Request") {
                    // For client errors (4xx), return detailed message to help user fix their request
                    ports::CompletionError::InvalidParams(format!(
                        "Invalid request parameters: {e}"
                    ))
                } else {
                    // For server errors (5xx), log details but return generic message to user
                    tracing::error!(
                        model = %request.model,
                        error = %e,
                        "Provider error during chat completion stream"
                    );
                    ports::CompletionError::ProviderError(
                        "The model is currently unavailable. Please try again later.".to_string(),
                    )
                }
            })?;

        // Determine request type
        let request_type = if is_streaming {
            "chat_completion_stream"
        } else {
            "chat_completion"
        };

        // Create the completion event stream with usage tracking
        // Use model UUID for usage tracking
        let event_stream = self
            .handle_stream_with_context(
                llm_stream,
                organization_id,
                workspace_id,
                api_key_id,
                model.id,
                request_type,
            )
            .await;

        Ok(event_stream)
    }

    async fn create_chat_completion(
        &self,
        request: ports::CompletionRequest,
    ) -> Result<inference_providers::ChatCompletionResponseWithBytes, ports::CompletionError> {
        let chat_messages = Self::prepare_chat_messages(&request.messages);

        let mut chat_params = inference_providers::ChatCompletionParams {
            model: request.model.clone(),
            messages: chat_messages,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            top_p: request.top_p,
            stop: request.stop,
            stream: Some(false),
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
            extra: request.extra.clone(),
        };

        // Resolve model name (could be an alias) and get model details in a single DB call
        // This also validates that the model exists and is active
        let model = self
            .models_repository
            .resolve_and_get_model(&request.model)
            .await
            .map_err(|e| {
                ports::CompletionError::InternalError(format!("Failed to resolve model: {e}"))
            })?
            .ok_or_else(|| {
                ports::CompletionError::InvalidModel(format!(
                    "Model '{}' not found. It's not a valid model name or alias.",
                    request.model
                ))
            })?;

        let canonical_name = &model.model_name;

        // Update params with canonical name if it's different
        if canonical_name != &request.model {
            tracing::debug!(
                requested_model = %request.model,
                canonical_model = %canonical_name,
                "Resolved alias to canonical model name"
            );
            chat_params.model = canonical_name.clone();
        }

        let response_with_bytes = self
            .inference_provider_pool
            .chat_completion(chat_params, request.body_hash.clone())
            .await
            .map_err(|e| {
                // Check if this is a client error (HTTP 4xx) from the provider
                let error_str = e.to_string();
                if error_str.contains("HTTP 4") || error_str.contains("Bad Request") {
                    // For client errors (4xx), return detailed message to help user fix their request
                    ports::CompletionError::InvalidParams(format!(
                        "Invalid request parameters: {e}"
                    ))
                } else {
                    // For server errors (5xx), log details but return generic message to user
                    tracing::error!(
                        model = %request.model,
                        error = %e,
                        "Provider error during chat completion"
                    );
                    ports::CompletionError::ProviderError(
                        "The model is currently unavailable. Please try again later.".to_string(),
                    )
                }
            })?;

        // Store attestation signature
        let attestation_service = self.attestation_service.clone();
        let chat_id = response_with_bytes.response.id.clone();
        tokio::spawn(async move {
            if let Err(e) = attestation_service
                .store_chat_signature_from_provider(chat_id.as_str())
                .await
            {
                tracing::error!("Failed to store chat signature: {:?}", e);
            } else {
                tracing::debug!("Stored signature for chat_id: {}", chat_id);
            }
        });

        // Record usage with model UUID
        let usage_service = self.usage_service.clone();
        let organization_id = request.organization_id;
        let workspace_id = request.workspace_id;
        let api_key_id = uuid::Uuid::parse_str(&request.api_key_id).map_err(|e| {
            ports::CompletionError::InvalidParams(format!("Invalid API key ID: {e}"))
        })?;
        let model_id = model.id;
        let input_tokens = response_with_bytes.response.usage.prompt_tokens;
        let output_tokens = response_with_bytes.response.usage.completion_tokens;

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
                    request_type: "chat_completion".to_string(),
                })
                .await
            {
                tracing::error!("Failed to record usage in completion service: {}", e);
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

        Ok(response_with_bytes)
    }
}

pub use ports::*;
