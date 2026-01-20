//! Backend trait for external provider implementations
//!
//! This module defines the internal abstraction for different external AI providers.
//! Each backend handles the API-specific translation between our internal format
//! and the provider's native format.

use crate::{
    AudioError, AudioSpeechParams, AudioSpeechResponseWithBytes, AudioStreamingResult,
    AudioTranscriptionParams, AudioTranscriptionResponseWithBytes, ChatCompletionParams,
    ChatCompletionResponseWithBytes, CompletionError, ImageGenerationError, ImageGenerationParams,
    ImageGenerationResponseWithBytes, StreamingResult,
};
use async_trait::async_trait;
use std::collections::HashMap;

/// Configuration for a backend connection
#[derive(Debug, Clone)]
pub struct BackendConfig {
    /// Base URL for the provider API
    pub base_url: String,
    /// API key for authentication
    pub api_key: String,
    /// Request timeout in seconds
    pub timeout_seconds: i64,
    /// Provider-specific extra configuration (e.g., organization_id, version)
    pub extra: HashMap<String, String>,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            timeout_seconds: 120,
            extra: HashMap::new(),
        }
    }
}

/// Internal backend trait for different API formats
///
/// Each backend implementation handles the translation between our standard
/// ChatCompletionParams format and the provider's native format.
#[async_trait]
pub trait ExternalBackend: Send + Sync {
    /// Returns the backend type identifier (e.g., "openai_compatible", "anthropic", "gemini")
    fn backend_type(&self) -> &'static str;

    /// Performs a streaming chat completion request
    ///
    /// The backend is responsible for:
    /// - Translating ChatCompletionParams to provider-specific format
    /// - Making the HTTP request
    /// - Parsing the SSE response and translating it back to our StreamChunk format
    async fn chat_completion_stream(
        &self,
        config: &BackendConfig,
        model: &str,
        params: ChatCompletionParams,
    ) -> Result<StreamingResult, CompletionError>;

    /// Performs a non-streaming chat completion request
    ///
    /// The backend is responsible for:
    /// - Translating ChatCompletionParams to provider-specific format
    /// - Making the HTTP request
    /// - Parsing the response and translating it back to our ChatCompletionResponse format
    async fn chat_completion(
        &self,
        config: &BackendConfig,
        model: &str,
        params: ChatCompletionParams,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError>;

    /// Performs an image generation request
    ///
    /// The backend is responsible for:
    /// - Translating ImageGenerationParams to provider-specific format
    /// - Making the HTTP request
    /// - Parsing the response and translating it back to our ImageGenerationResponse format
    ///
    /// Default implementation returns an error indicating image generation is not supported.
    async fn image_generation(
        &self,
        _config: &BackendConfig,
        _model: &str,
        _params: ImageGenerationParams,
    ) -> Result<ImageGenerationResponseWithBytes, ImageGenerationError> {
        Err(ImageGenerationError::GenerationError(format!(
            "Image generation is not supported by the {} backend.",
            self.backend_type()
        )))
    }

    /// Performs an audio transcription (speech-to-text) request
    ///
    /// The backend is responsible for:
    /// - Sending audio data to the provider
    /// - Parsing the response and translating it back to our format
    ///
    /// Default implementation returns an error indicating audio transcription is not supported.
    async fn audio_transcription(
        &self,
        _config: &BackendConfig,
        _model: &str,
        _params: AudioTranscriptionParams,
    ) -> Result<AudioTranscriptionResponseWithBytes, AudioError> {
        Err(AudioError::ModelNotSupported(format!(
            "Audio transcription is not supported by the {} backend.",
            self.backend_type()
        )))
    }

    /// Performs a text-to-speech request (non-streaming)
    ///
    /// The backend is responsible for:
    /// - Sending text to the provider
    /// - Returning the audio data
    ///
    /// Default implementation returns an error indicating TTS is not supported.
    async fn audio_speech(
        &self,
        _config: &BackendConfig,
        _model: &str,
        _params: AudioSpeechParams,
    ) -> Result<AudioSpeechResponseWithBytes, AudioError> {
        Err(AudioError::ModelNotSupported(format!(
            "Text-to-speech is not supported by the {} backend.",
            self.backend_type()
        )))
    }

    /// Performs a streaming text-to-speech request
    ///
    /// The backend is responsible for:
    /// - Sending text to the provider
    /// - Streaming audio chunks back
    ///
    /// Default implementation returns an error indicating streaming TTS is not supported.
    async fn audio_speech_stream(
        &self,
        _config: &BackendConfig,
        _model: &str,
        _params: AudioSpeechParams,
    ) -> Result<AudioStreamingResult, AudioError> {
        Err(AudioError::ModelNotSupported(format!(
            "Streaming text-to-speech is not supported by the {} backend.",
            self.backend_type()
        )))
    }
}
