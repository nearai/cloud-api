//! E2E tests for dedicated audio API endpoints (/v1/audio/transcriptions and /v1/audio/speech)

mod common;

use common::*;

/// Helper to build multipart form with file and optional fields
fn build_transcription_form(
    audio_data: Vec<u8>,
    model: &str,
    extra_fields: Vec<(&str, &str)>,
) -> axum_test::multipart::MultipartForm {
    let mut form = axum_test::multipart::MultipartForm::new()
        .add_part(
            "file",
            axum_test::multipart::Part::bytes(audio_data)
                .file_name("test.wav")
                .mime_type("audio/wav"),
        )
        .add_text("model", model);

    for (key, value) in extra_fields {
        form = form.add_text(key, value);
    }

    form
}

/// Helper function to create sample audio data (minimal WAV header + silence)
fn create_sample_audio() -> Vec<u8> {
    // Minimal WAV file: 44-byte header + 100 bytes of silence (zeros)
    vec![
        // RIFF header
        0x52, 0x49, 0x46, 0x46, // "RIFF"
        0x6C, 0x00, 0x00, 0x00, // File size - 8 (108 bytes)
        0x57, 0x41, 0x56, 0x45, // "WAVE"
        // fmt subchunk
        0x66, 0x6D, 0x74, 0x20, // "fmt "
        0x10, 0x00, 0x00, 0x00, // Subchunk1Size (16)
        0x01, 0x00, // AudioFormat (1 = PCM)
        0x01, 0x00, // NumChannels (1)
        0x44, 0xAC, 0x00, 0x00, // SampleRate (44100)
        0x88, 0x58, 0x01, 0x00, // ByteRate
        0x02, 0x00, // BlockAlign
        0x10, 0x00, // BitsPerSample (16)
        // data subchunk
        0x64, 0x61, 0x74, 0x61, // "data"
        0x64, 0x00, 0x00, 0x00, // Subchunk2Size (100)
        // 100 bytes of silence (zeros)
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ]
}

// ============================================================================
// AUDIO TRANSCRIPTION TESTS (/v1/audio/transcriptions)
// ============================================================================

#[tokio::test]
async fn test_audio_transcription_minimal_request() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00
    let api_key = get_api_key_for_org(&server, org.id).await;

    let audio_data = create_sample_audio();

    let response = server
        .post("/v1/audio/transcriptions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .multipart(
            axum_test::multipart::MultipartForm::new()
                .add_part(
                    "file",
                    axum_test::multipart::Part::bytes(audio_data)
                        .file_name("test.wav")
                        .mime_type("audio/wav"),
                )
                .add_text("model", "whisper-1"),
        )
        .await;

    // With mock provider, this should succeed
    assert_eq!(
        response.status_code(),
        200,
        "Transcription should succeed: {}",
        response.text()
    );

    let body: serde_json::Value = response.json();
    assert!(
        body.get("text").is_some(),
        "Response should contain 'text' field"
    );
}

#[tokio::test]
async fn test_audio_transcription_missing_file() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/audio/transcriptions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .multipart(axum_test::multipart::MultipartForm::new().add_text("model", "whisper-1"))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Missing file should return 400 Bad Request"
    );

    let body: serde_json::Value = response.json();
    assert_eq!(
        body["error"]["message"].as_str().unwrap_or(""),
        "file is required",
        "Should indicate file is required"
    );
}

#[tokio::test]
async fn test_audio_transcription_missing_model() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let audio_data = create_sample_audio();

    let response = server
        .post("/v1/audio/transcriptions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .multipart(
            axum_test::multipart::MultipartForm::new().add_part(
                "file",
                axum_test::multipart::Part::bytes(audio_data)
                    .file_name("test.wav")
                    .mime_type("audio/wav"),
            ),
        )
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Missing model should return 400 Bad Request"
    );

    let body: serde_json::Value = response.json();
    assert_eq!(
        body["error"]["message"].as_str().unwrap_or(""),
        "model is required",
        "Should indicate model is required"
    );
}

#[tokio::test]
async fn test_audio_transcription_with_language() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let audio_data = create_sample_audio();

    let response = server
        .post("/v1/audio/transcriptions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .multipart(build_transcription_form(
            audio_data,
            "whisper-1",
            vec![("language", "en")],
        ))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Transcription with language should succeed: {}",
        response.text()
    );

    let body: serde_json::Value = response.json();
    assert!(body.get("text").is_some(), "Response should contain text");
}

#[tokio::test]
async fn test_audio_transcription_with_response_format_json() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let audio_data = create_sample_audio();

    let response = server
        .post("/v1/audio/transcriptions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .multipart(build_transcription_form(
            audio_data,
            "whisper-1",
            vec![("response_format", "json")],
        ))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Transcription with JSON format should succeed"
    );

    let body: serde_json::Value = response.json();
    assert!(body.get("text").is_some());
}

