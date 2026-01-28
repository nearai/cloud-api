//! Realtime service for voice-to-voice conversations
//!
//! This module implements the realtime API for bidirectional audio streaming,
//! handling the STT -> LLM -> TTS pipeline.

pub mod ports;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use futures::stream::{self, StreamExt};
use inference_providers::{AudioTranscriptionParams, StreamChunk};
use ports::{
    ContentPart, ConversationItem, ConversationMessage, RealtimeError, RealtimeServiceTrait,
    RealtimeSession, ResponseInfo, ServerEvent, ServerEventStream, SessionConfig,
    TranscriptionResult, WorkspaceContext,
};
use std::sync::Arc;
use uuid::Uuid;

use crate::audio::ports::AudioServiceTrait;
use crate::completions::ports::CompletionServiceTrait;
use crate::inference_provider_pool::InferenceProviderPool;
use crate::models::ports::ModelsServiceTrait;
use crate::usage::{ports::UsageServiceTrait, RecordUsageServiceRequest, StopReason};

/// Maximum size for audio buffer in bytes (10MB)
const MAX_AUDIO_BUFFER_SIZE: usize = 10 * 1024 * 1024;

/// Maximum size for base64-encoded audio chunks before decoding (14MB)
const MAX_BASE64_CHUNK_SIZE: usize = 14 * 1024 * 1024;

/// Parameters for recording usage
struct UsageParams {
    organization_id: Uuid,
    workspace_id: Uuid,
    api_key_id: Uuid,
    model_id: Uuid,
    inference_type: String,
    input_tokens: i32,
    output_tokens: i32,
}

/// Realtime service implementation
pub struct RealtimeServiceImpl {
    inference_pool: Arc<InferenceProviderPool>,
    completion_service: Arc<dyn CompletionServiceTrait>,
    audio_service: Arc<dyn AudioServiceTrait>,
    usage_service: Arc<dyn UsageServiceTrait>,
    models_service: Arc<dyn ModelsServiceTrait>,
}

impl RealtimeServiceImpl {
    /// Create a new realtime service
    pub fn new(
        inference_pool: Arc<InferenceProviderPool>,
        completion_service: Arc<dyn CompletionServiceTrait>,
        audio_service: Arc<dyn AudioServiceTrait>,
        usage_service: Arc<dyn UsageServiceTrait>,
        models_service: Arc<dyn ModelsServiceTrait>,
    ) -> Self {
        Self {
            inference_pool,
            completion_service,
            audio_service,
            usage_service,
            models_service,
        }
    }

    /// Generate a unique ID for items
    fn generate_id(prefix: &str) -> String {
        let uuid_str = Uuid::new_v4().to_string();
        let short_id = uuid_str.replace("-", "");
        format!("{}_{}", prefix, &short_id[..24])
    }

    /// Convert audio format codec name to file extension
    /// Maps codec names like "pcm16", "mp3", "wav" to proper file extensions
    fn audio_format_to_extension(format: &str) -> &'static str {
        match format.to_lowercase().as_str() {
            "pcm16" | "pcm" | "raw" => "wav", // PCM16 is typically sent as WAV
            "mp3" | "mpeg" => "mp3",
            "wav" | "wave" => "wav",
            "ogg" | "opus" => "ogg",
            "flac" => "flac",
            "m4a" | "aac" => "m4a",
            "webm" => "webm",
            _ => "wav", // Default to WAV for unknown formats
        }
    }

    /// Record usage for a realtime operation
    async fn record_usage(&self, params: UsageParams) {
        let usage_request = RecordUsageServiceRequest {
            organization_id: params.organization_id,
            workspace_id: params.workspace_id,
            api_key_id: params.api_key_id,
            model_id: params.model_id,
            input_tokens: params.input_tokens,
            output_tokens: params.output_tokens,
            inference_type: params.inference_type.clone(),
            ttft_ms: None,
            avg_itl_ms: None,
            inference_id: None,
            provider_request_id: None,
            stop_reason: Some(StopReason::Completed),
            response_id: None,
            image_count: None,
        };

        if let Err(e) = self.usage_service.record_usage(usage_request).await {
            tracing::error!(
                error = %e,
                organization_id = %params.organization_id,
                workspace_id = %params.workspace_id,
                inference_type = %params.inference_type,
                "Failed to record realtime usage"
            );
        }
    }

    /// Resolve a model name to its UUID
    async fn resolve_model_id(&self, model_name: &str) -> Result<Uuid, RealtimeError> {
        self.models_service
            .resolve_and_get_model(model_name)
            .await
            .map_err(|e| RealtimeError::InternalError(format!("Failed to resolve model: {}", e)))
            .map(|model| model.id)
    }
}

