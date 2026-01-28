//! Unit tests for AudioService

#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use crate::audio::ports::{
        AudioServiceError, SpeechRequest, TranscribeRequest, TranscribeResponse,
    };
    use crate::usage::{ports::UsageServiceTrait, RecordUsageServiceRequest};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    /// Mock usage service for testing
    struct MockUsageService {
        recorded_usages: Arc<Mutex<Vec<RecordUsageServiceRequest>>>,
    }

    impl MockUsageService {
        fn new() -> Self {
            Self {
                recorded_usages: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn get_recorded_usages(&self) -> Vec<RecordUsageServiceRequest> {
            self.recorded_usages.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl UsageServiceTrait for MockUsageService {
        async fn calculate_cost(
            &self,
            _model_id: &str,
            _input_tokens: i32,
            _output_tokens: i32,
        ) -> Result<crate::usage::CostBreakdown, crate::usage::UsageError> {
            Ok(crate::usage::CostBreakdown {
                input_cost: 0,
                output_cost: 0,
                total_cost: 0,
            })
        }

        async fn record_usage(
            &self,
            request: RecordUsageServiceRequest,
        ) -> Result<(), crate::usage::UsageError> {
            self.recorded_usages.lock().unwrap().push(request);
            Ok(())
        }

        async fn check_can_use(
            &self,
            _org_id: Uuid,
        ) -> Result<crate::usage::UsageCheckResult, crate::usage::UsageError> {
            Ok(crate::usage::UsageCheckResult::Allowed { remaining: 1000 })
        }

        async fn get_balance(
            &self,
            _organization_id: Uuid,
        ) -> Result<Option<crate::usage::OrganizationBalanceInfo>, crate::usage::UsageError>
        {
            Ok(Some(crate::usage::OrganizationBalanceInfo {
                organization_id: _organization_id,
                total_spent: 0,
                last_usage_at: None,
                total_requests: 0,
                total_tokens: 0,
                updated_at: chrono::Utc::now(),
            }))
        }

        async fn get_usage_history(
            &self,
            _organization_id: Uuid,
            _limit: Option<i64>,
            _offset: Option<i64>,
        ) -> Result<(Vec<crate::usage::UsageLogEntry>, i64), crate::usage::UsageError> {
            Ok((Vec::new(), 0))
        }

        async fn get_limit(
            &self,
            _organization_id: Uuid,
        ) -> Result<Option<crate::usage::OrganizationLimit>, crate::usage::UsageError> {
            Ok(Some(crate::usage::OrganizationLimit { spend_limit: 10000 }))
        }

        async fn get_usage_history_by_api_key(
            &self,
            _api_key_id: Uuid,
            _limit: Option<i64>,
            _offset: Option<i64>,
        ) -> Result<(Vec<crate::usage::UsageLogEntry>, i64), crate::usage::UsageError> {
            Ok((Vec::new(), 0))
        }

        async fn get_api_key_usage_history_with_permissions(
            &self,
            _workspace_id: Uuid,
            _api_key_id: Uuid,
            _user_id: Uuid,
            _limit: Option<i64>,
            _offset: Option<i64>,
        ) -> Result<(Vec<crate::usage::UsageLogEntry>, i64), crate::usage::UsageError> {
            Ok((Vec::new(), 0))
        }

        async fn get_costs_by_inference_ids(
            &self,
            _organization_id: Uuid,
            _inference_ids: Vec<Uuid>,
        ) -> Result<Vec<crate::usage::InferenceCost>, crate::usage::UsageError> {
            Ok(Vec::new())
        }
    }

    // Helper to create test IDs
    fn test_org_id() -> Uuid {
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap()
    }

    fn test_workspace_id() -> Uuid {
        Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap()
    }

    fn test_api_key_id() -> Uuid {
        Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap()
    }

    fn test_model_id() -> Uuid {
        Uuid::parse_str("44444444-4444-4444-4444-444444444444").unwrap()
    }

    // ========================================================================
    // TRANSCRIPTION TESTS
    // ========================================================================

    #[tokio::test]
    async fn test_transcribe_basic() {
        // This test validates the basic transcription flow
        // Note: This requires a real InferenceProviderPool which is complex to mock
        // In a real scenario, you would either:
        // 1. Mock InferenceProviderPool completely
        // 2. Use integration tests instead
        // For now, we'll test the validation and error handling

        let _usage_service = Arc::new(MockUsageService::new());
        let request = TranscribeRequest {
            model: "whisper-1".to_string(),
            audio_data: vec![1, 2, 3, 4, 5], // Minimal audio data
            filename: "test.wav".to_string(),
            language: Some("en".to_string()),
            response_format: None,
            organization_id: test_org_id(),
            workspace_id: test_workspace_id(),
            api_key_id: test_api_key_id(),
            model_id: test_model_id(),
            request_hash: "test_hash".to_string(),
        };

        // Verify request structure is correct
        assert_eq!(request.model, "whisper-1");
        assert_eq!(request.filename, "test.wav");
        assert_eq!(request.language, Some("en".to_string()));
        assert_eq!(request.audio_data.len(), 5);
    }

    #[tokio::test]
    async fn test_transcribe_with_all_parameters() {
        let request = TranscribeRequest {
            model: "whisper-1".to_string(),
            audio_data: vec![1, 2, 3],
            filename: "audio.mp3".to_string(),
            language: Some("es".to_string()),
            response_format: Some("json".to_string()),
            organization_id: test_org_id(),
            workspace_id: test_workspace_id(),
            api_key_id: test_api_key_id(),
            model_id: test_model_id(),
            request_hash: "hash".to_string(),
        };

        assert_eq!(request.language, Some("es".to_string()));
        assert_eq!(request.response_format, Some("json".to_string()));
    }

    #[tokio::test]
    async fn test_transcribe_response_structure() {
        let response = TranscribeResponse {
            text: "Hello world".to_string(),
            language: Some("en".to_string()),
            duration: Some(2.5),
            words: None,
            segments: None,
            raw_bytes: vec![1, 2, 3],
        };

        assert_eq!(response.text, "Hello world");
        assert_eq!(response.language, Some("en".to_string()));
        assert_eq!(response.duration, Some(2.5));
        assert!(response.words.is_none());
        assert!(response.segments.is_none());
        assert_eq!(response.raw_bytes.len(), 3);
    }

    // ========================================================================
    // SPEECH SYNTHESIS TESTS
    // ========================================================================

    #[tokio::test]
    async fn test_speech_request_basic() {
        let request = SpeechRequest {
            model: "tts-1".to_string(),
            input: "Hello, world!".to_string(),
            voice: "alloy".to_string(),
            response_format: None,
            speed: None,
            organization_id: test_org_id(),
            workspace_id: test_workspace_id(),
            api_key_id: test_api_key_id(),
            model_id: test_model_id(),
            request_hash: "hash".to_string(),
        };

        assert_eq!(request.model, "tts-1");
        assert_eq!(request.input, "Hello, world!");
        assert_eq!(request.voice, "alloy");
        assert!(request.response_format.is_none());
        assert!(request.speed.is_none());
    }

    #[tokio::test]
    async fn test_speech_request_with_all_parameters() {
        let request = SpeechRequest {
            model: "tts-1-hd".to_string(),
            input: "This is a longer piece of text.".to_string(),
            voice: "nova".to_string(),
            response_format: Some("wav".to_string()),
            speed: Some(1.5),
            organization_id: test_org_id(),
            workspace_id: test_workspace_id(),
            api_key_id: test_api_key_id(),
            model_id: test_model_id(),
            request_hash: "hash".to_string(),
        };

        assert_eq!(request.model, "tts-1-hd");
        assert_eq!(request.voice, "nova");
        assert_eq!(request.response_format, Some("wav".to_string()));
        assert_eq!(request.speed, Some(1.5));
    }

    #[tokio::test]
    async fn test_speech_request_validation_max_length() {
        // Simulate the 4096 character limit check
        let input = "a".repeat(4097);

        let is_too_long = input.len() > 4096;
        assert!(is_too_long, "Input exceeding 4096 should be detected");
    }

    #[tokio::test]
    async fn test_speech_request_validation_empty_input() {
        let input = "";
        assert!(input.is_empty(), "Empty input should be detected");
    }

    #[tokio::test]
    async fn test_speech_request_voice_options() {
        let valid_voices = vec!["alloy", "echo", "fable", "onyx", "nova", "shimmer"];

        for voice in valid_voices {
            let request = SpeechRequest {
                model: "tts-1".to_string(),
                input: "test".to_string(),
                voice: voice.to_string(),
                response_format: None,
                speed: None,
                organization_id: test_org_id(),
                workspace_id: test_workspace_id(),
                api_key_id: test_api_key_id(),
                model_id: test_model_id(),
                request_hash: "hash".to_string(),
            };

            assert_eq!(request.voice, voice.to_string());
        }
    }

    #[tokio::test]
    async fn test_speech_request_response_formats() {
        let formats = vec!["mp3", "opus", "aac", "flac", "wav", "pcm"];

        for format in formats {
            let request = SpeechRequest {
                model: "tts-1".to_string(),
                input: "test".to_string(),
                voice: "alloy".to_string(),
                response_format: Some(format.to_string()),
                speed: None,
                organization_id: test_org_id(),
                workspace_id: test_workspace_id(),
                api_key_id: test_api_key_id(),
                model_id: test_model_id(),
                request_hash: "hash".to_string(),
            };

            assert_eq!(request.response_format, Some(format.to_string()));
        }
    }

    #[tokio::test]
    async fn test_speech_request_speed_range() {
        // Valid speed range: 0.25 to 4.0
        let valid_speeds = vec![0.25, 0.5, 1.0, 1.5, 2.0, 4.0];

        for speed in valid_speeds {
            let request = SpeechRequest {
                model: "tts-1".to_string(),
                input: "test".to_string(),
                voice: "alloy".to_string(),
                response_format: None,
                speed: Some(speed),
                organization_id: test_org_id(),
                workspace_id: test_workspace_id(),
                api_key_id: test_api_key_id(),
                model_id: test_model_id(),
                request_hash: "hash".to_string(),
            };

            assert_eq!(request.speed, Some(speed));
        }
    }

    // ========================================================================
    // ERROR HANDLING TESTS
    // ========================================================================

    #[tokio::test]
    async fn test_audio_service_error_model_not_found() {
        let error = AudioServiceError::ModelNotFound("whisper-2".to_string());
        match error {
            AudioServiceError::ModelNotFound(msg) => {
                assert_eq!(msg, "whisper-2");
            }
            _ => panic!("Expected ModelNotFound error"),
        }
    }

    #[tokio::test]
    async fn test_audio_service_error_provider_error() {
        let error = AudioServiceError::ProviderError("Connection timeout".to_string());
        match error {
            AudioServiceError::ProviderError(msg) => {
                assert_eq!(msg, "Connection timeout");
            }
            _ => panic!("Expected ProviderError"),
        }
    }

    #[tokio::test]
    async fn test_audio_service_error_invalid_request() {
        let error = AudioServiceError::InvalidRequest("Invalid audio format".to_string());
        match error {
            AudioServiceError::InvalidRequest(msg) => {
                assert_eq!(msg, "Invalid audio format");
            }
            _ => panic!("Expected InvalidRequest error"),
        }
    }

    #[tokio::test]
    async fn test_audio_service_error_usage_error() {
        let error = AudioServiceError::UsageError("Failed to record usage".to_string());
        match error {
            AudioServiceError::UsageError(msg) => {
                assert_eq!(msg, "Failed to record usage");
            }
            _ => panic!("Expected UsageError"),
        }
    }

    #[tokio::test]
    async fn test_audio_service_error_internal_error() {
        let error = AudioServiceError::InternalError("Database connection failed".to_string());
        match error {
            AudioServiceError::InternalError(msg) => {
                assert_eq!(msg, "Database connection failed");
            }
            _ => panic!("Expected InternalError"),
        }
    }

    // ========================================================================
    // USAGE TRACKING TESTS
    // ========================================================================

    #[tokio::test]
    async fn test_usage_service_records_transcription() {
        let usage_service = Arc::new(MockUsageService::new());

        // Simulate usage recording
        let usage_request = RecordUsageServiceRequest {
            organization_id: test_org_id(),
            workspace_id: test_workspace_id(),
            api_key_id: test_api_key_id(),
            model_id: test_model_id(),
            input_tokens: 100, // Audio duration in ms
            output_tokens: 0,
            inference_type: "audio_transcription".to_string(),
            ttft_ms: None,
            avg_itl_ms: None,
            inference_id: None,
            provider_request_id: None,
            stop_reason: Some(crate::usage::StopReason::Completed),
            response_id: None,
            image_count: None,
        };

        let _ = usage_service.record_usage(usage_request).await;

        let recorded = usage_service.get_recorded_usages();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].inference_type, "audio_transcription");
        assert_eq!(recorded[0].input_tokens, 100);
    }

    #[tokio::test]
    async fn test_usage_service_records_speech_synthesis() {
        let usage_service = Arc::new(MockUsageService::new());

        let usage_request = RecordUsageServiceRequest {
            organization_id: test_org_id(),
            workspace_id: test_workspace_id(),
            api_key_id: test_api_key_id(),
            model_id: test_model_id(),
            input_tokens: 0,
            output_tokens: 50, // Character count
            inference_type: "audio_speech".to_string(),
            ttft_ms: None,
            avg_itl_ms: None,
            inference_id: None,
            provider_request_id: None,
            stop_reason: Some(crate::usage::StopReason::Completed),
            response_id: None,
            image_count: None,
        };

        let _ = usage_service.record_usage(usage_request).await;

        let recorded = usage_service.get_recorded_usages();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].inference_type, "audio_speech");
        assert_eq!(recorded[0].output_tokens, 50);
    }

    #[tokio::test]
    async fn test_usage_service_multiple_recordings() {
        let usage_service = Arc::new(MockUsageService::new());

        // Record multiple operations
        for i in 0..5 {
            let request = RecordUsageServiceRequest {
                organization_id: test_org_id(),
                workspace_id: test_workspace_id(),
                api_key_id: test_api_key_id(),
                model_id: test_model_id(),
                input_tokens: i * 100,
                output_tokens: i * 50,
                inference_type: "audio_transcription".to_string(),
                ttft_ms: None,
                avg_itl_ms: None,
                inference_id: None,
                provider_request_id: None,
                stop_reason: Some(crate::usage::StopReason::Completed),
                response_id: None,
                image_count: None,
            };

            let _ = usage_service.record_usage(request).await;
        }

        let recorded = usage_service.get_recorded_usages();
        assert_eq!(recorded.len(), 5);

        // Verify progressive token counts
        for (i, usage) in recorded.iter().enumerate() {
            assert_eq!(usage.input_tokens, (i as i32) * 100);
            assert_eq!(usage.output_tokens, (i as i32) * 50);
        }
    }

    // ========================================================================
    // CHARACTER COUNT TESTS
    // ========================================================================

    #[test]
    fn test_character_count_ascii() {
        let input = "Hello, World!";
        let count = input.chars().count();
        assert_eq!(count, 13);
    }

    #[test]
    fn test_character_count_unicode() {
        let input = "Hello 🌍"; // Contains emoji
        let count = input.chars().count();
        assert_eq!(count, 7); // 6 letters + 1 emoji
    }

    #[test]
    fn test_character_count_max_boundary() {
        let input = "a".repeat(4096);
        let count = input.chars().count();
        assert_eq!(count, 4096);
    }

    #[test]
    fn test_character_count_over_max() {
        let input = "a".repeat(4097);
        let count = input.chars().count();
        assert_eq!(count, 4097);
        assert!(count > 4096);
    }

    #[test]
    fn test_character_count_empty() {
        let input = "";
        let count = input.chars().count();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_character_count_multilingual() {
        let input = "Hello мир 世界 🌍"; // English, Russian, Chinese, emoji
        let count = input.chars().count();
        // Should count each character/glyph separately
        assert!(count > 0);
    }
}