#[tokio::test]
async fn test_audio_transcription_with_temperature() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let audio_data = create_sample_audio();

    let response = server
        .post("/v1/audio/transcriptions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .multipart(build_transcription_form(
            audio_data,
            "whisper-1",
            vec![("temperature", "0.5")],
        ))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Transcription with temperature should succeed"
    );

    let body: serde_json::Value = response.json();
    assert!(body.get("text").is_some());
}

#[tokio::test]
async fn test_audio_transcription_unauthorized() {
    let (server, _guard) = setup_test_server().await;

    let audio_data = create_sample_audio();

    let response = server
        .post("/v1/audio/transcriptions")
        .multipart(build_transcription_form(audio_data, "whisper-1", vec![]))
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "Missing API key should return 401 Unauthorized"
    );
}

#[tokio::test]
async fn test_audio_transcription_invalid_api_key() {
    let (server, _guard) = setup_test_server().await;

    let audio_data = create_sample_audio();

    let response = server
        .post("/v1/audio/transcriptions")
        .add_header("Authorization", "Bearer invalid_key")
        .multipart(build_transcription_form(audio_data, "whisper-1", vec![]))
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "Invalid API key should return 401"
    );
}

// ============================================================================
// AUDIO SPEECH TESTS (/v1/audio/speech)
// ============================================================================

#[tokio::test]
async fn test_audio_speech_minimal_request() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/audio/speech")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "tts-1",
            "input": "Hello, world!",
            "voice": "alloy"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Speech synthesis should succeed: {}",
        response.text()
    );

    // Response should be audio binary data
    let body = response.as_bytes();
    assert!(!body.is_empty(), "Response should contain audio data");
}

#[tokio::test]
async fn test_audio_speech_missing_model() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/audio/speech")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "input": "Hello, world!",
            "voice": "alloy"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Missing model should return 400"
    );
}

#[tokio::test]
async fn test_audio_speech_missing_input() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/audio/speech")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "tts-1",
            "voice": "alloy"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Missing input should return 400"
    );
}

#[tokio::test]
async fn test_audio_speech_missing_voice() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/audio/speech")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "tts-1",
            "input": "Hello, world!"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Missing voice should return 400"
    );
}

#[tokio::test]
async fn test_audio_speech_empty_input() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/audio/speech")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "tts-1",
            "input": "",
            "voice": "alloy"
        }))
        .await;

    assert_eq!(response.status_code(), 400, "Empty input should return 400");
}

#[tokio::test]
async fn test_audio_speech_input_exceeds_max_length() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create input exceeding 4096 characters
    let long_input = "a".repeat(4097);

    let response = server
        .post("/v1/audio/speech")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "tts-1",
            "input": long_input,
            "voice": "alloy"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Input exceeding 4096 chars should return 400"
    );

    let body: serde_json::Value = response.json();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("4096"),
        "Error should mention 4096 character limit"
    );
}

#[tokio::test]
async fn test_audio_speech_with_response_format_mp3() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/audio/speech")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "tts-1",
            "input": "Hello, world!",
            "voice": "alloy",
            "response_format": "mp3"
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "audio/mpeg"
    );
}

#[tokio::test]
async fn test_audio_speech_with_response_format_wav() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/audio/speech")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "tts-1",
            "input": "Hello, world!",
            "voice": "alloy",
            "response_format": "wav"
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "audio/wav"
    );
}

#[tokio::test]
async fn test_audio_speech_with_speed() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/audio/speech")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "tts-1",
            "input": "Hello, world!",
            "voice": "alloy",
            "speed": 1.5
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Speech with speed should succeed"
    );
}

#[tokio::test]
async fn test_audio_speech_different_voices() {
    let (server, _guard) = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let voices = vec!["alloy", "echo", "fable", "onyx", "nova", "shimmer"];

    for voice in voices {
        let response = server
            .post("/v1/audio/speech")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&serde_json::json!({
                "model": "tts-1",
                "input": "Hello, world!",
                "voice": voice
            }))
            .await;

        assert_eq!(
            response.status_code(),
            200,
            "Voice '{}' should succeed",
            voice
        );
    }
}

#[tokio::test]
async fn test_audio_speech_unauthorized() {
    let (server, _guard) = setup_test_server().await;

    let response = server
        .post("/v1/audio/speech")
        .json(&serde_json::json!({
            "model": "tts-1",
            "input": "Hello, world!",
            "voice": "alloy"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "Missing API key should return 401"
    );
}

#[tokio::test]
async fn test_audio_speech_insufficient_credits() {
    let (server, _guard) = setup_test_server().await;
    // Setup org with very small credit limit
    let org = setup_org_with_credits(&server, 1i64).await; // $0.000000001
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/audio/speech")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "tts-1",
            "input": "This is a longer text that should require more credits for synthesis",
            "voice": "alloy"
        }))
        .await;

    // Should fail with payment required or insufficient credits
    assert!(
        response.status_code() == 402 || response.status_code() == 400,
        "Insufficient credits should return 402 or 400"
    );
}
