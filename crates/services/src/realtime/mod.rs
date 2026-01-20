//! Realtime service for voice-to-voice conversations
//!
//! This module implements the realtime API for bidirectional audio streaming,
//! handling the STT -> LLM -> TTS pipeline.

pub mod ports;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use futures::stream;
use inference_providers::{AudioSpeechParams, AudioTranscriptionParams, ChatCompletionParams, ChatMessage, MessageRole};
use ports::{
    ConversationItem, ConversationMessage, ContentPart, RealtimeError, RealtimeServiceTrait,
    RealtimeSession, ResponseInfo, ServerEvent, ServerEventStream, SessionConfig,
    TranscriptionResult, WorkspaceContext,
};
use std::sync::Arc;
use uuid::Uuid;

use crate::inference_provider_pool::InferenceProviderPool;

/// Realtime service implementation
pub struct RealtimeServiceImpl {
    inference_pool: Arc<InferenceProviderPool>,
}

impl RealtimeServiceImpl {
    /// Create a new realtime service
    pub fn new(inference_pool: Arc<InferenceProviderPool>) -> Self {
        Self { inference_pool }
    }

    /// Generate a unique ID for items
    fn generate_id(prefix: &str) -> String {
        format!("{}_{}", prefix, Uuid::new_v4().to_string().replace("-", "")[..24].to_string())
    }

    /// Convert conversation context to chat messages
    fn context_to_messages(
        context: &[ConversationMessage],
        instructions: &Option<String>,
    ) -> Vec<ChatMessage> {
        let mut messages = Vec::new();

        // Add system instructions if present
        if let Some(ref instructions) = instructions {
            messages.push(ChatMessage {
                role: MessageRole::System,
                content: Some(serde_json::Value::String(instructions.clone())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            });
        }

        // Add conversation context
        for msg in context {
            let role = match msg.role.as_str() {
                "user" => MessageRole::User,
                "assistant" => MessageRole::Assistant,
                "system" => MessageRole::System,
                _ => MessageRole::User,
            };
            messages.push(ChatMessage {
                role,
                content: Some(serde_json::Value::String(msg.content.clone())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            });
        }

        messages
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
        // Decode base64 audio and append to buffer
        let audio_bytes = BASE64
            .decode(audio_base64)
            .map_err(|e| RealtimeError::InvalidAudioData(format!("Invalid base64: {e}")))?;

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
        _ctx: &WorkspaceContext,
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

        // Call transcription service
        let params = AudioTranscriptionParams {
            model: session.config.stt_model.clone(),
            audio_data,
            filename: format!("audio_{}.{}", item_id, session.config.input_audio_format),
            language: None,
            prompt: None,
            response_format: Some("json".to_string()),
            temperature: None,
            timestamp_granularities: None,
        };

        let request_hash = format!("realtime_{}", Uuid::new_v4());

        let response = self
            .inference_pool
            .audio_transcription(params, request_hash)
            .await
            .map_err(|e| RealtimeError::TranscriptionFailed(e.to_string()))?;

        let transcript = response.response.text.clone();

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

        // Build chat completion request
        let messages = Self::context_to_messages(&session.context, &session.config.instructions);

        let params = ChatCompletionParams {
            model: session.config.llm_model.clone(),
            messages,
            max_tokens: None,
            temperature: Some(session.config.temperature),
            top_p: None,
            stop: None,
            stream: Some(false), // Non-streaming for simplicity
            tools: None,
            max_completion_tokens: None,
            n: None,
            frequency_penalty: None,
            presence_penalty: None,
            logit_bias: None,
            logprobs: None,
            top_logprobs: None,
            user: Some(ctx.user_id.to_string()),
            seed: None,
            tool_choice: None,
            parallel_tool_calls: None,
            metadata: None,
            store: None,
            stream_options: None,
            modalities: None,
            extra: std::collections::HashMap::new(),
        };

        let request_hash = format!("realtime_llm_{}", Uuid::new_v4());

        // Get LLM response
        let llm_response = self
            .inference_pool
            .chat_completion(params, request_hash)
            .await
            .map_err(|e| RealtimeError::LlmError(e.to_string()))?;

        let assistant_text = llm_response
            .response
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default();

        // Add assistant response to context
        session.context.push(ConversationMessage {
            role: "assistant".to_string(),
            content: assistant_text.clone(),
        });

        // Generate TTS audio
        let tts_params = AudioSpeechParams {
            model: session.config.tts_model.clone(),
            input: assistant_text.clone(),
            voice: session.config.voice.clone(),
            response_format: Some(session.config.output_audio_format.clone()),
            speed: None,
        };

        let tts_request_hash = format!("realtime_tts_{}", Uuid::new_v4());

        let tts_response = self
            .inference_pool
            .audio_speech(tts_params, tts_request_hash)
            .await
            .map_err(|e| RealtimeError::TtsError(e.to_string()))?;

        let audio_base64 = BASE64.encode(&tts_response.audio_data);

        // Build event stream
        let response_id_clone = response_id.clone();

        let events = vec![
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
            ServerEvent::ResponseTextDelta {
                item_id: item_id.clone(),
                delta: assistant_text.clone(),
            },
            ServerEvent::ResponseTextDone {
                item_id: item_id.clone(),
                text: assistant_text.clone(),
            },
            ServerEvent::ResponseAudioDelta {
                item_id: item_id.clone(),
                delta: audio_base64,
            },
            ServerEvent::ResponseAudioDone {
                item_id: item_id.clone(),
            },
            ServerEvent::ResponseOutputItemDone {
                item: ConversationItem {
                    id: item_id.clone(),
                    item_type: "message".to_string(),
                    role: Some("assistant".to_string()),
                    content: Some(vec![ContentPart {
                        part_type: "text".to_string(),
                        text: Some(assistant_text),
                        audio: None,
                        transcript: None,
                    }]),
                },
            },
            ServerEvent::ResponseDone {
                response: ResponseInfo {
                    id: response_id,
                    status: "completed".to_string(),
                    output: None,
                },
            },
        ];

        tracing::info!(
            session_id = %session.session_id,
            response_id = %response_id_clone,
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
}
