//! Audio service implementation
//!
//! This module provides audio transcription (STT) and synthesis (TTS) services.
//! It handles provider routing and usage tracking.

pub mod ports;

#[cfg(test)]
mod tests;

use async_trait::async_trait;
use futures::stream::StreamExt;
use inference_providers::{AudioSpeechParams, AudioTranscriptionParams};
use ports::{
    AudioServiceError, AudioServiceTrait, SpeechRequest, SpeechResponse, SpeechStreamResult,
    TranscribeRequest, TranscribeResponse,
};
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    inference_provider_pool::InferenceProviderPool,
    usage::{ports::UsageServiceTrait, RecordUsageServiceRequest, StopReason},
};

/// Audio service implementation
pub struct AudioServiceImpl {
    inference_pool: Arc<InferenceProviderPool>,
    usage_service: Arc<dyn UsageServiceTrait>,
}

impl AudioServiceImpl {
    /// Create a new audio service
    pub fn new(
        inference_pool: Arc<InferenceProviderPool>,
        usage_service: Arc<dyn UsageServiceTrait>,
    ) -> Self {
        Self {
            inference_pool,
            usage_service,
        }
    }

    /// Record usage for audio operations
    #[allow(clippy::too_many_arguments)]
    async fn record_usage(
        &self,
        organization_id: Uuid,
        workspace_id: Uuid,
        api_key_id: Uuid,
        model_id: Uuid,
        inference_type: &str,
        input_tokens: i32,
        output_tokens: i32,
    ) {
        let usage_request = RecordUsageServiceRequest {
            organization_id,
            workspace_id,
            api_key_id,
            model_id,
            input_tokens,
            output_tokens,
            inference_type: inference_type.to_string(),
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
                %organization_id,
                %workspace_id,
                inference_type = %inference_type,
                "Failed to record audio usage"
            );
        }
    }
}

#[async_trait]
impl AudioServiceTrait for AudioServiceImpl {
    async fn transcribe(
        &self,
        request: TranscribeRequest,
    ) -> Result<TranscribeResponse, AudioServiceError> {
        tracing::debug!(
            model = %request.model,
            "Processing audio transcription request"
        );

        // Convert service request to provider params
        let provider_params = AudioTranscriptionParams {
            model: request.model.clone(),
            audio_data: request.audio_data,
            filename: request.filename,
            language: request.language,
            prompt: None,
            response_format: request.response_format,
            temperature: None,
            timestamp_granularities: None,
            sample_rate_hertz: None,
        };

        // Call the inference provider
        let response = self
            .inference_pool
            .audio_transcription(provider_params, request.request_hash)
            .await
            .map_err(|e| AudioServiceError::ProviderError(e.to_string()))?;

        // Record usage based on audio duration
        // For STT, we track audio seconds as "input tokens" (scaled by 1000 for precision)
        // Use i64 to prevent overflow for very long audio (> 35 minutes)
        let audio_seconds_scaled = response
            .audio_duration_seconds
            .map(|d| ((d * 1000.0) as i64).min(i32::MAX as i64) as i32)
            .unwrap_or(0);

        self.record_usage(
            request.organization_id,
            request.workspace_id,
            request.api_key_id,
            request.model_id,
            "audio_transcription",
            audio_seconds_scaled, // Audio duration in milliseconds
            0,                    // No output tokens for STT
        )
        .await;

        tracing::info!(
            model = %request.model,
            duration_seconds = ?response.audio_duration_seconds,
            "Audio transcription completed"
        );

        Ok(TranscribeResponse {
            text: response.response.text,
            language: response.response.language,
            duration: response.response.duration,
            words: response.response.words,
            segments: response.response.segments,
            raw_bytes: response.raw_bytes,
        })
    }

    async fn synthesize(
        &self,
        request: SpeechRequest,
    ) -> Result<SpeechResponse, AudioServiceError> {
        tracing::debug!(
            model = %request.model,
            voice = %request.voice,
            "Processing text-to-speech request"
        );

        // Validate input length using character count (consistent with billing)
        let character_count = request.input.chars().count();
        if character_count > 4096 {
            return Err(AudioServiceError::InvalidRequest(
                "Input text exceeds maximum length of 4096 characters".to_string(),
            ));
        }

        // Convert service request to provider params
        let provider_params = AudioSpeechParams {
            model: request.model.clone(),
            input: request.input.clone(),
            voice: request.voice.clone(),
            response_format: request.response_format,
            speed: request.speed,
        };

        let character_count = character_count as i32;

        // Call the inference provider
        let response = self
            .inference_pool
            .audio_speech(provider_params, request.request_hash)
            .await
            .map_err(|e| AudioServiceError::ProviderError(e.to_string()))?;

        // Record usage based on character count
        // For TTS, we track characters as "output tokens"
        self.record_usage(
            request.organization_id,
            request.workspace_id,
            request.api_key_id,
            request.model_id,
            "audio_speech",
            0,               // No input tokens for TTS
            character_count, // Character count as output tokens
        )
        .await;

        tracing::info!(
            model = %request.model,
            voice = %request.voice,
            characters = character_count,
            "Text-to-speech completed"
        );

        Ok(SpeechResponse {
            audio_data: response.audio_data,
            content_type: response.content_type,
        })
    }

    async fn synthesize_stream(
        &self,
        request: SpeechRequest,
    ) -> Result<SpeechStreamResult, AudioServiceError> {
        tracing::debug!(
            model = %request.model,
            voice = %request.voice,
            "Processing streaming text-to-speech request"
        );

        // Validate input length using character count (consistent with billing)
        let character_count = request.input.chars().count();
        if character_count > 4096 {
            return Err(AudioServiceError::InvalidRequest(
                "Input text exceeds maximum length of 4096 characters".to_string(),
            ));
        }

        // Convert service request to provider params
        let provider_params = AudioSpeechParams {
            model: request.model.clone(),
            input: request.input.clone(),
            voice: request.voice.clone(),
            response_format: request.response_format,
            speed: request.speed,
        };

        let character_count = character_count as i32;

        // Call the inference provider
        let audio_stream = self
            .inference_pool
            .audio_speech_stream(provider_params, request.request_hash)
            .await
            .map_err(|e| AudioServiceError::ProviderError(e.to_string()))?;

        // Record usage upfront for streaming (we know the character count before streaming starts)
        // This is done immediately rather than fire-and-forget to prevent data loss on shutdown
        self.record_usage(
            request.organization_id,
            request.workspace_id,
            request.api_key_id,
            request.model_id,
            "audio_speech_stream",
            0,               // No input tokens for TTS
            character_count, // Character count as output tokens
        )
        .await;

        // Map the provider stream to our service stream
        let service_stream = audio_stream.map(|result| {
            result
                .map(|chunk| chunk.data)
                .map_err(|e| AudioServiceError::ProviderError(e.to_string()))
        });

        Ok(Box::pin(service_stream))
    }
}
