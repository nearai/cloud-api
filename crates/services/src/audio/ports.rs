//! Audio service ports (trait definitions)
//!
//! This module defines the contracts for audio services following the ports and adapters pattern.
//! Services depend on these traits, not concrete implementations.

use async_trait::async_trait;
use futures::stream::Stream;
use inference_providers::{TranscriptionSegment, TranscriptionWord};
use std::pin::Pin;
use uuid::Uuid;

// ==================== Request Types ====================

/// Request for audio transcription (speech-to-text)
#[derive(Debug, Clone)]
pub struct TranscribeRequest {
    /// Model to use for transcription (e.g., "whisper-1")
    pub model: String,
    /// Raw audio data bytes
    pub audio_data: Vec<u8>,
    /// Original filename (e.g., "audio.mp3")
    pub filename: String,
    /// Optional language hint (ISO-639-1)
    pub language: Option<String>,
    /// Response format: json, text, srt, verbose_json, vtt
    pub response_format: Option<String>,
    /// Organization ID for usage tracking
    pub organization_id: Uuid,
    /// Workspace ID for usage tracking
    pub workspace_id: Uuid,
    /// API key ID for usage tracking
    pub api_key_id: Uuid,
    /// Model ID (resolved from database) for usage tracking
    pub model_id: Uuid,
    /// Request hash for attestation
    pub request_hash: String,
}

/// Request for text-to-speech synthesis
#[derive(Debug, Clone)]
pub struct SpeechRequest {
    /// Model to use for synthesis (e.g., "tts-1", "tts-1-hd")
    pub model: String,
    /// Text to convert to speech (max 4096 characters)
    pub input: String,
    /// Voice to use (e.g., "alloy", "echo", "fable", "onyx", "nova", "shimmer")
    pub voice: String,
    /// Response format: mp3, opus, aac, flac, wav, pcm
    pub response_format: Option<String>,
    /// Speed of speech (0.25 to 4.0)
    pub speed: Option<f32>,
    /// Organization ID for usage tracking
    pub organization_id: Uuid,
    /// Workspace ID for usage tracking
    pub workspace_id: Uuid,
    /// API key ID for usage tracking
    pub api_key_id: Uuid,
    /// Model ID (resolved from database) for usage tracking
    pub model_id: Uuid,
    /// Request hash for attestation
    pub request_hash: String,
}

// ==================== Response Types ====================

/// Response from audio transcription
#[derive(Debug, Clone)]
pub struct TranscribeResponse {
    /// Transcribed text
    pub text: String,
    /// Detected or specified language
    pub language: Option<String>,
    /// Audio duration in seconds
    pub duration: Option<f64>,
    /// Word-level timestamps (if requested)
    pub words: Option<Vec<TranscriptionWord>>,
    /// Segment-level timestamps (if requested)
    pub segments: Option<Vec<TranscriptionSegment>>,
    /// Raw response bytes for verification
    pub raw_bytes: Vec<u8>,
}

/// Response from text-to-speech synthesis
#[derive(Debug, Clone)]
pub struct SpeechResponse {
    /// Generated audio data
    pub audio_data: Vec<u8>,
    /// Content type of the audio (e.g., "audio/mpeg")
    pub content_type: String,
}

// ==================== Error Types ====================

/// Errors that can occur in audio service operations
#[derive(Debug, thiserror::Error)]
pub enum AudioServiceError {
    /// The requested model was not found
    #[error("Model not found: {0}")]
    ModelNotFound(String),

    /// The provider returned an error
    #[error("Provider error: {0}")]
    ProviderError(String),

    /// The request was invalid
    #[error("Invalid request: {0}")]
    InvalidRequest(String),

    /// Usage tracking failed
    #[error("Usage error: {0}")]
    UsageError(String),

    /// Internal server error
    #[error("Internal error: {0}")]
    InternalError(String),
}

// ==================== Service Trait ====================

/// Type alias for streaming speech results
pub type SpeechStreamResult =
    Pin<Box<dyn Stream<Item = Result<Vec<u8>, AudioServiceError>> + Send>>;

/// Audio service trait
///
/// Provides audio transcription (STT) and synthesis (TTS) capabilities.
#[async_trait]
pub trait AudioServiceTrait: Send + Sync {
    /// Transcribe audio to text
    ///
    /// Sends audio data to the specified model and returns the transcribed text.
    /// Also records usage for billing purposes.
    async fn transcribe(
        &self,
        request: TranscribeRequest,
    ) -> Result<TranscribeResponse, AudioServiceError>;

    /// Synthesize text to speech (non-streaming)
    ///
    /// Converts text to audio using the specified model and voice.
    /// Returns the complete audio data.
    async fn synthesize(&self, request: SpeechRequest)
        -> Result<SpeechResponse, AudioServiceError>;

    /// Synthesize text to speech (streaming)
    ///
    /// Converts text to audio and streams chunks as they become available.
    async fn synthesize_stream(
        &self,
        request: SpeechRequest,
    ) -> Result<SpeechStreamResult, AudioServiceError>;
}
