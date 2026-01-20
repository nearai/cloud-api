//! Realtime service ports (trait definitions)
//!
//! This module defines the contracts for realtime voice-to-voice services.

use async_trait::async_trait;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use uuid::Uuid;

// ==================== Session Configuration ====================

/// Session configuration for realtime connections
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    /// Model for speech-to-text (e.g., "whisper-1")
    #[serde(default = "default_stt_model")]
    pub stt_model: String,
    /// Model for LLM inference (e.g., "gpt-4")
    #[serde(default = "default_llm_model")]
    pub llm_model: String,
    /// Model for text-to-speech (e.g., "tts-1")
    #[serde(default = "default_tts_model")]
    pub tts_model: String,
    /// Voice for TTS (e.g., "alloy")
    #[serde(default = "default_voice")]
    pub voice: String,
    /// Instructions/system prompt for the LLM
    #[serde(default)]
    pub instructions: Option<String>,
    /// Temperature for LLM
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    /// Input audio format
    #[serde(default = "default_input_audio_format")]
    pub input_audio_format: String,
    /// Output audio format
    #[serde(default = "default_output_audio_format")]
    pub output_audio_format: String,
}

fn default_stt_model() -> String {
    "whisper-1".to_string()
}
fn default_llm_model() -> String {
    "gpt-4".to_string()
}
fn default_tts_model() -> String {
    "tts-1".to_string()
}
fn default_voice() -> String {
    "alloy".to_string()
}
fn default_temperature() -> f32 {
    0.8
}
fn default_input_audio_format() -> String {
    "pcm16".to_string()
}
fn default_output_audio_format() -> String {
    "pcm16".to_string()
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            stt_model: default_stt_model(),
            llm_model: default_llm_model(),
            tts_model: default_tts_model(),
            voice: default_voice(),
            instructions: None,
            temperature: default_temperature(),
            input_audio_format: default_input_audio_format(),
            output_audio_format: default_output_audio_format(),
        }
    }
}

// ==================== Session State ====================

/// Session state for a realtime connection
#[derive(Debug, Clone)]
pub struct RealtimeSession {
    /// Unique session ID
    pub session_id: String,
    /// Associated conversation ID (if any)
    pub conversation_id: Option<Uuid>,
    /// Session configuration
    pub config: SessionConfig,
    /// Accumulated audio input buffer
    pub audio_buffer: Vec<u8>,
    /// Conversation history for context
    pub context: Vec<ConversationMessage>,
}

/// Message in conversation context
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessage {
    /// Role: user, assistant, system
    pub role: String,
    /// Text content
    pub content: String,
}

/// Conversation item for the API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationItem {
    /// Item ID
    pub id: String,
    /// Item type: message, function_call, function_call_output
    #[serde(rename = "type")]
    pub item_type: String,
    /// Role (for messages)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Content (for messages)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ContentPart>>,
}

/// Content part in a conversation item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentPart {
    /// Type: text, audio
    #[serde(rename = "type")]
    pub part_type: String,
    /// Text content (for text type)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Audio data base64 (for audio type)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<String>,
    /// Transcript of audio (for audio type)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
}

// ==================== Client Events ====================

/// Events sent from client to server
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientEvent {
    /// Update session configuration
    #[serde(rename = "session.update")]
    SessionUpdate { session: SessionConfig },
    /// Append audio to input buffer
    #[serde(rename = "input_audio_buffer.append")]
    InputAudioBufferAppend {
        /// Base64-encoded audio data
        audio: String,
    },
    /// Commit audio buffer for transcription
    #[serde(rename = "input_audio_buffer.commit")]
    InputAudioBufferCommit,
    /// Clear audio buffer
    #[serde(rename = "input_audio_buffer.clear")]
    InputAudioBufferClear,
    /// Create a conversation item
    #[serde(rename = "conversation.item.create")]
    ConversationItemCreate { item: ConversationItem },
    /// Request a response
    #[serde(rename = "response.create")]
    ResponseCreate {
        #[serde(default)]
        response: Option<ResponseConfig>,
    },
    /// Cancel in-progress response
    #[serde(rename = "response.cancel")]
    ResponseCancel,
}