#[async_trait]
impl RealtimeServiceTrait for RealtimeServiceImpl {
    async fn create_session(
        &self,
        config: SessionConfig,
        _ctx: &WorkspaceContext,
    ) -> Result<RealtimeSession, RealtimeError> {
        let session_id = Self::generate_id("sess");

        tracing::info!(
            session_id = %session_id,
            stt_model = %config.stt_model,
            llm_model = %config.llm_model,
            tts_model = %config.tts_model,
            "Created realtime session"
        );

        Ok(RealtimeSession {
            session_id,
            conversation_id: None,
            config,
            audio_buffer: Vec::new(),
            context: Vec::new(),
        })
    }

    async fn handle_audio_chunk(
        &self,
        session: &mut RealtimeSession,
        audio_base64: &str,
    ) -> Result<(), RealtimeError> {
        // Validate base64 size before decoding
        if audio_base64.len() > MAX_BASE64_CHUNK_SIZE {
            return Err(RealtimeError::InvalidAudioData(
                "Audio chunk exceeds size limit (max 14MB base64)".to_string(),
            ));
        }

        // Decode base64 audio and append to buffer
        let audio_bytes = BASE64.decode(audio_base64).map_err(|_| {
            RealtimeError::InvalidAudioData("Invalid audio data format".to_string())
        })?;

        // Check if adding this chunk would exceed buffer limit
        if session.audio_buffer.len() + audio_bytes.len() > MAX_AUDIO_BUFFER_SIZE {
            return Err(RealtimeError::InvalidAudioData(format!(
                "Audio buffer size limit exceeded (max {} bytes)",
                MAX_AUDIO_BUFFER_SIZE
            )));
        }

        session.audio_buffer.extend(audio_bytes);

        tracing::debug!(
            session_id = %session.session_id,
            buffer_size = session.audio_buffer.len(),
            "Appended audio to buffer"
        );

        Ok(())
    }

    async fn commit_audio_buffer(
        &self,
        session: &mut RealtimeSession,
        ctx: &WorkspaceContext,
    ) -> Result<TranscriptionResult, RealtimeError> {
        if session.audio_buffer.is_empty() {
            return Err(RealtimeError::InvalidAudioData(
                "Audio buffer is empty".to_string(),
            ));
        }

        let item_id = Self::generate_id("item");
        let audio_data = std::mem::take(&mut session.audio_buffer);

        tracing::debug!(
            session_id = %session.session_id,
            item_id = %item_id,
            audio_size = audio_data.len(),
            "Committing audio buffer for transcription"
        );

        // Resolve STT model ID for billing
        let model_id = self.resolve_model_id(&session.config.stt_model).await?;

        // Call transcription service
        let file_extension = Self::audio_format_to_extension(&session.config.input_audio_format);
        let params = AudioTranscriptionParams {
            model: session.config.stt_model.clone(),
            audio_data,
            filename: format!("audio_{}.{}", item_id, file_extension),
            language: None,
            prompt: None,
            response_format: Some("json".to_string()),
            temperature: None,
            timestamp_granularities: None,
            sample_rate_hertz: None,
        };

        let request_hash = format!("realtime_{}", Uuid::new_v4());

        let response = self
            .inference_pool
            .audio_transcription(params, request_hash)
            .await
            .map_err(|e| RealtimeError::TranscriptionFailed(e.to_string()))?;

        let transcript = response.response.text.clone();

        // Record STT usage
        let audio_seconds_scaled = response
            .audio_duration_seconds
            .map(|d| (d * 1000.0) as i32)
            .unwrap_or(0);

        self.record_usage(UsageParams {
            organization_id: ctx.organization_id,
            workspace_id: ctx.workspace_id,
            api_key_id: ctx.api_key_id,
            model_id,
            inference_type: "audio_transcription".to_string(),
            input_tokens: audio_seconds_scaled,
            output_tokens: 0,
        })
        .await;

        // Add to conversation context
        session.context.push(ConversationMessage {
            role: "user".to_string(),
            content: transcript.clone(),
        });

        tracing::info!(
            session_id = %session.session_id,
            item_id = %item_id,
            "Transcription completed"
        );

        Ok(TranscriptionResult {
            item_id,
            text: transcript,
        })
    }

