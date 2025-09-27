pub mod ports;

use crate::attestation::ports::AttestationService;
use crate::inference_provider_pool::InferenceProviderPool;
use inference_providers::{
    ChatMessage, InferenceProvider, MessageRole, StreamChunk, StreamingResult,
};
use std::sync::Arc;

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
                    if let Some(_usage) = &chat_chunk.usage {
                        let attestation_service = self.attestation_service.clone();
                        let chat_chunk_clone = chat_chunk.clone();
                        tokio::spawn(async move {
                            attestation_service
                                .get_chat_signature(chat_chunk_clone.id.as_str())
                                .await
                                .unwrap();
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
}

impl CompletionServiceImpl {
    pub fn new(
        inference_provider_pool: Arc<InferenceProviderPool>,
        attestation_service: Arc<dyn AttestationService>,
    ) -> Self {
        Self {
            inference_provider_pool,
            attestation_service,
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

    async fn handle_stream(&self, llm_stream: StreamingResult) -> StreamingResult {
        let intercepted_stream = InterceptStream {
            inner: llm_stream,
            attestation_service: self.attestation_service.clone(),
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
        let chat_messages = Self::prepare_chat_messages(&request.messages);

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
        let event_stream = self.handle_stream(llm_stream).await;

        Ok(event_stream)
    }
}

pub use ports::*;