/// Configuration for response generation
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResponseConfig {
    /// Override modalities for this response
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modalities: Option<Vec<String>>,
    /// Override instructions for this response
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

// ==================== Server Events ====================

/// Events sent from server to client
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerEvent {
    /// Session created
    #[serde(rename = "session.created")]
    SessionCreated { session: SessionInfo },
    /// Session updated
    #[serde(rename = "session.updated")]
    SessionUpdated { session: SessionInfo },
    /// Audio buffer committed
    #[serde(rename = "input_audio_buffer.committed")]
    InputAudioBufferCommitted { item_id: String },
    /// Audio buffer cleared
    #[serde(rename = "input_audio_buffer.cleared")]
    InputAudioBufferCleared,
    /// Speech detected in audio
    #[serde(rename = "input_audio_buffer.speech_started")]
    InputAudioBufferSpeechStarted { audio_start_ms: i32, item_id: String },
    /// Speech ended in audio
    #[serde(rename = "input_audio_buffer.speech_stopped")]
    InputAudioBufferSpeechStopped { audio_end_ms: i32, item_id: String },
    /// Conversation item created
    #[serde(rename = "conversation.item.created")]
    ConversationItemCreated { item: ConversationItem },
    /// Audio transcription completed
    #[serde(rename = "conversation.item.input_audio_transcription.completed")]
    ConversationItemInputAudioTranscriptionCompleted {
        item_id: String,
        transcript: String,
    },
    /// Response created
    #[serde(rename = "response.created")]
    ResponseCreated { response: ResponseInfo },
    /// Output item added to response
    #[serde(rename = "response.output_item.added")]
    ResponseOutputItemAdded { item: ConversationItem },
    /// Output item completed
    #[serde(rename = "response.output_item.done")]
    ResponseOutputItemDone { item: ConversationItem },
    /// Text delta in response
    #[serde(rename = "response.text.delta")]
    ResponseTextDelta { item_id: String, delta: String },
    /// Text completed
    #[serde(rename = "response.text.done")]
    ResponseTextDone { item_id: String, text: String },
    /// Audio delta in response (base64)
    #[serde(rename = "response.audio.delta")]
    ResponseAudioDelta { item_id: String, delta: String },
    /// Audio completed
    #[serde(rename = "response.audio.done")]
    ResponseAudioDone { item_id: String },
    /// Response completed
    #[serde(rename = "response.done")]
    ResponseDone { response: ResponseInfo },
    /// Error occurred
    #[serde(rename = "error")]
    Error { error: ErrorInfo },
}

/// Error information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorInfo {
    #[serde(rename = "type")]
    pub error_type: String,
    pub code: String,
    pub message: String,
}

/// Session information returned in events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub model: String,
    pub voice: String,
    pub instructions: Option<String>,
}

/// Response information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseInfo {
    pub id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Vec<ConversationItem>>,
}

/// Result of audio transcription
#[derive(Debug, Clone)]
pub struct TranscriptionResult {
    pub item_id: String,
    pub text: String,
}

// ==================== Error Types ====================

/// Errors in realtime service
#[derive(Debug, thiserror::Error)]
pub enum RealtimeError {
    #[error("Session not found: {0}")]
    SessionNotFound(String),
    #[error("Invalid audio data: {0}")]
    InvalidAudioData(String),
    #[error("Transcription failed: {0}")]
    TranscriptionFailed(String),
    #[error("LLM error: {0}")]
    LlmError(String),
    #[error("TTS error: {0}")]
    TtsError(String),
    #[error("Internal error: {0}")]
    InternalError(String),
}

// ==================== Service Trait ====================

/// Type alias for server event stream
pub type ServerEventStream = Pin<Box<dyn Stream<Item = ServerEvent> + Send>>;

/// Workspace context for authentication
#[derive(Debug, Clone)]
pub struct WorkspaceContext {
    pub organization_id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub user_id: Uuid,
}

/// Realtime service trait for voice-to-voice conversations
#[async_trait]
pub trait RealtimeServiceTrait: Send + Sync {
    /// Create a new realtime session
    async fn create_session(
        &self,
        config: SessionConfig,
        ctx: &WorkspaceContext,
    ) -> Result<RealtimeSession, RealtimeError>;

    /// Handle an audio chunk (append to buffer)
    async fn handle_audio_chunk(
        &self,
        session: &mut RealtimeSession,
        audio_base64: &str,
    ) -> Result<(), RealtimeError>;

    /// Commit the audio buffer and transcribe
    async fn commit_audio_buffer(
        &self,
        session: &mut RealtimeSession,
        ctx: &WorkspaceContext,
    ) -> Result<TranscriptionResult, RealtimeError>;

    /// Generate a response (LLM + TTS) and return event stream
    async fn generate_response(
        &self,
        session: &mut RealtimeSession,
        ctx: &WorkspaceContext,
    ) -> Result<ServerEventStream, RealtimeError>;

    /// Update session configuration
    async fn update_session(
        &self,
        session: &mut RealtimeSession,
        config: SessionConfig,
    ) -> Result<(), RealtimeError>;

    /// Clear the audio buffer
    async fn clear_audio_buffer(&self, session: &mut RealtimeSession) -> Result<(), RealtimeError>;
}