    async fn generate_response(
        &self,
        session: &mut RealtimeSession,
        ctx: &WorkspaceContext,
    ) -> Result<ServerEventStream, RealtimeError> {
        let response_id = Self::generate_id("resp");
        let item_id = Self::generate_id("item");

        tracing::debug!(
            session_id = %session.session_id,
            response_id = %response_id,
            "Generating response"
        );

        // Resolve model IDs for billing (fail if not found)
        let llm_model_id = self.resolve_model_id(&session.config.llm_model).await?;
        let tts_model_id = self.resolve_model_id(&session.config.tts_model).await?;

        // Convert realtime conversation context to completion messages
        let completion_messages: Vec<crate::completions::ports::CompletionMessage> = session
            .context
            .iter()
            .map(|msg| crate::completions::ports::CompletionMessage {
                role: msg.role.clone(),
                content: msg.content.clone(),
            })
            .collect();

        // Add system instructions if present
        let mut messages = Vec::new();
        if let Some(ref instructions) = session.config.instructions {
            messages.push(crate::completions::ports::CompletionMessage {
                role: "system".to_string(),
                content: instructions.clone(),
            });
        }
        messages.extend(completion_messages);

        // Build completion request through the service layer
        let completion_request = crate::completions::ports::CompletionRequest {
            model: session.config.llm_model.clone(),
            messages,
            max_tokens: None,
            temperature: Some(session.config.temperature),
            top_p: None,
            stop: None,
            stream: Some(true), // Enable streaming
            n: None,
            user_id: crate::UserId(ctx.user_id),
            api_key_id: ctx.api_key_id.to_string(),
            organization_id: ctx.organization_id,
            workspace_id: ctx.workspace_id,
            metadata: None,
            store: None,
            body_hash: format!("realtime_llm_{}", Uuid::new_v4()),
            response_id: None,
            extra: std::collections::HashMap::new(),
        };

        // Stream LLM response and collect events
        let mut events = vec![
            ServerEvent::ResponseCreated {
                response: ResponseInfo {
                    id: response_id.clone(),
                    status: "in_progress".to_string(),
                    output: None,
                },
            },
            ServerEvent::ResponseOutputItemAdded {
                item: ConversationItem {
                    id: item_id.clone(),
                    item_type: "message".to_string(),
                    role: Some("assistant".to_string()),
                    content: None,
                },
            },
        ];

        // Use completion service for streaming LLM response
        let mut llm_stream = self
            .completion_service
            .create_chat_completion_stream(completion_request)
            .await
            .map_err(|e| RealtimeError::LlmError(e.to_string()))?;

        let mut complete_text = String::new();
        let mut llm_input_tokens = 0i32;
        let mut llm_output_tokens = 0i32;

        while let Some(event_result) = llm_stream.next().await {
            match event_result {
                Ok(sse_event) => {
                    // Parse the SSE event for text content
                    match &sse_event.chunk {
                        StreamChunk::Chat(chat_chunk) => {
                            // Track token usage from completion response
                            if let Some(usage) = &chat_chunk.usage {
                                llm_input_tokens = usage.prompt_tokens;
                                llm_output_tokens = usage.completion_tokens;
                            }

                            for choice in &chat_chunk.choices {
                                if let Some(delta) = &choice.delta {
                                    if let Some(content) = &delta.content {
                                        if !content.is_empty() {
                                            complete_text.push_str(content);
                                            // Emit text delta event for real-time text delivery
                                            events.push(ServerEvent::ResponseTextDelta {
                                                item_id: item_id.clone(),
                                                delta: content.clone(),
                                            });
                                        }
                                    }
                                }
                            }
                        }
                        _ => {
                            // Ignore non-chat chunks
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(
                        session_id = %session.session_id,
                        error = %e,
                        "Error streaming LLM response"
                    );
                    return Err(RealtimeError::LlmError(e.to_string()));
                }
            }
        }

        // Record LLM usage
        self.record_usage(UsageParams {
            organization_id: ctx.organization_id,
            workspace_id: ctx.workspace_id,
            api_key_id: ctx.api_key_id,
            model_id: llm_model_id,
            inference_type: "chat_completion".to_string(),
            input_tokens: llm_input_tokens,
            output_tokens: llm_output_tokens,
        })
        .await;

        // Add complete text event
        events.push(ServerEvent::ResponseTextDone {
            item_id: item_id.clone(),
            text: complete_text.clone(),
        });

        // Add assistant response to session context
        session.context.push(ConversationMessage {
            role: "assistant".to_string(),
            content: complete_text.clone(),
        });

        // Use audio service for TTS synthesis
        let speech_request = crate::audio::ports::SpeechRequest {
            model: session.config.tts_model.clone(),
            input: complete_text.clone(),
            voice: session.config.voice.clone(),
            response_format: Some(session.config.output_audio_format.clone()),
            speed: None,
            organization_id: ctx.organization_id,
            workspace_id: ctx.workspace_id,
            api_key_id: ctx.api_key_id,
            model_id: tts_model_id,
            request_hash: format!("realtime_tts_{}", Uuid::new_v4()),
        };

        let tts_response = self
            .audio_service
            .synthesize(speech_request)
            .await
            .map_err(|e| RealtimeError::TtsError(e.to_string()))?;

        // Record TTS usage based on character count
        let character_count = complete_text.chars().count() as i32;
        self.record_usage(UsageParams {
            organization_id: ctx.organization_id,
            workspace_id: ctx.workspace_id,
            api_key_id: ctx.api_key_id,
            model_id: tts_model_id,
            inference_type: "audio_speech".to_string(),
            input_tokens: 0,
            output_tokens: character_count,
        })
        .await;

        let audio_base64 = BASE64.encode(&tts_response.audio_data);

        // Add audio events
        events.push(ServerEvent::ResponseAudioDelta {
            item_id: item_id.clone(),
            delta: audio_base64,
        });

        events.push(ServerEvent::ResponseAudioDone {
            item_id: item_id.clone(),
        });

        events.push(ServerEvent::ResponseOutputItemDone {
            item: ConversationItem {
                id: item_id.clone(),
                item_type: "message".to_string(),
                role: Some("assistant".to_string()),
                content: Some(vec![ContentPart {
                    part_type: "text".to_string(),
                    text: Some(complete_text),
                    audio: None,
                    transcript: None,
                }]),
            },
        });

        events.push(ServerEvent::ResponseDone {
            response: ResponseInfo {
                id: response_id.clone(),
                status: "completed".to_string(),
                output: None,
            },
        });

        tracing::info!(
            session_id = %session.session_id,
            response_id = %response_id,
            "Response generation completed"
        );

        Ok(Box::pin(stream::iter(events)))
    }

    async fn update_session(
        &self,
        session: &mut RealtimeSession,
        config: SessionConfig,
    ) -> Result<(), RealtimeError> {
        session.config = config;

        tracing::debug!(
            session_id = %session.session_id,
            "Session configuration updated"
        );

        Ok(())
    }

    async fn clear_audio_buffer(&self, session: &mut RealtimeSession) -> Result<(), RealtimeError> {
        session.audio_buffer.clear();

        tracing::debug!(
            session_id = %session.session_id,
            "Audio buffer cleared"
        );

        Ok(())
    }

    async fn add_conversation_item(
        &self,
        session: &mut RealtimeSession,
        item: ConversationItem,
    ) -> Result<(), RealtimeError> {
        // Only process message-type items
        if item.item_type != "message" {
            return Ok(());
        }

        // Extract text from content parts if both role and content are present
        if let (Some(role), Some(content)) = (item.role, item.content) {
            let text = content
                .iter()
                .filter_map(|part| part.text.clone())
                .collect::<Vec<_>>()
                .join("");

            session.context.push(ConversationMessage {
                role,
                content: text,
            });

            tracing::debug!(
                session_id = %session.session_id,
                item_id = %item.id,
                "Conversation item added to context"
            );
        }

        Ok(())
    }
}
